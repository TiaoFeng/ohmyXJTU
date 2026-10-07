//! 任務與事件的協定型別。
//!
//! 介面（`tui`）與背景工作執行緒之間的合約：[`Job`] 是介面下達的指令、
//! [`Event`] 是背景回報的結果、[`FailedTarget`] 說明失敗該落在哪個畫面。
//! 合併規則（`DataKey`、`Job::is_forced`）描述「任務是什麼」，因此與型別
//! 同住；怎麼執行則屬於 `super::worker` 的調度核心。
//!
//! 純映射函式（[`site_of`]、[`failed_target_of`]、[`resource_of`]、
//! [`is_account_switch_step`]）由 [`Job`] 推導站點、失敗落點與資源識別碼。

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::config::AccessPolicy;
use crate::credentials::{Credentials, Secret};
use crate::domain::homework::HomeworkItem;
use crate::domain::semester::{TermCode, TermSource};
use crate::domain::todo::Task;
use crate::model::{ActivityDetailView, FlowData, ScheduleData};
use crate::session::{AccessMode, SiteKind};
use crate::sites::lms::{ActivityKind, LmsActivity, LmsCourse};

/// 等待任務服務回應的上限。
///
/// 任務服務只做本機檔案與密碼學運算，正常情況下不會久等；超過這個時間就
/// 視為沒有回應（換口令會因此回報失敗並把任務檔換回舊口令）。
const TASK_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// 等待把手的共享狀態：結果插槽與喚醒用的條件變數。
type ReplyState = Arc<(Mutex<Option<Result<(), String>>>, Condvar)>;

/// 同步任務操作的等待把手。
///
/// 解鎖與修改口令必須等任務服務確認結果（換口令要在兩個檔案之間保持一致的
/// 順序）。[`Job`] 需要 `Clone`，而回覆通道不可複製，因此以共享狀態與條件變數
/// 實作；等待有上限，即使任務服務意外停止也不會讓工作者永久卡住。
#[derive(Clone, Default)]
pub struct TaskReply(ReplyState);

impl std::fmt::Debug for TaskReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TaskReply")
    }
}

impl TaskReply {
    /// 建立等待把手。
    pub fn new() -> Self {
        Self::default()
    }

    /// 由任務服務填入結果並喚醒等待者。
    pub fn resolve(&self, result: Result<(), String>) {
        let (slot, ready) = &*self.0;
        let mut slot = slot.lock().unwrap_or_else(|err| err.into_inner());
        *slot = Some(result);
        ready.notify_all();
    }

    /// 等待結果（任務服務只做本機檔案與密碼學運算，因此不會久等）。
    pub fn wait(&self) -> Result<(), String> {
        let (slot, ready) = &*self.0;
        let mut guard = slot.lock().unwrap_or_else(|err| err.into_inner());
        while guard.is_none() {
            let (next, timeout) = ready
                .wait_timeout(guard, TASK_REPLY_TIMEOUT)
                .unwrap_or_else(|err| err.into_inner());
            guard = next;
            if timeout.timed_out() && guard.is_none() {
                return Err("任务服务没有响应".to_owned());
            }
        }
        guard.clone().unwrap_or(Ok(()))
    }
}

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
    /// 解鎖後在背景預熱：登入兩個站點，再預載四個頁面的資料。
    ///
    /// 由介面在收到 [`Event::VaultReady`] 後立即送出（先於目前頁面的載入
    /// 任務）。工作者會依序登入尚未登入的站點——需要驗證碼或簡訊驗證時
    /// 走既有的互動覆蓋層——全部就緒後才把四個載入任務排入佇列，因此
    /// 使用者切換頁面時不必再等登入與首次載入。
    ///
    /// 預載只是「提前做」，失敗不影響解鎖：錯誤以 [`FailedTarget::Preload`]
    /// 回報，介面只顯示提示，四個頁面維持未載入，進入該頁時仍會正常重載。
    Preload,
    /// 提交圖片驗證碼（自動零化，避免明碼進入 `Debug`）。
    SubmitCaptcha(Secret),
    /// 重新取得驗證碼圖片。
    RefreshCaptcha,
    /// 發送簡訊驗證碼。
    SendMfaCode,
    /// 提交簡訊驗證碼（自動零化）。
    VerifyMfaCode(Secret),
    /// 重新開始登入（登入失敗後重試；沿用實際失敗的站點）。
    RetryLogin {
        /// 要重新登入的站點。
        site: SiteKind,
    },
    /// 以重新輸入的帳號密碼重試登入，成功後才寫入保險庫。
    RetryWithAccount {
        /// 原本失敗的站點（重試沿用同一個站點）。
        site: SiteKind,
        /// 使用者重新輸入的帳號密碼。
        credentials: Credentials,
        /// 加密口令（用於登入成功後更新保險庫）。
        passphrase: Secret,
    },
    /// 載入課表（含指定週次的考勤）。
    ///
    /// 週次由 [`Job::SetScheduleWeek`] 保存在工作者狀態；未指定時跟隨當前週。
    LoadSchedule {
        /// 是否略過課表快取強制重新查詢（使用者按 `r`）。
        force: bool,
    },
    /// 記住使用者選擇的週次；介面需要該週資料時一併重新載入（`[`／`]`）。
    ///
    /// `reload` 為假代表介面已持有該週資料（課表週快取），工作者只需記住週次
    /// ——後續按 `r` 才知道要重新載入哪一週，也不必再查一次考勤。
    SetScheduleWeek {
        /// 目標週次（1 起算）。
        week: u32,
        /// 是否重新載入該週（介面沒有這週的資料時為真）。
        reload: bool,
    },
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
        /// 是否略過快取強制重新查詢（使用者按 `r`）。
        force: bool,
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
    /// 新增自訂義任務。
    AddTask {
        /// 任務內容（識別碼由存儲指派）。
        task: Task,
    },
    /// 以新內容覆蓋指定任務。
    UpdateTask {
        /// 任務識別碼。
        id: u64,
        /// 新內容。
        task: Task,
    },
    /// 設定單一任務的完成狀態（`space`）。
    SetTaskDone {
        /// 任務識別碼。
        id: u64,
        /// 是否完成。
        done: bool,
    },
    /// 批次設定多個任務的完成狀態（多選菜單）。
    SetTasksDone {
        /// 任務識別碼。
        ids: Vec<u64>,
        /// 是否完成。
        done: bool,
    },
    /// 刪除單一任務（`^D` 連按兩次）。
    DeleteTask {
        /// 任務識別碼。
        id: u64,
    },
    /// 批次刪除多個任務（多選菜單）。
    DeleteTasks {
        /// 任務識別碼。
        ids: Vec<u64>,
    },
    /// 刪除所有已完成任務（`^T` 設置）。
    DeleteCompletedTasks,
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
    /// 以口令載入任務檔（解鎖與建立保險庫後由工作者送出）。
    ///
    /// 這是內部訊息：任務服務才是任務檔的擁有者，介面不會送它。
    InitTasks {
        /// 加密口令。
        passphrase: Secret,
    },
    /// 以新口令重新加密任務檔（修改口令時由工作者送出）。
    ///
    /// `reply` 讓工作者在寫入保險庫之前先確認結果；任務服務只做本機運算，
    /// 因此這個等待很短。
    RekeyTasks {
        /// 新口令。
        passphrase: Secret,
        /// 結果把手。
        reply: TaskReply,
    },
    /// 丟棄任務檔的記憶體金鑰（會話被停用時）。
    LockTasks,
    /// 結束工作執行緒。
    Shutdown,
}

impl Job {
    /// 任務說明（用於錯誤訊息）。
    pub fn label(&self) -> String {
        match self {
            Self::CreateVault { .. } => "创建凭证".to_owned(),
            Self::Unlock { .. } => "解锁凭证".to_owned(),
            Self::Preload => "预载".to_owned(),
            Self::SubmitCaptcha(_) | Self::RefreshCaptcha => "验证码".to_owned(),
            Self::SendMfaCode | Self::VerifyMfaCode(_) => "短信验证".to_owned(),
            Self::RetryLogin { .. } => "登录".to_owned(),
            Self::RetryWithAccount { .. } => "重新输入账户".to_owned(),
            Self::LoadSchedule { .. } => "课表".to_owned(),
            Self::SetScheduleWeek { .. } => "课表周次".to_owned(),
            Self::LoadHomework { .. } => "作业".to_owned(),
            Self::LoadFlow { .. } => "考勤流水".to_owned(),
            Self::LoadCourses { .. }
            | Self::LoadActivities { .. }
            | Self::LoadActivityDetail { .. } => "思源学堂".to_owned(),
            Self::OpenActivity { .. } => "打开活动".to_owned(),
            Self::SetHomeworkTerm { .. } => "学期选择".to_owned(),
            Self::AddTask { .. } => "添加任务".to_owned(),
            Self::UpdateTask { .. } => "修改任务".to_owned(),
            Self::SetTaskDone { .. } | Self::SetTasksDone { .. } => "标记任务".to_owned(),
            Self::DeleteTask { .. } | Self::DeleteTasks { .. } | Self::DeleteCompletedTasks => {
                "删除任务".to_owned()
            }
            Self::ChangeAccount { .. } => "修改账号".to_owned(),
            Self::ChangePassphrase { .. } => "修改口令".to_owned(),
            Self::SetAccessPolicy(_) => "访问模式".to_owned(),
            Self::AcceptAgreement => "用户协议".to_owned(),
            Self::CancelLogin => "取消登录".to_owned(),
            Self::InitTasks { .. } | Self::RekeyTasks { .. } | Self::LockTasks => "任务".to_owned(),
            Self::Shutdown => String::new(),
        }
    }

    /// 是否為自訂義任務操作（由任務服務處理，不經過網路）。
    pub fn is_task_op(&self) -> bool {
        matches!(
            self,
            Self::AddTask { .. }
                | Self::UpdateTask { .. }
                | Self::SetTaskDone { .. }
                | Self::SetTasksDone { .. }
                | Self::DeleteTask { .. }
                | Self::DeleteTasks { .. }
                | Self::DeleteCompletedTasks
        )
    }

    /// 是否為控制任務（登入、設定、憑證）；其餘為資料載入任務。
    pub fn is_control(&self) -> bool {
        !matches!(
            self,
            Self::LoadSchedule { .. }
                | Self::LoadHomework { .. }
                | Self::LoadFlow { .. }
                | Self::LoadCourses { .. }
                | Self::LoadActivities { .. }
                | Self::LoadActivityDetail { .. }
                | Self::OpenActivity { .. }
        )
    }

    /// 是否為互動式資料任務（使用者正在等待結果）。
    ///
    /// 這類任務由使用者操作直接觸發（例如按 `o` 開啟活動網頁）：長載入
    ///（作業彙總）進行時必須在下一個步進邊界立即執行，而不是排在整輪
    /// 載入之後。它仍屬於資料任務——去重與統一重新登入重試等語意不變。
    pub fn is_interactive(&self) -> bool {
        matches!(self, Self::OpenActivity { .. })
    }

    /// 連線層失敗時，是否可以原樣重送這個任務。
    ///
    /// 絕大多數任務重送一次就只是「再送同一個請求」，但發送簡訊驗證碼
    /// **有可見的副作用**：逾時可能代表請求已經送達、只是回應沒收到，重送
    /// 會讓使用者收到兩條簡訊。這類任務不自動重試，失敗直接回報，由使用者
    /// 自行決定要不要再按一次。
    pub fn is_replayable(&self) -> bool {
        !matches!(self, Self::SendMfaCode)
    }

    /// 資料任務的合併鍵；同鍵的排隊請求視為重複而合併。
    pub(super) fn data_key(&self) -> Option<DataKey> {
        match self {
            Self::LoadSchedule { .. } => Some(DataKey::Schedule),
            Self::LoadHomework { .. } => Some(DataKey::Homework),
            Self::LoadFlow { page } => Some(DataKey::Flow(*page)),
            Self::LoadCourses { .. } => Some(DataKey::Courses),
            Self::LoadActivities { course_id, .. } => Some(DataKey::Activities(course_id.clone())),
            Self::LoadActivityDetail { activity_id, .. } => {
                Some(DataKey::ActivityDetail(activity_id.clone()))
            }
            Self::OpenActivity { activity_id, .. } => {
                Some(DataKey::OpenActivity(activity_id.clone()))
            }
            _ => None,
        }
    }

    /// 是否為略過快取的強制刷新（課表、作業、課程、活動與活動詳情帶有 `force`）。
    pub(super) fn is_forced(&self) -> bool {
        match self {
            Self::LoadSchedule { force }
            | Self::LoadHomework { force }
            | Self::LoadCourses { force }
            | Self::LoadActivities { force, .. }
            | Self::LoadActivityDetail { force, .. } => *force,
            _ => false,
        }
    }
}

/// 資料任務的合併鍵。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum DataKey {
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
    /// 登入失敗（附站點，介面重試時沿用同一個站點）。
    LoginFailed {
        /// 登入失敗的站點。
        site: SiteKind,
        /// 失敗訊息。
        message: String,
    },
    /// 登入取消已完成（工作者已丟棄登入流程與待存憑證）。
    ///
    /// 介面在送出取消後、收到本事件前，不得再讓遲到的登入事件重開覆蓋層
    ///（它們都屬於正在被取消的那次登入）；收到後清除該等待狀態。
    LoginCancelled,
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
    /// 自訂義任務的完整快照（解鎖後與每次異動後回報）。
    Tasks(Vec<Task>),
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
    /// 課程分區所用的當前學期提示已變更（例如使用者明確選定了學期）。
    ///
    /// 課程清單本身不變，介面重新繪製即會依新的學期重新分區。
    CoursesTerm(Option<TermCode>),
    /// 課程列表（含當前學期提示）。
    Courses(CoursesData),
    /// 課程活動列表。
    ///
    /// 附上所屬課程識別碼：介面用它隔離不同課程的活動（並丟棄遲到的回應）。
    Activities {
        /// 這批活動所屬的課程識別碼。
        course_id: String,
        /// 活動列表。
        activities: Vec<LmsActivity>,
    },
    /// 活動詳情（`ActivityDetailView::id` 即其活動識別碼）。
    ActivityDetail(Box<ActivityDetailView>),
    /// 已解析的活動網址（依訪問模式改寫完成，等待介面以瀏覽器開啟）。
    OpenUrl(String),
    /// 帳號已更新。
    AccountUpdated,
    /// 帳號已驗證成功，但憑證寫入保險庫失敗。
    ///
    /// 介面必須解除表單的「處理中」狀態並就地顯示錯誤；否則表單會卡在
    /// 「正在处理」，且 busy 期間連 Esc 都被忽略。
    CredentialSaveFailed(String),
    /// 加密口令已更新。
    PassphraseUpdated,
    /// 訪問策略已更新。
    AccessPolicyUpdated(AccessPolicy),
    /// 使用者已同意本版用户协议（版本已寫入設定檔）。
    AgreementAccepted,
    /// 提示訊息（操作結果、進度、診斷）。
    ///
    /// 介面以**提示色**顯示。需要使用者注意但不影響繼續使用的訊息（功能降級、
    /// 資料被跳過、某個子功能不可用）請用 [`Event::Warning`]；使用者要求的操作
    /// 真的失敗時用 [`Event::Failed`]（錯誤色並標記受影響頁面）。
    Notice(String),
    /// 需要注意但不影響繼續使用（降級、跳過資料、子功能不可用）。
    ///
    /// 介面以**警告色**顯示：與 [`Event::Notice`] 的提示色、[`Event::Failed`]
    /// 的錯誤色一起構成「藍＝正常回報、黃＝有事但還能用、紅＝出事了」。
    Warning(String),
    /// 會話已停用（無法建立乾淨的新會話）：介面應回到解鎖畫面。
    SessionDisabled(String),
    /// 互動驗證（圖片驗證碼或簡訊驗證碼）未通過。
    ///
    /// 登入流程仍保留，介面應維持在原本的驗證碼／簡訊輸入畫面並就地顯示錯誤，
    /// 讓使用者重輸即可繼續同一次登入；不得當成一般登入失敗而作廢流程。
    VerificationRetry {
        /// 失敗的站點。
        site: SiteKind,
        /// 錯誤訊息。
        message: String,
    },
    /// 任務失敗。
    Failed {
        /// 任務名稱。
        what: String,
        /// 錯誤訊息。
        message: String,
        /// 失敗所屬的介面位置。
        target: FailedTarget,
        /// 失敗所屬的站點（登入類任務才有；供介面重試同一站點）。
        site: Option<SiteKind>,
        /// 失敗所屬的資源識別碼（活動為課程識別碼、詳情為活動識別碼）。
        ///
        /// 供介面隔離遲到的舊資源失敗：切到別的課程／活動後，前一個資源的
        /// 失敗不得把目前畫面標成失敗。非資源型任務為 `None`。
        resource: Option<String>,
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
    /// 解鎖後的背景預載（不動任何頁面，只留提示）。
    Preload,
    /// 登入流程（驗證碼、簡訊、重試）。
    Login,
    /// 憑證操作（建立保險庫、解鎖、修改帳號或口令）。
    Credentials,
    /// 自訂義任務（新增、修改、標記完成、刪除、換口令時重新加密）。
    Tasks,
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

/// 任務是否屬於「帳號切換」的一部分（切換本身或其互動驗證步驟）。
pub(super) fn is_account_switch_step(job: &Job) -> bool {
    matches!(
        job,
        Job::ChangeAccount { .. }
            | Job::RetryWithAccount { .. }
            | Job::RetryLogin { .. }
            | Job::SubmitCaptcha(_)
            | Job::SendMfaCode
            | Job::VerifyMfaCode(_)
    )
}

/// 這次失敗之後，登入流程是否已經不可能再繼續。
///
/// 除了送簡訊驗證碼：那只是「那一次發送」失敗，驅動器與流程都還在，使用者
/// 再按一次就能重送，因此不算流程結束。
pub(super) fn login_step_breaks_the_flow(job: &Job) -> bool {
    is_account_switch_step(job) && !matches!(job, Job::SendMfaCode)
}

/// 任務所屬站點。
pub(super) fn site_of(job: &Job) -> Option<SiteKind> {
    match job {
        Job::LoadSchedule { .. } | Job::LoadFlow { .. } => Some(SiteKind::Attendance),
        Job::LoadHomework { .. }
        | Job::LoadCourses { .. }
        | Job::LoadActivities { .. }
        | Job::LoadActivityDetail { .. }
        | Job::OpenActivity { .. } => Some(SiteKind::Lms),
        _ => None,
    }
}

/// 任務失敗時應由介面標記的位置。
pub(super) fn failed_target_of(job: &Job) -> FailedTarget {
    match job {
        Job::LoadSchedule { .. } | Job::SetScheduleWeek { .. } => FailedTarget::Schedule,
        Job::LoadHomework { .. } => FailedTarget::Homework,
        Job::LoadFlow { .. } => FailedTarget::Flow,
        Job::LoadCourses { .. } => FailedTarget::Courses,
        Job::LoadActivities { .. } => FailedTarget::Activities,
        Job::LoadActivityDetail { .. } => FailedTarget::ActivityDetail,
        Job::OpenActivity { .. } => FailedTarget::ActivityOpen,
        Job::Preload => FailedTarget::Preload,
        Job::AcceptAgreement => FailedTarget::Agreement,
        Job::SubmitCaptcha(_)
        | Job::RefreshCaptcha
        | Job::SendMfaCode
        | Job::VerifyMfaCode(_)
        | Job::RetryLogin { .. }
        | Job::RetryWithAccount { .. }
        | Job::CancelLogin => FailedTarget::Login,
        Job::CreateVault { .. }
        | Job::Unlock { .. }
        | Job::ChangeAccount { .. }
        | Job::ChangePassphrase { .. } => FailedTarget::Credentials,
        Job::AddTask { .. }
        | Job::UpdateTask { .. }
        | Job::SetTaskDone { .. }
        | Job::SetTasksDone { .. }
        | Job::DeleteTask { .. }
        | Job::DeleteTasks { .. }
        | Job::DeleteCompletedTasks => FailedTarget::Tasks,
        _ => FailedTarget::Settings,
    }
}

/// 任務失敗時所屬的資源識別碼（供介面隔離遲到的舊資源失敗）。
///
/// 活動為課程識別碼、詳情為活動識別碼；其他任務沒有可資隔離的資源。
pub(super) fn resource_of(job: &Job) -> Option<String> {
    match job {
        Job::LoadActivities { course_id, .. } => Some(course_id.clone()),
        Job::LoadActivityDetail { activity_id, .. } => Some(activity_id.clone()),
        _ => None,
    }
}
