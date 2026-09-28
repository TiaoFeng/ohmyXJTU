//! 待處理作業彙總。
//!
//! 規則：
//!
//! - 提交記錄數大於 0：視為已提交，不列入清單。
//! - 提交記錄數為 0 且已過截止時間：標記「逾期」。
//! - 提交記錄數為 0 且尚在期限內（或無截止時間）：標記「待提交」。
//! - 提交記錄數無法確認：標記「待核实」，不得誤判為已提交或未提交。

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone as _};

/// 作業狀態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeworkState {
    /// 尚未提交且未逾期。
    Pending,
    /// 尚未提交且已過截止時間。
    Overdue,
    /// 無法確認提交狀態。
    Unknown,
}

impl HomeworkState {
    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待提交",
            Self::Overdue => "逾期",
            Self::Unknown => "待核实",
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
}

impl HomeworkItem {
    /// 是否已逾期。
    pub fn is_overdue(&self) -> bool {
        self.state == HomeworkState::Overdue
    }
}

/// 判定單一作業的狀態。
///
/// 回傳 `None` 代表已有提交記錄，不需要列入待處理清單。
pub fn judge(
    submission_count: Option<usize>,
    end_time: Option<&str>,
    now: DateTime<FixedOffset>,
) -> Option<HomeworkState> {
    match submission_count {
        None => Some(HomeworkState::Unknown),
        Some(0) => Some(match parse_time(end_time) {
            Some(deadline) if deadline < now => HomeworkState::Overdue,
            _ => HomeworkState::Pending,
        }),
        Some(_) => None,
    }
}

/// 彙總待處理作業，依截止時間排序（無截止時間者排在最後）。
pub fn aggregate(items: &[HomeworkInput], now: DateTime<FixedOffset>) -> Vec<HomeworkItem> {
    let mut result: Vec<HomeworkItem> = items
        .iter()
        .filter_map(|input| {
            let state = judge(input.submission_count, input.end_time.as_deref(), now)?;
            Some(HomeworkItem {
                course_id: input.course_id.clone(),
                course_name: input.course_name.clone(),
                activity_id: input.activity_id.clone(),
                title: input.title.clone(),
                end_time: input.end_time.clone(),
                submit_by_group: input.submit_by_group,
                state,
            })
        })
        .collect();

    result.sort_by(|left, right| {
        match (
            parse_time(left.end_time.as_deref()),
            parse_time(right.end_time.as_deref()),
        ) {
            (Some(left_time), Some(right_time)) => left_time.cmp(&right_time),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.title.cmp(&right.title),
        }
    });
    result
}

/// 解析思源學堂的時間字串。
///
/// 支援帶時區的 RFC3339、`YYYY-MM-DD HH:MM:SS` 與 `YYYY-MM-DDTHH:MM:SS`；
/// 未帶時區者一律視為中國標準時間（+08:00）。
pub fn parse_time(value: Option<&str>) -> Option<DateTime<FixedOffset>> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Some(time);
    }

    let offset = FixedOffset::east_opt(8 * 3600)?;
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
