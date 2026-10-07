//! 互動式登入：驅動器生命週期、驗證碼／簡訊驗證與失敗重試。
//!
//! 登入由 [`LoginDriver`] 驅動：需要驗證碼或簡訊驗證時回報事件，使用者輸入後
//! 再繼續；登入態失效時由調度核心自動重新登入並重試原本的任務。失敗計數以
//!（帳號, 後端）為鍵保存，伺服器要求的驗證碼門檻才能跨驅動器累計。

use std::time::Instant;

use crate::auth::{AccountType, LoginDriver, LoginReply};
use crate::credentials::{Credentials, Secret};
use crate::error::{AppError, AppResult};
use crate::session::{AccessMode, LoginStage, SiteKind};
use crate::task::protocol::{Event, Job, failed_target_of};

use super::timing::Phase;
use super::{LoginFlow, PendingVault, Worker};

impl Worker {
    /// 登入失敗計數的鍵值：同帳號且同後端（直連／WebVPN）才累計。
    fn login_failure_key_for(
        &self,
        username: &str,
        site: SiteKind,
    ) -> (String, Option<AccessMode>) {
        (
            username.to_owned(),
            self.session
                .as_ref()
                .and_then(|session| session.resolved_access_mode(site)),
        )
    }

    /// 保存目前登入嘗試的失敗次數。
    fn store_login_failures(&mut self, count: u32) {
        let Some(key) = self.login_failure_key.clone() else {
            return;
        };
        if count == 0 {
            self.login_failures.remove(&key);
        } else {
            self.login_failures.insert(key, count);
        }
    }

    /// 清除目前登入嘗試的失敗次數（登入成功時）。
    fn clear_login_failures(&mut self) {
        if let Some(key) = self.login_failure_key.take() {
            self.login_failures.remove(&key);
        }
    }

    /// 結束一次登入的計時（登入成功、憑證被拒或取消時）。
    ///
    /// 只有作業載入計時進行中才納入：其他頁面的登入時間不屬於那一次載入。
    fn record_login(&mut self) {
        if self.timing.is_idle() {
            self.login_started = None;
            return;
        }
        if let Some(started) = self.login_started.take() {
            self.timing.record(Phase::Login, started, 1);
        }
    }

    /// 取消進行中的登入流程（介面關閉登入覆蓋層時）。
    ///
    /// 丟棄登入驅動器、待存憑證、待重試任務與暫存的驗證碼圖片：登入互動期間
    /// 資料任務一律延後，若不取消，關閉覆蓋層後使用者按 `r` 送出的任務會永遠
    /// 排不到。登入流程即使已經結束（憑證被拒），等待重登的頁面仍會被收斂；
    /// 這種情況下不覆蓋介面已顯示的提示。結束時一律發送
    /// [`Event::LoginCancelled`]，作為介面清除「等待取消」狀態的依據。
    pub(super) fn cancel_login(&mut self) -> AppResult<()> {
        self.record_login();
        // 即使登入流程本身已經結束，等待重登的資料任務仍可能留著：例如憑證
        // 被拒時流程與待存憑證都已丟棄（`flow`、`pending_vault` 皆為 `None`），
        // 但 `retry` 還握著原任務。不收拾它的話，該頁會永遠停在「載入中」
        //（登入互動期間資料任務一律延後，取消後沒有事件會再觸發）。
        let had_login = self.flow.is_some() || self.pending_vault.is_some();
        self.flow = None;
        // 使用者主動取消：一併放棄待補做的預載，不讓背景擅自重新登入。
        self.preload_pending = false;
        self.discard_pending_vault();
        self.settle_pending_retry();
        // 無論先前有無進行中的登入都必須回報取消完成：介面據此清除等待狀態，
        // 否則它會永遠停在那裡，之後真正的新登入事件會被誤擋。
        self.emit(Event::LoginCancelled);
        if had_login {
            self.clear_captcha();
            self.emit(Event::Notice("已取消登录流程，可重新刷新页面".to_owned()));
        }
        Ok(())
    }

    /// 丟棄等待重登的資料任務，並通知介面收斂該頁的載入狀態。
    ///
    /// 呼叫端：[`Self::cancel_login`]（使用者取消登入，不再重試）、
    /// [`Self::finish_login`]（登入流程沒有真的驗證成功）與
    /// [`Worker::report_data_failure`]（槽即將被另一個失敗的任務取代）。
    pub(super) fn settle_pending_retry(&mut self) {
        if let Some(job) = self.retry.take() {
            self.emit(Event::LoadingCancelled {
                target: failed_target_of(&job),
            });
        }
    }

    /// 等待重登的任務重新取得自動重登與重試額度（使用者手動重試時）。
    ///
    /// 額度按任務鍵各自保存：手動重試只為「正在等待的任務」重新計算，
    /// 不影響其他任務的額度。
    fn reset_pending_retry_budget(&mut self) {
        if let Some(key) = self.retry.as_ref().and_then(Job::data_key) {
            self.relogin.reset(&key);
            self.retries.reset(&key);
        }
    }

    /// 開始登入：取得第一個登入步驟後才回報進度。
    pub(super) fn begin_login(&mut self, site: SiteKind, retry: Option<Job>) -> AppResult<()> {
        let credentials = self
            .credentials
            .clone()
            .ok_or_else(|| AppError::config("尚未解锁凭证"))?;
        // 記下本次登入的站點：失敗訊息與介面重試都要能指出是哪個站點。
        self.login_site = Some(site);
        // 本次登入的計時起點（診斷用；登入成功或結束時結算）。
        self.login_started = Some(Instant::now());
        // 新的一輪登入：重新觀察是否真的提交過帳密。
        self.login_submitted_credentials = false;
        // 重新開始登入時丟棄上一個（多半已失敗的）流程與其驗證碼圖片。
        self.flow = None;
        self.clear_captcha();

        // 先取得登入步驟（這裡就會向登入入口發第一個請求），成功後才告訴
        // 介面「正在登入」：否則離線等情況下介面會先顯示進度，之後卻收不到
        // 任何後續事件而卡在該畫面。
        let stage = self.session_mut()?.next_login_step(site)?;
        self.emit(Event::LoginProgress(format!("正在登录{site}…")));
        self.drive(stage, site, credentials, retry)
    }

    /// 依登入階段推進流程（完成時收尾）。
    fn drive(
        &mut self,
        stage: LoginStage,
        site: SiteKind,
        credentials: Credentials,
        retry: Option<Job>,
    ) -> AppResult<()> {
        match stage {
            LoginStage::Done => self.finish_login(site, retry),
            LoginStage::Drive(mut driver) => {
                // 沿用同帳號、同後端的失敗次數：伺服器端的驗證碼門檻以連續失敗
                // 次數計算，重試時歸零會讓驗證碼永遠不會被要求。
                let key = self.login_failure_key_for(&credentials.username, site);
                driver.set_fail_count(self.login_failures.get(&key).copied().unwrap_or(0));
                self.login_failure_key = Some(key);
                let reply = driver.start(&credentials, AccountType::Undergraduate)?;
                // 同一次登入可能經過多個驅動器（WebVPN 後端 → 站點），
                // 只要其中任一個提交過帳密，就算驗證過新憑證。
                self.login_submitted_credentials |= !driver.used_existing_session();
                let failures = driver.fail_count();
                self.store_login_failures(failures);
                self.flow = Some(LoginFlow {
                    site,
                    driver,
                    retry,
                });
                let result = self.handle_reply(reply);
                if let Err(err) = &result
                    && !matches!(err, AppError::VerificationRetry(_))
                {
                    // 流程已無法繼續（例如取不到驗證碼圖片、身份選擇失敗）：不能
                    // 留著它——工作者主迴圈在 `flow.is_some()` 期間一律延後資料
                    // 任務，那樣頁面會卡在「載入中」直到使用者關掉登入彈窗。
                    //
                    // `VerificationRetry`（驗證碼填錯）是唯一的例外：流程仍可用，
                    // 使用者重輸即可（`handle_reply` 目前不會回這個錯誤，保留判斷
                    // 是為了不誤傷未來的可重試路徑）。
                    self.abort_broken_login();
                }
                result
            }
        }
    }

    /// 作廢一個不可能再被推進的登入流程。
    ///
    /// 工作者主迴圈在 `flow.is_some()` 期間一律延後資料任務，因此留下「沒有
    /// 人會再推進它」的流程會讓頁面卡在「載入中」——使用者按 `r` 送出的任務
    /// 只會被合併進待執行佇列。
    ///
    /// 等待重登的任務（`Worker::retry`）刻意不動：它是「登入成功後要續跑的
    /// 那一個任務」，使用者按 Enter 重試登入成功時仍要跑它（見
    /// [`Self::finish_login`]）；真的放棄則由 [`Self::cancel_login`] 收斂。
    pub(super) fn abort_broken_login(&mut self) {
        if self.flow.is_none() {
            return;
        }
        self.flow = None;
        // 驗證碼圖片屬於已作廢的流程，不再有用。
        self.clear_captcha();
    }

    /// 處理登入驅動器的回報（成功、失敗、驗證碼、簡訊或身份選擇）。
    pub(super) fn handle_reply(&mut self, reply: LoginReply) -> AppResult<()> {
        // 先把驅動器目前的失敗次數存回：失敗即丟棄驅動器，下次重試會重建，
        // 次數必須活過重建，伺服器要求的驗證碼才會出現。
        let failures = self.flow.as_ref().map(|flow| flow.driver.fail_count());
        if let Some(failures) = failures {
            self.store_login_failures(failures);
        }
        match reply {
            LoginReply::Success => {
                // 登入成功：該帳號的失敗計數歸零。
                self.clear_login_failures();
                self.complete_flow()
            }
            LoginReply::Fail { message } => {
                let site = self
                    .flow
                    .as_ref()
                    .map(|flow| flow.site)
                    .or(self.login_site)
                    .unwrap_or(SiteKind::Attendance);
                if self
                    .flow
                    .as_ref()
                    .is_some_and(|flow| flow.driver.last_attempt_submitted_captcha())
                {
                    // 圖片驗證碼填錯：流程與待存憑證都保留，介面留在輸入畫面讓
                    // 使用者直接重輸，不當成帳密錯誤而作廢整個帳號切換。
                    self.emit(Event::VerificationRetry {
                        site,
                        message: message.clone(),
                    });
                    // 伺服器多半已作廢舊驗證碼：盡力換一張新圖；換不到就沿用
                    // 舊圖（使用者至少還能重輸一次）。
                    let _ = self.show_captcha();
                    return Ok(());
                }
                // 憑證被拒：丟棄待存憑證（並還原舊憑證），不覆蓋保險庫中的舊憑證。
                self.record_login();
                self.flow = None;
                // 憑證被拒：預載也做不成，放棄待補做的那一次。
                self.preload_pending = false;
                self.discard_pending_vault();
                self.clear_captcha();
                self.emit(Event::LoginFailed { site, message });
                Ok(())
            }
            LoginReply::NeedCaptcha => self.show_captcha(),
            LoginReply::NeedMfa => {
                let phone = match self.driver_mut()?.mfa_phone() {
                    Ok(phone) => Some(phone),
                    Err(err) => {
                        // 取不到手機號時明確告知，不靜默顯示成「沒有手機號」。
                        self.emit(Event::Warning(format!("无法获取短信验证手机号：{err}")));
                        None
                    }
                };
                self.emit(Event::LoginNeedsMfa { phone, sent: false });
                Ok(())
            }
            LoginReply::NeedAccountChoice(_) => {
                // 本科身份由驅動器自動選擇；若選擇失敗會回報錯誤。
                let reply = self.driver_mut()?.resume()?;
                self.handle_reply(reply)
            }
        }
    }

    /// 完成目前驅動器並提交給站點（可能還有下一個驅動器）。
    fn complete_flow(&mut self) -> AppResult<()> {
        let Some(flow) = self.flow.take() else {
            return Ok(());
        };
        let stage = self
            .session_mut()?
            .complete_login_step(flow.site, &flow.driver)?;
        let credentials = self
            .credentials
            .clone()
            .ok_or_else(|| AppError::config("尚未解锁凭证"))?;
        self.drive(stage, flow.site, credentials, flow.retry)
    }

    /// 登入成功收尾：回報結果、保存憑證並續跑等待中的任務。
    pub(super) fn finish_login(&mut self, site: SiteKind, retry: Option<Job>) -> AppResult<()> {
        // 登入流程結束：先結算登入計時，再收尾（收尾會續跑等待中的任務，
        // 那是下一次載入的工作，不屬於登入）。
        self.record_login();
        // 登入成功：驗證碼圖片不再需要，立即清除。
        self.clear_captcha();
        // 換帳號時若整個流程都沒有提交帳密，代表伺服器端仍有舊帳號的登入態，
        // 新憑證從未被驗證：不得寫回保險庫（只丟棄待存狀態並回報錯誤）。
        if self.pending_vault.is_some() && !self.login_submitted_credentials {
            self.discard_pending_vault();
            self.settle_pending_retry();
            return Err(AppError::protocol(
                "当前会话仍处于登录状态，无法验证新账号（已保留原有凭证）",
            ));
        }
        let mode = self
            .session
            .as_ref()
            .and_then(|session| session.access_mode(site));
        // 先回報登入成功（主要結果），再處理憑證保存（附帶副作用）。介面的
        // 訊息是「後到者覆蓋先前的」，因此保存失敗必須是最後一個事件，否則
        // 會被緊接著的「登录成功」蓋掉，使用者就看不到失敗提醒。
        self.emit(Event::LoginSucceeded { site, mode });
        // 登入成功後才更新保險庫，失敗的憑證不會覆蓋舊憑證。
        self.commit_pending_vault();
        // 登入成功後續跑等待中的任務（可能是資料任務或控制任務）。
        //
        // 兩個來源都要跑：`retry` 是發起這次登入的任務（通常為 `None`），
        // `self.retry` 是等待重登的資料任務。**不可**寫成
        // `retry.or(self.retry.take())`：`Option::or` 的參數是值傳遞，
        // `take()` 一定會執行，但當 `retry` 已是 `Some` 時，取出後的那個任務
        // 就無人接手而靜默遺失。
        for job in [retry, self.retry.take()].into_iter().flatten() {
            if job.is_control() {
                let _ = self.handle_control(job);
            } else {
                self.run_data_job(job);
            }
        }
        // 預載先前被這次登入擋下（見 [`Self::preload`]）：登入已經結束，補做。
        // 由預載自己發起的登入帶著 [`Job::Preload`] 走上面的分支，不會重複。
        if std::mem::take(&mut self.preload_pending) {
            let _ = self.handle_control(Job::Preload);
        }
        Ok(())
    }

    pub(super) fn submit_captcha(&mut self, code: &str) -> AppResult<()> {
        let reply = self.driver_mut()?.submit_captcha(code)?;
        self.handle_reply(reply)
    }

    pub(super) fn refresh_captcha(&mut self) -> AppResult<()> {
        self.show_captcha()
    }

    /// 取得並顯示新的驗證碼圖片（覆寫暫存檔）。
    fn show_captcha(&mut self) -> AppResult<()> {
        let path = self.driver()?.fetch_captcha()?;
        self.captcha_path = Some(path.clone());
        self.emit(Event::LoginNeedsCaptcha(path));
        Ok(())
    }

    /// 清除暫存的驗證碼圖片（登入結束或重新開始時；失敗忽略）。
    pub(super) fn clear_captcha(&mut self) {
        if let Some(path) = self.captcha_path.take() {
            let _ = crate::auth::captcha::remove(&path);
        }
    }

    pub(super) fn send_mfa_code(&mut self) -> AppResult<()> {
        let phone = self.driver_mut()?.send_mfa_code()?;
        self.emit(Event::LoginNeedsMfa {
            phone: Some(phone),
            sent: true,
        });
        Ok(())
    }

    pub(super) fn verify_mfa_code(&mut self, code: &str) -> AppResult<()> {
        self.driver_mut()?.verify_mfa_code(code)?;
        let reply = self.driver_mut()?.resume()?;
        self.handle_reply(reply)
    }

    pub(super) fn retry_login(&mut self, site: SiteKind) -> AppResult<()> {
        // 使用者手動重試：等待重登的任務重新取得自動重登額度。
        self.reset_pending_retry_budget();
        self.begin_login(site, None)
    }

    /// 以使用者重新輸入的憑證重試登入；先驗證口令，登入成功後才寫入保險庫。
    ///
    /// `site` 為原本失敗的站點：重試不應被另一個站點的可達性牽制
    ///（例如思源學堂失敗卻要去連考勤系統）。
    pub(super) fn retry_with_account(
        &mut self,
        site: SiteKind,
        passphrase: &str,
        credentials: Credentials,
    ) -> AppResult<()> {
        // 口令錯誤時回報 [`AppError::WrongPassphrase`]，舊憑證不受影響。
        self.vault.load(passphrase)?;

        // 使用者手動重試：等待重登的任務重新取得自動重登額度。
        self.reset_pending_retry_budget();
        // 先記下保險庫中的舊憑證，取消或憑證被拒時才能還原（見 `discard_pending_vault`）。
        let previous = self.rollback_credentials();
        // 失敗計數以 (帳號, 後端) 為鍵保存：重複輸入同一帳號（含目前生效的仍是
        // 舊帳號的情形）必須保留計數，否則驗證碼永遠不會出現；換成別的帳號時
        // 它的鍵自然由 0 起算。
        self.login_failure_key = None;
        // 完整走一次帳號切換：重建後端（丟棄舊 cookie）並換用新憑證，
        // 否則站點仍在登入狀態時會直接進入成功分支，完全跳過網路驗證。
        {
            let session = self.session_mut()?;
            // 先重建後端（可能失敗）：失敗時連憑證都還沒換，狀態維持一致。
            session.reset_session()?;
            session.set_credentials(credentials.clone());
        }
        self.credentials = Some(credentials.clone());
        // 舊帳號的站點登入狀態、快取與頁面資料一律作廢。
        self.generation += 1;
        self.pending_data.clear();
        self.cache.clear();
        // 課表快取與選定週次都屬於舊帳號。
        self.schedule_cache = None;
        self.schedule_week = None;
        // 「當前學期」是上一次查考勤系統的結果，同樣屬於舊帳號（與
        // `change_account` 同一組清理；`chosen_term` 是使用者的選擇，保留）。
        self.known_term = None;
        self.emit(Event::SessionsCleared {
            account_changed: true,
        });
        self.pending_vault = Some(PendingVault {
            passphrase: Secret::from(passphrase),
            credentials,
            previous,
        });
        self.begin_login(site, None)
    }

    fn driver(&self) -> AppResult<&LoginDriver> {
        self.flow
            .as_ref()
            .map(|flow| flow.driver.as_ref())
            .ok_or_else(|| AppError::config("当前没有进行中的登录流程"))
    }

    fn driver_mut(&mut self) -> AppResult<&mut LoginDriver> {
        self.flow
            .as_mut()
            .map(|flow| flow.driver.as_mut())
            .ok_or_else(|| AppError::config("当前没有进行中的登录流程"))
    }
}
