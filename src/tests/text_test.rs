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

#[test]
fn wraps_at_spaces_by_display_width() {
    assert_eq!(
        wrap_display("alpha beta gamma", 11),
        vec!["alpha beta", "gamma"]
    );
    // 連續空白折成一個空格；行首與行尾不留空白。
    assert_eq!(wrap_display("  a   b  ", 3), vec!["a b"]);
    assert_eq!(wrap_display("a b", 3), vec!["a b"]);
    assert_eq!(wrap_display("aa bbbb", 6), vec!["aa", "bbbb"]);
}

#[test]
fn wraps_long_words_by_grapheme() {
    // 沒有空白可斷：逐字素硬切。
    assert_eq!(wrap_display("abcdef", 3), vec!["abc", "def"]);
    // 組合字元不被拆開。
    assert_eq!(wrap_display("e\u{301}xyz", 2), vec!["e\u{301}x", "yz"]);
}

#[test]
fn wraps_cjk_by_display_width() {
    // 「中文」各佔 2 欄：寬度 4 只放得下兩個字。
    assert_eq!(
        wrap_display("中文中文中文", 4),
        vec!["中文", "中文", "中文"]
    );
    assert_eq!(wrap_display("中文 abc", 6), vec!["中文", "abc"]);
    // 單一字素就超過欄寬時仍自成一列（不得無窮迴圈）。
    assert_eq!(wrap_display("中文", 1), vec!["中", "文"]);
}

#[test]
fn wrap_display_keeps_line_count_stable() {
    assert_eq!(wrap_display("", 10), vec![""]);
    assert_eq!(wrap_display("   ", 10), vec![""]);
    assert_eq!(wrap_display("abcd", 10), vec!["abcd"]);
    assert_eq!(wrap_display("abcd", 4), vec!["abcd"]);
    assert!(wrap_display("abcd", 0).is_empty());
}

/// 非 ASCII 空白（全形空格、不斷行空格）是排版用的可見字元，不得折成半形空白。
///
/// 中文排版常以全形空格做縮排與對齊；折成半形會讓「类型：作业\u{3000}截止：…」
/// 這類由介面自行拼出的文字走樣。
#[test]
fn wrap_display_keeps_non_ascii_whitespace() {
    assert_eq!(
        wrap_display("类型：作业\u{3000}截止：2026-10-12 23:59", 60),
        vec!["类型：作业\u{3000}截止：2026-10-12 23:59"]
    );
    assert_eq!(
        wrap_display("\u{3000}\u{3000}第一章", 60),
        vec!["\u{3000}\u{3000}第一章"]
    );
    // 不斷行空格也不得成為斷行點。
    assert_eq!(wrap_display("分数\u{a0}10", 40), vec!["分数\u{a0}10"]);
    // ASCII 空白照舊折疊與斷行。
    assert_eq!(wrap_display("a \t b", 40), vec!["a b"]);
    assert_eq!(wrap_display("alpha beta", 6), vec!["alpha", "beta"]);
}
