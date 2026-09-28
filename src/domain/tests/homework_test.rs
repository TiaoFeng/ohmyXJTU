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
        submit_by_group: false,
        submission_count,
    }
}

#[test]
fn skips_submitted_homework() {
    assert_eq!(judge(Some(1), Some("2026-09-30 23:59:59"), now()), None);
    assert_eq!(judge(Some(3), None, now()), None);
}

#[test]
fn marks_overdue_only_when_deadline_passed() {
    assert_eq!(
        judge(Some(0), Some("2026-09-27T23:59:00+08:00"), now()),
        Some(HomeworkState::Overdue)
    );
    assert_eq!(
        judge(Some(0), Some("2026-09-30T23:59:00+08:00"), now()),
        Some(HomeworkState::Pending)
    );
    // 沒有截止時間時不標逾期。
    assert_eq!(judge(Some(0), None, now()), Some(HomeworkState::Pending));
    // 無法確認提交狀態時標「待核实」，且不受截止時間影響。
    assert_eq!(
        judge(None, Some("2026-09-01T00:00:00+08:00"), now()),
        Some(HomeworkState::Unknown)
    );
}

#[test]
fn aggregates_and_sorts_by_deadline() {
    let items = vec![
        input("无截止时间", None, Some(0)),
        input("已提交", Some("2026-09-30T23:59:00+08:00"), Some(2)),
        input("逾期作业", "2026-09-26T23:59:00+08:00".into(), Some(0)),
        input("即将到期", Some("2026-09-29T23:59:00+08:00"), Some(0)),
        input("待核实作业", Some("2026-09-30T23:59:00+08:00"), None),
    ];

    let result = aggregate(&items, now());
    let titles: Vec<&str> = result.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["逾期作业", "即将到期", "待核实作业", "无截止时间"],
        "应依截止时间排序，无截止时间者最后"
    );
    assert!(result[0].is_overdue());
    assert_eq!(result[2].state, HomeworkState::Unknown);
    assert_eq!(result[2].state.label(), "待核实");
}

#[test]
fn parses_lms_time_formats() {
    let offset = 8 * 3600;
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
