//! 輸入框測試：字素層級編輯、遮罩與水平捲動。
//!
//! 顯示寬度與欄位對齊工具的測試見 `src/tests/text_test.rs`。

use super::*;
use crate::text::display_width;
use crate::tui::ui::input_window;

#[test]
fn debug_is_masked_and_clear_empties() {
    let mut line = InputLine::with_value("secret-password");
    let debug = format!("{line:?}");
    assert!(
        !debug.contains("secret-password"),
        "Debug 不得泄漏输入内容：{debug}"
    );
    assert!(debug.contains("len: 15"), "Debug 可显示长度：{debug}");

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
    assert_eq!(line.len(), 1, "组合字符不应自成一个字素");
    assert_eq!(line.cursor(), 1, "光标不应越过合并后的字素");

    // 游標在尾端：backspace 應刪掉整個字素，而不是看似無效。
    assert!(line.backspace(), "组合字符后仍应能删除");
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
fn scrolls_by_display_width_for_wide_characters() {
    // 8 欄的輸入區：游標在尾端時可見內容不得超過 8 欄。按字素計數的舊寫法
    // 會保留 8 個全形字＝16 欄，把游標一起推出輸入框。
    let mut line = InputLine::with_value("中文中文中文");
    let (visible, column) = input_window(&line, 8);
    assert!(
        display_width(&visible) <= 8,
        "可见内容不得超出输入区：{visible}（{} 栏）",
        display_width(&visible)
    );
    assert_eq!(visible, "文中文");
    assert_eq!(
        usize::from(column),
        display_width(&visible),
        "光标应紧接在可见内容之后"
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
    assert_eq!(column, 0, "光标在字素起点");

    // 游標在尾端時改顯示游標前的那一個字素。
    line.move_end();
    let (visible, _column) = input_window(&line, 1);
    assert_eq!(visible, "中");

    // 游標在中間時，游標前的欄位數仍以顯示寬度計算。
    let mut line = InputLine::with_value("中a文b");
    line.move_home();
    line.move_right();
    let (visible, column) = input_window(&line, 5);
    assert_eq!(column, 2, "光标前的全形字占两栏");
    assert!(
        display_width(&visible) <= 5,
        "可见内容不得超出输入区：{visible}"
    );
    assert_eq!(visible, "中a文");
}

// ── 多行描述輸入框 ───────────────────────────────────────

#[test]
fn text_area_splits_and_joins_lines() {
    let mut area = TextArea::new("");
    assert!(area.is_empty());
    assert_eq!(area.value(), "");
    assert_eq!(area.line_count(), 1, "空内容仍有一行");

    for character in "第一行".chars() {
        area.insert(character);
    }
    assert_eq!(area.value(), "第一行");
    assert_eq!(area.row(), 0, "单行时游标在第一行");

    // 在游標處（尾端）換行，游標移到新的一行。
    area.insert('\n');
    assert_eq!(area.line_count(), 2);
    assert_eq!(area.row(), 1);
    for character in "第二行".chars() {
        area.insert(character);
    }
    assert_eq!(area.value(), "第一行\n第二行");

    // 行首退格：與上一行合併，游標停在接縫處。
    area.move_home();
    area.backspace();
    assert_eq!(area.value(), "第一行第二行");
    assert_eq!(area.line_count(), 1);
    assert_eq!(area.line(0).expect("第一行仍存在").value(), "第一行第二行");
    assert_eq!(area.focused_line().cursor(), 3, "游标应停在合并处");

    // 行尾刪除沒有任何效果（已是最後一行）。
    area.move_end();
    let before = area.value();
    area.delete();
    assert_eq!(area.value(), before);
}

#[test]
fn text_area_edits_across_lines_with_arrows() {
    let mut area = TextArea::new("甲\n乙");
    assert_eq!(area.line_count(), 2);
    assert_eq!(area.row(), 1, "以初值建立时游标在最后一行");
    assert!(area.line(0).is_some_and(|line| line.value() == "甲"));
    assert!(area.line(1).is_some_and(|line| line.value() == "乙"));
    assert!(area.line(2).is_none());

    // 上移：游標回到第一行的同一欄位（越界時夾取）。
    area.move_up();
    assert_eq!(area.row(), 0);
    assert_eq!(area.focused_line().cursor(), 1, "应夹到该行长度");
    area.move_down();
    assert_eq!(area.row(), 1);

    // 行首左移會接到上一行尾端；行尾右移會接到下一行開頭。
    area.move_home();
    area.move_left();
    assert_eq!(area.row(), 0, "行首左移应移到上一行");
    assert_eq!(area.focused_line().cursor(), 1, "并停在上一行尾端");
    area.move_right();
    assert_eq!(area.row(), 1, "行尾右移应移到下一行");
    assert_eq!(area.focused_line().cursor(), 0, "并停在下一行开头");

    // 行尾刪除會把下一行併入（在第 0 行的行尾）。
    area.move_up();
    area.move_end();
    area.delete();
    assert_eq!(area.value(), "甲乙");
    assert_eq!(area.line_count(), 1);

    // 邊界不越界：首行行首退格、末行行尾刪除都是空操作。
    let mut boundary = TextArea::new("甲\n乙");
    boundary.move_up();
    boundary.move_home();
    boundary.backspace();
    assert_eq!(boundary.value(), "甲\n乙", "首行行首退格不得越界");
    assert_eq!(boundary.row(), 0);
    boundary.move_down();
    boundary.move_end();
    boundary.delete();
    assert_eq!(boundary.value(), "甲\n乙", "末行行尾删除不得越界");
    assert_eq!(boundary.row(), 1);
}

#[test]
fn text_area_window_follows_the_cursor() {
    let area = TextArea::new("一\n二\n三\n四\n五");
    assert_eq!(area.row(), 4);
    // 三行視窗、游標在最後一行：視窗往下移，讓游標行可見。
    let start = area.window_start(3);
    assert_eq!(start, 2, "视窗应滚动到游标所在行");

    // 空行（只有換行）也算一行，仍能取到值。
    let empty = TextArea::new("\n");
    assert_eq!(empty.line_count(), 2, "换行符切出两个空行");
    assert_eq!(empty.value(), "\n");
    assert!(empty.is_empty(), "两行都空时视为空内容");
}
