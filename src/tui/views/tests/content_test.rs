//! 截止時間與提交時間的文字格式化測試。

use super::*;

#[test]
fn deadline_labels_normalize_to_school_time() {
    // API 以 UTC（`Z`／`+00:00`）傳送，顯示必須換算為 +08:00。
    for value in ["2026-10-12T15:59:59.000Z", "2026-10-12T15:59:59+00:00"] {
        assert_eq!(
            deadline_list_label(Some(value), false),
            "2026-10-12 23:59",
            "{value}"
        );
        assert_eq!(
            deadline_list_label(Some(value), true),
            "10-12 23:59",
            "{value}"
        );
    }
    // 已是 +08:00 或未帶時區者維持原樣（未帶時區視為 +08:00）。
    assert_eq!(
        deadline_list_label(Some("2026-10-12T23:59:00+08:00"), false),
        "2026-10-12 23:59"
    );
    assert_eq!(
        deadline_list_label(Some("2026-10-12 23:59:00"), false),
        "2026-10-12 23:59"
    );
    // 無法解析時回退原始字串；缺值顯示固定文案。
    assert_eq!(deadline_list_label(Some("时间待定"), false), "时间待定");
    assert_eq!(deadline_list_label(None, false), "无截止时间");
}

#[test]
fn submission_time_labels_normalize_to_school_time() {
    assert_eq!(
        submission_time_label(Some("2026-09-20T02:00:00Z")),
        "2026-09-20 10:00"
    );
    assert_eq!(
        submission_time_label(Some("2026-09-21 09:30:00")),
        "2026-09-21 09:30"
    );
    assert_eq!(
        submission_time_label(Some(" 2026-09-22T00:00:00+00:00 ")),
        "2026-09-22 08:00"
    );
    assert_eq!(submission_time_label(None), "未知时间");
    assert_eq!(submission_time_label(Some("稍后公布")), "稍后公布");
}
