//! 輸入框測試：字素層級編輯、遮罩與水平捲動。

use super::*;
use crate::tui::ui::input_window;

#[test]
fn debug_is_masked_and_clear_empties() {
    let mut line = InputLine::with_value("secret-password");
    let debug = format!("{line:?}");
    assert!(
        !debug.contains("secret-password"),
        "Debug 不得洩漏輸入內容：{debug}"
    );
    assert!(debug.contains("len: 15"), "Debug 可顯示長度：{debug}");

    line.clear();
    assert!(line.is_empty());
    assert_eq!(line.value(), "");
    assert_eq!(line.cursor(), 0);
}

#[test]
fn edits_by_grapheme() {
    let mut line = InputLine::new();
    for character in "中文abc".chars() {
        line.insert(character);
    }
    assert_eq!(line.value(), "中文abc");
    assert_eq!(line.len(), 5);
    assert_eq!(line.cursor(), 5);

    // 刪掉游標前的 'c'，而不是切掉半個中文字。
    assert!(line.backspace());
    assert_eq!(line.value(), "中文ab");

    line.move_home();
    assert!(line.move_right());
    // 游標在「文」之前，delete 刪掉的是游標右側的字素。
    assert!(line.delete());
    assert_eq!(line.value(), "中ab");
    assert_eq!(line.cursor(), 1);
}

#[test]
fn inserts_in_the_middle() {
    let mut line = InputLine::with_value("ab");
    line.move_left();
    line.insert('中');
    assert_eq!(line.value(), "a中b");
    assert_eq!(line.cursor(), 2);
}

#[test]
fn masks_masked_content() {
    let mut line = InputLine::new().masked(true);
    for character in "abc".chars() {
        line.insert(character);
    }
    assert!(line.is_masked());
    assert_eq!(line.value(), "abc");
    assert_eq!(line.display_graphemes(), vec!["•", "•", "•"]);

    line.clear();
    assert!(line.is_empty());
    assert_eq!(line.cursor(), 0);
    assert!(!line.backspace());
    assert!(!line.delete());
}

#[test]
fn scrolls_to_keep_cursor_visible() {
    let mut line = InputLine::with_value("0123456789");
    let (visible, column) = input_window(&line, 4);
    assert_eq!(visible, "789");
    // "789" 之後是游標位置（3 個半角字元）。
    assert_eq!(column, 3);

    line.move_home();
    let (visible, column) = input_window(&line, 4);
    assert_eq!(visible, "0123");
    assert_eq!(column, 0);
}

#[test]
fn counts_wide_characters_for_cursor() {
    let mut line = InputLine::with_value("中文");
    let (visible, column) = input_window(&line, 8);
    assert_eq!(visible, "中文");
    // 中文為全角，游標應落在第 4 欄。
    assert_eq!(column, 4);

    line.move_home();
    let (_, column) = input_window(&line, 8);
    assert_eq!(column, 0);
}

#[test]
fn zero_width_window_is_safe() {
    let line = InputLine::with_value("abc");
    assert_eq!(input_window(&line, 0), (String::new(), 0));
}

#[test]
fn combining_character_keeps_cursor_on_one_grapheme() {
    let mut line = InputLine::new();
    line.insert('a');
    // U+0301 組合重音：與前一個 'a' 合併為單一字素。
    line.insert('\u{0301}');
    assert_eq!(line.value(), "a\u{0301}");
    assert_eq!(line.len(), 1, "組合字元不應自成一個字素");
    assert_eq!(line.cursor(), 1, "游標不應越過合併後的字素");

    // 游標在尾端：backspace 應刪掉整個字素，而不是看似無效。
    assert!(line.backspace(), "組合字元後仍應能刪除");
    assert_eq!(line.value(), "");
    assert_eq!(line.cursor(), 0);
}

#[test]
fn set_replaces_content_and_moves_cursor_to_end() {
    let mut line = InputLine::with_value("old");
    line.set("new-value");
    assert_eq!(line.value(), "new-value");
    assert_eq!(line.cursor(), line.len());
}

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
fn scrolls_by_display_width_for_wide_characters() {
    // 8 欄的輸入區：游標在尾端時可見內容不得超過 8 欄。按字素計數的舊寫法
    // 會保留 8 個全形字＝16 欄，把游標一起推出輸入框。
    let mut line = InputLine::with_value("中文中文中文");
    let (visible, column) = input_window(&line, 8);
    assert!(
        display_width(&visible) <= 8,
        "可見內容不得超出輸入區：{visible}（{} 欄）",
        display_width(&visible)
    );
    assert_eq!(visible, "文中文");
    assert_eq!(
        usize::from(column),
        display_width(&visible),
        "游標應緊接在可見內容之後"
    );

    // 游標在開頭時可放滿四欄全形字。
    line.move_home();
    let (visible, column) = input_window(&line, 8);
    assert_eq!(visible, "中文中文");
    assert_eq!(column, 0);
}

#[test]
fn keeps_the_cursor_inside_a_narrow_field() {
    // 極窄輸入區（1 欄）連一個全形字都放不下：至少顯示最接近游標的字素，
    // 而不是讓使用者面對一片空白。
    let mut line = InputLine::with_value("中");
    line.move_home();
    let (visible, column) = input_window(&line, 1);
    assert_eq!(visible, "中");
    assert_eq!(column, 0, "游標在字素起點");

    // 游標在尾端時改顯示游標前的那一個字素。
    line.move_end();
    let (visible, _column) = input_window(&line, 1);
    assert_eq!(visible, "中");

    // 游標在中間時，游標前的欄位數仍以顯示寬度計算。
    let mut line = InputLine::with_value("中a文b");
    line.move_home();
    line.move_right();
    let (visible, column) = input_window(&line, 5);
    assert_eq!(column, 2, "游標前的全形字佔兩欄");
    assert!(
        display_width(&visible) <= 5,
        "可見內容不得超出輸入區：{visible}"
    );
    assert_eq!(visible, "中a文");
}
