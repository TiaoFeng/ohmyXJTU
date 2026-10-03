//! 作業彙總規則測試。

use chrono::{DateTime, FixedOffset};

use super::*;

fn now() -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间")
}

fn input(title: &str, end_time: Option<&str>, submission_count: Option<usize>) -> HomeworkInput {
    HomeworkInput {
        course_id: "42".to_owned(),
        course_name: "编译原理".to_owned(),
        activity_id: format!("a-{title}"),
        title: title.to_owned(),
        end_time: end_time.map(str::to_owned),
        description: None,
        submit_by_group: Some(false),
        submission_count,
        note: None,
    }
}

#[test]
fn completed_homework_is_included_as_completed() {
    assert_eq!(
        judge(Some(1), Some("2026-09-30 23:59:59"), now()),
        HomeworkState::Completed
    );
    assert_eq!(judge(Some(3), None, now()), HomeworkState::Completed);
    assert_eq!(HomeworkState::Completed.group(), HomeworkGroup::Completed);
    assert_eq!(HomeworkState::Completed.label(), "已完成");
}

#[test]
fn marks_overdue_only_when_deadline_passed() {
    assert_eq!(
        judge(Some(0), Some("2026-09-27T23:59:00+08:00"), now()),
        HomeworkState::Overdue
    );
    assert_eq!(
        judge(Some(0), Some("2026-09-30T23:59:00+08:00"), now()),
        HomeworkState::Pending
    );
    // 沒有截止時間時不標逾期。
    assert_eq!(judge(Some(0), None, now()), HomeworkState::Pending);
    // 無法確認提交狀態時標「待核实」，且不受截止時間影響。
    assert_eq!(
        judge(None, Some("2026-09-01T00:00:00+08:00"), now()),
        HomeworkState::Unknown
    );
}

#[test]
fn judges_overdue_by_instant_across_time_offsets() {
    // now 固定為 2026-09-28T12:00:00+08:00；UTC 截止時間以絕對瞬間比較。
    assert_eq!(
        judge(Some(0), Some("2026-09-28T15:59:00Z"), now()),
        HomeworkState::Pending,
        "15:59Z 即 23:59+08:00，尚未逾期"
    );
    assert_eq!(
        judge(Some(0), Some("2026-09-28T01:00:00Z"), now()),
        HomeworkState::Overdue,
        "01:00Z 即 09:00+08:00，已逾期"
    );
    assert_eq!(
        judge(Some(0), Some("2026-09-28T04:00:00Z"), now()),
        HomeworkState::Pending,
        "与 now 同一瞬间（12:00+08:00）不算逾期"
    );
}

#[test]
fn aggregates_by_group_and_deadline() {
    // 混用 UTC（`Z`）與 +08:00 字串：排序依絕對瞬間。
    let items = vec![
        input("无截止时间", None, Some(0)),
        input("已提交", Some("2026-09-30T23:59:00+08:00"), Some(2)),
        input("逾期作业", Some("2026-09-26T15:59:00Z"), Some(0)),
        input("即将到期", Some("2026-09-29T15:59:00Z"), Some(0)),
        input("待核实作业", Some("2026-09-30T23:59:00+08:00"), None),
    ];

    let result = aggregate(&items, now());
    let titles: Vec<&str> = result.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["逾期作业", "即将到期", "无截止时间", "已提交", "待核实作业"],
        "未完成在前（截止时间升序、无截止最后），其次已完成，最后待核实"
    );
    assert!(result[0].is_overdue());
    assert_eq!(result[2].state, HomeworkState::Pending);
    assert_eq!(result[3].state, HomeworkState::Completed);
    assert_eq!(result[4].state, HomeworkState::Unknown);
    assert_eq!(result[4].state.label(), "待核实");
}

#[test]
fn ties_break_by_course_title_and_activity() {
    let mut first = input("同名作业", Some("2026-09-30T23:59:00+08:00"), Some(0));
    first.course_name = "编译原理".to_owned();
    let mut second = input("同名作业", Some("2026-09-30T23:59:00+08:00"), Some(0));
    second.course_name = "操作系统".to_owned();
    let result = aggregate(&[second, first], now());
    let courses: Vec<&str> = result
        .iter()
        .map(|item| item.course_name.as_str())
        .collect();
    assert_eq!(
        courses,
        vec!["操作系统", "编译原理"],
        "同截止时间按课程稳定排序"
    );

    let items = vec![
        input("B 作业", Some("2026-09-30T23:59:00+08:00"), Some(0)),
        input("A 作业", Some("2026-09-30T23:59:00+08:00"), Some(0)),
    ];
    let result = aggregate(&items, now());
    let titles: Vec<&str> = result.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(titles, vec!["A 作业", "B 作业"], "再按标题稳定排序");
}

#[test]
fn carries_unknown_reason_note() {
    let mut item = input("待核实作业", None, None);
    item.note = Some("会话已过期".to_owned());
    let result = aggregate(&[item], now());
    assert_eq!(result[0].note.as_deref(), Some("会话已过期"));
}

#[test]
fn groups_cycle_in_display_order() {
    assert_eq!(HomeworkGroup::Unfinished.next(), HomeworkGroup::Completed);
    assert_eq!(HomeworkGroup::Completed.next(), HomeworkGroup::Unknown);
    assert_eq!(HomeworkGroup::Unknown.next(), HomeworkGroup::Unfinished);
    assert_eq!(HomeworkGroup::Unknown.previous(), HomeworkGroup::Completed);
    assert_eq!(HomeworkGroup::Unfinished.index(), 0);
    assert_eq!(HomeworkState::Overdue.group(), HomeworkGroup::Unfinished);
    assert_eq!(HomeworkGroup::ALL.len(), 3);
}

#[test]
fn parses_lms_time_formats() {
    let offset = CAMPUS_UTC_OFFSET_SECS;
    assert_eq!(
        parse_time(Some("2026-09-28T23:59:00+08:00")).map(|time| time.offset().local_minus_utc()),
        Some(offset)
    );
    assert_eq!(
        parse_time(Some("2026-09-28 23:59:00")).map(|time| time.offset().local_minus_utc()),
        Some(offset)
    );
    assert!(parse_time(Some("2026-09-28T23:59:00")).is_some());
    assert_eq!(parse_time(Some("  ")), None);
    assert_eq!(parse_time(None), None);
    assert_eq!(parse_time(Some("看不懂的时间")), None);
}

#[test]
fn normalizes_utc_lms_times_to_school_offset() {
    // 思源學堂以 UTC（`Z` 或 `+00:00`）傳送時間；解析結果一律換算為 +08:00。
    for value in ["2026-10-12T15:59:59.000Z", "2026-10-12T15:59:59+00:00"] {
        let time = parse_time(Some(value)).expect("应可解析");
        assert_eq!(
            time.offset().local_minus_utc(),
            CAMPUS_UTC_OFFSET_SECS,
            "{value}"
        );
        assert_eq!(
            time.format("%Y-%m-%d %H:%M").to_string(),
            "2026-10-12 23:59",
            "{value}"
        );
    }
    // 有無毫秒、`Z` 或 `+00:00` 都不得改變瞬間。
    assert_eq!(
        parse_time(Some("2026-10-12T15:59:59Z")).map(|time| time.timestamp()),
        parse_time(Some("2026-10-12T15:59:59.000Z")).map(|time| time.timestamp())
    );
}
