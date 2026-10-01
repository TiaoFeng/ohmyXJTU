//! TUI 應用狀態（畫面路由與各頁資料）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::widgets::ListState;

use crate::config::AccessPolicy;
use crate::domain::activity::{self, ActivityGroup};
use crate::domain::course_list::{self, CourseRow};
use crate::domain::homework::{HomeworkGroup, HomeworkItem};
use crate::domain::semester::TermCode;
use crate::model::{ActivityDetailView, FlowData, ScheduleData};
use crate::session::{AccessMode, SiteKind};
use crate::sites::lms::{LmsActivity, LmsCourse};
use crate::task::{FailedTarget, HomeworkIssue};
use crate::tui::text::InputLine;

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
            Self::Homework => "作业",
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

/// 表單欄位。
#[derive(Debug, Clone)]
pub struct FormField {
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
        Self::new(
            FormKind::Setup,
            [
                ("加密口令", true),
                ("确认口令", true),
                ("账号", false),
                ("密码", true),
                ("确认密码", true),
            ]
            .into_iter()
            .map(|(label, masked)| FormField {
                label,
                value: InputLine::new().masked(masked),
            })
            .collect(),
        )
    }

    /// 解鎖表單。
    pub fn unlock() -> Self {
        Self::new(
            FormKind::Unlock,
            vec![FormField {
                label: "加密口令",
                value: InputLine::new().masked(true),
            }],
        )
    }

    /// 登入失敗後重新輸入帳號密碼（密碼與口令皆遮蔽，欄位一律留空）。
    ///
    /// `site` 為原本失敗的站點：重試沿用同一個站點，不被另一個站點的可達性牽制。
    pub fn login_retry(site: SiteKind) -> Self {
        Self::new(
            FormKind::LoginRetry(site),
            [("账号", false), ("密码", true), ("加密口令", true)]
                .into_iter()
                .map(|(label, masked)| FormField {
                    label,
                    value: InputLine::new().masked(masked),
                })
                .collect(),
        )
    }

    /// 修改帳號表單。
    pub fn change_account() -> Self {
        Self::new(
            FormKind::ChangeAccount,
            [
                ("原加密口令", true),
                ("新账号", false),
                ("新密码", true),
                ("确认新密码", true),
            ]
            .into_iter()
            .map(|(label, masked)| FormField {
                label,
                value: InputLine::new().masked(masked),
            })
            .collect(),
        )
    }

    /// 修改加密口令表單。
    pub fn change_passphrase() -> Self {
        Self::new(
            FormKind::ChangePassphrase,
            [
                ("原加密口令", true),
                ("新加密口令", true),
                ("确认新口令", true),
            ]
            .into_iter()
            .map(|(label, masked)| FormField {
                label,
                value: InputLine::new().masked(masked),
            })
            .collect(),
        )
    }

    /// 清空敏感欄位（口令與密碼）；保留非敏感輸入（例如帳號）。
    pub fn clear_secrets(&mut self) {
        let sensitive: &[usize] = match self.kind {
            FormKind::Setup => &[0, 1, 3, 4],
            FormKind::Unlock => &[0],
            FormKind::LoginRetry(_) => &[1, 2],
            FormKind::ChangeAccount => &[0, 2, 3],
            FormKind::ChangePassphrase => &[0, 1, 2],
        };
        for index in sensitive {
            if let Some(field) = self.fields.get_mut(*index) {
                field.value.clear();
            }
        }
    }

    fn new(kind: FormKind, fields: Vec<FormField>) -> Self {
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
    /// 訪問模式項目的索引。
    pub const POLICY_INDEX: usize = 2;

    /// 開啟設定彈窗。
    pub fn open(policy: AccessPolicy) -> Self {
        Self {
            index: 0,
            draft: Some(policy),
            saving: false,
        }
    }

    /// 項目標籤。
    pub fn label(index: usize) -> &'static str {
        match index % Self::COUNT {
            0 => "修改账号",
            1 => "修改加密口令",
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
}

/// 應用程式狀態。
#[derive(Debug)]
pub struct App {
    /// 目前畫面。
    pub screen: Screen,
    /// 登入互動覆蓋層（進度、驗證碼、簡訊、失敗與重新輸入憑證）。
    pub login: Option<Box<LoginScreen>>,
    /// 用户协议閱讀門（首次啟動或協議改版後；開啟時獨占畫面與按鍵）。
    pub agreement: Option<Box<AgreementState>>,
    /// 目前頁面。
    pub nav: NavItem,
    /// 課表頁。
    pub schedule: Page<ScheduleData>,
    /// 作業頁。
    pub homework: Page<HomeworkData>,
    /// 作業頁目前分組。
    pub homework_group: HomeworkGroup,
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
            agreement: None,
            nav: NavItem::Schedule,
            schedule: Page::Idle,
            homework: Page::Idle,
            homework_group: HomeworkGroup::Unfinished,
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
            flow_detail: false,
            updated_at: UpdatedAt::default(),
        }
    }

    /// 目前頁面的項目數量。
    pub fn page_len(&self) -> usize {
        match self.nav {
            NavItem::Schedule => self.schedule.ready().map_or(0, |data| data.lessons.len()),
            NavItem::Homework => self
                .homework
                .ready()
                .map_or(0, |data| data.group_count(self.homework_group)),
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

    /// 會話重置後的頁面失效處理。
    ///
    /// - 更換帳號（`account_changed=true`）：所有頁面資料都屬於舊帳號，
    ///   一律清空並回到未載入（思源學堂回到課程層），切換選取與時間標記。
    /// - 切換訪問模式（`account_changed=false`）：資料仍有效，只把卡在
    ///   「載入中」的頁面收斂（對應的進行中與排隊任務已作廢）。
    pub fn invalidate_data(&mut self, account_changed: bool) {
        if account_changed {
            self.schedule = Page::Idle;
            self.homework = Page::Idle;
            self.attendance = Page::Idle;
            // 舊帳號的課程、活動與詳情一律清空。
            self.lms = LmsState::default();
            self.updated_at = UpdatedAt::default();
            self.schedule_state.select(Some(0));
            self.homework_state.select(Some(0));
            self.flow_state.select(Some(0));
            self.course_state.select(Some(0));
            self.activity_state.select(Some(0));
            return;
        }
        Self::settle_loading(&mut self.schedule);
        Self::settle_loading(&mut self.homework);
        Self::settle_loading(&mut self.attendance);
        Self::settle_loading(&mut self.lms.courses);
        Self::settle_loading(&mut self.lms.activities);
        Self::settle_loading(&mut self.lms.detail);
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
            | FailedTarget::Agreement => {}
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
            | FailedTarget::Agreement => {}
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

    /// 是否在主畫面（含設定彈窗與學期選擇器）。
    pub fn is_main(&self) -> bool {
        matches!(
            self.screen,
            Screen::Main | Screen::Settings(_) | Screen::SettingsForm(_) | Screen::TermPicker(_)
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
