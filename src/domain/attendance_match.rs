//! 課表與考勤記錄的匹配。
//!
//! 匹配鍵為（上課日期、起訖節次、地點、教師）；找不到對應記錄時一律回報
//! 「沒有記錄」，由 [`display_label`] 決定顯示文字——**缺失記錄不得推斷為正常**。

use chrono::NaiveDate;

use crate::sites::attendance::{AttendanceStatus, WaterRecord};

use super::schedule::CourseSlot;

/// 找出課程在指定日期的考勤狀態。
///
/// 同一時段若有多筆記錄，取嚴重度最高者；`None` 代表沒有可用記錄。
pub fn status_for(
    slot: &CourseSlot,
    date: NaiveDate,
    records: &[WaterRecord],
) -> Option<AttendanceStatus> {
    let expected_date = date.to_string();
    records
        .iter()
        .filter(|record| {
            record.start_section == slot.start_section
                && record.end_section == slot.end_section
                && record.attendance_date.trim() == expected_date
                && matches_optional(slot.classroom.as_deref(), record.classroom_name.as_deref())
                && matches_optional(slot.teacher.as_deref(), record.teacher_name.as_deref())
        })
        .map(WaterRecord::status)
        .max_by_key(|status| status.severity())
}

/// 顯示用標籤。
///
/// - 有記錄：顯示伺服器回報的狀態。
/// - 無記錄且課程尚未發生：顯示「待考勤」。
/// - 無記錄且課程已過：顯示「待核实」。
pub fn display_label(
    status: Option<AttendanceStatus>,
    lesson_date: NaiveDate,
    today: NaiveDate,
) -> &'static str {
    match status {
        Some(status) => status.label(),
        None if lesson_date > today => "待考勤",
        None => "待核实",
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
