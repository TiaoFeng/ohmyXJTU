//! 課表週次與合併邏輯測試。

use chrono::NaiveDate;

use super::*;
use crate::sites::attendance::TimetableCourse;

fn course(name: &str, day: u32, start: u32, end: u32, weeks: &str) -> TimetableCourse {
    TimetableCourse {
        course_name: name.to_owned(),
        teacher_name: Some("张老师".to_owned()),
        classroom_name: Some("主楼A101".to_owned()),
        day_of_week: day,
        start_section: start,
        end_section: end,
        week_ranges: weeks.to_owned(),
    }
}

#[test]
fn parses_week_ranges() {
    assert_eq!(
        parse_weeks("1-4,6,8-10")
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 6, 8, 9, 10]
    );
    assert_eq!(parse_weeks("").len(), 0);
    assert_eq!(
        parse_weeks("abc,3").iter().copied().collect::<Vec<_>>(),
        vec![3]
    );
    // 逆向區間也要能解析，且超長區間會被限制。
    assert_eq!(
        parse_weeks("5-3").iter().copied().collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    assert!(parse_weeks("1-1000").iter().all(|week| *week <= 60));
}

#[test]
fn merges_multi_segment_weeks_for_same_slot() {
    let slots = merge_courses(&[
        course("高等数学", 1, 1, 2, "1-4"),
        course("高等数学", 1, 1, 2, "6-8"),
        course("大学物理", 3, 5, 6, "1-16"),
    ]);

    assert_eq!(slots.len(), 2);
    let math = slots
        .iter()
        .find(|slot| slot.course_name == "高等数学")
        .expect("合併後應保留高等數學");
    assert_eq!(math.weeks_label(), "1-4,6-8");
    assert!(math.is_in_week(7));
    assert!(!math.is_in_week(5));
}

#[test]
fn drops_courses_without_weeks() {
    let slots = merge_courses(&[course("无周次课程", 2, 3, 4, "")]);
    assert!(slots.is_empty());
}

#[test]
fn computes_lesson_dates() {
    let start = NaiveDate::from_ymd_opt(2026, 9, 7).expect("学期开始日");
    let slot = merge_courses(&[course("线性代数", 3, 1, 2, "1-16")])
        .pop()
        .expect("课程");

    // 第 1 週週三。
    assert_eq!(
        slot.date_in_week(start, 1),
        NaiveDate::from_ymd_opt(2026, 9, 9)
    );
    // 第 2 週週三。
    assert_eq!(
        slot.date_in_week(start, 2),
        NaiveDate::from_ymd_opt(2026, 9, 16)
    );
}

#[test]
fn computes_week_numbers_and_windows() {
    let start = NaiveDate::from_ymd_opt(2026, 9, 7).unwrap();
    assert_eq!(
        week_number(start, NaiveDate::from_ymd_opt(2026, 9, 7).unwrap()),
        1
    );
    assert_eq!(
        week_number(start, NaiveDate::from_ymd_opt(2026, 9, 13).unwrap()),
        1
    );
    assert_eq!(
        week_number(start, NaiveDate::from_ymd_opt(2026, 9, 14).unwrap()),
        2
    );
    // 早於學期開始一律視為第 1 週。
    assert_eq!(
        week_number(start, NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()),
        1
    );

    let (monday, sunday) = week_window(NaiveDate::from_ymd_opt(2026, 9, 10).unwrap());
    assert_eq!(monday, NaiveDate::from_ymd_opt(2026, 9, 7).unwrap());
    assert_eq!(sunday, NaiveDate::from_ymd_opt(2026, 9, 13).unwrap());
}

#[test]
fn knows_semester_length() {
    assert_eq!(semester_length("2026-2027-1"), 22);
    assert_eq!(semester_length("2026-2027-3"), 8);
    assert_eq!(clamp_week(30, "2026-2027-3"), 8);
    assert_eq!(clamp_week(30, "2026-2027-1"), 22);
    assert_eq!(clamp_week(0, "2026-2027-1"), 1);
}
