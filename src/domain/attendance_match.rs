//! 課表與考勤記錄的匹配。
//!
//! 匹配鍵為（上課日期、起訖節次、地點、教師）；找不到對應記錄時一律回報
//! 「沒有記錄」，由 [`display_state`] 決定顯示狀態——**缺失記錄不得推斷為正常**。

use chrono::NaiveDate;

use crate::sites::attendance::{AttendanceStatus, WaterRecord};
use crate::tone::Tone;

use super::schedule::CourseSlot;

/// 找出課程在指定日期的考勤狀態。
///
/// 同一時段若有多筆記錄，取嚴重度最高者；`None` 代表沒有可用記錄。
/// 記錄的日期在解析時已正規化為 `YYYY-MM-DD`（見 [`crate::sites::date_string`]），
/// 因此這裡直接比較字串即可。
pub fn status_for(
    slot: &CourseSlot,
    date: NaiveDate,
    records: &[WaterRecord],
) -> Option<AttendanceStatus> {
    let expected_date = date.format("%Y-%m-%d").to_string();
    records
        .iter()
        .filter(|record| {
            record.start_section == slot.start_section
                && record.end_section == slot.end_section
                && record.attendance_date == expected_date
                && matches_optional(slot.classroom.as_deref(), record.classroom_name.as_deref())
                && matches_optional(slot.teacher.as_deref(), record.teacher_name.as_deref())
        })
        .map(WaterRecord::status)
        .max_by_key(|status| status.severity())
}

/// 課程的考勤顯示狀態。
///
/// 除了伺服器回報的狀態外，還包含兩種由缺失記錄推導的顯示狀態：尚未發生的
/// 課程為 [`Self::Pending`]，已發生但沒有記錄為 [`Self::Unknown`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LessonAttendance {
    /// 伺服器回報的考勤狀態。
    Recorded(AttendanceStatus),
    /// 課程尚未發生，尚無考勤記錄（待考勤）。
    Pending,
    /// 課程已過但沒有對應記錄（待核实）。
    Unknown,
}

impl LessonAttendance {
    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Recorded(status) => status.label(),
            Self::Pending => "待考勤",
            Self::Unknown => "待核实",
        }
    }

    /// 狀態語意色。
    pub fn tone(self) -> Tone {
        match self {
            Self::Recorded(status) => status.tone(),
            Self::Pending => Tone::Accent,
            Self::Unknown => Tone::Warning,
        }
    }
}

impl From<AttendanceStatus> for LessonAttendance {
    fn from(status: AttendanceStatus) -> Self {
        Self::Recorded(status)
    }
}

/// 顯示用狀態。
///
/// - 有記錄：顯示伺服器回報的狀態。
/// - 無記錄且課程在未來（`lesson_date > today`）：顯示「待考勤」。
/// - 無記錄且課程在當天或過去：顯示「待核实」——本程式沒有節次時間表，無法
///   判斷今天這堂是否已經上完，因此不推斷為「尚未發生」（缺失記錄不得推斷為正常）。
pub fn display_state(
    status: Option<AttendanceStatus>,
    lesson_date: NaiveDate,
    today: NaiveDate,
) -> LessonAttendance {
    match status {
        Some(status) => LessonAttendance::from(status),
        None if lesson_date > today => LessonAttendance::Pending,
        None => LessonAttendance::Unknown,
    }
}

/// 比對可選欄位：任一方缺少資訊時不比對該欄位（避免誤判為不匹配）。
fn matches_optional(expected: Option<&str>, actual: Option<&str>) -> bool {
    let expected = expected.map(str::trim).filter(|value| !value.is_empty());
    let actual = actual.map(str::trim).filter(|value| !value.is_empty());
    match (expected, actual) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => true,
    }
}

#[cfg(test)]
#[path = "tests/attendance_match_test.rs"]
mod attendance_match_test;
