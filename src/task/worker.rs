//! 背景工作執行緒。
//!
//! 所有網路請求都在這裡執行，介面只透過 [`Job`] 下指令、透過 [`Event`] 收結果
//!（協定型別見 `crate::task::protocol`）。登入流程由 [`LoginDriver`] 驅動：
//! 需要驗證碼或簡訊驗證時回報事件，使用者輸入後再繼續；登入態失效時會自動
//! 重新登入並重試原本的任務。
//!
//! 本檔只保留調度核心（任務佇列、去重與升級、代際取消、失敗路由）；依職責
//! 拆分的實作放在子模組：
//!
//! - `credentials`：憑證與會話生命週期（保險庫、帳號／口令、訪問策略）。
//! - `login`：互動式登入（驗證碼、簡訊、重試與失敗計數）。
//! - `data`：課表、考勤流水與思源學堂瀏覽。
//! - `homework`：作業載入的步進狀態機。
//! - `cache`：思源學堂課程／活動快取。
//!
//! 自訂義任務不在這裡：它們在專屬的任務服務執行緒上（見 `crate::task::tasks`），
//! 否則網路請求會把「新增任務」這種純本機操作也拖慢。
//!
//! 調度原則：
//!
//! - 控制任務（登入、設定、憑證）優先於資料任務；資料任務以「步進」執行，
//!   每一步之間先處理排隊中的控制任務，避免長查詢阻塞設定操作。
//! - 互動式資料任務（按 `o` 開啟活動網頁）同樣在步進邊界立即執行，不排在
//!   整輪載入之後；若其觸發重新登入，本輪載入先暫停並重新排隊。
//! - 重複的資料查詢會被合併；帳號或訪問模式變更後，進行中的資料任務立即
//!   中止且不再回報舊結果。
//! - 解鎖憑證後不預先登入任何站點：頁面需要時才按站點惰性登入，
//!   因此考勤系統故障不會拖垮思源學堂。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::Instant;

use crate::auth::LoginDriver;
use crate::config::Config;
use crate::credentials::{Credentials, Secret, Vault};
use crate::domain::semester::TermCode;
use crate::error::{AppError, AppResult};
use crate::session::{AccessMode, SessionManager, SiteKind};
use crate::task::protocol::{
    DataKey, Event, FailedTarget, Job, failed_target_of, is_account_switch_step, resource_of,
    site_of,
};
use crate::task::tasks::{self, TaskHandle};

mod cache;
mod credentials;
mod data;
mod homework;
mod login;
mod preload;
mod timing;

use cache::{LmsCache, ScheduleCache};
use timing::LoadTiming;

/// 自動重新登入的嘗試次數上限（首次 ＋ 1 次自動重登）。
///
/// 站點持續回報登入態失效時，「自動重登 → 重試 → 再失效」會形成無上限的
/// 迴圈；超過上限即停止自動重試並回報錯誤，由使用者手動按 `r` 重試
///（每次全新的使用者操作都會為該任務重新獲得額度）。
const MAX_LOGIN_ATTEMPTS: u8 = 2;

/// 連線層網路錯誤的嘗試次數上限（首次 ＋ 2 次重試）。
///
/// 僅用於「連線層」網路錯誤（逾時、連不上、DNS、TLS）：校內服務偶發的
/// 逾時或連線失敗多半是短暫抖動，自動重送同一個請求即可，不必勞煩使用者
/// 手動按 `r`。伺服器有回應但內容不符預期、憑證錯誤等不在此列
///（見 [`AppError::is_connection_error`]）。
///
/// 次數與逾時相乘即為最壞等待時間（3 × 15 秒）；刻意保持小，讓真正持續
/// 失敗的情況仍能快速回報。
const MAX_ATTEMPTS: u8 = 3;

/// 自動重試額度：以任務鍵（[`DataKey`]）各自計算，用完即停止自動重試。
///
/// 兩個上限各用一個實例（[`Worker::relogin`] 與 [`Worker::retries`]）：
/// 登入態失效必須重新登入才能恢復，而連線抖動只要重送請求；兩者的上限、
/// 觸發條件與歸零時機都不同，混在同一個計數器會讓「為什麼這次只重試一次」
/// 變得難以解釋。
///
/// 互動式任務（開啟活動網頁）與長載入（作業彙總）的額度互相獨立——一方
/// 重試不會讓另一方的失效被誤判；同一任務的重試鏈共用同一份額度（重試後
/// 再失效時遞減）。新的使用者操作（新的資料請求、手動重試、執行互動式
/// 任務）為該任務重新取得完整額度；工作階段重建、換帳號或切換訪問模式時
/// 全部清空。
#[derive(Debug)]
struct AttemptBudgets {
    /// 單一任務允許的總嘗試次數（首次 ＋ 重試）。
    max: u8,
    /// 各任務已使用的嘗試次數。
    used: HashMap<DataKey, u8>,
}

impl AttemptBudgets {
    /// 建立額度表：單一任務允許 `max` 次總嘗試（首次 ＋ `max - 1` 次重試）。
    fn new(max: u8) -> Self {
        Self {
            max,
            used: HashMap::new(),
        }
    }

    /// 讓指定任務重新取得完整額度（新的資料請求、執行互動式任務、
    /// 使用者手動重試）。
    fn reset(&mut self, key: &DataKey) {
        self.used.remove(key);
    }

    /// 清空所有任務的額度（工作階段重建、換帳號或切換訪問模式時）。
    fn clear(&mut self) {
        self.used.clear();
    }

    /// 嘗試為指定任務消耗一次額度；已達上限時回傳 `None`，否則回傳即將
    /// 進行的嘗試序號（2 起算，供進度提示使用）。
    ///
    /// 能走到這裡就代表「第 1 次嘗試已經失敗」，因此計數從 1 起算：上限 3
    /// 會依序回報 2、3，再下一次才回 `None`。
    fn try_consume(&mut self, key: &DataKey) -> Option<u8> {
        let used = self.used.entry(key.clone()).or_insert(1);
        if *used < self.max {
            *used += 1;
            Some(*used)
        } else {
            None
        }
    }
}

/// 進行中的登入流程。
struct LoginFlow {
    site: SiteKind,
    driver: Box<LoginDriver>,
    retry: Option<Job>,
}

/// 等待登入成功後才寫入保險庫的憑證。
struct PendingVault {
    /// 加密口令（寫入後隨即丟棄）。
    passphrase: Secret,
    /// 使用者重新輸入的帳號密碼。
    credentials: Credentials,
    /// 嘗試新憑證前的舊憑證：取消或憑證被拒時還原，避免記憶體中的憑證
    /// 與保險庫不一致（下次啟動、自動重登都應以保險庫為準）。
    previous: Option<Credentials>,
}

/// 資料載入失敗，連同真正失敗的站點。
///
/// 作業載入會先向考勤系統查當前學期、再向思源學堂查課程與提交記錄；只看任務
/// 種類（`Job::LoadHomework` → 思源學堂）會把考勤系統的登入失效誤報成思源學堂，
/// 重登與錯誤訊息都會指到錯的站點。
struct SiteFailure {
    /// 實際失敗的站點。
    site: SiteKind,
    /// 原始錯誤。
    err: AppError,
}

impl SiteFailure {
    /// 考勤系統的失敗。
    fn attendance(err: AppError) -> Self {
        Self {
            site: SiteKind::Attendance,
            err,
        }
    }
}

impl From<AppError> for SiteFailure {
    /// 作業載入的錯誤預設屬於思源學堂；考勤系統須以 [`SiteFailure::attendance`] 標記。
    fn from(err: AppError) -> Self {
        Self {
            site: SiteKind::Lms,
            err,
        }
    }
}

/// 背景工作執行緒。
struct Worker {
    jobs: Receiver<Job>,
    events: Sender<Event>,
    vault: Vault,
    config: Config,
    session: Option<SessionManager>,
    credentials: Option<Credentials>,
    flow: Option<LoginFlow>,
    retry: Option<Job>,
    pending_vault: Option<PendingVault>,
    /// 最近一次登入嘗試的站點：失敗訊息與介面重試據此定位，
    /// 不依赖任務種類猜測（考勤與思源學堂都可能發起登入）。
    login_site: Option<SiteKind>,
    /// 本次登入是否真的向伺服器提交過帳密。
    ///
    /// 伺服器端仍有登入態時，驅動器會直接回報成功而完全不提交帳密；
    /// 這種「登入」不能證明換帳號時的新憑證可用，不得寫回保險庫。
    login_submitted_credentials: bool,
    /// 各（帳號, 後端）已連續失敗的登入次數。
    ///
    /// 伺服器端的驗證碼門檻以「同一帳號連續失敗」計算，而驅動器每次重試都會
    /// 重建；次數保存於此並在重建後注入，門檻才達得到。
    login_failures: HashMap<(String, Option<AccessMode>), u32>,
    /// 最近一次登入嘗試的計數鍵值（帳號 + 後端）。
    login_failure_key: Option<(String, Option<AccessMode>)>,
    /// 最近一次取得的驗證碼圖片路徑（登入結束或重新開始時刪除）。
    captcha_path: Option<PathBuf>,
    /// 待執行的資料任務（依序、已去重）。
    pending_data: VecDeque<Job>,
    /// 資料任務代際：帳號或訪問模式變更時遞增，進行中的任務自動中止。
    generation: u64,
    /// 作業載入代際：使用者切換學期時遞增，進行中的作業載入自動中止。
    ///
    /// 學期變更不影響其他資料（課程清單不變，只有分區提示改變），因此不
    /// 共用 `generation`——那會連課程／活動載入一起取消。
    homework_epoch: u64,
    /// 自動重新登入額度（以任務鍵各自保存）。
    relogin: AttemptBudgets,
    /// 連線層網路錯誤的自動重試額度（以任務鍵各自保存）。
    retries: AttemptBudgets,
    /// 預載被進行中的登入擋下：該次登入收尾時補做一次（見 [`Worker::preload`]）。
    ///
    /// 使用者取消登入、憑證被拒或會話重建時一併放棄：那代表現在不該由背景
    /// 擅自重新登入。
    preload_pending: bool,
    /// 思源學堂課程／活動快取。
    cache: LmsCache,
    /// 自訂義任務服務的控制代碼（任務資料在專屬執行緒上，見 `task::tasks`）。
    tasks: TaskHandle,
    /// 課表快取（整學期課程；切換週次時重用）。
    schedule_cache: Option<ScheduleCache>,
    /// 使用者選擇的週次；`None` 代表跟隨當前週。
    schedule_week: Option<u32>,
    /// 本會話曾查得的考勤學期（供課程分區使用，不重複請求）。
    known_term: Option<TermCode>,
    /// 使用者在本工作階段按 `s` 明確選擇的學期。
    ///
    /// 明確的選擇優先於考勤的當前學期，否則「選擇要查看的學期」不會生效；
    /// 它不持久化，重新開啟程式後仍以考勤為權威，而設定檔中的
    /// `homework_term` 則作為考勤不可用時的後備。
    chosen_term: Option<TermCode>,
    /// 作業載入的分階段計時（診斷用，見 [`timing`]）。
    timing: LoadTiming,
    /// 最近一次自動重新登入的起點（診斷用）。
    login_started: Option<Instant>,
    /// 已收到結束指令；[`Worker::run`] 於迴圈開頭立即返回。
    shutdown: bool,
}

/// 啟動背景工作執行緒，回傳（任務送出端, 事件接收端）。
///
/// 介面送出的所有任務都先進任務服務（見 [`crate::task::tasks`]）：任務操作
/// 由服務自行處理（純本機，不會等網路），其餘任務原封不動轉給這裡的工作
/// 執行緒。介面因此只面對一條通道，卻不必和網路請求搶排隊。
pub fn spawn(config: Config, vault: Vault) -> AppResult<(Sender<Job>, Receiver<Event>)> {
    spawn_with_tasks(config, vault, crate::io::tasks_path()?)
}

/// 啟動背景工作執行緒與任務服務，並指定任務檔路徑。
///
/// 供測試隔離資料目錄使用；正式路徑見 [`spawn`]。
pub(crate) fn spawn_with_tasks(
    config: Config,
    vault: Vault,
    tasks_path: PathBuf,
) -> AppResult<(Sender<Job>, Receiver<Event>)> {
    let (job_tx, job_rx) = channel();
    let (worker_tx, worker_rx) = channel();
    let (event_tx, event_rx) = channel();

    tasks::serve(event_tx.clone(), worker_tx, job_rx, tasks_path)?;
    let tasks = TaskHandle::new(job_tx.clone());

    let mut worker = Worker {
        jobs: worker_rx,
        events: event_tx,
        vault,
        config,
        session: None,
        credentials: None,
        flow: None,
        retry: None,
        pending_vault: None,
        login_site: None,
        login_submitted_credentials: false,
        login_failures: HashMap::new(),
        login_failure_key: None,
        captcha_path: None,
        pending_data: VecDeque::new(),
        generation: 0,
        homework_epoch: 0,
        relogin: AttemptBudgets::new(MAX_LOGIN_ATTEMPTS),
        retries: AttemptBudgets::new(MAX_ATTEMPTS),
        preload_pending: false,
        cache: LmsCache::default(),
        schedule_cache: None,
        schedule_week: None,
        known_term: None,
        chosen_term: None,
        timing: LoadTiming::default(),
        login_started: None,
        shutdown: false,
        tasks,
    };

    thread::Builder::new()
        .name("ohmyXJTU-worker".to_owned())
        .spawn(move || worker.run())
        .map_err(|err| AppError::config(format!("无法启动后台任务线程：{err}")))?;

    Ok((job_tx, event_rx))
}

impl Worker {
    fn run(&mut self) {
        // 設定檔在啟動時損毁重建：提醒使用者協議同意與記住的學期已重設。
        if self.config.rebuilt {
            self.emit(Event::Notice(
                "配置文件已损坏并重建：已同意的协议与记住的学期已重置".to_owned(),
            ));
        }
        loop {
            // 資料載入中途收到結束指令：立即停止（排隊中的任務一併丟棄）。
            if self.shutdown {
                return;
            }
            let job = if self.flow.is_none() {
                match self.pending_data.pop_front() {
                    Some(job) => job,
                    None => match self.jobs.recv() {
                        Ok(job) => job,
                        // 介面已結束。
                        Err(_) => return,
                    },
                }
            } else {
                // 互動式登入進行中：只處理控制任務（例如驗證碼提交），
                // 資料任務延後到登入完成後再跑。
                match self.jobs.recv() {
                    Ok(job) => job,
                    Err(_) => return,
                }
            };

            if job.is_control() {
                if self.handle_control(job) {
                    return;
                }
            } else if self.flow.is_some() {
                // 登入尚未完成：資料任務排入待執行（同鍵去重，強制優先）。
                self.merge_data_job(job, None);
            } else {
                self.run_fresh_data_job(job);
            }
        }
    }

    /// 執行使用者發起的全新資料任務：讓該任務重新取得自動重登額度後再執行。
    ///
    /// 自動重登後的重試（[`Self::finish_login`]）與連線錯誤的自動重試都不
    /// 走這裡，額度才會遞減；新的刷新請求則重新獲得完整額度。
    fn run_fresh_data_job(&mut self, job: Job) {
        if let Some(key) = job.data_key() {
            self.relogin.reset(&key);
            self.retries.reset(&key);
        }
        // 每次全新的作業載入都是新的一次量測：自動重登後的重試走
        // [`Self::run_data_job`]，不會重置，登入時間因此計入同一輪。
        if matches!(job, Job::LoadHomework { .. }) {
            self.timing.begin(timing::enabled());
        }
        self.run_data_job(job);
    }

    /// 執行控制任務；回傳是否收到結束指令。
    fn handle_control(&mut self, job: Job) -> bool {
        let shutdown = matches!(job, Job::Shutdown);
        let what = job.label();
        let target = failed_target_of(&job);
        let site = self.login_site_of(&job);
        let resource = resource_of(&job);
        // 帳號切換失敗時必須丟棄待存憑證：交由 [`Self::dispatch_control`] 統一
        // 處理（含互動驗證步驟與連線錯誤的自動重試），這裡只需要清掉暫存的
        // 驗證碼圖片。
        if let Err(err) = self.dispatch_control(job) {
            // 登入類任務出錯即視為本次登入結束：清掉暫存的驗證碼圖片，
            // 並丟棄計時起點（該段時間仍留在載入的牆鐘總量中）。
            if target == FailedTarget::Login {
                self.clear_captcha();
                self.login_started = None;
            }
            // 驗證碼填錯已由 [`Self::dispatch_control`] 回報成可重試事件
            //（介面要留在輸入畫面），不另外彈出一般失敗。
            if !matches!(err, AppError::VerificationRetry(_)) {
                self.emit(Event::Failed {
                    what: if what.is_empty() {
                        "操作".to_owned()
                    } else {
                        what
                    },
                    message: err.to_string(),
                    target,
                    site,
                    resource,
                });
            }
        }
        shutdown
    }

    /// 登入類任務對應的站點（介面據此決定重試哪個站點）。
    fn login_site_of(&self, job: &Job) -> Option<SiteKind> {
        match job {
            Job::RetryLogin { site } | Job::RetryWithAccount { site, .. } => Some(*site),
            // 預載本身就是一連串登入：失敗歸屬於它當時正在登入的站點
            //（否則介面一律當成考勤）。
            Job::Preload => self.login_site,
            Job::SubmitCaptcha(_)
            | Job::RefreshCaptcha
            | Job::SendMfaCode
            | Job::VerifyMfaCode(_)
            | Job::CancelLogin => self.login_site,
            _ => None,
        }
    }

    /// 回報資料任務失敗。
    ///
    /// `site` 為實際失敗的站點；無法判定時為 `None`（介面就不會把它當成登入失敗）。
    fn emit_failed(&mut self, job: &Job, site: Option<SiteKind>, err: AppError) {
        // 作業載入到此為止：先輸出計時報告（若啟用），再讓失敗訊息蓋過它——
        // 終止失敗時使用者要看的是錯誤原因。
        if matches!(job, Job::LoadHomework { .. }) {
            self.finish_timing();
        }
        let what = job.label();
        self.emit(Event::Failed {
            what: if what.is_empty() {
                "操作".to_owned()
            } else {
                what
            },
            message: err.to_string(),
            target: failed_target_of(job),
            site,
            resource: resource_of(job),
        });
    }

    /// 測試用：同步執行單一任務（與 [`Self::run`] 相同的路由）。
    #[cfg(test)]
    fn dispatch(&mut self, job: Job) -> AppResult<()> {
        if job.is_control() {
            self.dispatch_control(job)
        } else {
            self.run_fresh_data_job(job);
            Ok(())
        }
    }

    /// 分派控制任務；資料任務不在這裡處理。
    ///
    /// 連線層的失敗（逾時、連不上、DNS、TLS）會直接重試整個任務，至多
    /// [`MAX_ATTEMPTS`] 次嘗試：控制任務多半是登入，短暫的網路抖動不該讓
    /// 使用者重新按一次按鈕。需要互動輸入的登入不會走到重試分支——那種
    /// 情況下任務回傳 `Ok`，登入流程留在 `flow` 裡等使用者輸入。
    ///
    /// 伺服器已經回應的失敗（業務錯誤、格式不符）、憑證或驗證碼錯誤一律
    /// 不重試（見 [`AppError::is_connection_error`]）；有可見副作用的任務
    /// 也不重試（見 [`Job::is_replayable`]）。
    fn dispatch_control(&mut self, job: Job) -> AppResult<()> {
        let what = job.label();
        let mut attempt: u8 = 1;
        loop {
            match self.dispatch_control_once(job.clone()) {
                Ok(()) => return Ok(()),
                Err(err)
                    if attempt < MAX_ATTEMPTS
                        && job.is_replayable()
                        && err.is_connection_error() =>
                {
                    attempt += 1;
                    self.emit(Event::Notice(format!(
                        "{}失败，正在重试（{attempt}/{MAX_ATTEMPTS}）…",
                        if what.is_empty() { "操作" } else { &what }
                    )));
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// 執行一次控制任務，並處理帳號切換失敗的善後。
    ///
    /// 善後集中在這裡：切換本身與它的互動驗證步驟（圖片驗證碼、簡訊驗證、
    /// 重試）都算同一次切換，任何一個失敗都必須丟棄待存憑證，否則稍後一次
    /// 成功的登入會把沒驗證成功的帳密寫進保險庫。唯一例外是「驗證碼填錯」：
    /// 使用者重輸即可繼續同一次切換。
    fn dispatch_control_once(&mut self, job: Job) -> AppResult<()> {
        let rolls_back = is_account_switch_step(&job);
        let result = self.dispatch_control_inner(job);
        if let Err(err) = &result {
            if matches!(err, AppError::VerificationRetry(_)) {
                // 互動驗證填錯：登入流程仍保留，介面就地顯示錯誤即可——既不進
                // 失敗畫面（會蓋掉輸入框），也不作廢待存憑證。
                let site = self
                    .flow
                    .as_ref()
                    .map(|flow| flow.site)
                    .or(self.login_site)
                    .unwrap_or(SiteKind::Attendance);
                self.emit(Event::VerificationRetry {
                    site,
                    message: err.to_string(),
                });
            } else if rolls_back {
                self.discard_pending_vault();
            }
        }
        result
    }

    /// 控制任務的實際分派。
    fn dispatch_control_inner(&mut self, job: Job) -> AppResult<()> {
        match job {
            Job::CreateVault {
                passphrase,
                credentials,
            } => self.create_vault(&passphrase, credentials),
            Job::Unlock { passphrase } => self.unlock(&passphrase),
            Job::Preload => self.preload(),
            Job::SubmitCaptcha(code) => self.submit_captcha(&code),
            Job::RefreshCaptcha => self.refresh_captcha(),
            Job::SendMfaCode => self.send_mfa_code(),
            Job::VerifyMfaCode(code) => self.verify_mfa_code(&code),
            Job::RetryLogin { site } => self.retry_login(site),
            Job::RetryWithAccount {
                site,
                credentials,
                passphrase,
            } => self.retry_with_account(site, &passphrase, credentials),
            Job::ChangeAccount {
                passphrase,
                credentials,
            } => self.change_account(&passphrase, credentials),
            Job::ChangePassphrase { old, new } => self.change_passphrase(&old, &new),
            Job::SetAccessPolicy(policy) => self.set_access_policy(policy),
            Job::SetHomeworkTerm { term } => self.set_homework_term(&term),
            Job::SetScheduleWeek { week } => self.set_schedule_week(week),
            Job::AcceptAgreement => self.accept_agreement(),
            Job::CancelLogin => self.cancel_login(),
            Job::Shutdown => Ok(()),
            // 自訂義任務由任務服務處理；InitTasks／RekeyTasks／LockTasks 只會
            // 出現在服務的通道上（工作者自己也只送出、不會收到）。
            Job::AddTask { .. }
            | Job::UpdateTask { .. }
            | Job::SetTaskDone { .. }
            | Job::SetTasksDone { .. }
            | Job::DeleteTask { .. }
            | Job::DeleteTasks { .. }
            | Job::DeleteCompletedTasks
            | Job::InitTasks { .. }
            | Job::RekeyTasks { .. }
            | Job::LockTasks => Ok(()),
            // 資料任務由 [`Self::run_data_job`] 負責。
            Job::LoadSchedule { .. }
            | Job::LoadHomework { .. }
            | Job::LoadFlow { .. }
            | Job::LoadCourses { .. }
            | Job::LoadActivities { .. }
            | Job::LoadActivityDetail { .. }
            | Job::OpenActivity { .. } => Ok(()),
        }
    }

    /// 執行資料任務（作業以步進方式執行，其餘為單步）。
    fn run_data_job(&mut self, job: Job) {
        match job {
            Job::LoadHomework { force } => self.run_homework_job(force),
            other => self.run_single_job(other),
        }
    }

    /// 執行單步資料任務。
    fn run_single_job(&mut self, job: Job) {
        let generation = self.generation;
        // 先處理排隊中的控制任務，並合併與本任務重複的請求。
        if !self.drain_channel(&job) {
            return;
        }
        if generation != self.generation {
            // 控制任務（換帳號、切換訪問模式）已使本任務失效：
            // 通知介面解除載入中狀態，避免頁面停留在永久的「載入中」。
            self.emit(Event::LoadingCancelled {
                target: failed_target_of(&job),
            });
            return;
        }

        self.execute_data_job(job);
    }

    /// 執行單步資料請求並回報結果（錯誤走統一的重新登入與路由回退）。
    fn execute_data_job(&mut self, job: Job) {
        let generation = self.generation;
        match self.load_once(&job) {
            Ok(Some(event)) => self.emit(event),
            Ok(None) => {}
            Err(err) => match site_of(&job) {
                Some(site) => self.report_data_failure(site, job, generation, err),
                None => self.emit_failed(&job, None, err),
            },
        }
    }

    /// 在長任務（作業載入）的步進邊界執行排隊中的互動式資料任務。
    ///
    /// 按 `o` 開啟活動網頁之類的操作由使用者觸發，不應等待整輪載入：在下
    /// 一個步進邊界立即執行（每個邊界只會執行恰一次）。每個任務都是新的
    /// 使用者操作，其自動重登額度以任務鍵獨立保存——互動操作與長載入互不
    /// 消耗（一方重登後，另一方的失效仍能嘗試自己的重登）；任務其後的重試
    /// 鏈共用同一份額度，仍受 [`MAX_LOGIN_ATTEMPTS`] 上限約束。回傳 `true`
    /// 代表目前有互動式登入正在進行（原本就在進行，或由本次執行觸發），
    /// 呼叫端必須暫停本輪載入（重新排隊後返回），等登入完成或取消後再繼續。
    fn flush_interactive(&mut self) -> bool {
        if self.flow.is_some() {
            return true;
        }
        while let Some(index) = self.pending_data.iter().position(Job::is_interactive) {
            let Some(job) = self.pending_data.remove(index) else {
                continue;
            };
            // 互動式任務是新的使用者操作：讓該任務重新取得完整重登與重試額度。
            if let Some(key) = job.data_key() {
                self.relogin.reset(&key);
                self.retries.reset(&key);
            }
            self.execute_data_job(job);
            if self.flow.is_some() {
                return true;
            }
        }
        false
    }

    /// 執行一次單步請求，回傳要回報的事件。
    fn load_once(&mut self, job: &Job) -> AppResult<Option<Event>> {
        let event = match job {
            Job::LoadSchedule { force } => Event::Schedule(Box::new(self.load_schedule(*force)?)),
            Job::LoadFlow { page } => Event::Flow(Box::new(self.load_flow(*page)?)),
            Job::LoadCourses { force } => Event::Courses(self.load_courses(*force)?),
            Job::LoadActivities { course_id, force } => Event::Activities {
                course_id: course_id.clone(),
                activities: self.load_activities(course_id, *force)?,
            },
            Job::LoadActivityDetail { activity_id } => {
                Event::ActivityDetail(Box::new(self.load_activity_detail(activity_id)?))
            }
            Job::OpenActivity {
                activity_id,
                course_id,
                kind,
            } => {
                Event::OpenUrl(self.open_activity_url(activity_id, course_id.as_deref(), *kind)?)
            }
            _ => return Ok(None),
        };
        Ok(Some(event))
    }

    /// 合併資料任務到待執行佇列。
    ///
    /// 以 [`Job::data_key`] 為資源鍵、[`Job::is_forced`] 為強度：同鍵任務至多
    /// 保留一筆，且以最強者為準——佇列中的非強制任務會被強制任務原位升級，
    /// 強制任務不會被降級或重複。`running` 為目前進行中的任務；同鍵且不弱於
    /// 來者時視為重複而丟棄（例如長查詢期間重複按 `r`）。傳 `None` 代表忽略
    /// 執行中任務（學期變更後，進行中的載入已基於舊學期，必須保留一次重載）。
    fn merge_data_job(&mut self, job: Job, running: Option<&Job>) {
        let Some(key) = job.data_key() else {
            return;
        };
        let forced = job.is_forced();
        if let Some(running) = running
            && running.data_key().as_ref() == Some(&key)
            && (!forced || running.is_forced())
        {
            // 執行中的任務已涵蓋此請求：丟棄。
            return;
        }
        // 與排隊中的同鍵任務合併：強制優先且原位升級，不得降級或重複。
        for queued in &mut self.pending_data {
            if queued.data_key().as_ref() == Some(&key) {
                if forced && !queued.is_forced() {
                    *queued = job;
                }
                return;
            }
        }
        self.pending_data.push_back(job);
    }

    /// 重新查詢資料前先排空通道：控制任務優先處理、重複的資料請求合併。
    ///
    /// `running` 為目前進行中的任務；同鍵的排隊請求經 [`Self::merge_data_job`]
    /// 合併後至多保留一筆最強者（例如長查詢期間重複按 `r`）。回傳 `false` 代表
    /// 收到結束指令，呼叫端應立即停止。
    fn drain_channel(&mut self, running: &Job) -> bool {
        loop {
            match self.jobs.try_recv() {
                Ok(Job::Shutdown) => {
                    // 資料載入中途收到結束指令：記錄後停止目前任務，
                    // 由 [`Self::run`] 的迴圈開頭結束整個工作執行緒。
                    self.shutdown = true;
                    return false;
                }
                Ok(job) if job.is_control() => {
                    let _ = self.handle_control(job);
                }
                Ok(job) => self.merge_data_job(job, Some(running)),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// 資料任務失敗的統一處理：等待登入 → 自動重試 → 有限回退 → 回報錯誤。
    ///
    /// `site` 為實際失敗的站點，由呼叫端決定（不一律從任務種類推導）。
    fn report_data_failure(&mut self, site: SiteKind, job: Job, generation: u64, err: AppError) {
        // 代際已變（帳號或訪問模式被切換）：舊任務的錯誤直接忽略。
        if generation != self.generation {
            return;
        }

        // 已經有登入在進行（例如解鎖後的背景預載正在登入）：不重啟登入流程
        //——那會丟掉目前流程等待續跑的任務——把本次任務排回待執行即可。
        // 登入完成後 [`Self::run`] 會重新取出它，屆時成功就不會再走到這裡。
        if self.flow.is_some() {
            self.merge_data_job(job, None);
            return;
        }

        // 直連失敗：Auto 模式下允許改走 WebVPN 一次。
        let switched = matches!(
            &err,
            AppError::Network { kind, .. } if kind.is_connection_level()
        ) && self
            .session
            .as_mut()
            .is_some_and(|session| session.fallback_to_webvpn(site));
        if switched {
            self.emit(Event::Notice(format!(
                "直连不可用，已改用 WebVPN 重试：{site}"
            )));
        }

        if err.needs_relogin() || switched {
            // 登入態失效（或剛切換路由）：記下任務，重新登入後自動重試。
            self.emit(Event::SessionExpired { site });
            // 額度按任務鍵各自計算：互動式任務與長載入互不消耗；同一任務的
            // 自動重試鏈則共用同一份額度（重試後再失效時遞減）。
            let key = job.data_key();
            self.retry = Some(job);
            if !key.is_some_and(|key| self.relogin.try_consume(&key).is_some()) {
                // 自動重登後站點仍回報登入態失效：停止自動重試，避免
                //「重登→重試→再失效」的無上限迴圈，交由使用者手動重試。
                if let Some(job) = self.retry.take() {
                    self.emit_failed(&job, Some(site), AppError::ReloginExhausted);
                }
                return;
            }
            if let Err(login_err) = self.begin_login(site, None) {
                // 登入流程連開始都做不到（離線、DNS 失敗、登入頁取不到…）：
                // 原任務必須收斂（否則頁面永遠停在「載入中」）。
                //
                // 這裡刻意「不」額外發送登入失敗事件：使用者只是斷網時不需
                // 要一個無故彈出的登入框；若介面上已經有「正在登入」的覆蓋層
                //（`begin_login` 已進到會顯示進度的階段），介面會在收到本失敗
                // 事件時把它收斂成可重試的失敗畫面。
                if let Some(job) = self.retry.take() {
                    self.emit_failed(&job, Some(site), login_err);
                }
            }
            return;
        }

        // 連線層錯誤（逾時、連不上、DNS、TLS）：多半是校內服務的短暫抖動，
        // 自動重送同一個請求即可；伺服器有回應但內容不符預期等情況不會
        // 走到這裡（見 [`AppError::is_connection_error`]）。重試就地進行，
        // 每個任務鍵至多 [`MAX_ATTEMPTS`] 次嘗試，額度用完才回報失敗。
        if err.is_connection_error()
            && let Some(key) = job.data_key()
            && let Some(attempt) = self.retries.try_consume(&key)
        {
            self.emit(Event::Notice(format!(
                "{}失败，正在重试（{attempt}/{MAX_ATTEMPTS}）…",
                job.label()
            )));
            self.run_data_job(job);
            return;
        }

        self.emit_failed(&job, Some(site), err);
    }

    // ── 工具 ─────────────────────────────────────────────

    /// 結束作業載入計時：啟用時輸出一行分階段報告（見 [`timing`]）。
    fn finish_timing(&mut self) {
        if let Some(report) = self.timing.take_report() {
            self.emit(Event::Notice(report));
        }
    }

    /// 目前會話已送出的請求數（診斷用）。
    fn request_count(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, SessionManager::request_count)
    }

    fn session_mut(&mut self) -> AppResult<&mut SessionManager> {
        self.session
            .as_mut()
            .ok_or_else(|| AppError::config("会话尚未建立，请先解锁凭证"))
    }

    fn emit(&self, event: Event) {
        // 介面若已結束，事件自然被丟棄。
        let _ = self.events.send(event);
    }
}

/// 單項查詢失敗是否可保留為「待核实」並繼續（否則向上傳播）。
fn is_recoverable(err: &AppError) -> bool {
    if err.needs_relogin() {
        return false;
    }
    !matches!(err, AppError::Network { kind, .. } if kind.is_connection_level())
}

#[cfg(test)]
#[path = "tests/fixtures.rs"]
mod fixtures;

#[cfg(test)]
#[path = "tests/worker_test.rs"]
mod worker_test;

#[cfg(test)]
#[path = "tests/scheduler_test.rs"]
mod scheduler_test;
