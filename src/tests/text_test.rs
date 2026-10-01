//! 顯示寬度與欄位對齊工具測試：寬度計算、截斷、補白與切分。

use super::*;

#[test]
fn measures_display_width() {
    assert_eq!(display_width("abc"), 3);
    assert_eq!(display_width("中文"), 4);
    assert_eq!(display_width("中a"), 3);
    assert_eq!(display_width(""), 0);
}

#[test]
fn truncates_by_display_width() {
    assert_eq!(truncate_display("abc", 3), "abc");
    assert_eq!(truncate_display("中文", 4), "中文");
    assert_eq!(truncate_display("中文中文", 5), "中文…");
    assert_eq!(display_width(&truncate_display("中文中文", 5)), 5);
    assert_eq!(truncate_display("abc", 1), "…");
    assert_eq!(truncate_display("ab", 0), "");
}

#[test]
fn pads_and_fits_by_display_width() {
    assert_eq!(pad_display("中文", 6), "中文  ");
    assert_eq!(pad_display("中文", 4), "中文");
    assert_eq!(pad_display("abcdef", 3), "abcdef");
    assert_eq!(fit_display("中文中文中文", 9), "中文中文…");
    assert_eq!(fit_display("中文中文中文", 8), "中文中… ");
}

#[test]
fn fits_start_aligned_columns_by_display_width() {
    assert_eq!(pad_display_start("1-2", 5), "  1-2");
    assert_eq!(pad_display_start("11-12", 5), "11-12");
    assert_eq!(pad_display_start("中文", 6), "  中文");
    assert_eq!(fit_display_start("1-2", 5), "  1-2");
    assert_eq!(fit_display_start("中文中文", 5), "中文…");
    assert_eq!(display_width(&fit_display_start("中文中文", 5)), 5);
    assert_eq!(fit_display_start("abc", 0), "");
}

#[test]
fn splits_a_line_by_display_width() {
    // ASCII：欄數即字元數。
    assert_eq!(split_at_display("abcdef", 3), ("abc", "def"));

    // 中文：欄數以顯示寬度計算（「用途」佔 4 欄），且不切開字素。
    let text = "  用途        ：统一身份认证";
    let (label, value) = split_at_display(text, 16);
    assert_eq!(label, "  用途        ：");
    assert_eq!(display_width(label), 16);
    assert_eq!(value, "统一身份认证");

    // 邊界情形：0 欄與超過整列寬度。
    assert_eq!(split_at_display("abc", 0), ("", "abc"));
    assert_eq!(split_at_display("abc", 9), ("abc", ""));
}

#[test]
fn split_at_display_never_cuts_a_grapheme() {
    // 要求 1 欄，但「中」佔 2 欄：整個字素歸入前段。
    assert_eq!(split_at_display("中文", 1), ("中", "文"));

    // 組合字元自成一個字素，不會被拆成 e 與重音。
    let text = "e\u{301}xyz";
    assert_eq!(split_at_display(text, 1), ("e\u{301}", "xyz"));
}
