//! TUI 應用狀態（畫面路由與各頁資料）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use ratatui::widgets::ListState;

use crate::config::AccessPolicy;
use crate::domain::homework::HomeworkItem;
use crate::session::AccessMode;
use crate::sites::attendance::{AttendanceStatus, FlowRecord};
use crate::sites::lms::{LmsActivity, LmsCourse, LmsSubmission};
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
}

/// 頁面資料狀態。
#[derive(Debug, Default)]
pub enum Page<T> {
    /// 尚未載入。
    #[default]
    Idle,
    /// 載入中。
    Loading {
        /// 進度說明。
        note: String,
    },
    /// 已載入。
    Ready(T),
    /// 載入失敗。
    Failed(String),
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

    /// 已載入的資料。
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    /// 已載入的資料（可變）。
    pub fn ready_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }

    /// 載入中或失敗的說明文字。
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Loading { note } => Some(note),
            Self::Failed(message) => Some(message),
            _ => None,
        }
    }

    /// 進入載入中狀態。
    pub fn start_loading(&mut self, note: impl Into<String>) {
        *self = Self::Loading { note: note.into() };
    }

    /// 進入失敗狀態。
    pub fn fail(&mut self, message: impl Into<String>) {
        *self = Self::Failed(message.into());
    }
}

/// 本週的一堂課。
#[derive(Debug, Clone)]
pub struct LessonEntry {
    /// 上課日期。
    pub date: NaiveDate,
    /// 節次，例如 `1-2`。
    pub sections: String,
    /// 課程名稱。
    pub course_name: String,
    /// 上課地點。
    pub classroom: String,
    /// 教師。
    pub teacher: String,
    /// 週次說明。
    pub weeks: String,
    /// 考勤狀態（`None` 代表沒有記錄）。
    pub status: Option<AttendanceStatus>,
    /// 顯示用考勤標籤。
    pub label: &'static str,
}

/// 課表頁資料。
#[derive(Debug, Clone, Default)]
pub struct ScheduleData {
    /// 學期說明，例如 `2026-2027-1`。
    pub semester: String,
    /// 本週週次。
    pub week: u32,
    /// 本週課程。
    pub lessons: Vec<LessonEntry>,
    /// 因格式問題被跳過的課程筆數。
    pub skipped: usize,
}

/// 考勤流水頁資料。
#[derive(Debug, Clone, Default)]
pub struct FlowData {
    /// 本頁流水。
    pub records: Vec<FlowRecord>,
    /// 目前頁碼。
    pub page: u32,
    /// 總頁數。
    pub total_pages: u32,
    /// 總筆數。
    pub total: u64,
}

/// 活動詳情檢視資料。
#[derive(Debug, Clone, Default)]
pub struct ActivityDetailView {
    /// 標題。
    pub title: String,
    /// 類型標籤。
    pub kind: String,
    /// 截止時間。
    pub end_time: Option<String>,
    /// 是否小組作業。
    pub submit_by_group: bool,
    /// 提交記錄；`None` 代表無法確認。
    pub submissions: Option<Vec<LmsSubmission>>,
    /// 補充說明（例如無法確認提交狀態的原因）。
    pub note: Option<String>,
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
    /// 選取的課程索引。
    pub course_index: usize,
    /// 選取的活動索引。
    pub activity_index: usize,
    /// 目前層級。
    pub level: LmsLevel,
}

/// 表單種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKind {
    /// 首次設定（加密口令＋帳號）。
    Setup,
    /// 解鎖保險庫。
    Unlock,
    /// 登入失敗後重新輸入帳號密碼。
    LoginRetry,
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
    pub fn login_retry() -> Self {
        Self::new(
            FormKind::LoginRetry,
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
        /// 錯誤訊息。
        message: String,
    },
    /// 重新輸入帳號密碼（登入失敗後的可恢復入口）。
    Credentials {
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
}

impl SettingsState {
    /// 設定項目數量。
    pub const COUNT: usize = 3;

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
}

/// 畫面。
#[derive(Debug)]
pub enum Screen {
    /// 首次設定。
    Setup(FormState),
    /// 解鎖。
    Unlock(FormState),
    /// 登入互動。
    Login(Box<LoginScreen>),
    /// 主畫面。
    Main,
    /// 帳戶設定彈窗。
    Settings(SettingsState),
    /// 設定中的表單。
    SettingsForm(FormState),
}

/// 應用程式狀態。
#[derive(Debug)]
pub struct App {
    /// 目前畫面。
    pub screen: Screen,
    /// 目前頁面。
    pub nav: NavItem,
    /// 課表頁。
    pub schedule: Page<ScheduleData>,
    /// 作業頁。
    pub homework: Page<Vec<HomeworkItem>>,
    /// 考勤流水頁。
    pub attendance: Page<FlowData>,
    /// 思源學堂頁。
    pub lms: LmsState,
    /// 訪問策略設定。
    pub access_policy: AccessPolicy,
    /// 目前實際使用的訪問方式。
    pub access_mode: Option<AccessMode>,
    /// 驗證碼圖片路徑（顯示於狀態列）。
    pub captcha_path: Option<PathBuf>,
    /// 暫時訊息（自動過期）。
    pub message: Option<(String, Instant)>,
    /// 是否結束程式。
    pub quit: bool,
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
}

impl App {
    /// 建立應用狀態。
    pub fn new(access_policy: AccessPolicy) -> Self {
        Self {
            screen: Screen::Unlock(FormState::unlock()),
            nav: NavItem::Schedule,
            schedule: Page::Idle,
            homework: Page::Idle,
            attendance: Page::Idle,
            lms: LmsState::default(),
            access_policy,
            access_mode: None,
            captcha_path: None,
            message: None,
            quit: false,
            nav_state: ListState::default().with_selected(Some(0)),
            schedule_state: ListState::default().with_selected(Some(0)),
            homework_state: ListState::default().with_selected(Some(0)),
            flow_state: ListState::default().with_selected(Some(0)),
            course_state: ListState::default().with_selected(Some(0)),
            activity_state: ListState::default().with_selected(Some(0)),
            schedule_detail: false,
            homework_detail: false,
            flow_detail: false,
        }
    }

    /// 目前頁面的項目數量。
    pub fn page_len(&self) -> usize {
        match self.nav {
            NavItem::Schedule => self.schedule.ready().map_or(0, |data| data.lessons.len()),
            NavItem::Homework => self.homework.ready().map_or(0, Vec::len),
            NavItem::Attendance => self.attendance.ready().map_or(0, |data| data.records.len()),
            NavItem::Lms => match self.lms.level {
                LmsLevel::Courses => self.lms.courses.ready().map_or(0, Vec::len),
                LmsLevel::Activities => self.lms.activities.ready().map_or(0, Vec::len),
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

    /// 下一個項目。
    pub fn select_next(&mut self) {
        let len = self.page_len();
        if len == 0 {
            return;
        }
        let next = (self.page_selection() + 1).min(len - 1);
        self.set_selection(next);
    }

    /// 上一個項目。
    pub fn select_previous(&mut self) {
        let current = self.page_selection();
        self.set_selection(current.saturating_sub(1));
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

    /// 將仍在載入中的頁面標記為失敗。
    pub fn fail_loading(&mut self, message: &str) {
        if self.schedule.is_loading() {
            self.schedule.fail(message);
        }
        if self.homework.is_loading() {
            self.homework.fail(message);
        }
        if self.attendance.is_loading() {
            self.attendance.fail(message);
        }
        if self.lms.courses.is_loading() {
            self.lms.courses.fail(message);
        }
        if self.lms.activities.is_loading() {
            self.lms.activities.fail(message);
        }
        if self.lms.detail.is_loading() {
            self.lms.detail.fail(message);
        }
    }

    /// 設定畫面。
    pub fn set_screen(&mut self, screen: Screen) {
        self.screen = screen;
    }

    /// 是否在主畫面（含設定彈窗）。
    pub fn is_main(&self) -> bool {
        matches!(
            self.screen,
            Screen::Main | Screen::Settings(_) | Screen::SettingsForm(_)
        )
    }

    /// 若尚未進入主畫面（例如仍在登入畫面），切換到主畫面。
    pub fn ensure_main(&mut self) {
        if !self.is_main() {
            self.screen = Screen::Main;
        }
    }

    /// 顯示暫時訊息。
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
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

    /// 目前頁面的載入／錯誤說明。
    pub fn current_note(&self) -> Option<&str> {
        match self.nav {
            NavItem::Schedule => self.schedule.note(),
            NavItem::Homework => self.homework.note(),
            NavItem::Attendance => self.attendance.note(),
            NavItem::Lms => match self.lms.level {
                LmsLevel::Courses => self.lms.courses.note(),
                LmsLevel::Activities => self.lms.activities.note(),
                LmsLevel::Detail => self.lms.detail.note(),
            },
        }
    }
}

#[cfg(test)]
#[path = "tests/app_test.rs"]
mod app_test;
