//! 顯示寬度與欄位對齊工具。
//!
//! 終端欄寬與字元數不同（全形字佔 2 欄），列表欄位排版一律以顯示寬度計算。
//! 這些工具與終端繪製無關，因此獨立於 [`crate::tui`]：文件排版（[`crate::privacy`]）
//! 與介面共用同一套寬度語意，避免兩處算法漂移。

use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation as _;

/// 文字的終端顯示寬度（全形字以 2 欄計）。
pub fn display_width(text: &str) -> usize {
    Line::from(text).width()
}

/// 依顯示寬度截斷；超長時保留上限內並以「…」結尾，保證不切出半個全形字。
pub fn truncate_display(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_owned();
    }
    let budget = max_width - display_width("…");
    let mut result = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if width + grapheme_width > budget {
            break;
        }
        result.push_str(grapheme);
        width += grapheme_width;
    }
    result.push('…');
    result
}

/// 依顯示寬度補齊尾端空白（不截斷，超長原樣返回）。
pub fn pad_display(text: &str, width: usize) -> String {
    let current = display_width(text);
    let mut result = String::with_capacity(text.len() + width.saturating_sub(current));
    result.push_str(text);
    for _ in current..width {
        result.push(' ');
    }
    result
}

/// 依顯示寬度切成「前 `columns` 欄」與其餘兩段（不切開字素）。
///
/// 用於把一列拆成標籤與值兩個區段：邊界落在寬字元中間時，整個字素歸入前段。
pub fn split_at_display(text: &str, columns: usize) -> (&str, &str) {
    if columns == 0 {
        return ("", text);
    }
    let mut width = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        if width >= columns {
            return text.split_at(offset);
        }
        width += display_width(grapheme);
    }
    (text, "")
}

/// 截斷並補齊到固定顯示寬度（列表欄位排版用；`width` 為 0 時為空字串）。
pub fn fit_display(text: &str, width: usize) -> String {
    let truncated = truncate_display(text, width);
    pad_display(&truncated, width)
}

/// 依顯示寬度補齊首端空白（靠右對齊；不截斷，超長原樣返回）。
pub fn pad_display_start(text: &str, width: usize) -> String {
    let current = display_width(text);
    let mut result = String::with_capacity(text.len() + width.saturating_sub(current));
    for _ in current..width {
        result.push(' ');
    }
    result.push_str(text);
    result
}

/// 截斷並靠右補齊到固定顯示寬度（列表欄位排版用；`width` 為 0 時為空字串）。
pub fn fit_display_start(text: &str, width: usize) -> String {
    let truncated = truncate_display(text, width);
    pad_display_start(&truncated, width)
}

/// 依顯示寬度貪婪換行（空白處斷行優先，過長的詞逐字素硬切）。
///
/// 行首與行尾不留空白、連續空白視為一個空格；`width` 為 0 時回傳空 vec
/// （沒有可顯示的欄位）。空字串回傳單一空行，讓呼叫端維持固定的列數。
///
/// 只有 ASCII 空白會被折疊成斷行點：其他 Unicode 空白（全形空格 `\u{3000}`、
/// 不斷行空格等）視為一般字元，依實際顯示寬度計算——中文排版常以它們做縮排與
/// 對齊，折成半形會使版面走樣。
///
/// 這是預先排版用的工具：需要精確控制「內容佔幾列」（例如可捲動的詳情面板）
/// 時，必須自行換行，不能依賴繪製端的 `Wrap`——後者無法回報實際列數。
pub fn wrap_display(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0_usize;
    // 本行最後一個可斷行處（`current` 的位元組位移）：只在空白之後成立。
    let mut break_index: Option<usize> = None;
    // 下一個非空白字素前需要補一個空白（詞間分隔）。
    let mut pending_space = false;

    for grapheme in text.graphemes(true) {
        if grapheme
            .chars()
            .all(|character| character.is_ascii_whitespace())
        {
            if !current.is_empty() {
                break_index = Some(current.len());
                pending_space = true;
            }
            continue;
        }

        let grapheme_width = display_width(grapheme);
        let needed = grapheme_width + usize::from(pending_space);
        if current_width + needed > width && !current.is_empty() {
            match break_index {
                Some(index) => {
                    // 斷點之後的部分（含詞間空格）移到下一行。
                    let tail = current.split_off(index);
                    lines.push(current.trim_end().to_owned());
                    current = tail.trim_start().to_owned();
                    current_width = display_width(&current);
                }
                None => {
                    // 單一詞超出一列：硬切（不切開字素）。
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
            }
            break_index = None;
            pending_space = false;
        }

        if pending_space {
            current.push(' ');
            current_width += 1;
            pending_space = false;
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
#[path = "tests/text_test.rs"]
mod text_test;
