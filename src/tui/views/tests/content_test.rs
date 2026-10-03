//! 內容面板的文字格式化與寬度契約測試。

use super::*;

use crate::domain::homework::HomeworkState;
use crate::text::display_width;
use crate::tui::views::content::columns::homework_min_row_width;

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

/// 作業詳情面板的可用寬度必須容得下狀態列。
///
/// 詳情面板與清單同寬（同一個垂直分割），且狀態列不預先換行（多種顏色組成，
/// 折行會散開），因此一旦比面板寬就會被直接裁掉——`homework_lines` 的
/// `debug_assert!` 就是這個不變式。這裡把「清單寬度下限 → 面板內寬 → 最寬
/// 狀態列」的關係固定下來：調整任何一邊都會在此失敗，而不是等到 debug 建置
/// 繪製時才 panic。
#[test]
fn homework_detail_panel_fits_the_widest_status_line() {
    // 清單扣掉外框左右欄線與選取列的高亮符號（3 欄）；面板只扣外框（2 欄），
    // 因此面板內寬 = 清單寬度下限 − 1。
    let panel_width = homework_min_row_width() - 1;
    // 「　提交单位：小组」是提交單位三種寫法中最寬的一種（其餘同寬）。
    for state in [
        HomeworkState::Pending,
        HomeworkState::Overdue,
        HomeworkState::Completed,
        HomeworkState::Unknown,
    ] {
        let line = format!("状态：{}{}", state.label(), "　提交单位：小组");
        assert!(
            display_width(&line) <= panel_width,
            "状态列「{line}」宽 {} 超过最小面板宽度 {panel_width}",
            display_width(&line)
        );
    }
}
