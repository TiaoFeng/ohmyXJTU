//! TUI 應用狀態（畫面路由與各頁資料）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;

use crate::config::AccessPolicy;
use crate::domain::activity::{self, ActivityGroup};
use crate::domain::course_list::{self, CourseRow};
use crate::domain::homework::{HomeworkGroup, HomeworkItem};
use crate::domain::semester::TermCode;
use crate::domain::todo::{self, PageRow, Priority, SortKey, SortMode, Task};
use crate::model::{ActivityDetailView, FlowData, ScheduleData};
use crate::session::{AccessMode, SiteKind};
use crate::sites::lms::{LmsActivity, LmsCourse};
use crate::task::{FailedTarget, HomeworkIssue};
use crate::tui::text::{InputLine, TextArea};

/// 訊息保留時間。
const MESSAGE_TTL: Duration = Duration::from_secs(8);

/// 側邊項目的數量。
const NAV_COUNT: usize = 4;

/// 左側導航項目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    /// 課表。
    Schedule,
    /// 作業。
    Homework,
    /// 考勤流水。
    Attendance,
    /// 思源學堂。
    Lms,
}

impl NavItem {
    /// 全部項目（依畫面順序）。
    pub const ALL: [Self; NAV_COUNT] =
        [Self::Schedule, Self::Homework, Self::Attendance, Self::Lms];

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Schedule => "课表",
            Self::Homework => "任务",
            Self::Attendance => "考勤流水",
            Self::Lms => "思源学堂",
        }
    }

    /// 索引。
    pub fn index(self) -> usize {
        match self {
            Self::Schedule => 0,
            Self::Homework => 1,
            Self::Attendance => 2,
            Self::Lms => 3,
        }
    }

    /// 由索引取得項目。
    pub fn from_index(index: usize) -> Self {
        Self::ALL[index % NAV_COUNT]
    }

    /// 下一個項目。
    pub fn next(self) -> Self {
        Self::from_index(self.index() + 1)
    }

    /// 上一個項目。
    pub fn previous(self) -> Self {
        Self::from_index((self.index() + NAV_COUNT - 1) % NAV_COUNT)
    }

    /// 頁面資料所屬站點。
    pub fn site(self) -> SiteKind {
        match self {
            Self::Schedule | Self::Attendance => SiteKind::Attendance,
            Self::Homework | Self::Lms => SiteKind::Lms,
        }
    }
}

/// 頁面資料狀態。
#[derive(Debug, Default)]
pub enum Page<T> {
    /// 尚未載入。
    #[default]
    Idle,
    /// 載入中（可保留舊資料供繼續顯示）。
    Loading {
        /// 進度說明。
        note: String,
        /// 上一次成功載入的資料。
        stale: Option<T>,
    },
    /// 已載入。
    Ready(T),
    /// 載入失敗（可保留舊資料供繼續顯示）。
    Failed {
        /// 錯誤訊息。
        message: String,
        /// 上一次成功載入的資料。
        stale: Option<T>,
    },
}

impl<T> Page<T> {
    /// 是否尚未載入。
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// 是否載入中。
    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }

    /// 目前可顯示的資料（含刷新中或失敗後保留的舊資料）。
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            Self::Loading { stale, .. } | Self::Failed { stale, .. } => stale.as_ref(),
            Self::Idle => None,
        }
    }

    /// 目前可顯示的資料（可變）。
    pub fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Ready(value) => Some(value),
            Self::Loading { stale, .. } | Self::Failed { stale, .. } => stale.as_mut(),
            Self::Idle => None,
        }
    }

    /// 載入中或失敗的說明文字。
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Loading { note, .. } => Some(note),
            Self::Failed { message, .. } => Some(message),
            _ => None,
        }
    }

    /// 進入載入中狀態（保留既有資料供繼續顯示）。
    pub fn start_loading(&mut self, note: impl Into<String>) {
        let stale = match std::mem::take(self) {
            Self::Ready(value) => Some(value),
            Self::Loading { stale, .. } | Self::Failed { stale, .. } => stale,
            Self::Idle => None,
        };
        *self = Self::Loading {
            note: note.into(),
            stale,
        };
    }

    /// 進入載入中狀態，且**不**保留既有資料。
    ///
    /// 用於切換到不同資源（例如改看另一門課程的活動）時：舊資料屬於前一個
    /// 資源，保留它只會讓使用者在回應抵達前看到不屬於目前選取項的內容。
    pub fn reset_loading(&mut self, note: impl Into<String>) {
        *self = Self::Loading {
            note: note.into(),
            stale: None,
        };
    }

    /// 進入失敗狀態（保留既有資料供繼續顯示）。
    pub fn fail(&mut self, message: impl Into<String>) {
        let stale = match std::mem::take(self) {
            Self::Ready(value) => Some(value),
            Self::Loading { stale, .. } | Self::Failed { stale, .. } => stale,
            Self::Idle => None,
        };
        *self = Self::Failed {
            message: message.into(),
            stale,
        };
    }
}

/// 思源學堂的瀏覽層級。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LmsLevel {
    /// 課程列表。
    #[default]
    Courses,
    /// 活動列表。
    Activities,
    /// 活動詳情。
    Detail,
}

/// 思源學堂頁狀態。
#[derive(Debug, Default)]
pub struct LmsState {
    /// 課程列表。
    pub courses: Page<Vec<LmsCourse>>,
    /// 目前課程的活動列表。
    pub activities: Page<Vec<LmsActivity>>,
    /// 目前活動的詳情。
    pub detail: Page<ActivityDetailView>,
    /// `activities` 所屬的課程識別碼（`None` 表示尚未載入任何課程的活動）。
    ///
    /// 切換課程時用來判斷既有資料是否仍適用，並過濾遲到的回應。
    pub activities_course: Option<String>,
    /// `detail` 所屬的活動識別碼（`None` 表示尚未載入任何活動詳情）。
    ///
    /// 用途同 [`Self::activities_course`]。
    pub detail_activity: Option<String>,
    /// 活動詳情的捲動狀態。
    pub detail_scroll: ScrollState,
    /// 選取的課程索引。
    pub course_index: usize,
    /// 選取的活動索引（相對於目前分組過濾後的清單）。
    pub activity_index: usize,
    /// 活動列表目前顯示的分組。
    pub activity_group: ActivityGroup,
    /// 課程列表的當前學期（供分區顯示；`None` 表示無法判定）。
    pub courses_term: Option<TermCode>,
    /// 目前層級。
    pub level: LmsLevel,
}

/// 作業頁資料。
#[derive(Debug, Clone, Default)]
pub struct HomeworkData {
    /// 學期標籤。
    pub term_label: Option<String>,
    /// 學期判定來源標籤。
    pub term_source: Option<&'static str>,
    /// 納入查詢的課程數。
    pub courses_included: usize,
    /// 缺少學期資訊而未納入的課程數。
    pub courses_skipped: usize,
    /// 可選學期（供選擇器使用）。
    pub term_options: Vec<TermCode>,
    /// 全部作業（已依分組與截止時間排序）。
    pub items: Vec<HomeworkItem>,
    /// 「待核实」作業的共同原因彙總（依項數遞減）。
    pub issues: Vec<HomeworkIssue>,
    /// 活動列表查詢失敗而略過的課程數。
    pub courses_failed: usize,
    /// 載入進度（完成, 總數）；`None` 表示載入完成。
    pub progress: Option<(usize, usize)>,
}

impl HomeworkData {
    /// 指定分組的作業（保持排序）。
    pub fn group_items(&self, group: HomeworkGroup) -> Vec<&HomeworkItem> {
        self.items
            .iter()
            .filter(|item| item.state.group() == group)
            .collect()
    }

    /// 指定分組的項目數。
    pub fn group_count(&self, group: HomeworkGroup) -> usize {
        self.items
            .iter()
            .filter(|item| item.state.group() == group)
            .count()
    }
}

/// 任務頁各分組的項目數（作業＋任務；不受搜尋過濾影響）。
///
/// 標題、分組標籤列與提示列每幀都要用到這三個數字：由 [`App::task_page_group_counts`]
/// 一次掃描算好，不要逐組重複統計（主迴圈固定 200ms 重繪一次）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskPageCounts {
    /// 未完成分組。
    pub unfinished: usize,
    /// 已完成分組。
    pub completed: usize,
    /// 待核实分組。
    pub unknown: usize,
}

impl TaskPageCounts {
    /// 指定分組的計數。
    pub fn get(&self, group: HomeworkGroup) -> usize {
        match group {
            HomeworkGroup::Unfinished => self.unfinished,
            HomeworkGroup::Completed => self.completed,
            HomeworkGroup::Unknown => self.unknown,
        }
    }

    /// 累加一個項目所屬分組的計數。
    fn add(&mut self, group: HomeworkGroup) {
        match group {
            HomeworkGroup::Unfinished => self.unfinished += 1,
            HomeworkGroup::Completed => self.completed += 1,
            HomeworkGroup::Unknown => self.unknown += 1,
        }
    }
}

/// 任務頁項目的穩定識別：排序或內容變動後據此把選取錨定回同一個項目。
///
/// 位置（索引）會因為分組、搜尋與排序改變而失效；識別碼不會。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEntryId {
    /// 自訂義任務的識別碼。
    Task(u64),
    /// 思源學堂作業的活動識別碼。
    Homework(String),
}

/// 任務頁目前選取的項目。
#[derive(Debug, Clone, Copy)]
pub enum TaskEntry<'a> {
    /// 自訂義任務。
    Task(&'a Task),
    /// 思源學堂作業。
    Homework(&'a HomeworkItem),
}

impl TaskEntry<'_> {
    /// 穩定識別（跨排序、跨重新載入）。
    pub fn id(&self) -> TaskEntryId {
        match self {
            Self::Task(task) => TaskEntryId::Task(task.id),
            Self::Homework(item) => TaskEntryId::Homework(item.activity_id.clone()),
        }
    }
}

/// 學期選擇器狀態。
#[derive(Debug, Default)]
pub struct TermPickerState {
    /// 可選學期（由新到舊）。
    pub options: Vec<TermCode>,
    /// 目前選取索引。
    pub index: usize,
    /// 依日期推算的建議學期（僅作預選）。
    pub suggestion: Option<TermCode>,
    /// 需要手動選擇的原因。
    pub reason: String,
}

impl TermPickerState {
    /// 建立選擇器（套用建議預選）。
    pub fn new(options: Vec<TermCode>, suggestion: Option<TermCode>, reason: String) -> Self {
        let index = suggestion
            .and_then(|suggestion| options.iter().position(|option| *option == suggestion))
            .unwrap_or(0);
        Self {
            options,
            index,
            suggestion,
            reason,
        }
    }

    /// 選取下一個。
    pub fn next(&mut self) {
        if !self.options.is_empty() {
            self.index = (self.index + 1) % self.options.len();
        }
    }

    /// 選取上一個。
    pub fn previous(&mut self) {
        if !self.options.is_empty() {
            self.index = (self.index + self.options.len() - 1) % self.options.len();
        }
    }

    /// 目前選取的學期。
    pub fn selected(&self) -> Option<TermCode> {
        self.options.get(self.index).copied()
    }
}

/// 各頁最近一次成功載入的時間（顯示用）。
#[derive(Debug, Clone, Default)]
pub struct UpdatedAt {
    /// 課表。
    pub schedule: Option<String>,
    /// 作業。
    pub homework: Option<String>,
    /// 考勤流水。
    pub attendance: Option<String>,
    /// 思源學堂。
    pub lms: Option<String>,
}

/// 任務表單的模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFormMode {
    /// 新增任務（識別碼由存儲指派）。
    Add,
    /// 編輯既有任務。
    Edit {
        /// 任務識別碼。
        id: u64,
    },
}

/// 任務表單的聚焦欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskField {
    /// 內容（必填）。
    Content,
    /// 標籤（可選；寬度上限見 [`crate::domain::todo::TAG_MAX_WIDTH`]）。
    Tag,
    /// 描述（多行）。
    Description,
    /// 截止時間。
    Deadline,
    /// 優先級。
    Priority,
    /// 完成狀態。
    Completed,
}

impl TaskField {
    /// 全部欄位（畫面順序）。
    pub const ALL: [Self; 6] = [
        Self::Content,
        Self::Tag,
        Self::Description,
        Self::Deadline,
        Self::Priority,
        Self::Completed,
    ];

    /// 下一個欄位（循環）。
    pub fn next(self) -> Self {
        match self {
            Self::Content => Self::Tag,
            Self::Tag => Self::Description,
            Self::Description => Self::Deadline,
            Self::Deadline => Self::Priority,
            Self::Priority => Self::Completed,
            Self::Completed => Self::Content,
        }
    }

    /// 上一個欄位（循環）。
    pub fn previous(self) -> Self {
        match self {
            Self::Content => Self::Completed,
            Self::Tag => Self::Content,
            Self::Description => Self::Tag,
            Self::Deadline => Self::Description,
            Self::Priority => Self::Deadline,
            Self::Completed => Self::Priority,
        }
    }
}

/// 新增／編輯任務的表單狀態。
#[derive(Debug, Clone)]
pub struct TaskFormState {
    /// 表單模式。
    pub mode: TaskFormMode,
    /// 內容。
    pub content: InputLine,
    /// 標籤（單行，可留空）。
    pub tag: InputLine,
    /// 描述（多行）。
    pub description: TextArea,
    /// 截止時間（文字輸入，保存時解析）。
    pub deadline: InputLine,
    /// 優先級。
    pub priority: Priority,
    /// 是否已完成。
    pub completed: bool,
    /// 目前聚焦的欄位。
    pub focus: TaskField,
    /// 是否正在送出（等待工作者回報）。
    pub busy: bool,
    /// 驗證或保存失敗的原因（就地顯示）。
    pub error: Option<String>,
}

impl TaskFormState {
    /// 新增表單。
    pub fn add() -> Self {
        Self {
            mode: TaskFormMode::Add,
            content: InputLine::new(),
            tag: InputLine::new(),
            description: TextArea::new(""),
            deadline: InputLine::new(),
            priority: Priority::default(),
            completed: false,
            focus: TaskField::Content,
            busy: false,
            error: None,
        }
    }

    /// 以既有任務預填編輯表單。
    pub fn edit(task: &Task) -> Self {
        Self {
            mode: TaskFormMode::Edit { id: task.id },
            content: InputLine::with_value(task.content.clone()),
            tag: InputLine::with_value(task.display_tag().unwrap_or_default()),
            description: TextArea::new(task.description.as_deref().unwrap_or_default()),
            deadline: InputLine::with_value(
                task.deadline
                    .map(|deadline| deadline.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default(),
            ),
            priority: task.priority,
            completed: task.completed,
            focus: TaskField::Content,
            busy: false,
            error: None,
        }
    }

    /// 切換到下一個欄位。
    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    /// 切換到上一個欄位。
    pub fn focus_previous(&mut self) {
        self.focus = self.focus.previous();
    }
}

/// 任務設置選單（`^T`）的項目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskMenuKind {
    /// 進入多選模式（批量操作）。
    Multi,
    /// 刪除所有已完成任務。
    DeleteCompleted,
}

impl TaskMenuKind {
    /// 全部項目（畫面順序）。
    pub const ALL: [Self; 2] = [Self::Multi, Self::DeleteCompleted];

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Multi => "多选（批量操作）",
            Self::DeleteCompleted => "删除所有已完成的任务",
        }
    }
}

/// 任務設置彈窗狀態。
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskMenuState {
    /// 目前選取的項目。
    pub index: usize,
}

/// 多選後的批量操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskBatchOp {
    /// 標記完成。
    Done,
    /// 標記未完成。
    Undone,
    /// 刪除。
    Delete,
}

impl TaskBatchOp {
    /// 全部操作（畫面順序）。
    pub const ALL: [Self; 3] = [Self::Done, Self::Undone, Self::Delete];

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Done => "标记完成",
            Self::Undone => "标记未完成",
            Self::Delete => "删除",
        }
    }
}

/// 多選批量操作選單狀態。
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskBatchMenuState {
    /// 目前選取的項目。
    pub index: usize,
}

/// 刪除已完成任務的二次確認狀態。
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskConfirmState {
    /// 將被刪除的任務數。
    pub count: usize,
}

/// 表單種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    /// 首次設定（加密口令＋帳號）。
    Setup,
    /// 解鎖保險庫。
    Unlock,
    /// 登入失敗後重新輸入帳號密碼（附原失敗站點）。
    LoginRetry(SiteKind),
    /// 修改帳號。
    ChangeAccount,
    /// 修改加密口令。
    ChangePassphrase,
}

impl FormKind {
    /// 表單欄位表（角色＋標籤）；順序即畫面順序，是欄位的單一來源。
    fn fields(self) -> &'static [(FieldRole, &'static str)] {
        match self {
            Self::Setup => &[
                (FieldRole::Passphrase, "加密口令"),
                (FieldRole::PassphraseConfirm, "确认口令"),
                (FieldRole::Username, "账号"),
                (FieldRole::Password, "密码"),
                (FieldRole::PasswordConfirm, "确认密码"),
            ],
            Self::Unlock => &[(FieldRole::Passphrase, "加密口令")],
            Self::LoginRetry(_) => &[
                (FieldRole::Username, "账号"),
                (FieldRole::Password, "密码"),
                (FieldRole::Passphrase, "加密口令"),
            ],
            Self::ChangeAccount => &[
                (FieldRole::OldPassphrase, "原加密口令"),
                (FieldRole::NewUsername, "新账号"),
                (FieldRole::NewPassword, "新密码"),
                (FieldRole::NewPasswordConfirm, "确认新密码"),
            ],
            Self::ChangePassphrase => &[
                (FieldRole::OldPassphrase, "原加密口令"),
                (FieldRole::NewPassphrase, "新加密口令"),
                (FieldRole::NewPassphraseConfirm, "确认新口令"),
            ],
        }
    }
}

/// 表單欄位的語意角色。
///
/// 角色決定是否遮蔽（[`FieldRole::is_secret`]）、失敗時是否清空，以及
/// [`crate::tui::controller::FormValues`] 取值的對應位置——調整欄位順序或
/// 標籤都不會讓值落進錯誤的欄位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRole {
    /// 保險庫加密口令（建立、解鎖、驗證）。
    Passphrase,
    /// 加密口令的確認輸入。
    PassphraseConfirm,
    /// 帳號。
    Username,
    /// 帳號密碼。
    Password,
    /// 密碼的確認輸入。
    PasswordConfirm,
    /// 修改前的原加密口令。
    OldPassphrase,
    /// 新的加密口令。
    NewPassphrase,
    /// 新加密口令的確認輸入。
    NewPassphraseConfirm,
    /// 新的帳號。
    NewUsername,
    /// 新的帳號密碼。
    NewPassword,
    /// 新密碼的確認輸入。
    NewPasswordConfirm,
}

impl FieldRole {
    /// 是否為敏感欄位（遮蔽輸入、失敗時清空）。
    pub fn is_secret(self) -> bool {
        !matches!(self, Self::Username | Self::NewUsername)
    }
}

/// 表單欄位。
#[derive(Debug, Clone)]
pub struct FormField {
    /// 欄位語意角色。
    pub role: FieldRole,
    /// 欄位標籤。
    pub label: &'static str,
    /// 欄位內容。
    pub value: InputLine,
}

/// 表單狀態。
#[derive(Debug, Clone)]
pub struct FormState {
    /// 表單種類。
    pub kind: FormKind,
    /// 欄位。
    pub fields: Vec<FormField>,
    /// 目前聚焦的欄位。
    pub focus: usize,
    /// 錯誤訊息。
    pub error: Option<String>,
    /// 是否正在送出。
    pub busy: bool,
}

impl FormState {
    /// 首次設定表單。
    pub fn setup() -> Self {
        Self::new(FormKind::Setup)
    }

    /// 解鎖表單。
    pub fn unlock() -> Self {
        Self::new(FormKind::Unlock)
    }

    /// 登入失敗後重新輸入帳號密碼（密碼與口令皆遮蔽，欄位一律留空）。
    ///
    /// `site` 為原本失敗的站點：重試沿用同一個站點，不被另一個站點的可達性牽制。
    pub fn login_retry(site: SiteKind) -> Self {
        Self::new(FormKind::LoginRetry(site))
    }

    /// 修改帳號表單。
    pub fn change_account() -> Self {
        Self::new(FormKind::ChangeAccount)
    }

    /// 修改加密口令表單。
    pub fn change_passphrase() -> Self {
        Self::new(FormKind::ChangePassphrase)
    }

    /// 清空敏感欄位（口令與密碼）；保留非敏感輸入（例如帳號）。
    pub fn clear_secrets(&mut self) {
        for field in &mut self.fields {
            if field.role.is_secret() {
                field.value.clear();
            }
        }
    }

    fn new(kind: FormKind) -> Self {
        let fields = kind
            .fields()
            .iter()
            .map(|&(role, label)| FormField {
                role,
                label,
                value: InputLine::new().masked(role.is_secret()),
            })
            .collect();
        Self {
            kind,
            fields,
            focus: 0,
            error: None,
            busy: false,
        }
    }

    /// 目前聚焦的欄位。
    pub fn focused(&self) -> Option<&FormField> {
        self.fields.get(self.focus)
    }

    /// 目前聚焦的欄位（可變）。
    pub fn focused_mut(&mut self) -> Option<&mut FormField> {
        self.fields.get_mut(self.focus)
    }

    /// 依標籤取得欄位內容。
    pub fn value(&self, label: &str) -> &str {
        self.fields
            .iter()
            .find(|field| field.label == label)
            .map_or("", |field| field.value.value())
    }

    /// 聚焦下一個欄位。
    pub fn focus_next(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + 1) % self.fields.len();
        }
    }

    /// 聚焦上一個欄位。
    pub fn focus_previous(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
        }
    }
}

/// 登入互動狀態。
#[derive(Debug)]
pub enum LoginScreen {
    /// 正在登入。
    Progress {
        /// 進度說明。
        note: String,
    },
    /// 需要圖片驗證碼。
    Captcha {
        /// 驗證碼圖片的暫存路徑。
        path: PathBuf,
        /// 輸入框。
        input: InputLine,
        /// 上一次的錯誤。
        error: Option<String>,
    },
    /// 需要簡訊驗證碼。
    Mfa {
        /// 綁定手機號（中間四位遮蔽）。
        phone: Option<String>,
        /// 是否已發送驗證碼。
        sent: bool,
        /// 輸入框。
        input: InputLine,
        /// 上一次的錯誤。
        error: Option<String>,
    },
    /// 登入失敗。
    Failed {
        /// 失敗的站點（重試時沿用）。
        site: SiteKind,
        /// 錯誤訊息。
        message: String,
    },
    /// 重新輸入帳號密碼（登入失敗後的可恢復入口）。
    Credentials {
        /// 失敗的站點（重試時沿用）。
        site: SiteKind,
        /// 表單（帳號、密碼、加密口令）。
        form: FormState,
        /// 上一次的失敗訊息。
        message: String,
    },
}

impl Default for LoginScreen {
    fn default() -> Self {
        Self::Progress {
            note: "正在登录…".to_owned(),
        }
    }
}

/// 帳戶設定彈窗狀態。
#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsState {
    /// 目前選取的項目。
    pub index: usize,
    /// 訪問模式的草稿值（開啟設定時以已生效值初始化）。
    pub draft: Option<AccessPolicy>,
    /// 訪問模式是否正在保存。
    pub saving: bool,
}

impl SettingsState {
    /// 設定項目數量。
    pub const COUNT: usize = 3;
    /// 修改帳號項目的索引。
    pub const ACCOUNT_INDEX: usize = 0;
    /// 修改加密口令項目的索引。
    pub const PASSPHRASE_INDEX: usize = 1;
    /// 訪問模式項目的索引。
    pub const POLICY_INDEX: usize = 2;

    /// 開啟設定彈窗。
    pub fn open(policy: AccessPolicy) -> Self {
        Self {
            index: Self::ACCOUNT_INDEX,
            draft: Some(policy),
            saving: false,
        }
    }

    /// 項目標籤。
    pub fn label(index: usize) -> &'static str {
        match index % Self::COUNT {
            Self::ACCOUNT_INDEX => "修改账号",
            Self::PASSPHRASE_INDEX => "修改加密口令",
            _ => "访问模式",
        }
    }

    /// 選取下一個項目。
    pub fn next(&mut self) {
        self.index = (self.index + 1) % Self::COUNT;
    }

    /// 選取上一個項目。
    pub fn previous(&mut self) {
        self.index = (self.index + Self::COUNT - 1) % Self::COUNT;
    }

    /// 目前顯示的訪問模式（草稿優先）。
    pub fn policy(&self, current: AccessPolicy) -> AccessPolicy {
        self.draft.unwrap_or(current)
    }

    /// 循環調整訪問模式草稿（`delta` 為 +1／-1）。
    pub fn cycle_policy(&mut self, current: AccessPolicy, delta: i32) {
        let all = AccessPolicy::ALL;
        let position = all
            .iter()
            .position(|policy| *policy == self.policy(current))
            .unwrap_or(0);
        let len = i32::try_from(all.len()).unwrap_or(1);
        let next = (i32::try_from(position).unwrap_or(0) + delta).rem_euclid(len);
        self.draft = Some(all[usize::try_from(next).unwrap_or(0)]);
    }

    /// 草稿與已生效值是否不同。
    pub fn policy_dirty(&self, current: AccessPolicy) -> bool {
        self.policy(current) != current
    }
}

/// 用户协议閱讀門狀態。
///
/// 捲動位置以「換行後的文件列」為單位；視窗高度與總列數由繪製流程回寫
/// （與課程清單的 offset 寫回同一模式），讓按鍵處理能在不重算版面的情況下
/// 夾取位置並判定是否已讀到底部。
#[derive(Debug, Default)]
pub struct AgreementState {
    /// 目前捲動列。
    scroll: u16,
    /// 上次繪製的可見高度（列）。
    viewport: u16,
    /// 上次繪製的總列數。
    total: u16,
    /// 是否曾捲到最底部（黏性：捲回上方後仍可確認）。
    reached_bottom: bool,
    /// 同意是否正在保存。
    pub saving: bool,
    /// 上次保存失敗的訊息。
    pub error: Option<String>,
}

impl AgreementState {
    /// 建立閱讀門狀態（從文件開頭開始）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 目前捲動列（供繪製）。
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// 已閱讀比例（0..=100；版面未知時為 0）。
    pub fn progress(&self) -> u16 {
        if self.total == 0 {
            return 0;
        }
        let read = self.scroll.saturating_add(self.viewport).min(self.total);
        u16::try_from(u32::from(read) * 100 / u32::from(self.total)).unwrap_or(100)
    }

    /// 是否可確認同意（已讀到底部且不在保存中）。
    pub fn can_confirm(&self) -> bool {
        self.reached_bottom && !self.saving
    }

    /// 繪製後回寫版面資訊：夾取捲動位置並更新「已讀到底部」。
    pub fn sync_layout(&mut self, viewport: u16, total: u16) {
        self.viewport = viewport;
        self.total = total;
        self.scroll = self.scroll.min(self.max_scroll());
        self.note_bottom();
    }

    /// 捲動 `delta` 列（正為向下；超出範圍即夾取）。
    pub fn scroll_by(&mut self, delta: i32) {
        let next = i64::from(self.scroll) + i64::from(delta);
        self.scroll = u16::try_from(next.clamp(0, i64::from(self.max_scroll()))).unwrap_or(0);
        self.note_bottom();
    }

    /// 翻頁（`pages` 為 +1／-1）。
    pub fn page_by(&mut self, pages: i32) {
        let step = i32::from(self.viewport).max(1);
        self.scroll_by(pages.saturating_mul(step));
    }

    /// 回到文件開頭。
    pub fn to_top(&mut self) {
        self.scroll = 0;
    }

    /// 跳到文件結尾。
    pub fn to_bottom(&mut self) {
        self.scroll = self.max_scroll();
        self.note_bottom();
    }

    /// 進入保存中狀態（清除舊錯誤）。
    pub fn start_saving(&mut self) {
        self.saving = true;
        self.error = None;
    }

    /// 保存失敗：解除保存中並顯示錯誤。
    pub fn fail(&mut self, message: String) {
        self.saving = false;
        self.error = Some(message);
    }

    /// 最大捲動列（總列數小於視窗時為 0）。
    fn max_scroll(&self) -> u16 {
        self.total.saturating_sub(self.viewport)
    }

    /// 目前位置是否已達底部；達到後記錄為黏性狀態。
    fn note_bottom(&mut self) {
        if self.total > 0 && self.scroll >= self.max_scroll() {
            self.reached_bottom = true;
        }
    }
}

/// 畫面。
///
/// 根畫面只代表「底層內容」；登入互動是疊加在主畫面之上的覆蓋層
/// （見 [`App::login`]），不會替換根畫面。
#[derive(Debug)]
pub enum Screen {
    /// 首次設定。
    Setup(FormState),
    /// 解鎖。
    Unlock(FormState),
    /// 主畫面。
    Main,
    /// 帳戶設定彈窗。
    Settings(SettingsState),
    /// 設定中的表單。
    SettingsForm(FormState),
    /// 學期選擇器。
    TermPicker(TermPickerState),
    /// 任務設置彈窗（`^T`）。
    TaskMenu(TaskMenuState),
    /// 多選後的批量操作選單。
    TaskBatchMenu(TaskBatchMenuState),
    /// 刪除已完成任務的二次確認。
    TaskConfirm(TaskConfirmState),
    /// 新增／編輯任務的表單彈窗。
    TaskForm(Box<TaskFormState>),
    /// 任務頁排序提示（`^L`）：只顯示提示列，畫面其餘部分照常。
    Sort,
}

/// 可捲動內容的位移狀態。
///
/// 視窗高度與總列數由繪製端回寫（見 [`Self::sync`]）：換行後的實際列數只有
/// 繪製時才知道，按鍵端必須以同一組數字夾取位移，否則會捲過頭或捲不到底。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollState {
    offset: u16,
    viewport: u16,
    total: u16,
}

impl ScrollState {
    /// 目前位移（列）。
    pub fn offset(&self) -> u16 {
        self.offset
    }

    /// 內容是否超過視窗（決定是否顯示捲動提示）。
    pub fn scrollable(&self) -> bool {
        self.total > self.viewport
    }

    /// 回到頂端。
    pub fn reset(&mut self) {
        self.offset = 0;
    }

    /// 本幀沒有繪製可捲動面板（載入中、空分組、終端過窄…）：清掉視窗資訊，
    /// 讓底欄的捲動提示不再沿用上一幀的值；位移保留，下次繪製時由 [`Self::sync`]
    /// 依新的內容重新夾取。
    pub fn clear(&mut self) {
        self.viewport = 0;
        self.total = 0;
    }

    /// 由繪製端回寫視窗高度與總列數，並夾取目前位移。
    pub fn sync(&mut self, viewport: u16, total: usize) {
        self.viewport = viewport;
        self.total = u16::try_from(total).unwrap_or(u16::MAX);
        self.offset = self.offset.min(self.max_offset());
    }

    /// 往上（`-1`）／下（`+1`）捲動一頁。
    pub fn page(&mut self, delta: i32) {
        let step = i64::from(self.viewport.max(1)) * i64::from(delta);
        let offset = (i64::from(self.offset) + step).max(0);
        self.offset = u16::try_from(offset)
            .unwrap_or(u16::MAX)
            .min(self.max_offset());
    }

    /// 捲到頂端。
    pub fn to_top(&mut self) {
        self.offset = 0;
    }

    /// 捲到底端。
    pub fn to_bottom(&mut self) {
        self.offset = self.max_offset();
    }

    /// 最大位移（內容不超過視窗時為 0）。
    fn max_offset(&self) -> u16 {
        self.total.saturating_sub(self.viewport)
    }
}

/// 應用程式狀態。
#[derive(Debug)]
pub struct App {
    /// 目前畫面。
    pub screen: Screen,
    /// 登入互動覆蓋層（進度、驗證碼、簡訊、失敗與重新輸入憑證）。
    pub login: Option<Box<LoginScreen>>,
    /// 使用者已關閉登入覆蓋層並要求取消（等待工作者回報取消完成）。
    ///
    /// 期間內遲到的登入事件（進度、驗證碼、簡訊）不得再打開覆蓋層：它們都
    /// 屬於正在被取消的那次登入，工作者隨後就會丟棄該流程。
    pub login_cancel_pending: bool,
    /// 用户协议閱讀門（首次啟動或協議改版後；開啟時獨占畫面與按鍵）。
    pub agreement: Option<Box<AgreementState>>,
    /// 目前頁面。
    pub nav: NavItem,
    /// 課表頁。
    pub schedule: Page<ScheduleData>,
    /// 目前顯示（或正在載入）的課表週次；`None` 表示尚未載入。
    pub schedule_week: Option<u32>,
    /// 學期總週數（切換週次的邊界）。
    pub schedule_total: Option<u32>,
    /// 使用者以 `[`／`]` 指定、但尚未收到該週資料的目標週次。
    ///
    /// 課表載入是單步任務：切週指令要等已在執行中的舊週載入回報後才生效，
    /// 那筆舊週結果仍會送達介面。套用課表事件前據此過濾週次不符的回應，避免
    /// 畫面閃回舊週，或（新週載入失敗時）停在舊週的課程資料與標題。
    ///
    /// 與 `schedule_week` 分開：`schedule_week` 是「要顯示的週次」，而工作者在
    /// 使用者從未切週且學期已結束／尚未開始時會回應正規化後的週次（與上次顯示
    /// 的週次不同），拿它比對會誤丟合法結果。收到相符的資料後即清空。
    pub schedule_pending_week: Option<u32>,
    /// 作業頁。
    pub homework: Page<HomeworkData>,
    /// 作業頁目前分組。
    pub homework_group: HomeworkGroup,
    /// 自訂義任務（本機資料；與帳號無關，換帳號時不清空）。
    pub tasks: Vec<Task>,
    /// 任務頁的搜尋關鍵字（`None` 表示未篩選；作業與任務都會被過濾）。
    pub task_filter: Option<String>,
    /// 任務頁的搜尋輸入框（開啟時獨占任務頁的按鍵）。
    pub task_search: Option<InputLine>,
    /// 任務頁的多選模式：已勾選的任務識別碼（`None` 表示不在多選模式）。
    pub task_multi: Option<HashSet<u64>>,
    /// 任務頁的排序方式（`^L`；只影響顯示，不改動資料）。
    pub task_sort: SortMode,
    /// `^D` 第一次按下後等待第二次確認的任務（識別碼、內容）。
    pub task_pending_delete: Option<(u64, String)>,
    /// 搜尋框內以 `↑`／`↓` 選取標籤建議的游標（`None` 表示尚未開始選取）。
    pub task_tag_cursor: Option<usize>,
    /// 最近一次得知的可選學期（供學期選擇器）。
    pub term_options: Vec<TermCode>,
    /// 考勤流水頁。
    pub attendance: Page<FlowData>,
    /// 思源學堂頁。
    pub lms: LmsState,
    /// 訪問策略設定。
    pub access_policy: AccessPolicy,
    /// 各站點目前的登入狀態（站點 → 實際訪問方式）。
    pub site_modes: HashMap<SiteKind, AccessMode>,
    /// 驗證碼圖片路徑（顯示於狀態列）。
    pub captcha_path: Option<PathBuf>,
    /// 等待主迴圈以系統瀏覽器開啟的網址。
    pub pending_open: Option<String>,
    /// 暫時訊息（自動過期）。
    pub message: Option<(String, Instant)>,
    /// 是否結束程式。
    pub quit: bool,
    /// 動畫影格計數（每幀遞增；供載入指示燈動畫使用）。
    pub tick: u64,
    /// 側邊欄選取狀態。
    pub nav_state: ListState,
    /// 課表選取狀態。
    pub schedule_state: ListState,
    /// 作業選取狀態。
    pub homework_state: ListState,
    /// 考勤流水選取狀態。
    pub flow_state: ListState,
    /// 課程選取狀態。
    pub course_state: ListState,
    /// 活動選取狀態。
    pub activity_state: ListState,
    /// 課表是否展開詳情。
    pub schedule_detail: bool,
    /// 作業是否展開詳情。
    pub homework_detail: bool,
    /// 作業詳情的捲動狀態。
    pub homework_scroll: ScrollState,
    /// 流水是否展開詳情。
    pub flow_detail: bool,
    /// 各頁最近一次成功載入的時間（顯示用）。
    pub updated_at: UpdatedAt,
}

impl App {
    /// 建立應用狀態。
    pub fn new(access_policy: AccessPolicy) -> Self {
        Self {
            screen: Screen::Unlock(FormState::unlock()),
            login: None,
            login_cancel_pending: false,
            agreement: None,
            nav: NavItem::Schedule,
            schedule: Page::Idle,
            schedule_week: None,
            schedule_total: None,
            schedule_pending_week: None,
            homework: Page::Idle,
            homework_group: HomeworkGroup::Unfinished,
            tasks: Vec::new(),
            task_filter: None,
            task_search: None,
            task_multi: None,
            task_sort: SortMode::default(),
            task_pending_delete: None,
            task_tag_cursor: None,
            term_options: Vec::new(),
            attendance: Page::Idle,
            lms: LmsState::default(),
            access_policy,
            site_modes: HashMap::new(),
            captcha_path: None,
            pending_open: None,
            message: None,
            quit: false,
            tick: 0,
            nav_state: ListState::default().with_selected(Some(0)),
            schedule_state: ListState::default().with_selected(Some(0)),
            homework_state: ListState::default().with_selected(Some(0)),
            flow_state: ListState::default().with_selected(Some(0)),
            course_state: ListState::default().with_selected(Some(0)),
            activity_state: ListState::default().with_selected(Some(0)),
            schedule_detail: false,
            homework_detail: false,
            homework_scroll: ScrollState::default(),
            flow_detail: false,
            updated_at: UpdatedAt::default(),
        }
    }

    /// 目前頁面的項目數量。
    pub fn page_len(&self) -> usize {
        match self.nav {
            NavItem::Schedule => self.schedule.ready().map_or(0, |data| data.lessons.len()),
            NavItem::Homework => todo::selectable_len(
                self.task_group_items(self.homework_group).len(),
                self.homework_group_items(self.homework_group).len(),
            ),
            NavItem::Attendance => self.attendance.ready().map_or(0, |data| data.records.len()),
            NavItem::Lms => match self.lms.level {
                LmsLevel::Courses => self.lms.courses.ready().map_or(0, Vec::len),
                LmsLevel::Activities => self.lms_activities_in_group().len(),
                LmsLevel::Detail => 0,
            },
        }
    }

    /// 目前頁面的選取索引。
    pub fn page_selection(&self) -> usize {
        self.page_state().selected().unwrap_or(0)
    }

    /// 目前頁面的清單狀態。
    pub fn page_state(&self) -> ListState {
        match self.nav {
            NavItem::Schedule => self.schedule_state,
            NavItem::Homework => self.homework_state,
            NavItem::Attendance => self.flow_state,
            NavItem::Lms => match self.lms.level {
                LmsLevel::Activities => self.activity_state,
                _ => self.course_state,
            },
        }
    }

    /// 下一個項目（非空清單首尾循環）。
    pub fn select_next(&mut self) {
        self.move_selection(1);
    }

    /// 上一個項目（非空清單首尾循環）。
    pub fn select_previous(&mut self) {
        self.move_selection(-1);
    }

    /// 移動選取（`delta` 為 +1／-1；非空清單首尾循環）。
    ///
    /// 思源學堂課程層的畫面順序（本學期置頂、其後歷史課程）與原始索引不同，
    /// 必須依「可見順序」移動，否則上下鍵會跳過畫面上的下一門課。
    fn move_selection(&mut self, delta: i32) {
        if self.nav == NavItem::Lms
            && self.lms.level == LmsLevel::Courses
            && let Some(index) = self.course_neighbor(delta)
        {
            self.course_state.select(Some(index));
            return;
        }
        let len = self.page_len();
        if len == 0 {
            return;
        }
        // 先將可能過期的索引正規化，再取下一個（尾端回到開頭）。
        let current = self.page_selection().min(len - 1);
        let next = if delta >= 0 {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.set_selection(next);
        // 換一筆作業時詳情內容整組替換，捲動位置回到頂端。
        if self.nav == NavItem::Homework {
            self.homework_scroll.reset();
        }
    }

    /// 依畫面可見順序找下一門課（跳過標題／空白列），回傳其真實課程索引。
    ///
    /// 無法取得課程清單時回 `None`，由呼叫端退回一般（原始索引）的移動。
    fn course_neighbor(&self, delta: i32) -> Option<usize> {
        let courses = self.lms.courses.ready()?;
        let rows = course_list::course_rows(courses, self.lms.courses_term);
        if rows.is_empty() {
            return None;
        }
        let mut position = self
            .course_state
            .selected()
            .and_then(|index| course_list::visual_index(&rows, index))
            .unwrap_or(0);
        // 逐列前進直到命中課程列（標題與空白列不可選取）；最多走一輪。
        for _ in 0..rows.len() {
            position = if delta >= 0 {
                (position + 1) % rows.len()
            } else {
                (position + rows.len() - 1) % rows.len()
            };
            if let CourseRow::Course { course_index, .. } = &rows[position] {
                return Some(*course_index);
            }
        }
        None
    }

    /// 記錄站點登入成功時的訪問方式。
    pub fn set_site_mode(&mut self, site: SiteKind, mode: AccessMode) {
        self.site_modes.insert(site, mode);
    }

    /// 清除單一站點的登入狀態。
    pub fn clear_site_mode(&mut self, site: SiteKind) {
        self.site_modes.remove(&site);
    }

    /// 清除所有站點的登入狀態（解鎖、換帳號、切換訪問模式時）。
    pub fn clear_site_modes(&mut self) {
        self.site_modes.clear();
    }

    /// 目前可編輯的表單（登入覆蓋層的憑證表單優先於底層表單）。
    pub fn form_mut(&mut self) -> Option<&mut FormState> {
        if let Some(screen) = self.login.as_mut()
            && let LoginScreen::Credentials { form, .. } = screen.as_mut()
        {
            return Some(form);
        }
        match &mut self.screen {
            Screen::Setup(form) | Screen::Unlock(form) | Screen::SettingsForm(form) => Some(form),
            _ => None,
        }
    }

    /// 會話重置後的頁面失效處理。
    ///
    /// - 更換帳號（`account_changed=true`）：所有頁面資料都屬於舊帳號，
    ///   一律清空並回到未載入（思源學堂回到課程層），切換選取與時間標記。
    /// - 切換訪問模式（`account_changed=false`）：資料仍有效，只把卡在
    ///   「載入中」的頁面收斂（對應的進行中與排隊任務已作廢）。
    pub fn invalidate_data(&mut self, account_changed: bool) {
        if account_changed {
            self.schedule = Page::Idle;
            self.schedule_week = None;
            self.schedule_total = None;
            self.schedule_pending_week = None;
            self.homework = Page::Idle;
            self.attendance = Page::Idle;
            // 自訂義任務屬於本機資料，與帳號無關：內容保留，只清掉任務頁的
            // 暫時狀態（搜尋、多選、待確認刪除）。
            self.task_filter = None;
            self.task_search = None;
            self.task_multi = None;
            self.task_pending_delete = None;
            self.task_tag_cursor = None;
            // 舊帳號的課程、活動與詳情一律清空。
            self.lms = LmsState::default();
            self.updated_at = UpdatedAt::default();
            self.schedule_state.select(Some(0));
            self.homework_state.select(Some(0));
            self.flow_state.select(Some(0));
            self.course_state.select(Some(0));
            self.activity_state.select(Some(0));
            self.homework_scroll.reset();
            return;
        }
        Self::settle_loading(&mut self.schedule);
        Self::settle_loading(&mut self.homework);
        Self::settle_loading(&mut self.attendance);
        Self::settle_loading(&mut self.lms.courses);
        Self::settle_loading(&mut self.lms.activities);
        Self::settle_loading(&mut self.lms.detail);
        self.homework_scroll.reset();
        self.lms.detail_scroll.reset();
    }

    /// 目前頁面的登入狀態文字（底欄顯示用）。
    ///
    /// 已登入時只顯示實際訪問方式（「直连」「WebVPN」）——登入正常不需佔用
    /// 版面；只有「未登录」才需要提示。
    pub fn session_label(&self) -> String {
        match self.site_modes.get(&self.nav.site()) {
            Some(mode) => mode.label().to_owned(),
            None => "未登录".to_owned(),
        }
    }

    /// 目前分組的活動（過濾＋穩定排序；供繪製、選取與開啟使用）。
    pub fn lms_activities_in_group(&self) -> Vec<&LmsActivity> {
        let Some(activities) = self.lms.activities.ready() else {
            return Vec::new();
        };
        activity::grouped(activities, self.lms.activity_group)
    }

    /// 各活動分組的項目數（依顯示順序）。
    pub fn activity_group_counts(&self) -> [(ActivityGroup, usize); ActivityGroup::ALL.len()] {
        let Some(activities) = self.lms.activities.ready() else {
            return ActivityGroup::ALL.map(|group| (group, 0));
        };
        activity::counts(activities)
    }

    /// 任務頁目前選取的項目（與列模型順序一致）。
    pub fn selected_entry(&self) -> Option<TaskEntry<'_>> {
        if self.nav != NavItem::Homework {
            return None;
        }
        self.task_page_entries().get(self.page_selection()).copied()
    }

    /// 任務頁目前選取項目的穩定識別（不受目前頁面影響）。
    ///
    /// 與 [`Self::selected_entry`] 不同，這裡不依 `nav`：背景資料更新時使用者
    /// 可能正在別的頁面，仍然要能保留任務頁的選取。
    pub fn task_page_selected_id(&self) -> Option<TaskEntryId> {
        self.task_page_entries()
            .get(self.homework_state.selected()?)
            .map(TaskEntry::id)
    }

    /// 依先前記下的識別把任務頁的選取錨定回同一個項目。
    ///
    /// 找不到（項目已移除、換分組或被搜尋過濾）時夾取舊索引。長度一律以
    /// `task_page_entries()` 為準，而非 [`Self::page_len`]——後者依 `nav`
    /// 分派，背景更新時可能回傳別的頁面長度。
    pub fn anchor_task_selection(&mut self, previous: Option<TaskEntryId>) {
        let fallback = self.homework_state.selected().unwrap_or(0);
        let restored = {
            let entries = self.task_page_entries();
            let len = entries.len();
            let found = previous
                .and_then(|id| entries.iter().position(|entry| entry.id() == id))
                .unwrap_or(fallback);
            found.min(len.saturating_sub(1))
        };
        self.homework_state.select(Some(restored));
    }

    /// 任務頁的可選取項目（依目前的排序方式排列）。
    ///
    /// 預設排序維持「任務段在前、作業段在後」；其餘排序方式把兩者混在一起，依
    /// 排序鍵排列（作業的優先級一律視為「高」）。分組篩選與搜尋在兩者都適用。
    pub fn task_page_entries(&self) -> Vec<TaskEntry<'_>> {
        let group = self.homework_group;
        let tasks = self.task_group_items(group);
        let homework = self.homework_group_items(group);
        if !self.task_sort.is_sorted() {
            let mut entries: Vec<TaskEntry<'_>> = tasks.into_iter().map(TaskEntry::Task).collect();
            entries.extend(homework.into_iter().map(TaskEntry::Homework));
            return entries;
        }
        let mut keyed: Vec<(SortKey, TaskEntry<'_>)> = tasks
            .iter()
            .map(|task| (todo::task_sort_key(task), TaskEntry::Task(task)))
            .chain(
                homework
                    .iter()
                    .map(|item| (todo::homework_sort_key(item), TaskEntry::Homework(item))),
            )
            .collect();
        keyed.sort_by(|left, right| todo::compare(self.task_sort, &left.0, &right.0));
        keyed.into_iter().map(|(_, entry)| entry).collect()
    }

    /// 指定分組的作業（依搜尋關鍵字過濾；保持原本排序）。
    pub fn homework_group_items(&self, group: HomeworkGroup) -> Vec<&HomeworkItem> {
        let Some(data) = self.homework.ready() else {
            return Vec::new();
        };
        let keyword = self.task_filter.as_deref();
        data.group_items(group)
            .into_iter()
            .filter(|item| keyword.is_none_or(|keyword| item.matches(keyword)))
            .collect()
    }

    /// 指定分組的自訂義任務（依搜尋關鍵字過濾；已排序）。
    pub fn task_group_items(&self, group: HomeworkGroup) -> Vec<&Task> {
        let keyword = self.task_filter.as_deref();
        self.tasks
            .iter()
            .filter(|task| task.group() == group)
            .filter(|task| keyword.is_none_or(|keyword| task.matches(keyword)))
            .collect()
    }

    /// 所有任務用過的標籤（去重、保留首次出現順序；供 `^F` 的上下鍵預填）。
    pub fn task_tag_options(&self) -> Vec<String> {
        todo::tag_options(&self.tasks)
    }

    /// 任務頁各分組的項目數（作業＋任務；不受搜尋過濾影響）。
    ///
    /// 一次掃描算好整組計數：標題、分組標籤列與提示列共用同一份結果，
    /// 不要逐組重複統計。
    pub fn task_page_group_counts(&self) -> TaskPageCounts {
        let mut counts = TaskPageCounts::default();
        if let Some(data) = self.homework.ready() {
            for item in &data.items {
                counts.add(item.state.group());
            }
        }
        for task in &self.tasks {
            counts.add(task.group());
        }
        counts
    }

    /// 任務頁目前的列模型。
    ///
    /// 預設排序分段顯示（任务段在前、作业段在后，只含非空分段）；混合排序則把
    /// 排好的項目直接列成一串，不再插入分段標題與空白列。
    pub fn task_page_rows(&self) -> Vec<PageRow<'_>> {
        if !self.task_sort.is_sorted() {
            let group = self.homework_group;
            return todo::page_rows(
                &self.task_group_items(group),
                &self.homework_group_items(group),
            );
        }
        self.task_page_entries()
            .into_iter()
            .map(|entry| match entry {
                TaskEntry::Task(task) => PageRow::Task(task),
                TaskEntry::Homework(item) => PageRow::Homework(item),
            })
            .collect()
    }

    /// 目前分組中符合搜尋關鍵字的項目數（作業＋任務）。
    pub fn task_filter_matches(&self) -> usize {
        let group = self.homework_group;
        self.task_group_items(group).len() + self.homework_group_items(group).len()
    }

    /// 設定目前頁面的選取索引。
    pub fn set_selection(&mut self, index: usize) {
        match self.nav {
            NavItem::Schedule => *self.schedule_state.selected_mut() = Some(index),
            NavItem::Homework => *self.homework_state.selected_mut() = Some(index),
            NavItem::Attendance => *self.flow_state.selected_mut() = Some(index),
            NavItem::Lms => match self.lms.level {
                LmsLevel::Activities => *self.activity_state.selected_mut() = Some(index),
                _ => *self.course_state.selected_mut() = Some(index),
            },
        }
    }

    /// 將指定位置標記為失敗（錯誤只影響對應頁面）。
    pub fn fail_target(&mut self, target: FailedTarget, message: &str) {
        match target {
            FailedTarget::Schedule => self.schedule.fail(message),
            FailedTarget::Homework => self.homework.fail(message),
            FailedTarget::Flow => self.attendance.fail(message),
            FailedTarget::Courses => self.lms.courses.fail(message),
            FailedTarget::Activities => self.lms.activities.fail(message),
            FailedTarget::ActivityDetail => self.lms.detail.fail(message),
            FailedTarget::Login
            | FailedTarget::Credentials
            | FailedTarget::ActivityOpen
            | FailedTarget::Settings
            | FailedTarget::Agreement
            | FailedTarget::Tasks => {}
        }
    }

    /// 解除載入中狀態（進行中的載入被取消時）。
    ///
    /// 保留已取得的部分資料（轉為就緒），沒有資料時回到未載入。
    pub fn cancel_loading(&mut self, target: FailedTarget) {
        match target {
            FailedTarget::Schedule => Self::settle_loading(&mut self.schedule),
            FailedTarget::Homework => Self::settle_loading(&mut self.homework),
            FailedTarget::Flow => Self::settle_loading(&mut self.attendance),
            FailedTarget::Courses => Self::settle_loading(&mut self.lms.courses),
            FailedTarget::Activities => Self::settle_loading(&mut self.lms.activities),
            FailedTarget::ActivityDetail => Self::settle_loading(&mut self.lms.detail),
            FailedTarget::Login
            | FailedTarget::Credentials
            | FailedTarget::ActivityOpen
            | FailedTarget::Settings
            | FailedTarget::Agreement
            | FailedTarget::Tasks => {}
        }
    }

    /// 將載入中的頁面收斂為終態（保留已取得的資料）。
    fn settle_loading<T>(page: &mut Page<T>) {
        if let Page::Loading { stale, .. } = page {
            *page = match stale.take() {
                Some(value) => Page::Ready(value),
                None => Page::Idle,
            };
        }
    }

    /// 設定畫面。
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// 是否在主畫面（含設定、學期選擇器與任務彈窗）。
    pub fn is_main(&self) -> bool {
        matches!(
            self.screen,
            Screen::Main
                | Screen::Settings(_)
                | Screen::SettingsForm(_)
                | Screen::TermPicker(_)
                | Screen::TaskMenu(_)
                | Screen::TaskBatchMenu(_)
                | Screen::TaskConfirm(_)
                | Screen::TaskForm(_)
                | Screen::Sort
        )
    }

    /// 若尚未進入主畫面（例如仍在登入畫面），切換到主畫面。
    ///
    /// 學期選擇器屬於主畫面上的彈窗，不算「尚未進入主畫面」，因此不會被
    /// 背景資料更新（例如作業進度）意外關閉。
    pub fn ensure_main(&mut self) {
        if !self.is_main() {
            self.screen = Screen::Main;
        }
    }

    /// 顯示暫時訊息。
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
    }

    /// 清除暫時訊息。
    ///
    /// 使用者按下任何按鍵時呼叫（見 `handler::handle_key`）：底部提示列會立刻
    /// 換回「目前畫面」的快捷鍵，不會因為上一則通知還在而看起來毫無反應。
    pub fn clear_message(&mut self) {
        self.message = None;
    }

    /// 前進一個動畫影格（供載入指示燈動畫使用）。
    pub fn advance_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// 清除過期的訊息。
    pub fn expire_message(&mut self) {
        if let Some((_, at)) = &self.message
            && at.elapsed() >= MESSAGE_TTL
        {
            self.message = None;
        }
    }

    /// 目前要顯示的訊息。
    pub fn message_text(&self) -> Option<&str> {
        self.message.as_ref().map(|(message, _)| message.as_str())
    }

    /// 切換到下一個頁面。
    pub fn nav_next(&mut self) {
        self.nav = self.nav.next();
    }

    /// 切換到上一個頁面。
    pub fn nav_previous(&mut self) {
        self.nav = self.nav.previous();
    }

    /// 指定頁面是否正在載入。
    pub fn is_loading(&self, nav: NavItem) -> bool {
        match nav {
            NavItem::Schedule => self.schedule.is_loading(),
            NavItem::Homework => self.homework.is_loading(),
            NavItem::Attendance => self.attendance.is_loading(),
            NavItem::Lms => self.lms.courses.is_loading(),
        }
    }
}

#[cfg(test)]
#[path = "tests/app_test.rs"]
mod app_test;
