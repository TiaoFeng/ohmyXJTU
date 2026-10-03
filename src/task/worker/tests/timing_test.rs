//! 作業載入分階段計時的單元測試。

use std::time::{Duration, Instant};

use super::{LoadTiming, Phase};

/// 模擬一個階段：以過去的起點取樣，`delay` 即為該階段的耗時。
fn record_after(timing: &mut LoadTiming, phase: Phase, delay: Duration, items: usize) {
    let started = Instant::now()
        .checked_sub(delay)
        .unwrap_or_else(Instant::now);
    timing.record(phase, started, items);
}

#[test]
fn disabled_timing_produces_no_report() {
    let mut timing = LoadTiming::default();
    timing.begin(false);
    record_after(&mut timing, Phase::Login, Duration::from_secs(3), 1);
    assert!(timing.take_report().is_none(), "未启用时不应输出报告");
    assert!(timing.is_idle(), "取用后应重设");
    assert!(timing.take_report().is_none());
}

#[test]
fn report_lists_every_phase_with_item_counts() {
    let mut timing = LoadTiming::default();
    timing.begin(true);
    record_after(&mut timing, Phase::Login, Duration::from_secs(3), 1);
    record_after(&mut timing, Phase::Term, Duration::from_millis(200), 1);
    record_after(&mut timing, Phase::Courses, Duration::from_millis(100), 1);
    record_after(
        &mut timing,
        Phase::Activities,
        Duration::from_millis(500),
        2,
    );
    record_after(&mut timing, Phase::Detail, Duration::from_secs(2), 3);
    record_after(
        &mut timing,
        Phase::Submission,
        Duration::from_millis(400),
        5,
    );
    timing.note_first_item();
    timing.note_first_shown();
    for _ in 0..4 {
        timing.note_hit();
    }
    timing.note_list_item(true, true, false);
    timing.note_list_item(true, false, true);
    timing.note_list_item(false, false, false);

    let report = timing.take_report().expect("启用时应输出报告");
    assert!(report.starts_with("计时 总"), "报告前缀不符：{report}");
    for label in [
        "登录",
        "学期",
        "课程",
        "活动",
        "/2",
        "详情",
        "提交",
        "首项",
        "首显",
        "命中4",
        "作业3[组2 数1 ID1]",
    ] {
        assert!(report.contains(label), "报告缺少 {label}：{report}");
    }
    assert!(timing.is_idle(), "报告输出后应重设");
}

#[test]
fn first_item_is_recorded_only_once() {
    let mut timing = LoadTiming::default();
    timing.begin(true);
    timing.note_first_item();
    let first = timing.first_item;
    assert!(first.is_some(), "第一项应先被记录");
    timing.note_first_item();
    assert_eq!(timing.first_item, first, "重复记录不应被覆写");
}

#[test]
fn first_shown_is_recorded_only_once() {
    let mut timing = LoadTiming::default();
    timing.begin(true);
    timing.note_first_shown();
    let first = timing.first_shown;
    assert!(first.is_some(), "首显应先被记录");
    timing.note_first_shown();
    assert_eq!(timing.first_shown, first, "重复记录不应被覆写");
}

#[test]
fn abandon_discards_the_report() {
    let mut timing = LoadTiming::default();
    timing.begin(true);
    record_after(&mut timing, Phase::Detail, Duration::from_secs(1), 1);
    timing.abandon();
    assert!(timing.is_idle());
    assert!(timing.take_report().is_none());
}

#[test]
fn recording_without_a_running_timing_is_harmless() {
    let mut timing = LoadTiming::default();
    // 沒有進行中的計時（例如其他頁面觸發登入）時取樣不得 panic，
    // 也不得讓下一次載入的報告混入舊資料。
    record_after(&mut timing, Phase::Login, Duration::from_secs(5), 1);
    timing.note_first_item();
    timing.note_hit();
    timing.begin(true);
    let report = timing.take_report().expect("启用时应输出报告");
    assert!(report.contains("登录0.0s"), "旧取样不应残留：{report}");
    assert!(report.contains("命中0"), "旧命中不应残留：{report}");
    assert!(report.contains("首项-"), "旧首项不应残留：{report}");
    assert!(
        report.contains("作业0[组0 数0 ID0]"),
        "旧列表计数不应残留：{report}"
    );
}
