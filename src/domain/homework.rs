//! 作業彙總。
//!
//! 規則：
//!
//! - 提交記錄數大於 0：標記「已完成」（存在有效提交；是否評分不在本階段範圍）。
//! - 提交記錄數為 0 且已過截止時間：標記「逾期」。
//! - 提交記錄數為 0 且尚在期限內（或無截止時間）：標記「待提交」。
//! - 提交記錄數無法確認：標記「待核实」，不得誤判為已提交或未提交。
//!
//! 「未完成」＝待提交＋逾期。清單依分組（未完成 → 已完成 → 待核實）與截止
//! 時間排序：同組內較早截止者在前、無截止時間者在最後，同鍵值再按課程、
//! 標題、活動 ID 穩定排序。

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone as _};

/// 作業狀態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeworkState {
    /// 尚未提交且未逾期。
    Pending,
    /// 尚未提交且已過截止時間。
    Overdue,
    /// 已有有效提交（不等同已評分）。
    Completed,
    /// 無法確認提交狀態。
    Unknown,
}

impl HomeworkState {
    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待提交",
            Self::Overdue => "逾期",
            Self::Completed => "已完成",
            Self::Unknown => "待核实",
        }
    }

    /// 所屬分組。
    pub fn group(self) -> HomeworkGroup {
        match self {
            Self::Pending | Self::Overdue => HomeworkGroup::Unfinished,
            Self::Completed => HomeworkGroup::Completed,
            Self::Unknown => HomeworkGroup::Unknown,
        }
    }
}

/// 作業分組。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HomeworkGroup {
    /// 未完成（待提交與逾期）。
    Unfinished,
    /// 已完成（存在有效提交）。
    Completed,
    /// 待核實（無法確認提交狀態）。
    Unknown,
}

impl HomeworkGroup {
    /// 全部分組（顯示順序）。
    pub const ALL: [Self; 3] = [Self::Unfinished, Self::Completed, Self::Unknown];

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Unfinished => "未完成",
            Self::Completed => "已完成",
            Self::Unknown => "待核实",
        }
    }

    /// 下一個分組（循環）。
    pub fn next(self) -> Self {
        match self {
            Self::Unfinished => Self::Completed,
            Self::Completed => Self::Unknown,
            Self::Unknown => Self::Unfinished,
        }
    }

    /// 上一個分組（循環）。
    pub fn previous(self) -> Self {
        match self {
            Self::Unfinished => Self::Unknown,
            Self::Completed => Self::Unfinished,
            Self::Unknown => Self::Completed,
        }
    }

    /// 顯示順序索引。
    pub fn index(self) -> usize {
        match self {
            Self::Unfinished => 0,
            Self::Completed => 1,
            Self::Unknown => 2,
        }
    }
}

/// 彙總輸入（由站點層組裝）。
#[derive(Debug, Clone)]
pub struct HomeworkInput {
    /// 課程識別碼。
    pub course_id: String,
    /// 課程名稱。
    pub course_name: String,
    /// 活動識別碼。
    pub activity_id: String,
    /// 作業標題。
    pub title: String,
    /// 截止時間（原始字串）。
    pub end_time: Option<String>,
    /// 是否以小組為單位提交。
    pub submit_by_group: bool,
    /// 提交記錄數；`None` 代表無法確認。
    pub submission_count: Option<usize>,
    /// 無法確認提交狀態時的原因（顯示於「待核实」項目）。
    pub note: Option<String>,
}

/// 彙總輸出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeworkItem {
    /// 課程識別碼。
    pub course_id: String,
    /// 課程名稱。
    pub course_name: String,
    /// 活動識別碼。
    pub activity_id: String,
    /// 作業標題。
    pub title: String,
    /// 截止時間（原始字串）。
    pub end_time: Option<String>,
    /// 是否以小組為單位提交。
    pub submit_by_group: bool,
    /// 判定狀態。
    pub state: HomeworkState,
    /// 無法確認提交狀態時的原因（顯示於「待核实」項目）。
    pub note: Option<String>,
}

impl HomeworkItem {
    /// 是否已逾期。
    pub fn is_overdue(&self) -> bool {
        self.state == HomeworkState::Overdue
    }
}

/// 判定單一作業的狀態。
pub fn judge(
    submission_count: Option<usize>,
    end_time: Option<&str>,
    now: DateTime<FixedOffset>,
) -> HomeworkState {
    match submission_count {
        None => HomeworkState::Unknown,
        Some(0) => match parse_time(end_time) {
            Some(deadline) if deadline < now => HomeworkState::Overdue,
            _ => HomeworkState::Pending,
        },
        Some(_) => HomeworkState::Completed,
    }
}

/// 彙總作業：依分組與截止時間穩定排序（無截止時間者排在同組最後）。
pub fn aggregate(items: &[HomeworkInput], now: DateTime<FixedOffset>) -> Vec<HomeworkItem> {
    let mut result: Vec<HomeworkItem> = items
        .iter()
        .map(|input| HomeworkItem {
            course_id: input.course_id.clone(),
            course_name: input.course_name.clone(),
            activity_id: input.activity_id.clone(),
            title: input.title.clone(),
            end_time: input.end_time.clone(),
            submit_by_group: input.submit_by_group,
            state: judge(input.submission_count, input.end_time.as_deref(), now),
            note: input.note.clone(),
        })
        .collect();

    result.sort_by(|left, right| sort_key(left).cmp(&sort_key(right)));
    result
}

/// 排序鍵：（分組、有無截止時間、截止時間、課程、標題、活動 ID）。
fn sort_key(item: &HomeworkItem) -> (HomeworkGroup, u8, i64, &str, &str, &str) {
    let (flag, timestamp) = match parse_time(item.end_time.as_deref()) {
        Some(time) => (0_u8, time.timestamp()),
        None => (1_u8, 0),
    };
    (
        item.state.group(),
        flag,
        timestamp,
        &item.course_name,
        &item.title,
        &item.activity_id,
    )
}

/// 解析思源學堂的時間字串。
///
/// 支援帶時區的 RFC3339、`YYYY-MM-DD HH:MM:SS` 與 `YYYY-MM-DDTHH:MM:SS`。
/// 回傳的瞬間一律以校園時區（中國標準時間，+08:00）表示：帶時區者（如 API 的
/// UTC `Z`／`+00:00`）先換算，未帶時區者視為 +08:00。排序與逾期判定使用絕對
/// 瞬間（`timestamp()`／比較），不受顯示時區影響。
pub fn parse_time(value: Option<&str>) -> Option<DateTime<FixedOffset>> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let offset = FixedOffset::east_opt(8 * 3600)?;
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Some(time.with_timezone(&offset));
    }

    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return offset.from_local_datetime(&naive).single();
        }
    }
    None
}

#[cfg(test)]
#[path = "tests/homework_test.rs"]
mod homework_test;
