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
    assert!(
        parse_weeks("1-1000")
            .iter()
            .all(|week| *week <= MAX_PARSED_WEEK)
    );
    // 上限邊界：60 含在內，61 起被截掉。
    assert_eq!(
        parse_weeks("59-61").iter().copied().collect::<Vec<_>>(),
        vec![59, 60]
    );
}

/// 單值週次與 0 同樣受範圍限制。
///
/// 上限決定 `max_week`（使用者能翻到第幾週）；原本只有區間分支受限，異常的
/// 單值會把可翻週次撐到任意大，每次翻頁還會對荒謬的日期範圍查考勤。
#[test]
fn single_weeks_respect_the_same_bounds() {
    assert_eq!(
        parse_weeks("5,99999").iter().copied().collect::<Vec<_>>(),
        vec![5]
    );
    assert!(parse_weeks("99999").is_empty());
    assert_eq!(
        parse_weeks("60,61").iter().copied().collect::<Vec<_>>(),
        vec![60]
    );
    // 週次從 1 起算：0 不是有效週次（課表反而永遠不會顯示這門課）。
    assert!(parse_weeks("0").is_empty());
    assert_eq!(
        parse_weeks("0-2").iter().copied().collect::<Vec<_>>(),
        vec![1, 2]
    );
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
        .expect("合并后应保留高等数学");
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

    // 第 1 週（學期開始日為週一）與第 3 週的範圍。
    let (monday, sunday) = week_bounds(start, 1).expect("第 1 周");
    assert_eq!(monday, NaiveDate::from_ymd_opt(2026, 9, 7).unwrap());
    assert_eq!(sunday, NaiveDate::from_ymd_opt(2026, 9, 13).unwrap());
    let (monday, sunday) = week_bounds(start, 3).expect("第 3 周");
    assert_eq!(monday, NaiveDate::from_ymd_opt(2026, 9, 21).unwrap());
    assert_eq!(sunday, NaiveDate::from_ymd_opt(2026, 9, 27).unwrap());
}

/// 學期開始日不是週一時，範圍仍以「開始日 + (週次-1)×7」錨定，
/// 與課程日期（[`CourseSlot::date_in_week`]）落在同一組日期。
#[test]
fn anchors_week_bounds_to_semester_start() {
    // 2026-09-09 是週三。
    let start = NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let (monday, sunday) = week_bounds(start, 1).expect("第 1 周");
    assert_eq!(monday, start);
    assert_eq!(sunday, NaiveDate::from_ymd_opt(2026, 9, 15).unwrap());
    // 早於第 1 週的輸入視為第 1 週。
    assert_eq!(week_bounds(start, 0).expect("第 1 周"), (monday, sunday));
}

#[test]
fn bounds_weeks_by_the_last_course_week() {
    // 課表最晚有課的週次就是上限（與參考實作的考勤來源一致）。
    assert_eq!(total_weeks(Some(19), 12), 19);
    // 今天已進入考試週（第 20 週）而課表只排到第 16 週時，上限至少涵蓋今天，
    // 避免出現「第 N/M 周」而 N > M。
    assert_eq!(total_weeks(Some(16), 20), 20);
    // 沒有課表資料時以今天的週次為上限，不謊報教學週數。
    assert_eq!(total_weeks(None, 7), 7);
    assert_eq!(total_weeks(None, 0), 1);
}

#[test]
fn knows_semester_length() {
    assert_eq!(semester_length("2026-2027-1"), 22);
    assert_eq!(semester_length("2026-2027-3"), 8);
    assert_eq!(clamp_week(30, "2026-2027-3"), 8);
    assert_eq!(clamp_week(30, "2026-2027-1"), 22);
    assert_eq!(clamp_week(0, "2026-2027-1"), 1);
}
