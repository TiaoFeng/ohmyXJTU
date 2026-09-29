//! 主題語意色測試：狀態標籤的顏色映射必須穩定且可辨。

use super::*;

#[test]
fn flow_states_have_distinct_colors() {
    let effective = THEME.status_style("有效").fg;
    let unmatched = THEME.status_style("未匹配").fg;
    assert_eq!(effective, Some(THEME.green), "有效應為成功綠");
    assert_eq!(unmatched, Some(THEME.yellow), "未匹配應為警告黃");
    assert_ne!(effective, unmatched, "兩種流水狀態不得同色");
}

#[test]
fn known_states_keep_their_semantic_colors() {
    assert_eq!(THEME.status_style("正常").fg, Some(THEME.green));
    assert_eq!(THEME.status_style("已完成").fg, Some(THEME.green));
    assert_eq!(THEME.status_style("迟到").fg, Some(THEME.yellow));
    assert_eq!(THEME.status_style("待核实").fg, Some(THEME.yellow));
    assert_eq!(THEME.status_style("未知").fg, Some(THEME.yellow));
    assert_eq!(THEME.status_style("缺勤").fg, Some(THEME.red));
    assert_eq!(THEME.status_style("逾期").fg, Some(THEME.red));
    assert_eq!(THEME.status_style("请假").fg, Some(THEME.blue));
    assert_eq!(THEME.status_style("待考勤").fg, Some(THEME.accent));
    assert_eq!(THEME.status_style("待提交").fg, Some(THEME.accent));
    assert_eq!(THEME.status_style("不考勤").fg, Some(THEME.muted));
    assert_eq!(THEME.status_style("未映射的标签").fg, Some(THEME.yellow));
}
