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
use crate::domain::homework::{HomeworkInput, HomeworkItem};
use crate::domain::semester::{self, TermCode, TermResolution, TermSource};
use crate::domain::{attendance_match, homework, schedule};
use crate::error::{AppError, AppResult};
use crate::session::{AccessMode, LoginStage, SessionManager, SiteKind};
use crate::sites::attendance::{AttendanceApi, AttendanceSite};
use crate::sites::lms::{
    self, ActivityKind, LmsActivity, LmsApi, LmsCourse, LmsSite, SubmissionSummary,
    submission_failure_note,
};
use crate::tui::app::{ActivityDetailView, FlowData, LessonEntry, ScheduleData};

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

/// 介面送到背景的任務。
#[derive(Debug, Clone)]
pub enum Job {
    /// 首次建立保險庫並登入。
    CreateVault {
        /// 使用者設定的加密口令。
        passphrase: Secret,
        /// 帳號密碼。
        credentials: Credentials,
    },
    /// 解鎖保險庫。
    Unlock {
        /// 加密口令。
        passphrase: Secret,
    },
    /// 提交圖片驗證碼。
    SubmitCaptcha(String),
    /// 重新取得驗證碼圖片。
    RefreshCaptcha,
    /// 發送簡訊驗證碼。
    SendMfaCode,
    /// 提交簡訊驗證碼。
    VerifyMfaCode(String),
    /// 重新開始登入（登入失敗後重試）。
    RetryLogin,
    /// 以重新輸入的帳號密碼重試登入，成功後才寫入保險庫。
    RetryWithAccount {
        /// 使用者重新輸入的帳號密碼。
        credentials: Credentials,
        /// 加密口令（用於登入成功後更新保險庫）。
        passphrase: Secret,
    },
    /// 載入課表（含本週考勤）。
    LoadSchedule,
    /// 載入作業彙總。
    LoadHomework {
        /// 是否略過快取強制重新查詢。
        force: bool,
    },
    /// 載入考勤流水。
    LoadFlow {
        /// 頁碼。
        page: u32,
    },
    /// 載入思源學堂課程。
    LoadCourses {
        /// 是否略過快取強制重新查詢。
        force: bool,
    },
    /// 載入課程活動。
    LoadActivities {
        /// 課程識別碼。
        course_id: String,
        /// 是否略過快取強制重新查詢。
        force: bool,
    },
    /// 載入活動詳情。
    LoadActivityDetail {
        /// 活動識別碼。
        activity_id: String,
    },
    /// 開啟活動網頁（`o`）：解析目標網址後以系統瀏覽器開啟。
    OpenActivity {
        /// 活動識別碼。
        activity_id: String,
        /// 所屬課程識別碼（作業的前端網址需要；其他類型可為 `None`）。
        course_id: Option<String>,
        /// 活動類型（決定網址來源）。
        kind: ActivityKind,
    },
    /// 記住使用者選擇的學期，並重新載入作業。
    SetHomeworkTerm {
        /// 學期代碼（`YYYY-YYYY+1-T`）。
        term: String,
    },
    /// 修改帳號。
    ChangeAccount {
        /// 原加密口令。
        passphrase: Secret,
        /// 新帳號密碼。
        credentials: Credentials,
    },
    /// 修改加密口令。
    ChangePassphrase {
        /// 原口令。
        old: Secret,
        /// 新口令。
        new: Secret,
    },
    /// 切換訪問策略。
    SetAccessPolicy(AccessPolicy),
    /// 記錄使用者已同意的用户协议版本。
    AcceptAgreement,
    /// 取消進行中的登入流程（介面關閉登入覆蓋層時）。
    ///
    /// 沒有這個任務時，「登入互動期間資料任務一律延後」會讓關閉覆蓋層後的
    /// 重新整理永遠排不到：`r` 送出的任務只能躺在待執行佇列裡。
    CancelLogin,
    /// 結束工作執行緒。
    Shutdown,
}

impl Job {
    /// 任務說明（用於錯誤訊息）。
    pub fn label(&self) -> String {
        match self {
            Self::CreateVault { .. } => "创建凭证".to_owned(),
            Self::Unlock { .. } => "解锁凭证".to_owned(),
            Self::SubmitCaptcha(_) | Self::RefreshCaptcha => "验证码".to_owned(),
            Self::SendMfaCode | Self::VerifyMfaCode(_) => "短信验证".to_owned(),
            Self::RetryLogin => "登录".to_owned(),
            Self::RetryWithAccount { .. } => "重新输入账户".to_owned(),
            Self::LoadSchedule => "课表".to_owned(),
            Self::LoadHomework { .. } => "作业".to_owned(),
            Self::LoadFlow { .. } => "考勤流水".to_owned(),
            Self::LoadCourses { .. }
            | Self::LoadActivities { .. }
            | Self::LoadActivityDetail { .. } => "思源学堂".to_owned(),
            Self::OpenActivity { .. } => "打开活动".to_owned(),
            Self::SetHomeworkTerm { .. } => "学期选择".to_owned(),
            Self::ChangeAccount { .. } => "修改账号".to_owned(),
            Self::ChangePassphrase { .. } => "修改口令".to_owned(),
            Self::SetAccessPolicy(_) => "访问模式".to_owned(),
            Self::AcceptAgreement => "用户协议".to_owned(),
            Self::CancelLogin => "取消登录".to_owned(),
            Self::Shutdown => String::new(),
        }
    }

    /// 是否為控制任務（登入、設定、憑證）；其餘為資料載入任務。
    pub fn is_control(&self) -> bool {
        !matches!(
            self,
            Self::LoadSchedule
                | Self::LoadHomework { .. }
                | Self::LoadFlow { .. }
                | Self::LoadCourses { .. }
                | Self::LoadActivities { .. }
                | Self::LoadActivityDetail { .. }
                | Self::OpenActivity { .. }
        )
    }

    /// 資料任務的合併鍵；同鍵的排隊請求視為重複而合併。
    fn data_key(&self) -> Option<DataKey> {
        match self {
            Self::LoadSchedule => Some(DataKey::Schedule),
            Self::LoadHomework { .. } => Some(DataKey::Homework),
            Self::LoadFlow { page } => Some(DataKey::Flow(*page)),
            Self::LoadCourses { .. } => Some(DataKey::Courses),
            Self::LoadActivities { course_id, .. } => Some(DataKey::Activities(course_id.clone())),
            Self::LoadActivityDetail { activity_id } => {
                Some(DataKey::ActivityDetail(activity_id.clone()))
            }
            Self::OpenActivity { activity_id, .. } => {
                Some(DataKey::OpenActivity(activity_id.clone()))
            }
            _ => None,
        }
    }

    /// 是否為略過快取的強制刷新（僅作業、課程與活動載入帶有 `force`）。
    fn is_forced(&self) -> bool {
        match self {
            Self::LoadHomework { force }
            | Self::LoadCourses { force }
            | Self::LoadActivities { force, .. } => *force,
            _ => false,
        }
    }
}

/// 資料任務的合併鍵。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DataKey {
    Schedule,
    Homework,
    Flow(u32),
    Courses,
    Activities(String),
    ActivityDetail(String),
    OpenActivity(String),
}

/// 背景回報的事件。
#[derive(Debug)]
pub enum Event {
    /// 保險庫已建立或解鎖；介面可直接進入主畫面並觸發頁面載入。
    VaultReady,
    /// 登入進度。
    LoginProgress(String),
    /// 需要圖片驗證碼（附圖檔路徑）。
    LoginNeedsCaptcha(PathBuf),
    /// 需要簡訊驗證碼。
    LoginNeedsMfa {
        /// 綁定手機號（中間遮蔽）。
        phone: Option<String>,
        /// 是否已發送驗證碼。
        sent: bool,
    },
    /// 登入失敗。
    LoginFailed(String),
    /// 登入成功（附完成登入的站點與實際訪問方式）。
    LoginSucceeded {
        /// 完成登入的站點。
        site: SiteKind,
        /// 實際使用的訪問方式（無法取得時為 `None`）。
        mode: Option<AccessMode>,
    },
    /// 會話已全部重置（解鎖、換帳號或切換訪問模式）：介面清除站點登入狀態。
    SessionsCleared {
        /// 是否因更換帳號而重置：為真時介面必須清空所有頁面的舊資料
        ///（屬於前一個帳號）；為假（切換訪問模式）時資料仍有效，
        /// 只需解除卡住的載入狀態。
        account_changed: bool,
    },
    /// 進行中的資料載入已被取消（換帳號或切換訪問模式）：
    /// 介面解除該頁的載入中狀態，保留已取得的資料。
    LoadingCancelled {
        /// 被取消任務所屬的介面位置。
        target: FailedTarget,
    },
    /// 站點登入態已失效，即將重新登入。
    SessionExpired {
        /// 站點。
        site: SiteKind,
    },
    /// 課表資料。
    Schedule(Box<ScheduleData>),
    /// 作業載入更新（部分結果或最終結果）。
    Homework(HomeworkUpdate),
    /// 無法自動判定本學期，需要使用者選擇（附課程中出現的學期選項）。
    HomeworkNeedsTerm {
        /// 可選學期（由新到舊）。
        options: Vec<TermCode>,
        /// 依日期推算的建議學期（僅作預選）。
        suggestion: Option<TermCode>,
        /// 無法判定的原因。
        reason: String,
    },
    /// 考勤流水資料。
    Flow(Box<FlowData>),
    /// 課程列表（含當前學期提示）。
    Courses(CoursesData),
    /// 課程活動列表。
    Activities(Vec<LmsActivity>),
    /// 活動詳情。
    ActivityDetail(Box<ActivityDetailView>),
    /// 已解析的活動網址（依訪問模式改寫完成，等待介面以瀏覽器開啟）。
    OpenUrl(String),
    /// 帳號已更新。
    AccountUpdated,
    /// 加密口令已更新。
    PassphraseUpdated,
    /// 訪問策略已更新。
    AccessPolicyUpdated(AccessPolicy),
    /// 使用者已同意本版用户协议（版本已寫入設定檔）。
    AgreementAccepted,
    /// 提示訊息（例如有資料因格式問題被跳過）。
    Notice(String),
    /// 任務失敗。
    Failed {
        /// 任務名稱。
        what: String,
        /// 錯誤訊息。
        message: String,
        /// 失敗所屬的介面位置。
        target: FailedTarget,
    },
}

/// 錯誤所屬的介面位置（供介面只標記受影響的頁面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedTarget {
    /// 課表頁。
    Schedule,
    /// 作業頁。
    Homework,
    /// 考勤流水頁。
    Flow,
    /// 思源學堂課程列表。
    Courses,
    /// 思源學堂活動列表。
    Activities,
    /// 思源學堂活動詳情。
    ActivityDetail,
    /// 開啟活動網頁（不動任何頁面）。
    ActivityOpen,
    /// 登入流程（驗證碼、簡訊、重試）。
    Login,
    /// 憑證操作（建立保險庫、解鎖、修改帳號或口令）。
    Credentials,
    /// 帳戶設定（訪問模式等）。
    Settings,
    /// 用户协议閱讀門（顯示與同意）。
    Agreement,
}

/// 課程列表與當前學期（供介面分區顯示）。
#[derive(Debug)]
pub struct CoursesData {
    /// 課程清單（伺服器原始順序）。
    pub courses: Vec<LmsCourse>,
    /// 判定出的當前學期；`None` 表示無法判定（介面不分區）。
    pub current_term: Option<TermCode>,
}

/// 作業頁的一次載入更新（部分結果或最終結果）。
#[derive(Debug)]
pub struct HomeworkUpdate {
    /// 學期標籤（無法判定學期時為 `None`）。
    pub term_label: Option<String>,
    /// 學期判定來源（無法判定學期時為 `None`）。
    pub term_source: Option<TermSource>,
    /// 納入查詢的課程數。
    pub courses_included: usize,
    /// 因缺少學期資訊而未納入查詢的課程數。
    pub courses_skipped: usize,
    /// 可選學期（由新到舊；供介面顯示選擇器）。
    pub term_options: Vec<TermCode>,
    /// 目前已彙總的作業。
    pub items: Vec<HomeworkItem>,
    /// 提交狀態無法確認的原因彙總（依項數遞減）。
    pub issues: Vec<HomeworkIssue>,
    /// 因活動列表查詢失敗（非認證、非連線層錯誤）而略過的課程數。
    pub courses_failed: usize,
    /// 載入進度（已完成課程數, 課程總數）；`None` 表示已載入完成。
    pub progress: Option<(usize, usize)>,
    /// 本次載入已花費的時間（診斷用）。
    pub elapsed: Duration,
    /// 本次載入已送出的 HTTP 請求數（診斷用）。
    pub requests: usize,
}

/// 「待核实」作業的共同原因彙總（同一原因只列一次）。
#[derive(Debug, Clone)]
pub struct HomeworkIssue {
    /// 原因（階段化的失敗說明）。
    pub reason: String,
    /// 受影響的作業項數。
    pub count: usize,
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
    /// 最近一次取得的驗證碼圖片路徑（登入結束或重新開始時刪除）。
    captcha_path: Option<PathBuf>,
    /// 待執行的資料任務（依序、已去重）。
    pending_data: VecDeque<Job>,
    /// 資料任務代際：帳號或訪問模式變更時遞增，進行中的任務自動中止。
    generation: u64,
    /// 本輪載入已嘗試的自動重新登入次數（全新的載入請求歸零）。
    relogin_attempts: u8,
    /// 思源學堂課程／活動快取。
    cache: LmsCache,
    /// 本會話曾查得的考勤學期（供課程分區使用，不重複請求）。
    known_term: Option<TermCode>,
    /// 已收到結束指令；[`Worker::run`] 於迴圈開頭立即返回。
    shutdown: bool,
}

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
        captcha_path: None,
        pending_data: VecDeque::new(),
        generation: 0,
        relogin_attempts: 0,
        cache: LmsCache::default(),
        known_term: None,
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
        if let Err(err) = self.dispatch_control(job) {
            // 登入類任務出錯即視為本次登入結束：清掉暫存的驗證碼圖片。
            if target == FailedTarget::Login {
                self.clear_captcha();
            }
            self.emit(Event::Failed {
                what: if what.is_empty() {
                    "操作".to_owned()
                } else {
                    what
                },
                message: err.to_string(),
                target,
            });
        }
        shutdown
    }

    /// 回報資料任務失敗。
    fn emit_failed(&self, job: &Job, err: AppError) {
        let what = job.label();
        self.emit(Event::Failed {
            what: if what.is_empty() {
                "操作".to_owned()
            } else {
                what
            },
            message: err.to_string(),
            target: failed_target_of(job),
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
    fn dispatch_control(&mut self, job: Job) -> AppResult<()> {
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
            Job::RetryLogin => self.retry_login(),
            Job::RetryWithAccount {
                credentials,
                passphrase,
            } => self.retry_with_account(&passphrase, credentials),
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
        self.vault.store(passphrase, &credentials)?;
        self.start_session(credentials)?;
        self.emit(Event::AccountUpdated);
        self.emit(Event::VaultReady);
        self.report_vault_permissions();
        Ok(())
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
        self.emit(Event::PassphraseUpdated);
        Ok(())
    }

    /// 記住使用者選擇的學期，並立即重新載入作業。
    fn set_homework_term(&mut self, term: &str) -> AppResult<()> {
        let term = TermCode::parse(term)
            .ok_or_else(|| AppError::protocol(format!("学期格式无法识别：{term}")))?;
        self.config.homework_term = Some(term.to_string());
        self.config.save()?;
        self.emit(Event::Notice(format!("已记住学期 {}", term.label())));
        // 學期已變更：任何早於此開始的載入都基於舊學期，必須確保佇列中恰有
        // 一筆強制重載（忽略執行中任務），切換才會立即生效。
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
    /// 排不到。沒有進行中的流程時不做任何事（也不覆蓋介面已顯示的提示）。
    fn cancel_login(&mut self) -> AppResult<()> {
        if self.flow.is_none() && self.pending_vault.is_none() {
            return Ok(());
        }
        self.flow = None;
        self.discard_pending_vault();
        self.retry = None;
        self.clear_captcha();
        self.emit(Event::Notice("已取消登录流程，可重新刷新页面".to_owned()));
        Ok(())
    }

    /// 丟棄待存憑證，並把記憶體中的憑證還原為嘗試新憑證前的版本。
    ///
    /// 新憑證只有在登入成功後才寫入保險庫；取消或憑證被拒時，記憶體中的
    /// 憑證必須回到保險庫仍保存的舊憑證，否則之後的自動重登會拿一組從未
    /// 驗證、也沒被保存的憑證去登入。
    fn discard_pending_vault(&mut self) {
        let Some(pending) = self.pending_vault.take() else {
            return;
        };
        let Some(previous) = pending.previous else {
            return;
        };
        if let Some(session) = self.session.as_mut() {
            session.set_credentials(previous.clone());
        }
        self.credentials = Some(previous);
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
                let reply = driver.start(&credentials, AccountType::Undergraduate)?;
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
        match reply {
            LoginReply::Success => self.complete_flow(),
            LoginReply::Fail { message } => {
                self.flow = None;
                // 憑證被拒：丟棄待存憑證（並還原舊憑證），不覆蓋保險庫中的舊憑證。
                self.discard_pending_vault();
                self.clear_captcha();
                self.emit(Event::LoginFailed(message));
                Ok(())
            }
            LoginReply::NeedCaptcha => {
                let path = self.driver()?.fetch_captcha()?;
                self.captcha_path = Some(path.clone());
                self.emit(Event::LoginNeedsCaptcha(path));
                Ok(())
            }
            LoginReply::NeedMfa => {
                let phone = self.driver_mut()?.mfa_phone().ok();
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
        // 登入成功後才更新保險庫，失敗的憑證不會覆蓋舊憑證。
        self.commit_pending_vault();
        let mode = self
            .session
            .as_ref()
            .and_then(|session| session.access_mode(site));
        self.emit(Event::LoginSucceeded { site, mode });
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
    fn commit_pending_vault(&mut self) {
        let Some(pending) = self.pending_vault.take() else {
            return;
        };

        match self.vault.store(&pending.passphrase, &pending.credentials) {
            Ok(()) => self.emit(Event::AccountUpdated),
            Err(err) => self.emit(Event::Notice(format!("登录成功，但凭据保存失败：{err}"))),
        }
    }

    fn submit_captcha(&mut self, code: &str) -> AppResult<()> {
        let reply = self.driver_mut()?.submit_captcha(code)?;
        self.handle_reply(reply)
    }

    fn refresh_captcha(&mut self) -> AppResult<()> {
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

    fn retry_login(&mut self) -> AppResult<()> {
        // 使用者手動重試：自動重登額度重新計算。
        self.relogin_attempts = 0;
        self.begin_login(SiteKind::Attendance, None)
    }

    /// 以使用者重新輸入的憑證重試登入；先驗證口令，登入成功後才寫入保險庫。
    fn retry_with_account(&mut self, passphrase: &str, credentials: Credentials) -> AppResult<()> {
        // 口令錯誤時回報 [`AppError::WrongPassphrase`]，舊憑證不受影響。
        self.vault.load(passphrase)?;

        // 使用者手動重試：自動重登額度重新計算。
        self.relogin_attempts = 0;
        // 先記下舊憑證，取消或憑證被拒時才能還原（見 `discard_pending_vault`）。
        let previous = self.credentials.clone();
        self.session_mut()?.set_credentials(credentials.clone());
        self.credentials = Some(credentials.clone());
        self.pending_vault = Some(PendingVault {
            passphrase: Secret::from(passphrase),
            credentials,
            previous,
        });
        self.begin_login(SiteKind::Attendance, None)
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
            Err(err) => self.report_data_failure(job, generation, err),
        }
    }

    /// 執行一次單步請求，回傳要回報的事件。
    fn load_once(&mut self, job: &Job) -> AppResult<Option<Event>> {
        let event = match job {
            Job::LoadSchedule => Event::Schedule(Box::new(self.load_schedule()?)),
            Job::LoadFlow { page } => Event::Flow(Box::new(self.load_flow(*page)?)),
            Job::LoadCourses { force } => Event::Courses(self.load_courses(*force)?),
            Job::LoadActivities { course_id, force } => {
                Event::Activities(self.load_activities(course_id, *force)?)
            }
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
    fn report_data_failure(&mut self, job: Job, generation: u64, err: AppError) {
        // 代際已變（帳號或訪問模式被切換）：舊任務的錯誤直接忽略。
        if generation != self.generation {
            return;
        }
        let Some(site) = site_of(&job) else {
            self.emit_failed(&job, err);
            return;
        };

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
                    self.emit_failed(&job, AppError::ReloginExhausted);
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
                    self.emit_failed(&job, login_err);
                }
            }
            return;
        }

        self.emit_failed(&job, err);
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
        let job = Job::LoadHomework { force };

        let mut runner = match self.begin_homework(force) {
            Ok(Some(runner)) => runner,
            Ok(None) => return,
            Err(err) => {
                self.report_data_failure(job, generation, err);
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
            if matches!(runner.stage, HomeworkStage::Done) {
                self.emit_homework(&runner);
                return;
            }
            if let Err(err) = self.homework_step(&mut runner) {
                self.report_data_failure(job, generation, err);
                return;
            }
        }
    }

    /// 準備作業載入：判定學期、過濾課程並回報首批進度。
    fn begin_homework(&mut self, force: bool) -> AppResult<Option<HomeworkRunner>> {
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

        let attendance_term = self.attendance_term();
        let remembered = self
            .config
            .homework_term
            .as_deref()
            .and_then(TermCode::parse);
        let today = Local::now().date_naive();

        let (term, term_source) = match semester::resolve_term(attendance_term, remembered, today) {
            TermResolution::Resolved { term, source } => (term, source),
            TermResolution::NeedsChoice { suggestion } => {
                self.emit(Event::HomeworkNeedsTerm {
                    options: semester::course_terms(&courses),
                    suggestion,
                    reason: "无法自动判定当前学期：考勤系统不可用，且没有记住的学期。".to_owned(),
                });
                return Ok(None);
            }
        };

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
                if runner.activity_index >= runner.activities.len() {
                    runner.stage = HomeworkStage::AdvanceCourse;
                }
                self.emit_homework(runner);
            }
            HomeworkStage::AdvanceCourse => {
                runner.course_index += 1;
                runner.activities.clear();
                runner.activity_index = 0;
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

    /// 嘗試由考勤系統取得當前學期；未登入或查詢失敗時回傳 `None`（不觸發登入）。
    fn attendance_term(&mut self) -> Option<TermCode> {
        let term = {
            let session = self.session.as_mut()?;
            if !session.is_logged_in(SiteKind::Attendance) {
                return None;
            }
            let mut api = AttendanceApi::new(session);
            let semester = api.current_semester().ok()?;
            TermCode::parse(&semester.term_name())?
        };
        // 記住本會話得知的學期，供思源學堂課程分區使用（不重複請求）。
        self.known_term = Some(term);
        Some(term)
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
    /// 本會話曾查得的考勤學期 → 使用者記住的學期 → 無法判定。
    fn current_term_hint(&self) -> Option<TermCode> {
        self.known_term.or_else(|| {
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

/// 任務所屬站點。
fn site_of(job: &Job) -> Option<SiteKind> {
    match job {
        Job::LoadSchedule | Job::LoadFlow { .. } => Some(SiteKind::Attendance),
        Job::LoadHomework { .. }
        | Job::LoadCourses { .. }
        | Job::LoadActivities { .. }
        | Job::LoadActivityDetail { .. }
        | Job::OpenActivity { .. } => Some(SiteKind::Lms),
        _ => None,
    }
}

/// 任務失敗時應由介面標記的位置。
fn failed_target_of(job: &Job) -> FailedTarget {
    match job {
        Job::LoadSchedule => FailedTarget::Schedule,
        Job::LoadHomework { .. } => FailedTarget::Homework,
        Job::LoadFlow { .. } => FailedTarget::Flow,
        Job::LoadCourses { .. } => FailedTarget::Courses,
        Job::LoadActivities { .. } => FailedTarget::Activities,
        Job::LoadActivityDetail { .. } => FailedTarget::ActivityDetail,
        Job::OpenActivity { .. } => FailedTarget::ActivityOpen,
        Job::AcceptAgreement => FailedTarget::Agreement,
        Job::SubmitCaptcha(_)
        | Job::RefreshCaptcha
        | Job::SendMfaCode
        | Job::VerifyMfaCode(_)
        | Job::RetryLogin
        | Job::RetryWithAccount { .. }
        | Job::CancelLogin => FailedTarget::Login,
        Job::CreateVault { .. }
        | Job::Unlock { .. }
        | Job::ChangeAccount { .. }
        | Job::ChangePassphrase { .. } => FailedTarget::Credentials,
        _ => FailedTarget::Settings,
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
        .map_err(|err| AppError::protocol(format!("学期开始日期无法解析（{value}）：{err}")))
}

#[cfg(test)]
#[path = "tests/worker_test.rs"]
mod worker_test;

#[cfg(test)]
#[path = "tests/scheduler_test.rs"]
mod scheduler_test;
