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

#[cfg(test)]
#[path = "tests/text_test.rs"]
mod text_test;
