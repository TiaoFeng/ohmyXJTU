//! 思源學堂活動分組與排序測試。

use crate::domain::activity::{ActivityGroup, counts, grouped};
use crate::sites::lms::{ActivityKind, LmsActivity};

/// 測試用活動。
fn activity(id: &str, kind: &str, start: Option<&str>, end: Option<&str>) -> LmsActivity {
    LmsActivity {
        id: id.to_owned(),
        course_id: Some("1".to_owned()),
        kind: kind.to_owned(),
        title: Some(format!("活动 {id}")),
        start_time: start.map(str::to_owned),
        end_time: end.map(str::to_owned),
        submit_by_group: None,
        group_id: None,
        description: None,
        user_submit_count: None,
        published: None,
    }
}

#[test]
fn groups_cover_every_activity_kind() {
    assert_eq!(
        ActivityGroup::from_kind(ActivityKind::LectureLive),
        ActivityGroup::LectureLive
    );
    assert_eq!(
        ActivityGroup::from_kind(ActivityKind::Lesson),
        ActivityGroup::Lesson
    );
    assert_eq!(
        ActivityGroup::from_kind(ActivityKind::Homework),
        ActivityGroup::Homework
    );
    assert_eq!(
        ActivityGroup::from_kind(ActivityKind::Material),
        ActivityGroup::Material
    );
    assert_eq!(
        ActivityGroup::from_kind(ActivityKind::Unknown),
        ActivityGroup::Other
    );
}

#[test]
fn groups_cycle_in_display_order() {
    assert_eq!(ActivityGroup::LectureLive.previous(), ActivityGroup::Other);
    assert_eq!(ActivityGroup::Other.next(), ActivityGroup::LectureLive);
    assert_eq!(ActivityGroup::Homework.next(), ActivityGroup::Material);
    assert_eq!(ActivityGroup::Homework.previous(), ActivityGroup::Lesson);
}

#[test]
fn filters_only_the_selected_group() {
    let activities = vec![
        activity("1", "homework", None, None),
        activity("2", "lesson", None, None),
        activity("3", "homework", None, None),
        activity("4", "mystery", None, None),
    ];

    let homework = grouped(&activities, ActivityGroup::Homework);
    assert_eq!(
        homework
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["1", "3"]
    );

    let other = grouped(&activities, ActivityGroup::Other);
    assert_eq!(
        other
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["4"]
    );
    assert!(grouped(&activities, ActivityGroup::Material).is_empty());
}

#[test]
fn sorts_by_deadline_then_start_then_id_with_missing_last() {
    let activities = vec![
        // 最晚截止。
        activity("5", "homework", None, Some("2026-10-02 23:59:59")),
        // 截止相同時，數值識別碼小者在前（12 應排在 3 之後）。
        activity("12", "homework", None, Some("2026-10-01 23:59:59")),
        activity("3", "homework", None, Some("2026-10-01 23:59:59")),
        // 最早截止。
        activity("1", "homework", None, Some("2026-09-15 12:00:00")),
        // 無截止但有開始時間。
        activity("2", "homework", Some("2026-09-30 08:00:00"), None),
        // 無法解析的時間視同無值，排在有效時間之後。
        activity("7", "homework", None, Some("not-a-date")),
        // 完全沒有時間：先數值識別碼、再原始字串。
        activity("30", "homework", None, None),
        activity("abc", "homework", None, None),
    ];

    let sorted = grouped(&activities, ActivityGroup::Homework);
    assert_eq!(
        sorted
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["1", "3", "12", "5", "2", "7", "30", "abc"]
    );
}

#[test]
fn counts_items_in_display_order() {
    let activities = vec![
        activity("1", "lecture_live", None, None),
        activity("2", "homework", None, None),
        activity("3", "homework", None, None),
        activity("4", "unknown", None, None),
    ];

    let result: Vec<(&str, usize)> = counts(&activities)
        .iter()
        .map(|(group, count)| (group.label(), *count))
        .collect();
    assert_eq!(
        result,
        [
            ("直播", 1),
            ("课程内容", 0),
            ("作业", 2),
            ("资料", 0),
            ("其他", 1)
        ]
    );
}
