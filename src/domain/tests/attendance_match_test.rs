//! 課表與考勤記錄匹配測試。

use chrono::NaiveDate;

use super::*;
use crate::domain::schedule::{CourseSlot, merge_courses};
use crate::sites::attendance::TimetableCourse;

const CLASSROOM: &str = "主楼A101";
const TEACHER: &str = "张老师";

fn slot() -> CourseSlot {
    merge_courses(&[TimetableCourse {
        course_name: "高等数学".to_owned(),
        teacher_name: Some(TEACHER.to_owned()),
        classroom_name: Some(CLASSROOM.to_owned()),
        day_of_week: 1,
        start_section: 1,
        end_section: 2,
        week_ranges: "1-16".to_owned(),
    }])
    .pop()
    .expect("课程时段")
}

fn record(date: &str, status: &str) -> WaterRecord {
    WaterRecord {
        result_id: "r1".to_owned(),
        start_section: 1,
        end_section: 2,
        course_week: 2,
        classroom_name: Some(CLASSROOM.to_owned()),
        teacher_name: Some(TEACHER.to_owned()),
        attendance_status: status.to_owned(),
        attendance_date: date.to_owned(),
        semester_id: None,
    }
}

fn date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("日期")
}

#[test]
fn matches_record_by_date_sections_place_and_teacher() {
    let lesson = slot();
    let day = date("2026-09-14");

    assert_eq!(
        status_for(&lesson, day, &[record("2026-09-14", "NORMAL")]),
        Some(AttendanceStatus::Normal)
    );
    // 不同日期不匹配。
    assert_eq!(
        status_for(&lesson, day, &[record("2026-09-21", "ABSENT")]),
        None
    );
    // 不同地點不匹配。
    let mut other_place = record("2026-09-14", "ABSENT");
    other_place.classroom_name = Some("主楼B202".to_owned());
    assert_eq!(status_for(&lesson, day, &[other_place]), None);
    // 不同教師不匹配。
    let mut other_teacher = record("2026-09-14", "ABSENT");
    other_teacher.teacher_name = Some("李老师".to_owned());
    assert_eq!(status_for(&lesson, day, &[other_teacher]), None);
}

#[test]
fn tolerates_missing_optional_fields_in_records() {
    let lesson = slot();
    let day = date("2026-09-14");

    // 記錄缺少地點與教師時仍應匹配（不能因此漏掉考勤結果）。
    let mut incomplete = record("2026-09-14", "LATE");
    incomplete.classroom_name = None;
    incomplete.teacher_name = None;
    assert_eq!(
        status_for(&lesson, day, &[incomplete]),
        Some(AttendanceStatus::Late)
    );
}

#[test]
fn takes_the_most_severe_status() {
    let lesson = slot();
    let day = date("2026-09-14");
    let records = vec![
        record("2026-09-14", "NORMAL"),
        record("2026-09-14", "LEAVE"),
        record("2026-09-14", "LATE"),
    ];
    assert_eq!(
        status_for(&lesson, day, &records),
        Some(AttendanceStatus::Late)
    );
}

#[test]
fn missing_record_is_never_reported_as_normal() {
    let today = date("2026-09-20");

    // 過去的課沒有記錄 → 待核实（不得推斷為正常）。
    assert_eq!(display_label(None, date("2026-09-14"), today), "待核实");
    // 未來的課沒有記錄 → 待考勤。
    assert_eq!(display_label(None, date("2026-09-21"), today), "待考勤");
    // 有記錄 → 顯示伺服器回報的狀態。
    assert_eq!(
        display_label(Some(AttendanceStatus::Absent), date("2026-09-14"), today),
        "缺勤"
    );
}

#[test]
fn maps_unknown_server_status() {
    let lesson = slot();
    let day = date("2026-09-14");
    assert_eq!(
        status_for(&lesson, day, &[record("2026-09-14", "SOMETHING_NEW")]),
        Some(AttendanceStatus::Unknown)
    );
}
