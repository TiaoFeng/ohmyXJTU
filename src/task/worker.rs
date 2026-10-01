//! 背景工作執行緒。
//!
//! 所有網路請求都在這裡執行，介面只透過 [`Job`] 下指令、透過 [`Event`] 收結果。
//! 登入流程由 [`LoginDriver`] 驅動：需要驗證碼或簡訊驗證時回報事件，
//! 使用者輸入後再繼續；登入態失效時會自動重新登入並重試原本的任務。
//!
//! 調度原則：
//!
//! - 控制任務（登入、設定、憑證）優先於資料任務；資料任務以「步進」執行，
//!   每一步之間先處理排隊中的控制任務，避免長查詢阻塞設定操作。
//! - 重複的資料查詢會被合併；帳號或訪問模式變更後，進行中的資料任務立即
//!   中止且不再回報舊結果。
//! - 解鎖憑證後不預先登入任何站點：頁面需要時才按站點惰性登入，
//!   因此考勤系統故障不會拖垮思源學堂。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDate};

use crate::auth::webvpn;
use crate::auth::{AccountType, LoginDriver, LoginReply};
use crate::config::{AccessPolicy, Config};
use crate::credentials::{Credentials, Secret, Vault};
use crate::domain::homework::HomeworkInput;
use crate::domain::semester::{self, TermCode, TermResolution, TermSource};
use crate::domain::{attendance_match, homework, schedule};
use crate::error::{AppError, AppResult};
use crate::model::{ActivityDetailView, FlowData, LessonEntry, ScheduleData};
use crate::session::{AccessMode, LoginStage, SessionManager, SiteKind};
use crate::sites::attendance::{AttendanceApi, AttendanceSite};
use crate::sites::lms::{
    self, ActivityKind, LmsActivity, LmsApi, LmsCourse, LmsSite, SubmissionSummary,
    submission_failure_note,
};
use crate::task::protocol::{
    CoursesData, Event, FailedTarget, HomeworkIssue, HomeworkUpdate, Job, failed_target_of,
    is_account_switch_step, resource_of, site_of,
};

/// 考勤流水分頁大小。
const FLOW_PAGE_SIZE: u32 = 20;

/// 思源學堂課程／活動快取的有效時間。
const LMS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// 單一載入週期允許的自動重新登入次數上限。
///
/// 站點持續回報登入態失效時，「自動重登 → 重試 → 再失效」會形成無上限的
/// 迴圈；超過上限即停止自動重試並回報錯誤，由使用者手動按 `r` 重試
///（每次全新的載入請求都會重新獲得額度）。
const MAX_AUTO_RELOGINS: u8 = 1;

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
    /// 本輪載入已嘗試的自動重新登入次數（全新的載入請求歸零）。
    relogin_attempts: u8,
    /// 思源學堂課程／活動快取。
    cache: LmsCache,
    /// 本會話曾查得的考勤學期（供課程分區使用，不重複請求）。
    known_term: Option<TermCode>,
    /// 使用者在本工作階段按 `s` 明確選擇的學期。
    ///
    /// 明確的選擇優先於考勤的當前學期，否則「選擇要查看的學期」不會生效；
    /// 它不持久化，重新開啟程式後仍以考勤為權威，而設定檔中的
    /// `homework_term` 則作為考勤不可用時的後備。
    chosen_term: Option<TermCode>,
    /// 已收到結束指令；[`Worker::run`] 於迴圈開頭立即返回。
    shutdown: bool,
}

/// 作業載入進度事件的節流間隔。
///
/// 每完成這麼多項作業（或每門課程結束）才送出一次完整快照，避免大型學期
/// 逐項重算彙總造成 O(N²) 成本與無界的事件佇列。
const PROGRESS_EMIT_INTERVAL: usize = 10;

/// 作業載入的步進階段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeworkStage {
    /// 載入目前課程的作業活動。
    Activities,
    /// 載入目前活動的提交摘要。
    Submission,
    /// 切換到下一門課程。
    AdvanceCourse,
    /// 全部完成。
    Done,
}

/// 作業載入的步進狀態。
struct HomeworkRunner {
    /// 目標學期。
    term: TermCode,
    /// 學期判定來源。
    term_source: TermSource,
    /// 納入查詢的課程。
    courses: Vec<LmsCourse>,
    /// 缺少學期資訊而未納入的課程數。
    skipped_terms: usize,
    /// 可選學期（供介面顯示選擇器）。
    term_options: Vec<TermCode>,
    /// 下一門課程的索引。
    course_index: usize,
    /// 目前課程的作業活動。
    activities: Vec<LmsActivity>,
    /// 下一個活動的索引。
    activity_index: usize,
    /// 已彙總的輸入。
    inputs: Vec<HomeworkInput>,
    /// 活動列表查詢失敗而略過的課程數。
    failed_courses: usize,
    /// 距離上次發出進度事件以來完成的作業數（用於節流）。
    since_emit: usize,
    /// 目前階段。
    stage: HomeworkStage,
    /// 是否略過快取。
    force: bool,
    /// 本次載入開始時間（統計用）。
    started: Instant,
    /// 本次載入開始前的請求計數（統計用）。
    requests_baseline: usize,
}

impl HomeworkRunner {
    /// 目前課程。
    fn current_course(&self) -> Option<&LmsCourse> {
        self.courses.get(self.course_index)
    }

    /// 目前的載入更新（完成時 `progress` 為 `None`）。
    fn update(&self, elapsed: Duration, requests: usize) -> HomeworkUpdate {
        let now = Local::now().fixed_offset();
        let progress = if matches!(self.stage, HomeworkStage::Done) {
            None
        } else {
            Some((
                self.course_index.min(self.courses.len()),
                self.courses.len(),
            ))
        };
        HomeworkUpdate {
            term_label: Some(self.term.label()),
            term_source: Some(self.term_source),
            courses_included: self.courses.len(),
            courses_skipped: self.skipped_terms,
            term_options: self.term_options.clone(),
            items: homework::aggregate(&self.inputs, now),
            issues: homework_issues(&self.inputs),
            courses_failed: self.failed_courses,
            progress,
            elapsed,
            requests,
        }
    }
}

/// 彙總「待核实」作業的共同原因（同一原因只列一次，依項數遞減再按文字排序）。
fn homework_issues(inputs: &[HomeworkInput]) -> Vec<HomeworkIssue> {
    let mut grouped: Vec<HomeworkIssue> = Vec::new();
    for input in inputs
        .iter()
        .filter(|input| input.submission_count.is_none())
    {
        let reason = input
            .note
            .clone()
            .unwrap_or_else(|| "无法确认提交状态".to_owned());
        match grouped.iter_mut().find(|issue| issue.reason == reason) {
            Some(issue) => issue.count += 1,
            None => grouped.push(HomeworkIssue { reason, count: 1 }),
        }
    }
    grouped.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    grouped
}

/// 思源學堂課程／活動快取（記憶體、有效期五分鐘）。
#[derive(Default)]
struct LmsCache {
    courses: Option<(Vec<LmsCourse>, usize, Instant)>,
    activities: HashMap<String, (Vec<LmsActivity>, usize, Instant)>,
    details: HashMap<String, (LmsActivity, Instant)>,
    summaries: HashMap<String, (SubmissionSummary, Instant)>,
}

impl LmsCache {
    /// 清空快取（帳號或訪問模式變更時呼叫）。
    fn clear(&mut self) {
        self.courses = None;
        self.activities.clear();
        self.details.clear();
        self.summaries.clear();
    }

    /// 取課程快取（有效期內且非強制刷新時）。
    fn courses(&self, force: bool) -> Option<(Vec<LmsCourse>, usize)> {
        if force {
            return None;
        }
        let (courses, skipped, at) = self.courses.as_ref()?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| (courses.clone(), *skipped))
    }

    /// 寫入課程快取。
    fn store_courses(&mut self, courses: &[LmsCourse], skipped: usize) {
        self.courses = Some((courses.to_vec(), skipped, Instant::now()));
    }

    /// 取活動快取（有效期內且非強制刷新時）。
    fn activities(&self, course_id: &str, force: bool) -> Option<(Vec<LmsActivity>, usize)> {
        if force {
            return None;
        }
        let (activities, skipped, at) = self.activities.get(course_id)?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| (activities.clone(), *skipped))
    }

    /// 寫入活動快取。
    fn store_activities(&mut self, course_id: &str, activities: &[LmsActivity], skipped: usize) {
        self.activities.insert(
            course_id.to_owned(),
            (activities.to_vec(), skipped, Instant::now()),
        );
    }

    /// 取活動詳情快取（有效期內且非強制刷新時）。
    fn detail(&self, activity_id: &str, force: bool) -> Option<LmsActivity> {
        if force {
            return None;
        }
        let (detail, at) = self.details.get(activity_id)?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| detail.clone())
    }

    /// 寫入活動詳情快取。
    fn store_detail(&mut self, activity_id: &str, detail: &LmsActivity) {
        self.details
            .insert(activity_id.to_owned(), (detail.clone(), Instant::now()));
    }

    /// 取提交摘要快取（有效期內且非強制刷新時）。
    fn summary(&self, activity_id: &str, force: bool) -> Option<SubmissionSummary> {
        if force {
            return None;
        }
        let (summary, at) = self.summaries.get(activity_id)?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| summary.clone())
    }

    /// 寫入提交摘要快取。
    fn store_summary(&mut self, activity_id: &str, summary: &SubmissionSummary) {
        self.summaries
            .insert(activity_id.to_owned(), (summary.clone(), Instant::now()));
    }
}

/// 啟動背景工作執行緒，回傳（任務送出端, 事件接收端）。
pub fn spawn(config: Config, vault: Vault) -> AppResult<(Sender<Job>, Receiver<Event>)> {
    let (job_tx, job_rx) = channel();
    let (event_tx, event_rx) = channel();

    let mut worker = Worker {
        jobs: job_rx,
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
        relogin_attempts: 0,
        cache: LmsCache::default(),
        known_term: None,
        chosen_term: None,
        shutdown: false,
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

    /// 執行使用者發起的全新資料任務：重置自動重登額度後再執行。
    ///
    /// 自動重登後的重試（[`Self::finish_login`]）不走這裡，額度才會遞減；
    /// 新的刷新請求則重新獲得完整額度。
    fn run_fresh_data_job(&mut self, job: Job) {
        self.relogin_attempts = 0;
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
        // 處理（含互動驗證步驟），這裡只需要清掉暫存的驗證碼圖片。
        if let Err(err) = self.dispatch_control(job) {
            // 登入類任務出錯即視為本次登入結束：清掉暫存的驗證碼圖片。
            if target == FailedTarget::Login {
                self.clear_captcha();
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
    fn emit_failed(&self, job: &Job, site: Option<SiteKind>, err: AppError) {
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
    /// 帳號切換失敗的善後集中在這裡：切換本身與它的互動驗證步驟（
    /// 圖片驗證碼、簡訊驗證、重試）都算同一次切換，任何一個失敗都必須
    /// 丟棄待存憑證，否則稍後一次成功的登入會把沒驗證成功的帳密寫進保險庫。
    /// 唯一例外是「驗證碼填錯」：使用者重輸即可繼續同一次切換。
    fn dispatch_control(&mut self, job: Job) -> AppResult<()> {
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
            Job::AcceptAgreement => self.accept_agreement(),
            Job::CancelLogin => self.cancel_login(),
            Job::Shutdown => Ok(()),
            // 資料任務由 [`Self::run_data_job`] 負責。
            Job::LoadSchedule
            | Job::LoadHomework { .. }
            | Job::LoadFlow { .. }
            | Job::LoadCourses { .. }
            | Job::LoadActivities { .. }
            | Job::LoadActivityDetail { .. }
            | Job::OpenActivity { .. } => Ok(()),
        }
    }

    // ── 憑證 ─────────────────────────────────────────────

    fn create_vault(&mut self, passphrase: &str, credentials: Credentials) -> AppResult<()> {
        self.vault.store(passphrase, &credentials)?;
        self.start_session(credentials)?;
        // 不預先登入任何站點：頁面載入需要時才按站點惰性登入。
        self.emit(Event::VaultReady);
        Ok(())
    }

    fn unlock(&mut self, passphrase: &str) -> AppResult<()> {
        let credentials = self.vault.load(passphrase)?;
        self.start_session(credentials)?;
        // 不預先登入任何站點：頁面載入需要時才按站點惰性登入。
        self.emit(Event::VaultReady);
        self.report_vault_permissions();
        Ok(())
    }

    fn change_account(&mut self, passphrase: &str, credentials: Credentials) -> AppResult<()> {
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
        self.relogin_attempts = 0;
        self.pending_data.clear();
        self.cache.clear();
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

    fn change_passphrase(&mut self, old: &str, new: &str) -> AppResult<()> {
        self.vault.change_passphrase(old, new)?;
        // 待存憑證是以舊口令加密的計畫：口令已改變，該計畫立即失效（並還原舊憑證），
        // 否則稍後登入成功會用舊口令覆寫保險庫，把新口令蓋回去。
        self.discard_pending_vault();
        self.emit(Event::PassphraseUpdated);
        Ok(())
    }

    /// 記住使用者選擇的學期，並立即重新載入作業。
    fn set_homework_term(&mut self, term: &str) -> AppResult<()> {
        let term = TermCode::parse(term)
            .ok_or_else(|| AppError::protocol(format!("学期格式无法识别：{term}")))?;
        self.config.homework_term = Some(term.to_string());
        self.config.save()?;
        // 本次明確選擇：接下來的載入以它為準，不再被考勤的當前學期蓋過。
        self.chosen_term = Some(term);
        // 課程清單的分區以「當前學期」為準：學期改了要同步給介面，否則作業
        // 已切到所選學期，回到思源學堂仍按舊學期分區（要手動刷新才會更新）。
        self.emit(Event::CoursesTerm(Some(term)));
        self.emit(Event::Notice(format!("已记住学期 {}", term.label())));
        // 學期已變更：使進行中的作業載入失效（它基於舊學期），並確保佇列中
        // 恰有一筆強制重載（忽略執行中任務），切換才會立即生效。
        self.homework_epoch += 1;
        self.merge_data_job(Job::LoadHomework { force: true }, None);
        Ok(())
    }

    fn start_session(&mut self, credentials: Credentials) -> AppResult<()> {
        let mut session = SessionManager::new(&self.config)?;
        session.register(Box::new(AttendanceSite));
        session.register(Box::new(LmsSite));
        session.set_credentials(credentials.clone());
        self.session = Some(session);
        self.credentials = Some(credentials);
        self.flow = None;
        self.retry = None;
        // 待存憑證屬於舊帳號：換帳號後一律作廢。
        self.pending_vault = None;
        // 舊帳號的登入失敗計數不再適用。
        self.login_failures.clear();
        self.login_failure_key = None;
        // 換帳號後舊任務與快取一律作廢，進行中的資料任務不再回報。
        self.generation += 1;
        self.relogin_attempts = 0;
        self.pending_data.clear();
        self.cache.clear();
        // 舊帳號的站點登入狀態與頁面資料已失效：介面應清除。
        self.emit(Event::SessionsCleared {
            account_changed: true,
        });
        Ok(())
    }

    fn set_access_policy(&mut self, policy: AccessPolicy) -> AppResult<()> {
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
        // 訪問方式變更：進行中的資料任務作廢，快取失效。
        // 保存設定本身不觸發登入，後續登入由各頁面按需進行。
        self.generation += 1;
        self.relogin_attempts = 0;
        self.cache.clear();
        // 連線與登入態已重建：介面清除站點登入狀態；既有頁面資料仍有效，
        // 只解除因任務作廢而卡住的載入狀態。
        self.emit(Event::SessionsCleared {
            account_changed: false,
        });
        self.emit(Event::AccessPolicyUpdated(policy));
        Ok(())
    }

    /// 取消進行中的登入流程（介面關閉登入覆蓋層時）。
    ///
    /// 丟棄登入驅動器、待存憑證、待重試任務與暫存的驗證碼圖片：登入互動期間
    /// 資料任務一律延後，若不取消，關閉覆蓋層後使用者按 `r` 送出的任務會永遠
    /// 排不到。登入流程即使已經結束（憑證被拒），等待重登的頁面仍會被收斂；
    /// 這種情況下不覆蓋介面已顯示的提示。
    fn cancel_login(&mut self) -> AppResult<()> {
        // 即使登入流程本身已經結束，等待重登的資料任務仍可能留著：例如憑證
        // 被拒時流程與待存憑證都已丟棄（`flow`、`pending_vault` 皆為 `None`），
        // 但 `retry` 還握著原任務。不收拾它的話，該頁會永遠停在「載入中」
        //（登入互動期間資料任務一律延後，取消後沒有事件會再觸發）。
        let had_login = self.flow.is_some() || self.pending_vault.is_some();
        self.flow = None;
        self.discard_pending_vault();
        self.settle_pending_retry();
        if had_login {
            self.clear_captcha();
            self.emit(Event::Notice("已取消登录流程，可重新刷新页面".to_owned()));
        }
        Ok(())
    }

    /// 丟棄等待重登的資料任務，並通知介面收斂該頁的載入狀態。
    fn settle_pending_retry(&mut self) {
        if let Some(job) = self.retry.take() {
            self.emit(Event::LoadingCancelled {
                target: failed_target_of(&job),
            });
        }
    }

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

    /// 丟棄待存憑證，還原舊憑證，並作廢切換期間建立的新會話。
    ///
    /// 新憑證只有在登入成功後才寫入保險庫；取消、憑證被拒或流程失敗時，
    /// 記憶體中的憑證必須回到保險庫仍保存的舊憑證，否則之後的自動重登會拿
    /// 一組從未驗證、也沒被保存的憑證去登入。
    ///
    /// 只還原帳密並不夠：新帳號在登入過程中可能已在伺服器端留下登入態
    ///（cookie）。不重建後端的話，之後任何一次登入都會被判定為「已登入」
    /// 而略過帳密提交，畫面就會出現新帳號的資料。
    fn discard_pending_vault(&mut self) {
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
            self.emit(Event::SessionDisabled(format!(
                "无法建立新的会话，已停用当前会话：{err}"
            )));
        }
        // 切換期間取得的資料與進行中的任務都屬於新帳號：一併作廢。
        // （排隊中的資料任務不在此列：它們會以還原後的帳號重新執行。）
        self.generation += 1;
        self.cache.clear();
        self.login_failure_key = None;
    }

    /// 失敗時要還原的憑證。
    ///
    /// 保險庫中真正保存的那一組憑證優先：當上一次切換尚未結束（例如驗證碼填錯
    /// 後改輸入另一組帳密）時，`self.credentials` 已經是那組「尚未驗證、也還沒
    /// 寫回保險庫」的憑證，不能拿它當作還原目標。
    fn rollback_credentials(&self) -> Option<Credentials> {
        self.pending_vault
            .as_ref()
            .and_then(|pending| pending.previous.clone())
            .or_else(|| self.credentials.clone())
    }

    /// 記錄使用者已同意的用户协议版本；寫入失敗時不變更已保存的版本。
    fn accept_agreement(&mut self) -> AppResult<()> {
        let previous = self.config.privacy_version.clone();
        self.config.privacy_version = Some(crate::privacy::VERSION.to_owned());
        if let Err(err) = self.config.save() {
            self.config.privacy_version = previous;
            return Err(err);
        }
        self.emit(Event::AgreementAccepted);
        Ok(())
    }

    // ── 登入 ─────────────────────────────────────────────

    fn begin_login(&mut self, site: SiteKind, retry: Option<Job>) -> AppResult<()> {
        let credentials = self
            .credentials
            .clone()
            .ok_or_else(|| AppError::config("尚未解锁凭证"))?;
        // 記下本次登入的站點：失敗訊息與介面重試都要能指出是哪個站點。
        self.login_site = Some(site);
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
                self.handle_reply(reply)
            }
        }
    }

    fn handle_reply(&mut self, reply: LoginReply) -> AppResult<()> {
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
                self.flow = None;
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
                        self.emit(Event::Notice(format!("无法获取短信验证手机号：{err}")));
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

    fn finish_login(&mut self, site: SiteKind, retry: Option<Job>) -> AppResult<()> {
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
        if let Some(job) = retry.or(self.retry.take()) {
            if job.is_control() {
                let _ = self.handle_control(job);
            } else {
                self.run_data_job(job);
            }
        }
        Ok(())
    }

    /// 把等待中的憑證寫入保險庫；寫入失敗不影響已完成的登入。
    ///
    /// 等待期間加密口令可能已被更改（例如換帳號失敗後改口令）：寫入前先確認
    /// 待存口令仍能解開現行檔案，否則寫入會把保險庫改回舊口令、新口令失效。
    fn commit_pending_vault(&mut self) {
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

    fn submit_captcha(&mut self, code: &str) -> AppResult<()> {
        let reply = self.driver_mut()?.submit_captcha(code)?;
        self.handle_reply(reply)
    }

    fn refresh_captcha(&mut self) -> AppResult<()> {
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
    fn clear_captcha(&mut self) {
        if let Some(path) = self.captcha_path.take() {
            let _ = crate::auth::captcha::remove(&path);
        }
    }

    fn send_mfa_code(&mut self) -> AppResult<()> {
        let phone = self.driver_mut()?.send_mfa_code()?;
        self.emit(Event::LoginNeedsMfa {
            phone: Some(phone),
            sent: true,
        });
        Ok(())
    }

    fn verify_mfa_code(&mut self, code: &str) -> AppResult<()> {
        self.driver_mut()?.verify_mfa_code(code)?;
        let reply = self.driver_mut()?.resume()?;
        self.handle_reply(reply)
    }

    fn retry_login(&mut self, site: SiteKind) -> AppResult<()> {
        // 使用者手動重試：自動重登額度重新計算。
        self.relogin_attempts = 0;
        self.begin_login(site, None)
    }

    /// 以使用者重新輸入的憑證重試登入；先驗證口令，登入成功後才寫入保險庫。
    ///
    /// `site` 為原本失敗的站點：重試不應被另一個站點的可達性牽制
    ///（例如思源學堂失敗卻要去連考勤系統）。
    fn retry_with_account(
        &mut self,
        site: SiteKind,
        passphrase: &str,
        credentials: Credentials,
    ) -> AppResult<()> {
        // 口令錯誤時回報 [`AppError::WrongPassphrase`]，舊憑證不受影響。
        self.vault.load(passphrase)?;

        // 使用者手動重試：自動重登額度重新計算。
        self.relogin_attempts = 0;
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

    // ── 資料載入 ─────────────────────────────────────────

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

        match self.load_once(&job) {
            Ok(Some(event)) => self.emit(event),
            Ok(None) => {}
            Err(err) => match site_of(&job) {
                Some(site) => self.report_data_failure(site, job, generation, err),
                None => self.emit_failed(&job, None, err),
            },
        }
    }

    /// 執行一次單步請求，回傳要回報的事件。
    fn load_once(&mut self, job: &Job) -> AppResult<Option<Event>> {
        let event = match job {
            Job::LoadSchedule => Event::Schedule(Box::new(self.load_schedule()?)),
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

    /// 資料任務失敗的統一處理：有限回退 → 重新登入 → 回報錯誤。
    ///
    /// `site` 為實際失敗的站點，由呼叫端決定（不一律從任務種類推導）。
    fn report_data_failure(&mut self, site: SiteKind, job: Job, generation: u64, err: AppError) {
        // 代際已變（帳號或訪問模式被切換）：舊任務的錯誤直接忽略。
        if generation != self.generation {
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
            self.retry = Some(job);
            if self.relogin_attempts >= MAX_AUTO_RELOGINS {
                // 自動重登後站點仍回報登入態失效：停止自動重試，避免
                //「重登→重試→再失效」的無上限迴圈，交由使用者手動重試。
                if let Some(job) = self.retry.take() {
                    self.emit_failed(&job, Some(site), AppError::ReloginExhausted);
                }
                return;
            }
            self.relogin_attempts += 1;
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

        self.emit_failed(&job, Some(site), err);
    }

    fn load_schedule(&mut self) -> AppResult<ScheduleData> {
        let session = self.session_mut()?;
        let mut api = AttendanceApi::new(session);

        let semester = api.current_semester()?;
        let courses = api.weekly_courses(&semester.semester_id)?;
        let today = Local::now().date_naive();
        let semester_start = parse_date(&semester.start_date)?;
        let term = semester.term_name();
        let week = schedule::clamp_week(schedule::week_number(semester_start, today), &term);

        let (monday, sunday) = schedule::week_window(today);
        let records = api.records_between(monday, sunday)?;
        // 記錄本會話得知的學期，供思源學堂課程分區使用（不重複查詢考勤）。
        self.known_term = TermCode::parse(&term);

        let skipped = courses
            .iter()
            .filter(|course| schedule::parse_weeks(&course.week_ranges).is_empty())
            .count();

        let mut lessons: Vec<LessonEntry> = Vec::new();
        for slot in schedule::merge_courses(&courses)
            .into_iter()
            .filter(|slot| slot.is_in_week(week))
        {
            let Some(date) = slot.date_in_week(semester_start, week) else {
                continue;
            };
            let status = attendance_match::status_for(&slot, date, &records);
            let weeks = slot.weeks_label();
            lessons.push(LessonEntry {
                date,
                sections: format!("{}-{}", slot.start_section, slot.end_section),
                course_name: slot.course_name.clone(),
                classroom: slot.classroom.clone().unwrap_or_default(),
                teacher: slot.teacher.clone().unwrap_or_default(),
                weeks,
                status,
                label: attendance_match::display_label(status, date, today),
            });
        }
        lessons.sort_by(|left, right| {
            (left.date, left.sections.clone()).cmp(&(right.date, right.sections.clone()))
        });

        Ok(ScheduleData {
            semester: term,
            week,
            lessons,
            skipped,
        })
    }

    /// 作業載入（步進執行：每門課程、每項作業之間先處理控制任務）。
    fn run_homework_job(&mut self, force: bool) {
        let generation = self.generation;
        let epoch = self.homework_epoch;
        let job = Job::LoadHomework { force };

        let mut runner = match self.begin_homework(force) {
            Ok(Some(runner)) => runner,
            Ok(None) => return,
            Err(failure) => {
                self.report_data_failure(failure.site, job, generation, failure.err);
                return;
            }
        };

        loop {
            if !self.drain_channel(&job) {
                return;
            }
            if generation != self.generation {
                self.emit(Event::Notice(
                    "账号或访问模式已变更，已取消进行中的作业加载".to_owned(),
                ));
                self.emit(Event::LoadingCancelled {
                    target: FailedTarget::Homework,
                });
                return;
            }
            if epoch != self.homework_epoch {
                // 學期已切換：本輪基於舊學期，停止並讓已排入的強制重載接手，
                // 避免舊學期的進度與完成結果繼續回填畫面。
                self.emit(Event::LoadingCancelled {
                    target: FailedTarget::Homework,
                });
                return;
            }
            if matches!(runner.stage, HomeworkStage::Done) {
                self.emit_homework(&runner);
                return;
            }
            if let Err(err) = self.homework_step(&mut runner) {
                // 步進階段只會向思源學堂查詢。
                self.report_data_failure(SiteKind::Lms, job, generation, err);
                return;
            }
        }
    }

    /// 準備作業載入：判定學期、過濾課程並回報首批進度。
    fn begin_homework(&mut self, force: bool) -> Result<Option<HomeworkRunner>, SiteFailure> {
        let started = Instant::now();
        let requests_baseline = self.request_count();
        let (courses, skipped_data) = self.lms_courses(force)?;
        if skipped_data > 0 {
            self.emit(Event::Notice(format!(
                "已跳过 {skipped_data} 项无法解析的思源学堂数据"
            )));
        }

        // 沒有任何課程時無從（也無需）判定學期：直接回報空結果。
        if courses.is_empty() {
            self.emit(Event::Homework(HomeworkUpdate {
                term_label: None,
                term_source: None,
                courses_included: 0,
                courses_skipped: 0,
                term_options: Vec::new(),
                items: Vec::new(),
                issues: Vec::new(),
                courses_failed: 0,
                progress: None,
                elapsed: started.elapsed(),
                requests: self.request_count().saturating_sub(requests_baseline),
            }));
            return Ok(None);
        }

        let chosen = self.chosen_term;
        let remembered = self
            .config
            .homework_term
            .as_deref()
            .and_then(TermCode::parse);
        let today = Local::now().date_naive();

        // 使用者已明確選定學期時，不必（也不應）再查考勤：他指定的學期就是
        // 答案，考勤的登入狀態（過期、逾時）不該讓作業查詢跟著失敗。
        //
        // 其餘情況才向考勤取權威學期。考勤的失敗必須標成考勤站點：否則會用
        // `Job::LoadHomework` 推得思源學堂，重登之後還是會失敗，錯誤訊息也
        // 指向錯的站點；但只有「登入態已失效」值得中斷整批作業查詢——重登
        // 之後就能取回權威的學期，單純的連線層錯誤則降級為「考勤不可用」，
        // 免得考勤的暫時故障連帶拖垮本來可用的思源學堂。
        let (attendance_term, attendance_error) = if chosen.is_some() {
            (None, None)
        } else {
            match self.attendance_term() {
                Ok(term) => (term, None),
                Err(err) if err.needs_relogin() => return Err(SiteFailure::attendance(err)),
                Err(err) => (None, Some(err)),
            }
        };

        let (term, term_source) =
            match semester::resolve_term(chosen, attendance_term, remembered, today) {
                TermResolution::Resolved { term, source } => (term, source),
                TermResolution::NeedsChoice { suggestion } => {
                    self.emit(Event::HomeworkNeedsTerm {
                        options: semester::course_terms(&courses),
                        suggestion,
                        reason: "无法自动判定当前学期：考勤系统不可用，且没有选择或记住的学期。"
                            .to_owned(),
                    });
                    return Ok(None);
                }
            };
        // 考勤故障但仍在其他來源下繼續：明確告知使用者學期是從何而來的，
        // 否則他會以為看到的就是考勤認定的本學期。
        if let Some(err) = attendance_error {
            self.emit(Event::Notice(format!(
                "考勤系统暂时不可用（{err}），本学期改用{}判定",
                term_source.label()
            )));
        }

        // 可選學期：課程中出現過的學期；若目前學期不在其中（例如沿用上次選擇），
        // 也一併加入供切換。
        let mut term_options = semester::course_terms(&courses);
        if !term_options.contains(&term) {
            term_options.push(term);
            term_options.sort_unstable_by(|left, right| right.cmp(left));
        }

        let mut included: Vec<LmsCourse> = Vec::new();
        let mut skipped_terms = 0_usize;
        for course in courses {
            match semester::course_term(&course) {
                Some(code) if code == term => included.push(course),
                // 其他學期的課程不參與本輪查詢。
                Some(_) => {}
                None => skipped_terms += 1,
            }
        }

        let stage = if included.is_empty() {
            HomeworkStage::Done
        } else {
            HomeworkStage::Activities
        };
        let runner = HomeworkRunner {
            term,
            term_source,
            courses: included,
            skipped_terms,
            term_options,
            course_index: 0,
            activities: Vec::new(),
            activity_index: 0,
            inputs: Vec::new(),
            failed_courses: 0,
            since_emit: 0,
            stage,
            force,
            started,
            requests_baseline,
        };
        self.emit_homework(&runner);
        Ok(Some(runner))
    }

    /// 推進一格作業載入。
    fn homework_step(&mut self, runner: &mut HomeworkRunner) -> AppResult<()> {
        match runner.stage {
            HomeworkStage::Activities => {
                let course_id = runner
                    .current_course()
                    .map(|course| course.id.clone())
                    .ok_or_else(|| AppError::protocol("课程索引越界"))?;
                match self.lms_activities(&course_id, runner.force) {
                    Ok((activities, skipped)) => {
                        if skipped > 0 {
                            self.emit(Event::Notice(format!(
                                "已跳过 {skipped} 项无法解析的思源学堂数据"
                            )));
                        }
                        runner.activities = activities
                            .into_iter()
                            .filter(|activity| activity.kind() == ActivityKind::Homework)
                            .collect();
                        runner.activity_index = 0;
                        runner.stage = if runner.activities.is_empty() {
                            HomeworkStage::AdvanceCourse
                        } else {
                            HomeworkStage::Submission
                        };
                    }
                    // 登入態失效與連線層錯誤向上傳播（重登、路由回退或最終失敗）。
                    Err(err) if !is_recoverable(&err) => return Err(err),
                    // 單門課程的活動列表失敗：略過該課程，其餘課程照常載入，
                    // 並在彙總中以數量提示（不將整批標為失敗）。
                    Err(_) => {
                        runner.failed_courses += 1;
                        runner.stage = HomeworkStage::AdvanceCourse;
                    }
                }
            }
            HomeworkStage::Submission => {
                let activity = runner
                    .activities
                    .get(runner.activity_index)
                    .cloned()
                    .ok_or_else(|| AppError::protocol("活动索引越界"))?;
                let course = runner
                    .current_course()
                    .cloned()
                    .ok_or_else(|| AppError::protocol("课程索引越界"))?;
                let input = self.homework_input(&course, &activity, runner.force)?;
                runner.inputs.push(input);

                runner.activity_index += 1;
                runner.since_emit += 1;
                let finished_course = runner.activity_index >= runner.activities.len();
                if finished_course {
                    // 整門課程結束：由 `AdvanceCourse` 統一發出一次進度事件。
                    runner.stage = HomeworkStage::AdvanceCourse;
                } else if runner.since_emit >= PROGRESS_EMIT_INTERVAL {
                    // 課程尚未結束但已累積足夠作業：節流地補一次進度。
                    runner.since_emit = 0;
                    self.emit_homework(runner);
                }
            }
            HomeworkStage::AdvanceCourse => {
                runner.course_index += 1;
                runner.activities.clear();
                runner.activity_index = 0;
                runner.since_emit = 0;
                runner.stage = if runner.course_index >= runner.courses.len() {
                    HomeworkStage::Done
                } else {
                    HomeworkStage::Activities
                };
                self.emit_homework(runner);
            }
            HomeworkStage::Done => {}
        }
        Ok(())
    }

    /// 取得單一作業的提交摘要（詳情先行確定小組，再抓提交記錄）。
    fn homework_input(
        &mut self,
        course: &LmsCourse,
        activity: &LmsActivity,
        force: bool,
    ) -> AppResult<HomeworkInput> {
        let mut input = HomeworkInput {
            course_id: course.id.clone(),
            course_name: course.name.clone(),
            activity_id: activity.id.clone(),
            title: activity.display_title(),
            end_time: activity.end_time.clone(),
            submit_by_group: activity.submit_by_group.unwrap_or(false),
            submission_count: None,
            note: None,
        };

        match self.lms_submission_summary(&activity.id, force) {
            Ok(summary) => {
                input.submit_by_group = summary.submit_by_group;
                input.submission_count = summary.count;
                input.note = summary.note;
            }
            // 登入態失效與連線層錯誤向上傳播；其他單項失敗保留「待核实」。
            Err(err) if !is_recoverable(&err) => return Err(err),
            Err(err) => input.note = Some(submission_failure_note(&err)),
        }
        Ok(input)
    }

    /// 嘗試由考勤系統取得當前學期；未登入時回傳 `Ok(None)`（不觸發登入）。
    ///
    /// 已登入但遇到登入態失效或連線層錯誤時向上傳播（交由呼叫端走統一重登），
    /// 不再靜默降級為「沒有考勤學期」——否則作業清單會悄悄退回記憶中的舊學期，
    /// 使用者看不到登入已過期。
    fn attendance_term(&mut self) -> AppResult<Option<TermCode>> {
        let term = {
            let Some(session) = self.session.as_mut() else {
                return Ok(None);
            };
            if !session.is_logged_in(SiteKind::Attendance) {
                return Ok(None);
            }
            let mut api = AttendanceApi::new(session);
            match api.current_semester() {
                Ok(semester) => match TermCode::parse(&semester.term_name()) {
                    Some(term) => term,
                    // 學期名稱無法解析：可恢復，交由後續來源決定。
                    None => return Ok(None),
                },
                Err(err) if is_recoverable(&err) => return Ok(None),
                Err(err) => return Err(err),
            }
        };
        // 記住本會話得知的學期，供思源學堂課程分區使用（不重複請求）。
        self.known_term = Some(term);
        Ok(Some(term))
    }

    fn load_flow(&mut self, page: u32) -> AppResult<FlowData> {
        let session = self.session_mut()?;
        let mut api = AttendanceApi::new(session);
        let page_data = api.flow_page(page.max(1), FLOW_PAGE_SIZE)?;

        Ok(FlowData {
            total: page_data.total,
            total_pages: page_data.total_pages(),
            page: page_data.page,
            records: page_data.records,
        })
    }

    fn load_courses(&mut self, force: bool) -> AppResult<CoursesData> {
        let (courses, _) = self.lms_courses(force)?;
        Ok(CoursesData {
            courses,
            current_term: self.current_term_hint(),
        })
    }

    /// 目前已能確定的本學期（不觸發任何網路請求）：
    /// 使用者本次的明確選擇 → 本會話曾查得的考勤學期 → 設定檔記住的學期。
    fn current_term_hint(&self) -> Option<TermCode> {
        self.chosen_term.or(self.known_term).or_else(|| {
            self.config
                .homework_term
                .as_deref()
                .and_then(TermCode::parse)
        })
    }

    fn load_activities(&mut self, course_id: &str, force: bool) -> AppResult<Vec<LmsActivity>> {
        let (activities, _) = self.lms_activities(course_id, force)?;
        Ok(activities)
    }

    // ── 思源學堂快取 ─────────────────────────────────────

    /// 課程清單（含被跳過的項目數）；有效期內重用快取。
    fn lms_courses(&mut self, force: bool) -> AppResult<(Vec<LmsCourse>, usize)> {
        if let Some(cached) = self.cache.courses(force) {
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (courses, skipped) = api.my_courses()?;
        self.cache.store_courses(&courses, skipped);
        Ok((courses, skipped))
    }

    /// 課程活動（含被跳過的項目數）；有效期內重用快取。
    fn lms_activities(
        &mut self,
        course_id: &str,
        force: bool,
    ) -> AppResult<(Vec<LmsActivity>, usize)> {
        if let Some(cached) = self.cache.activities(course_id, force) {
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (activities, skipped) = api.course_activities(course_id)?;
        self.cache.store_activities(course_id, &activities, skipped);
        Ok((activities, skipped))
    }

    /// 活動詳情（記憶體內快取先行）。
    fn lms_activity_detail(&mut self, activity_id: &str, force: bool) -> AppResult<LmsActivity> {
        if let Some(cached) = self.cache.detail(activity_id, force) {
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.fetch_activity_detail(activity_id)?;
        self.cache.store_detail(activity_id, &detail);
        Ok(detail)
    }

    /// 作業提交摘要（快取先行；詳情先行確定小組）。
    fn lms_submission_summary(
        &mut self,
        activity_id: &str,
        force: bool,
    ) -> AppResult<SubmissionSummary> {
        if let Some(cached) = self.cache.summary(activity_id, force) {
            return Ok(cached);
        }
        let detail = self.lms_activity_detail(activity_id, force)?;
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let summary = api.submission_summary_for(&detail)?;
        self.cache.store_summary(activity_id, &summary);
        Ok(summary)
    }

    fn load_activity_detail(&mut self, activity_id: &str) -> AppResult<ActivityDetailView> {
        let activity = self.lms_activity_detail(activity_id, false)?;
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.activity_from(activity)?;
        let kind = detail.activity.kind();

        Ok(ActivityDetailView {
            id: detail.activity.id.clone(),
            title: detail.activity.display_title(),
            kind,
            end_time: detail.activity.end_time,
            submit_by_group: detail.activity.submit_by_group.unwrap_or(false),
            submissions: detail.submissions.map(|list| list.list),
            note: detail.note,
        })
    }

    /// 解析「開啟活動網頁」的目標網址（WebVPN 模式自動改址）。
    ///
    /// - 課程內容與直播：優先使用伺服器回傳的播放器網址。
    /// - 作業：開啟所屬課程的作業列表（前端路由；缺少課程識別碼時回退首頁）。
    /// - 資料與其他類型：思源學堂首頁（前端路由未經驗證，不拼接自造路徑）。
    fn open_activity_url(
        &mut self,
        activity_id: &str,
        course_id: Option<&str>,
        kind: ActivityKind,
    ) -> AppResult<String> {
        let mut url = lms::LOGIN_URL.to_owned();
        match kind {
            ActivityKind::Lesson | ActivityKind::LectureLive => {
                match self.lesson_player_url(activity_id) {
                    Ok(player_url) => url = player_url,
                    Err(err) if err.needs_relogin() => return Err(err),
                    Err(err) => {
                        self.emit(Event::Notice(format!(
                            "无法获取播放地址，已改为打开思源学堂首页：{err}"
                        )));
                    }
                }
            }
            ActivityKind::Homework => match course_id.and_then(lms::course_homework_url) {
                Some(homework_url) => url = homework_url,
                None => self.emit(Event::Notice(
                    "无法确定作业所属课程，已改为打开思源学堂首页".to_owned(),
                )),
            },
            ActivityKind::Material | ActivityKind::Unknown => {}
        }
        self.rewrite_for_mode(url)
    }

    /// 課程內容的播放器網址（由伺服器回傳，附帶存取 token）。
    fn lesson_player_url(&mut self, activity_id: &str) -> AppResult<String> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        api.lesson_player_url(activity_id)
    }

    /// WebVPN 模式下改寫校內網址；其他訪問模式或非校內站點原樣回傳。
    fn rewrite_for_mode(&self, url: String) -> AppResult<String> {
        let webvpn_mode = matches!(
            self.session
                .as_ref()
                .and_then(|session| session.access_mode(SiteKind::Lms)),
            Some(AccessMode::WebVpn)
        );
        if webvpn_mode && webvpn::should_rewrite(&url) {
            return webvpn::to_webvpn_url(&url);
        }
        Ok(url)
    }

    // ── 工具 ─────────────────────────────────────────────

    /// 目前會話已送出的請求數（診斷用）。
    fn request_count(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, SessionManager::request_count)
    }

    /// 發送一次作業載入更新（附耗時與本次載入已送出的請求數）。
    fn emit_homework(&self, runner: &HomeworkRunner) {
        let update = runner.update(
            runner.started.elapsed(),
            self.request_count()
                .saturating_sub(runner.requests_baseline),
        );
        self.emit(Event::Homework(update));
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

fn parse_date(value: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| AppError::protocol("学期开始日期格式无法识别（应为 YYYY-MM-DD）"))
}

#[cfg(test)]
#[path = "tests/worker_test.rs"]
mod worker_test;

#[cfg(test)]
#[path = "tests/scheduler_test.rs"]
mod scheduler_test;
