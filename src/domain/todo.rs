//! 自訂義任務（本機代辦）的資料模型、標籤規則與輸入解析。
//!
//! 任務與思源學堂作業共用「未完成／已完成」兩個分組顯示，但資料來源不同：
//! 任務由使用者自行新增，保存在本機（以加密口令加密），不隨學期或帳號變動。
//!
//! 任務頁的列模型與排序（清單分段、混合排序、選取映射）在 `page` 模組，並由本
//! 模組重新匯出；呼叫端一律使用 `crate::domain::todo::…`。

mod page;

pub use page::{
    HOMEWORK_HEADER, PageRow, SortKey, SortMode, TASKS_HEADER, compare, homework_sort_key,
    page_rows, selectable_len, task_sort_key, visual_index,
};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};

use crate::domain::homework::{CAMPUS_UTC_OFFSET_SECS, HomeworkGroup};
use crate::text::display_width;
use crate::tone::Tone;

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
    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::High => "高",
            Self::Medium => "中",
            Self::Low => "低",
        }
    }

    /// 下一個優先級（循環；`→` 的方向：低 → 中 → 高 → 低）。
    pub fn next(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Low,
        }
    }

    /// 上一個優先級（循環；`←` 的方向：高 → 中 → 低 → 高）。
    pub fn previous(self) -> Self {
        match self {
            Self::High => Self::Medium,
            Self::Medium => Self::Low,
            Self::Low => Self::High,
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
    /// 標籤（可選；長度上限見 [`TAG_MAX_WIDTH`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
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

    /// 有效標籤（去除前後空白；只剩空白者視為沒有標籤）。
    pub fn display_tag(&self) -> Option<&str> {
        self.tag
            .as_deref()
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
    }

    /// 關鍵字是否符合（內容、描述或標籤，不分大小寫）。
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
            || self
                .display_tag()
                .is_some_and(|tag| tag.to_lowercase().contains(&keyword))
    }
}

/// 標籤的長度上限（顯示欄；六個漢字寬度）。
///
/// 以終端顯示寬度計算（全形字佔 2 欄），與列表欄位排版的寬度語意一致。
pub const TAG_MAX_WIDTH: usize = 12;

/// 正規化使用者輸入的標籤：去除前後空白，空字串代表「沒有標籤」。
pub fn normalize_tag(raw: &str) -> Option<String> {
    let tag = raw.trim();
    (!tag.is_empty()).then(|| tag.to_owned())
}

/// 標籤在目前內容下能否再容納 `extra`（以顯示寬度計）。
pub fn tag_fits(current: &str, extra: &str) -> bool {
    display_width(current) + display_width(extra) <= TAG_MAX_WIDTH
}

/// 所有任務用過的標籤（去重、保留首次出現順序）。
///
/// 供 `^F` 搜尋框的上下鍵循環預填：順序跟著任務本身的排序，使用者看到的候選
/// 與列表一致。
pub fn tag_options(tasks: &[Task]) -> Vec<String> {
    let mut options: Vec<String> = Vec::new();
    for task in tasks {
        let Some(tag) = task.display_tag() else {
            continue;
        };
        if !options.iter().any(|option| option == tag) {
            options.push(tag.to_owned());
        }
    }
    options
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
        let naive = end_of_day(date).ok_or_else(|| "截止时间无效".to_owned())?;
        return offset
            .from_local_datetime(&naive)
            .single()
            .map(Some)
            .ok_or_else(|| "截止时间无效".to_owned());
    }
    Err("截止时间格式无法识别（示例：2026-12-31 或 2026-12-31 12:30）".to_owned())
}

/// 只輸入日期時的截止時間：當日 23:59:59。
fn end_of_day(date: NaiveDate) -> Option<NaiveDateTime> {
    date.and_hms_opt(23, 59, 59)
}

#[cfg(test)]
#[path = "tests/todo_test.rs"]
mod todo_test;
