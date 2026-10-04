//! 自訂義任務（本機代辦）的模型、排序、輸入解析與任務頁列模型。
//!
//! 任務與思源學堂作業共用「未完成／已完成」兩個分組顯示，但資料來源不同：
//! 任務由使用者自行新增，保存在本機（以加密口令加密），不隨學期或帳號變動。

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use crate::domain::homework::{CAMPUS_UTC_OFFSET_SECS, HomeworkGroup, HomeworkItem};
use crate::tone::Tone;

/// 分段標題：自訂義任務（顯示在作業段之前）。
pub const TASKS_HEADER: &str = "任务";
/// 分段標題：思源學堂作業。
pub const HOMEWORK_HEADER: &str = "作业";

/// 任務優先級。
///
/// 排序順序（`Ord`）即宣告順序：`High < Medium < Low`，讓高優先級排前面。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default, Hash,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// 高。
    High,
    /// 中。
    Medium,
    /// 低（預設）。
    #[default]
    Low,
}

impl Priority {
    /// 全部優先級（由高到低）。
    pub const ALL: [Self; 3] = [Self::High, Self::Medium, Self::Low];

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "高",
            Self::Medium => "中",
            Self::Low => "低",
        }
    }

    /// 下一個優先級（循環）。
    pub fn next(self) -> Self {
        match self {
            Self::High => Self::Medium,
            Self::Medium => Self::Low,
            Self::Low => Self::High,
        }
    }

    /// 上一個優先級（循環）。
    pub fn previous(self) -> Self {
        match self {
            Self::High => Self::Low,
            Self::Medium => Self::High,
            Self::Low => Self::Medium,
        }
    }

    /// 語意色：高＝危險、中＝警告、低＝次要。
    pub fn tone(self) -> Tone {
        match self {
            Self::High => Tone::Danger,
            Self::Medium => Tone::Warning,
            Self::Low => Tone::Muted,
        }
    }
}

/// 任務的顯示狀態（不包含「待核实」：任務的狀態一律可由本機資料判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// 待完成。
    Pending,
    /// 已逾期（未完成且已過截止時間）。
    Overdue,
    /// 已完成。
    Done,
}

impl TaskState {
    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待完成",
            Self::Overdue => "逾期",
            Self::Done => "已完成",
        }
    }

    /// 語意色。
    pub fn tone(self) -> Tone {
        match self {
            Self::Pending => Tone::Accent,
            Self::Overdue => Tone::Danger,
            Self::Done => Tone::Success,
        }
    }

    /// 所屬分組。
    pub fn group(self) -> HomeworkGroup {
        match self {
            Self::Pending | Self::Overdue => HomeworkGroup::Unfinished,
            Self::Done => HomeworkGroup::Completed,
        }
    }
}

/// 自訂義任務。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// 穩定識別碼（同一份任務檔內唯一）。
    pub id: u64,
    /// 任務內容（標題）。
    pub content: String,
    /// 任務描述（可選）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 截止時間（可選）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<FixedOffset>>,
    /// 優先級。
    #[serde(default)]
    pub priority: Priority,
    /// 是否已完成。
    #[serde(default)]
    pub completed: bool,
}

impl Task {
    /// 目前的顯示狀態。
    pub fn state(&self, now: DateTime<FixedOffset>) -> TaskState {
        if self.completed {
            return TaskState::Done;
        }
        match self.deadline {
            Some(deadline) if deadline < now => TaskState::Overdue,
            _ => TaskState::Pending,
        }
    }

    /// 所屬分組（未完成／已完成）。
    pub fn group(&self) -> HomeworkGroup {
        if self.completed {
            HomeworkGroup::Completed
        } else {
            HomeworkGroup::Unfinished
        }
    }

    /// 關鍵字是否符合（內容或描述，不分大小寫）。
    pub fn matches(&self, keyword: &str) -> bool {
        let keyword = keyword.trim().to_lowercase();
        if keyword.is_empty() {
            return true;
        }
        self.content.to_lowercase().contains(&keyword)
            || self
                .description
                .as_ref()
                .is_some_and(|description| description.to_lowercase().contains(&keyword))
    }
}

/// 排序鍵：（有無截止、截止時間、優先級、內容、識別碼）。
///
/// 無截止時間者排在最後；同鍵以內容與識別碼穩定排序。
fn sort_key(task: &Task) -> (u8, i64, Priority, String, u64) {
    let (flag, timestamp) = match task.deadline {
        Some(deadline) => (0_u8, deadline.timestamp()),
        None => (1_u8, 0),
    };
    (
        flag,
        timestamp,
        task.priority,
        task.content.clone(),
        task.id,
    )
}

/// 依排序鍵就地排序任務。
pub fn sort_tasks(tasks: &mut [Task]) {
    tasks.sort_by_key(sort_key);
}

/// 解析使用者輸入的截止時間；空字串代表「沒有截止時間」。
///
/// 支援 `YYYY-MM-DD`（自動補為當日 23:59:59）、`YYYY-MM-DD HH:MM[:SS]` 與
/// `YYYY-MM-DDTHH:MM[:SS]`；未帶時區者視為校園時區（+08:00）。錯誤訊息只
/// 描述格式，不夾帶輸入內容。
pub fn parse_deadline_input(input: &str) -> Result<Option<DateTime<FixedOffset>>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let Some(offset) = FixedOffset::east_opt(CAMPUS_UTC_OFFSET_SECS) else {
        return Err("无法确定本地时区".to_owned());
    };
    for format in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(input, format) {
            return offset
                .from_local_datetime(&naive)
                .single()
                .map(Some)
                .ok_or_else(|| "截止时间无效".to_owned());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let naive = date
            .and_hms_opt(23, 59, 59)
            .ok_or_else(|| "截止时间无效".to_owned())?;
        return offset
            .from_local_datetime(&naive)
            .single()
            .map(Some)
            .ok_or_else(|| "截止时间无效".to_owned());
    }
    Err("截止时间格式无法识别（示例：2026-12-31 或 2026-12-31 12:30）".to_owned())
}

/// 任務頁的一列；分段標題與空白列不可選取。
#[derive(Debug, Clone)]
pub enum PageRow<'a> {
    /// 分段標題（「任务」或「作业」）。
    Header(&'static str),
    /// 分段之間的空白列。
    Spacer,
    /// 自訂義任務。
    Task(&'a Task),
    /// 思源學堂作業。
    Homework(&'a HomeworkItem),
}

impl PageRow<'_> {
    /// 是否為可選取的列。
    pub fn is_selectable(&self) -> bool {
        matches!(self, Self::Task(_) | Self::Homework(_))
    }
}

/// 產生任務頁的列：任务段在前、作业段在后；只顯示非空的分段，分段之間留一列空白。
pub fn page_rows<'a>(tasks: &[&'a Task], homework: &[&'a HomeworkItem]) -> Vec<PageRow<'a>> {
    let mut rows = Vec::new();
    if !tasks.is_empty() {
        rows.push(PageRow::Header(TASKS_HEADER));
        rows.extend(tasks.iter().map(|task| PageRow::Task(task)));
    }
    if !homework.is_empty() {
        if !rows.is_empty() {
            rows.push(PageRow::Spacer);
        }
        rows.push(PageRow::Header(HOMEWORK_HEADER));
        rows.extend(homework.iter().map(|item| PageRow::Homework(item)));
    }
    rows
}

/// 可選取列的總數（任務在前、作業在後）。
pub fn selectable_len(tasks: usize, homework: usize) -> usize {
    tasks + homework
}

/// 選取索引在列模型中的位置（供清單高亮與 offset 映射；標題與空白列不計入）。
pub fn visual_index(rows: &[PageRow<'_>], selection: usize) -> Option<usize> {
    let mut seen = 0_usize;
    for (position, row) in rows.iter().enumerate() {
        if row.is_selectable() {
            if seen == selection {
                return Some(position);
            }
            seen += 1;
        }
    }
    None
}

/// 任務頁的排序方式（`^L`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// 預設：任務段在前、作業段在後（分段顯示）。
    #[default]
    Default,
    /// 依優先級（作業一律視為「高」）。
    Priority,
    /// 依截止時間（無截止時間者排最後）。
    Deadline,
}

impl SortMode {
    /// 顯示名稱（排序提示與標題列使用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "默认",
            Self::Priority => "优先级",
            Self::Deadline => "截止时间",
        }
    }

    /// 是否為混合排序：任務與作業混在一起，不再分段。
    pub fn is_sorted(self) -> bool {
        self != Self::Default
    }
}

/// 任務頁項目的排序鍵（任務與作業共用）。
///
/// 作業沒有優先級與本機識別碼：優先級一律視為 [`Priority::High`]，識別碼取活動
/// 識別碼。截止時間以絕對瞬間比較（任務直接取欄位、作業解析 `end_time`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// 是否沒有截止時間（`0` ＝有、`1` ＝無；讓有截止時間者排前面）。
    missing_deadline: u8,
    /// 截止時間的絕對瞬間（無截止時間時為 0）。
    deadline: i64,
    /// 優先級（`Ord` 為高 → 低）。
    priority: Priority,
    /// 標題或內容（僅作穩定排序的第二鍵，不分大小寫）。
    label: String,
    /// 識別碼：任務取流水號（補零以維持數值順序）、作業取活動識別碼。
    id: String,
}

impl SortKey {
    /// 組出排序鍵。
    fn new(deadline: Option<i64>, priority: Priority, label: &str, id: String) -> Self {
        Self {
            missing_deadline: u8::from(deadline.is_none()),
            deadline: deadline.unwrap_or(0),
            priority,
            label: label.to_lowercase(),
            id,
        }
    }
}

/// 自訂義任務的排序鍵。
pub fn task_sort_key(task: &Task) -> SortKey {
    SortKey::new(
        task.deadline.map(|deadline| deadline.timestamp()),
        task.priority,
        &task.content,
        format!("{:020}", task.id),
    )
}

/// 思源學堂作業的排序鍵（優先級一律視為「高」）。
pub fn homework_sort_key(item: &HomeworkItem) -> SortKey {
    SortKey::new(
        crate::domain::homework::parse_time(item.end_time.as_deref())
            .map(|deadline| deadline.timestamp()),
        Priority::High,
        &item.title,
        item.activity_id.clone(),
    )
}

/// 依排序方式比較兩個排序鍵。
///
/// 優先級：優先級 → 截止時間 → 標題 → 識別碼；
/// 截止時間：截止時間 → 優先級 → 標題 → 識別碼。
/// 兩者的截止時間都是升序，沒有截止時間者一律排在同組最後。
pub fn compare(mode: SortMode, left: &SortKey, right: &SortKey) -> Ordering {
    match mode {
        SortMode::Priority => (
            left.priority,
            left.missing_deadline,
            left.deadline,
            &left.label,
            &left.id,
        )
            .cmp(&(
                right.priority,
                right.missing_deadline,
                right.deadline,
                &right.label,
                &right.id,
            )),
        SortMode::Default | SortMode::Deadline => (
            left.missing_deadline,
            left.deadline,
            left.priority,
            &left.label,
            &left.id,
        )
            .cmp(&(
                right.missing_deadline,
                right.deadline,
                right.priority,
                &right.label,
                &right.id,
            )),
    }
}

#[cfg(test)]
#[path = "tests/todo_test.rs"]
mod todo_test;
