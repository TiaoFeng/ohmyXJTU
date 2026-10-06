//! 憑證與會話生命週期：保險庫操作、帳號／口令變更與訪問策略。
//!
//! 新憑證只在登入驗證成功後才寫回保險庫（`commit_pending_vault`）；取消、
//! 憑證被拒或流程失敗時由 `discard_pending_vault` 還原舊憑證並重建會話，
//! 避免未驗證的帳號留在記憶體或伺服器端的 cookie 裡。

use crate::config::AccessPolicy;
use crate::credentials::{Credentials, Secret};
use crate::error::AppResult;
use crate::session::{SessionManager, SiteKind};
use crate::sites::attendance::AttendanceSite;
use crate::sites::lms::LmsSite;
use crate::task::protocol::Event;

use super::{PendingVault, Worker};

impl Worker {
    /// 首次建立保險庫並開始會話。
    pub(super) fn create_vault(
        &mut self,
        passphrase: &str,
        credentials: Credentials,
    ) -> AppResult<()> {
        self.vault.store(passphrase, &credentials)?;
        self.start_session(credentials)?;
        // 任務服務與介面共用同一條通道（先進先出）：先把 `InitTasks` 送進去，
        // 之後介面在收到 `VaultReady` 時才可能送出的任務操作一定排在它後面。
        // 首次建立保險庫即以此口令準備好任務信封（任務檔首次保存時才落盤）。
        self.tasks.init(&passphrase.into());
        // 介面收到 [`Event::VaultReady`] 後會送出 `Job::Preload`：工作者隨即
        // 登入兩個站點並預載四個頁面（見 [`Worker::preload`]）。
        self.emit(Event::VaultReady);
        // 設定檔重建的提示要等介面進到主畫面（底欄）才看得見。
        self.report_config_rebuild();
        Ok(())
    }

    /// 解鎖保險庫並開始會話。
    pub(super) fn unlock(&mut self, passphrase: &str) -> AppResult<()> {
        let credentials = self.vault.load(passphrase)?;
        self.start_session(credentials)?;
        // 任務檔與保險庫共用同一組口令；解鎖後才有金鑰可以讀寫。先送
        // `InitTasks` 再回報 `VaultReady`：介面開始操作時服務已有口令。
        self.tasks.init(&passphrase.into());
        // 介面收到 [`Event::VaultReady`] 後會送出 `Job::Preload`：工作者隨即
        // 登入兩個站點並預載四個頁面（見 [`Worker::preload`]）。
        self.emit(Event::VaultReady);
        // 設定檔重建的提示要等介面進到主畫面（底欄）才看得見。
        self.report_config_rebuild();
        self.report_vault_permissions();
        Ok(())
    }

    /// 修改帳號：先驗舊口令，換用新憑證並重建會話，最後以登入驗證。
    pub(super) fn change_account(
        &mut self,
        passphrase: &str,
        credentials: Credentials,
    ) -> AppResult<()> {
        // 先以原口令解密，驗證口令正確（失敗會回報 [`AppError::WrongPassphrase`]）。
        self.vault.load(passphrase)?;
        // 記下保險庫中的舊憑證：登入失敗時還原記憶體中的憑證，避免用未驗證的新憑證繼續作業。
        let previous = self.rollback_credentials();
        // 換帳號：舊帳號的站點登入狀態、快取與頁面資料一律作廢，並換用新憑證登入。
        // 必須重建後端（全新 cookie jar）：只清狀態表不足以丟棄舊帳號在服務端
        // 留下的 SSO cookie，殘留登入態會讓新帳號的登入被判定為「已登入」
        // 而略過帳密提交。
        {
            let session = self.session_mut()?;
            // 先重建後端（可能失敗）：失敗時連憑證都還沒換，狀態維持一致。
            session.reset_session()?;
            session.set_credentials(credentials.clone());
        }
        self.credentials = Some(credentials.clone());
        self.retry = None;
        self.generation += 1;
        self.relogin.clear();
        self.retries.clear();
        self.pending_data.clear();
        self.cache.clear();
        // 課表快取與選定週次都屬於舊帳號。
        self.schedule_cache = None;
        self.schedule_week = None;
        // 失敗計數以 (帳號, 後端) 為鍵保存：換了帳號自然從 0 起算，
        // 同一帳號重試則保留——否則伺服器要求的驗證碼永遠不會出現。
        self.login_failure_key = None;
        self.emit(Event::SessionsCleared {
            account_changed: true,
        });
        // 暫存新憑證：只有登入成功才由 `commit_pending_vault` 寫回保險庫，
        // 因此打錯新密碼不會覆蓋正確的舊憑證。
        self.pending_vault = Some(PendingVault {
            passphrase: Secret::from(passphrase),
            credentials,
            previous,
        });
        // 立即登入以驗證新憑證（失敗時介面顯示登入錯誤，舊憑證保持不變）。
        self.begin_login(SiteKind::Attendance, None)
    }

    /// 憑證檔權限若過寬（例如由他處複製進來而帶有 0644），收緊並告知使用者。
    ///
    /// 只在讀取既有憑證後檢查；寫入本身已固定 0600，非 Unix 平台不檢查。
    fn report_vault_permissions(&mut self) {
        let path = self.vault.path().to_path_buf();
        if let Ok(true) = crate::io::ensure_private(&path) {
            self.emit(Event::Notice(format!(
                "凭证文件权限过宽（其他用户可读），已收紧为仅本人可读写：{}",
                path.display()
            )));
        }
    }

    /// 設定檔在啟動時損毀重建：解鎖後才提示。
    ///
    /// 重建會一併重設「已同意的協議版本」與「記住的學期」（`PRIVACY.md` 的
    /// `config.json` 説明有這項承諾）。提示不能提早到啟動時發：那時的畫面是
    /// 協議閱讀門或解鎖表單，兩者都不繪製底欄訊息，而且進到主畫面時
    /// `apply_vault_ready` 還會把訊息覆寫成「凭证已就绪」。
    ///
    /// `rebuilt` 不是持久化欄位，`mem::take` 同時保證同一次執行只提示一次。
    fn report_config_rebuild(&mut self) {
        if std::mem::take(&mut self.config.rebuilt) {
            self.emit(Event::Notice(
                "配置文件已损坏并重建：已同意的协议与记住的学期已重置".to_owned(),
            ));
        }
    }

    /// 修改加密口令：憑證保險庫與任務檔必須使用同一組口令。
    ///
    /// 兩者是獨立的檔案，無法一起原子寫入；順序固定為「先驗舊口令 → 先寫
    /// 任務檔 → 再寫保險庫」，保險庫寫入失敗時把任務檔換回舊口令，避免留下
    /// 「保險庫是新口令、任務檔是舊口令」的不一致狀態。
    pub(super) fn change_passphrase(&mut self, old: &str, new: &str) -> AppResult<()> {
        // 先以舊口令解密，驗證口令正確（失敗會回報 [`AppError::WrongPassphrase`]）。
        let credentials = self.vault.load(old)?;
        self.tasks.rekey(&new.into())?;
        if let Err(err) = self.vault.store(new, &credentials) {
            // 保險庫仍是舊口令：把任務檔換回舊口令。回復失敗時明確回報，
            // 讓使用者知道任務檔可能需要以舊口令重新解鎖。
            if let Err(rollback) = self.tasks.rekey(&old.into()) {
                self.emit(Event::Notice(format!(
                    "口令修改失败，且任务文件未能还原（{rollback}）；请以原口令重新解锁"
                )));
            }
            return Err(err);
        }
        // 待存憑證是以舊口令加密的計畫：口令已改變，該計畫立即失效（並還原舊憑證），
        // 否則稍後登入成功會用舊口令覆寫保險庫，把新口令蓋回去。
        self.discard_pending_vault();
        self.emit(Event::PassphraseUpdated);
        Ok(())
    }

    /// 建立新的會話管理器（不預先登入任何站點）。
    fn start_session(&mut self, credentials: Credentials) -> AppResult<()> {
        let mut session = SessionManager::new(&self.config)?;
        session.register(Box::new(AttendanceSite));
        session.register(Box::new(LmsSite));
        session.set_credentials(credentials.clone());
        self.session = Some(session);
        self.credentials = Some(credentials);
        self.flow = None;
        self.retry = None;
        // 新會話（換帳號或切換訪問模式）：舊的預載待辦一併作廢。
        self.preload_pending = false;
        // 待存憑證屬於舊帳號：換帳號後一律作廢。
        self.pending_vault = None;
        // 舊帳號的登入失敗計數不再適用。
        self.login_failures.clear();
        self.login_failure_key = None;
        // 換帳號後舊任務與快取一律作廢，進行中的資料任務不再回報。
        self.generation += 1;
        self.relogin.clear();
        self.retries.clear();
        self.pending_data.clear();
        self.cache.clear();
        // 課表快取與選定週次都屬於舊帳號：一併作廢（介面也會清除頁面資料）。
        self.schedule_cache = None;
        self.schedule_week = None;
        // 舊帳號的站點登入狀態與頁面資料已失效：介面應清除。
        self.emit(Event::SessionsCleared {
            account_changed: true,
        });
        Ok(())
    }

    /// 切換訪問策略（只存檔與調整會話；後續登入由各頁面按需進行）。
    pub(super) fn set_access_policy(&mut self, policy: AccessPolicy) -> AppResult<()> {
        let previous = self.config.access_policy;
        self.config.access_policy = policy;
        if let Err(err) = self.config.save() {
            // 寫入失敗：保留原設定，不變更已生效的策略。
            self.config.access_policy = previous;
            return Err(err);
        }

        if let Some(session) = self.session.as_mut() {
            session.set_access_policy(policy);
        }
        // 進行中的登入流程配着舊的後端與路線，續用它會在完成登入時把舊客戶端
        // 的 cookie 與新的訪問方式湊在一起（見 `SessionManager::set_access_policy`）。
        // 一併取消：介面會關閉登入覆蓋層，之後的登入重新走新模式的完整流程。
        if self.flow.is_some() || self.pending_vault.is_some() {
            self.cancel_login()?;
        }
        // 訪問方式變更：進行中的資料任務作廢，快取失效。
        // 保存設定本身不觸發登入，後續登入由各頁面按需進行。
        self.generation += 1;
        self.relogin.clear();
        self.retries.clear();
        self.cache.clear();
        // 訪問方式變更：課表快取一併作廢（重新登入後重建）；選定週次保留。
        self.schedule_cache = None;
        // 連線與登入態已重建：介面清除站點登入狀態；既有頁面資料仍有效，
        // 只解除因任務作廢而卡住的載入狀態。
        self.emit(Event::SessionsCleared {
            account_changed: false,
        });
        self.emit(Event::AccessPolicyUpdated(policy));
        Ok(())
    }

    /// 丟棄待存憑證，還原舊憑證，並作廢切換期間建立的新會話。
    ///
    /// 新憑證只有在登入成功後才寫入保險庫；取消、憑證被拒或流程失敗時，
    /// 記憶體中的憑證必須回到保險庫仍保存的舊憑證，否則之後的自動重登會拿
    /// 一組從未驗證、也沒被保存的憑證去登入。
    ///
    /// 只還原帳密並不夠：新帳號在登入過程中可能已在伺服器端留下登入態
    ///（cookie）。不重建後端的話，之後任何一次登入都會被判定為「已登入」
    /// 而略過帳密提交，畫面就會出現新帳號的資料。
    pub(super) fn discard_pending_vault(&mut self) {
        let Some(pending) = self.pending_vault.take() else {
            return;
        };
        // 進行中的登入流程屬於已放棄的切換：一併作廢。
        self.flow = None;
        if let Some(previous) = pending.previous {
            if let Some(session) = self.session.as_mut() {
                session.set_credentials(previous.clone());
            }
            self.credentials = Some(previous);
        }
        // 重建後端（新的 cookie jar）以丟棄新帳號留下的登入態；失敗時不能
        // 繼續沿用被污染的會話——那會讓後續請求繼續帶著新帳號的 cookie，
        // 畫面顯示成新帳號的資料。此時直接停用會話，並請使用者重新解鎖。
        let reset = self.session.as_mut().map(|session| session.reset_session());
        if let Some(Err(err)) = reset {
            self.session = None;
            // 會話停用後介面會回到解鎖畫面：任務金鑰一併丟棄，重新解鎖時重建。
            self.tasks.lock();
            self.emit(Event::SessionDisabled(format!(
                "无法建立新的会话，已停用当前会话：{err}"
            )));
        }
        // 切換期間取得的資料與進行中的任務都屬於新帳號：一併作廢。
        // （排隊中的資料任務不在此列：它們會以還原後的帳號重新執行。）
        self.generation += 1;
        self.cache.clear();
        // 課表快取屬於被丟棄的切換：作廢（選定週次保留，介面未清空）。
        self.schedule_cache = None;
        self.login_failure_key = None;
    }

    /// 失敗時要還原的憑證。
    ///
    /// 保險庫中真正保存的那一組憑證優先：當上一次切換尚未結束（例如驗證碼填錯
    /// 後改輸入另一組帳密）時，`self.credentials` 已經是那組「尚未驗證、也還沒
    /// 寫回保險庫」的憑證，不能拿它當作還原目標。
    pub(super) fn rollback_credentials(&self) -> Option<Credentials> {
        self.pending_vault
            .as_ref()
            .and_then(|pending| pending.previous.clone())
            .or_else(|| self.credentials.clone())
    }

    /// 記錄使用者已同意的用户协议版本；寫入失敗時不變更已保存的版本。
    pub(super) fn accept_agreement(&mut self) -> AppResult<()> {
        let previous = self.config.privacy_version.clone();
        self.config.privacy_version = Some(crate::privacy::VERSION.to_owned());
        if let Err(err) = self.config.save() {
            self.config.privacy_version = previous;
            return Err(err);
        }
        self.emit(Event::AgreementAccepted);
        Ok(())
    }

    /// 把等待中的憑證寫入保險庫；寫入失敗不影響已完成的登入。
    ///
    /// 等待期間加密口令可能已被更改（例如換帳號失敗後改口令）：寫入前先確認
    /// 待存口令仍能解開現行檔案，否則寫入會把保險庫改回舊口令、新口令失效。
    pub(super) fn commit_pending_vault(&mut self) {
        let Some(pending) = self.pending_vault.take() else {
            return;
        };

        if self.vault.load(&pending.passphrase).is_err() {
            self.emit(Event::CredentialSaveFailed(
                "登录成功，但加密口令已变更，未保存新的账号凭据".to_owned(),
            ));
            return;
        }

        match self.vault.store(&pending.passphrase, &pending.credentials) {
            Ok(()) => self.emit(Event::AccountUpdated),
            Err(err) => self.emit(Event::CredentialSaveFailed(format!(
                "登录成功，但凭据保存失败：{err}"
            ))),
        }
    }
}
