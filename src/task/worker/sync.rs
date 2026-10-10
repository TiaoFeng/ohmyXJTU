//! 堅果雲 WebDAV 同步：測試連線、導入、上傳／下載與設定。
//!
//! 同步屬網路任務，因此由工作者執行（任務服務明文不經網路）。磁碟上的檔案
//! 本來就是加密的，這裡只做檔案傳輸與三方比對，不在記憶體中解開密文。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::sync::config::{self, FileRecord, SyncConfig, SyncFile};
use crate::sync::engine::{self, Plan};
use crate::sync::webdav::{Precondition, WebDav};
use crate::task::protocol::{Event, SyncStateView};

use super::Worker;

/// 導入時暫存的設定與記錄：解鎖後才寫得進 `sync.vault`。
pub(super) struct PendingSync {
    config: SyncConfig,
    records: Vec<(SyncFile, FileRecord)>,
}

impl Worker {
    /// 建立 WebDAV 端點（依設定）。
    fn webdav_for(&self, config: &SyncConfig) -> WebDav {
        WebDav::new(
            Arc::clone(&self.webdav),
            config.url.clone(),
            &config.account,
            &config.app_password,
        )
    }

    /// 解鎖後載入同步設定，並（有導入暫存時）寫回設定與記錄。
    pub(super) fn init_sync(&mut self, passphrase: &str) {
        match self.sync.init(passphrase) {
            Ok(Some(notice)) => self.emit(Event::Warning(notice)),
            Ok(None) => {}
            Err(err) => {
                let message = format!("同步设置不可用（{err}）；本次会话无法使用坚果云同步");
                self.sync.mark_unavailable(message.clone());
                self.emit(Event::Warning(message));
            }
        }
        if let Some(pending) = self.pending_sync.take()
            && let Err(err) = self.persist_pending_sync(pending)
        {
            self.emit(Event::Warning(format!("同步设置保存失败：{err}")));
        }
        // 未設定且存儲可用時不必打擾介面（預設就是未設定）；存儲不可用時一定要
        // 回報，否則介面只會顯示「配置并启用同步」，而那個動作必定失敗。
        if self.sync.config().is_some() || self.sync.unavailable_reason().is_some() {
            self.emit_sync_state();
        }
    }

    /// 把導入暫存的設定與記錄寫回 `sync.vault`。
    ///
    /// **先寫設定、再寫記錄**：`set_config` 在換伺服器／帳號時會作廢舊記錄
    ///（那些記錄指向另一個雲端），順序顛倒的話，導入當下剛算出、指向**新**目標的
    /// 記錄會被那一步清掉——之後的「立即同步」就會把「兩邊一致」誤報成衝突。
    fn persist_pending_sync(&mut self, pending: PendingSync) -> AppResult<()> {
        self.sync.set_config(pending.config)?;
        for (file, record) in pending.records {
            self.sync.set_record(file, record)?;
        }
        Ok(())
    }

    /// 回報目前同步設定（不含秘密）。
    fn emit_sync_state(&mut self) {
        let unavailable = self.sync.unavailable_reason().is_some();
        let view = match self.sync.config() {
            Some(config) => SyncStateView {
                configured: true,
                unavailable,
                url: config.url.clone(),
                account: config.account.clone(),
                auto_sync: config.auto_sync,
            },
            None => SyncStateView {
                unavailable,
                ..SyncStateView::default()
            },
        };
        self.emit(Event::SyncState(Box::new(view)));
    }

    /// 已設定時的連線設定（複製）；設定不可用時回錯。
    fn require_sync_config(&self) -> AppResult<SyncConfig> {
        let config = self
            .sync
            .config()
            .cloned()
            .ok_or_else(|| AppError::config("尚未配置坚果云同步"))?;
        config::validate(&config)?;
        Ok(config)
    }

    /// 測試連線與寫入權限（登入畫面與設定表單共用）。
    pub(super) fn sync_test_connection(&mut self, config: SyncConfig) -> AppResult<()> {
        let result = config::validate(&config).and_then(|()| self.webdav_for(&config).check());
        let event = match result {
            Ok(()) => Event::SyncTestResult {
                ok: true,
                message: "连接成功".to_owned(),
            },
            Err(err) => Event::SyncTestResult {
                ok: false,
                message: err.to_string(),
            },
        };
        self.emit(event);
        Ok(())
    }

    /// 從雲端導入：下載遠端檔案覆寫本機，暫存設定與記錄待解鎖後保存。
    pub(super) fn sync_import(&mut self, config: SyncConfig) -> AppResult<()> {
        config::validate(&config)?;
        let dav = self.webdav_for(&config);
        let records = engine::import(&dav, &self.sync_local)?;
        self.pending_sync = Some(PendingSync { config, records });
        self.emit(Event::SyncImported {
            message: "已从坚果云导入，请输入加密口令解锁".to_owned(),
        });
        Ok(())
    }

    /// 智慧同步：逐檔三方比對，只做無衝突的動作。
    ///
    /// 下載會覆寫本機檔案：只要評估出任何 `Plan::Pull`，就先暫停任務服務寫入，
    /// 並在**暫停生效後**重新核對該檔的本機指紋（評估到暫停之間使用者仍可能
    /// 寫入），已變更則改報衝突；且一旦真的下載過，錯誤路徑也必須重新鎖定
    ///（見 [`Self::settle_sync`]）。
    pub(super) fn sync_now(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let plans = self.evaluate_all(&dav)?;
        let paused = plans
            .iter()
            .any(|(_, _, evaluation)| evaluation.plan == Plan::Pull);
        if paused {
            self.pause_tasks_for_sync()?;
        }
        let outcome = self.apply_plans(&dav, plans);
        self.settle_sync(outcome, paused)
    }

    /// 暫停任務服務寫入並等待確認；失敗時補送一次恢復才回報。
    ///
    /// 逾時（`TaskReply` 有上限）代表**不知道暫停是否已生效**：不補送恢復的話，
    /// 存儲可能就此卡在暫停態，之後每次任務操作都失敗，直到重新解鎖。恢復是
    /// 幂等的，服務已停止時只是再一次送不出去，不影響回報的錯誤。
    fn pause_tasks_for_sync(&mut self) -> AppResult<()> {
        if let Err(err) = self.tasks.pause() {
            self.tasks.resume();
            return Err(err);
        }
        Ok(())
    }

    /// 唯讀評估所有同步檔案（只做 `HEAD` 與本機讀取，不寫入）。
    fn evaluate_all(
        &mut self,
        dav: &WebDav,
    ) -> AppResult<Vec<(SyncFile, PathBuf, engine::Evaluation)>> {
        let mut plans = Vec::new();
        for (file, path) in self.sync_local.clone() {
            let record = self.sync.record(file).clone();
            let evaluation = engine::evaluate(dav, file, &path, &record)?;
            plans.push((file, path, evaluation));
        }
        Ok(plans)
    }

    /// 依評估結果執行上傳／下載；衝突只記錄、不覆蓋。
    fn apply_plans(
        &mut self,
        dav: &WebDav,
        plans: Vec<(SyncFile, PathBuf, engine::Evaluation)>,
    ) -> AppResult<()> {
        let (mut uploaded, mut downloaded) = (0_u32, 0_u32);
        let mut conflicts: Vec<&'static str> = Vec::new();
        for (file, path, evaluation) in plans {
            match evaluation.plan {
                Plan::Noop => {}
                Plan::Push => {
                    let record = self.sync.record(file).clone();
                    let precondition = evaluation.precondition(record.etag.as_deref());
                    match engine::push(dav, file, &path, precondition) {
                        Ok(new) => {
                            self.sync.set_record(file, new)?;
                            uploaded += 1;
                        }
                        // 檢查後遠端被其他裝置改動（條件未通過）：不覆蓋，改報衝突。
                        Err(AppError::WebDavConflict) => conflicts.push(file.remote_name()),
                        Err(err) => return Err(err),
                    }
                }
                Plan::Pull => {
                    // 評估（`HEAD`）到暫停生效之間，使用者仍可能改動本機檔案；
                    // 暫停已生效後重新核對指紋，已變更就改報衝突，不用雲端覆寫
                    // 剛保存的修改。（有 Pull 計畫就一定有暫停，故重驗後到寫入
                    // 之間不會再有並行寫入。）
                    let record = self.sync.record(file).clone();
                    if engine::local_changed(&path, &record)? {
                        conflicts.push(file.remote_name());
                    } else if self.pull_file(dav, file, &path)? {
                        downloaded += 1;
                    }
                }
                Plan::Conflict => conflicts.push(file.remote_name()),
            }
        }
        self.finish_sync(&conflicts, uploaded, downloaded);
        Ok(())
    }

    /// 下載單一檔案，回傳是否確實覆寫了本機檔案。
    ///
    /// 覆寫後**立刻**立旗標 `sync_pulled`（在 `set_record` 之前）：之後無論
    /// `set_record` 或下一筆下載失敗，都由 [`Self::settle_sync`] 確保重新鎖定。
    fn pull_file(&mut self, dav: &WebDav, file: SyncFile, path: &Path) -> AppResult<bool> {
        let Some(new) = engine::pull(dav, file, path)? else {
            return Ok(false);
        };
        self.sync_pulled = true;
        self.sync.set_record(file, new)?;
        Ok(true)
    }

    /// 下載型同步的收尾：覆寫過本機就重新鎖定，否則恢復任務服務寫入。
    ///
    /// **錯誤路徑同樣處理**——這正是修復的核心：`engine::pull` 覆寫本機後若後續
    /// 步驟失敗，記憶體內容已與磁碟不一致，不重新鎖定就會被舊內容反過來覆寫。
    /// 重新鎖定會丟棄任務金鑰，無需（也不該）恢復寫入；`paused` 由下次解鎖的
    /// `TaskStore::init` 重設。
    fn settle_sync(&mut self, outcome: AppResult<()>, paused: bool) -> AppResult<()> {
        let pulled = std::mem::take(&mut self.sync_pulled);
        if pulled {
            self.relock_after_pull();
        } else if paused {
            self.tasks.resume();
        }
        outcome
    }

    /// 自動同步（背景）：僅在「本機變更、遠端未變」時上傳。
    ///
    /// 其餘情況（無變更、遠端也變了、遠端較新）一律不動：自動模式絕不覆蓋
    /// 遠端，也不在本機狀態可能因此變舊時下載——需要下載或解決衝突時，留給
    /// 使用者在設定中按「立即同步」。背景失敗只留提示，不當成操作失敗。
    pub(super) fn sync_auto(&mut self) -> AppResult<()> {
        if !self.sync.config().is_some_and(|config| config.auto_sync) {
            return Ok(());
        }
        if let Err(err) = self.run_auto_upload() {
            self.emit(Event::Warning(format!("自动同步失败：{err}")));
        }
        Ok(())
    }

    /// 執行自動上傳（無變更時不動作）。
    ///
    /// 自動模式只上傳、從不下載，因此不需暫停任務服務。
    fn run_auto_upload(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let mut uploaded = 0_u32;
        for (file, path) in self.sync_local.clone() {
            let record = self.sync.record(file).clone();
            let evaluation = engine::evaluate(&dav, file, &path, &record)?;
            if evaluation.plan != Plan::Push {
                continue;
            }
            let precondition = evaluation.precondition(record.etag.as_deref());
            match engine::push(&dav, file, &path, precondition) {
                Ok(new) => {
                    self.sync.set_record(file, new)?;
                    uploaded += 1;
                }
                // 自動模式絕不覆蓋遠端：檢查後遠端被改動時靜默略過。
                Err(AppError::WebDavConflict) => {}
                Err(err) => return Err(err),
            }
        }
        if uploaded > 0 {
            self.emit(Event::SyncDone {
                summary: format!("已自动上传 {uploaded} 个文件"),
            });
        }
        Ok(())
    }

    /// 以本機覆蓋遠端（強制；用於解決衝突）。
    pub(super) fn sync_push(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        let mut uploaded = 0_u32;
        for (file, path) in self.sync_local.clone() {
            if !path.is_file() {
                continue;
            }
            // 強制上傳：使用者主動要求以本機覆蓋遠端，故不帶條件請求。
            let new = engine::push(&dav, file, &path, Precondition::Any)?;
            self.sync.set_record(file, new)?;
            uploaded += 1;
        }
        self.emit(Event::SyncDone {
            summary: format!("已上传 {uploaded} 个文件"),
        });
        Ok(())
    }

    /// 以遠端覆蓋本機（強制；用於解決衝突）。
    pub(super) fn sync_pull(&mut self) -> AppResult<()> {
        let config = self.require_sync_config()?;
        let dav = self.webdav_for(&config);
        // 強制下載一定會覆寫本機檔案：先暫停任務服務寫入。
        self.pause_tasks_for_sync()?;
        let outcome = self.run_pull(&dav);
        self.settle_sync(outcome, true)
    }

    /// 逐檔以遠端覆寫本機並回報下載數。
    fn run_pull(&mut self, dav: &WebDav) -> AppResult<()> {
        let mut downloaded = 0_u32;
        for (file, path) in self.sync_local.clone() {
            if self.pull_file(dav, file, &path)? {
                downloaded += 1;
            }
        }
        self.emit(Event::SyncDone {
            summary: format!("已下载 {downloaded} 个文件"),
        });
        Ok(())
    }

    /// 下載已覆寫本機檔案：丟棄金鑰與會話，請介面回到解鎖畫面重新輸入口令。
    ///
    /// 記憶體中的憑證與任務清單仍是下載**前**的版本。不重建工作階段的話：
    ///
    /// - 任務服務的 `save()` 會用記憶體中的舊清單覆寫剛下載的 `tasks.vault`，
    ///   而緊接著的自動同步又會把舊清單上傳回雲端——兩邊的資料都被舊的蓋掉；
    /// - 憑證仍會沿用舊帳號，直到使用者自行重啟程式。
    ///
    /// 這裡把會話與任務／同步金鑰一併丟棄（任務內容仍在記憶體，但重新解鎖時
    /// `TaskStore::init` 會以磁碟上的新檔案完整覆寫）。重新解鎖同時是對下載
    /// 內容的驗證：口令不符或檔案不是本程式的容器都會當場回報，而不是安靜地
    /// 沿用舊狀態。
    fn relock_after_pull(&mut self) {
        self.reset_session_for_unlock();
        // 憑證檔本身也剛被雲端內容取代：記憶體中的憑證必須一併丟棄，否則它會
        // 反過來覆寫下載結果（見 `set_sync_config` 對「下載後重新解鎖」的說明）。
        // `discard_pending_vault` 共用同一段清理時刻意保留憑證，因此這一行留在這裡。
        self.credentials = None;
        self.emit(Event::SyncRelocked);
    }

    /// 儲存連線設定：先測試連線，通過才保存（「測試連線通過才啟用」）。
    pub(super) fn set_sync_config(&mut self, mut config: SyncConfig) -> AppResult<()> {
        config::validate(&config)?;
        self.webdav_for(&config).check()?;
        // 編輯既有設定時沿用目前的自動同步偏好，避免被重設為關閉。
        if let Some(existing) = self.sync.config() {
            config.auto_sync = existing.auto_sync;
        }
        self.sync.set_config(config)?;
        self.emit_sync_state();
        Ok(())
    }

    /// 切換自動同步。
    pub(super) fn set_sync_auto(&mut self, enabled: bool) -> AppResult<()> {
        self.sync.set_auto_sync(enabled)?;
        self.emit_sync_state();
        Ok(())
    }

    /// 清除同步設定與記錄。
    pub(super) fn clear_sync_config(&mut self) -> AppResult<()> {
        self.sync.clear()?;
        self.emit_sync_state();
        Ok(())
    }

    /// 回報同步結果或衝突。
    fn finish_sync(&mut self, conflicts: &[&'static str], uploaded: u32, downloaded: u32) {
        if conflicts.is_empty() {
            self.emit(Event::SyncDone {
                summary: format!("同步完成（上传 {uploaded}，下载 {downloaded}）"),
            });
        } else {
            self.emit(Event::SyncConflict {
                message: format!(
                    "以下文件在本机与云端都被修改，未自动覆盖：{}（请在设置中选择上传或下载）",
                    conflicts.join("、")
                ),
            });
        }
    }
}
