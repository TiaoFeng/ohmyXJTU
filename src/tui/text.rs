//! 單行文字輸入。
//!
//! 游標以「字素」為單位移動，因此中文與表情符號都不會被切成半個字；
//! 密碼欄位以 [`InputLine::masked`] 標記，繪製時統一以 `•` 呈現。

use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation as _;

/// 遮罩字元。
pub const MASK_CHAR: char = '•';

/// 單行輸入框的內容與游標。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputLine {
    value: String,
    cursor: usize,
    masked: bool,
}

impl InputLine {
    /// 建立空輸入框。
    pub fn new() -> Self {
        Self::default()
    }

    /// 設定初值（游標移到尾端）。
    pub fn with_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = grapheme_len(&value);
        Self {
            value,
            cursor,
            masked: false,
        }
    }

    /// 標記為密碼欄位。
    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// 是否為密碼欄位。
    pub fn is_masked(&self) -> bool {
        self.masked
    }

    /// 目前內容。
    pub fn value(&self) -> &str {
        &self.value
    }

    /// 是否為空。
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// 游標位置（字素索引）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 內容的字素長度。
    pub fn len(&self) -> usize {
        grapheme_len(&self.value)
    }

    /// 清空內容。
    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// 取代全部內容。
    pub fn set(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = grapheme_len(&self.value);
    }

    /// 在游標處插入字元。
    pub fn insert(&mut self, character: char) {
        let byte = grapheme_byte_index(&self.value, self.cursor);
        self.value.insert(byte, character);
        self.cursor += 1;
    }

    /// 刪除游標前的字素。
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = grapheme_byte_index(&self.value, self.cursor - 1);
        let end = grapheme_byte_index(&self.value, self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    /// 刪除游標後的字素。
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.len() {
            return false;
        }
        let start = grapheme_byte_index(&self.value, self.cursor);
        let end = grapheme_byte_index(&self.value, self.cursor + 1);
        self.value.replace_range(start..end, "");
        true
    }

    /// 游標左移。
    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// 游標右移。
    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// 游標移到開頭。
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// 游標移到尾端。
    pub fn move_end(&mut self) {
        self.cursor = self.len();
    }

    /// 繪製用文字（密碼欄位以遮罩字元取代）。
    pub fn display_graphemes(&self) -> Vec<String> {
        let graphemes: Vec<&str> = self.value.graphemes(true).collect();
        if self.masked {
            graphemes.iter().map(|_| MASK_CHAR.to_string()).collect()
        } else {
            graphemes.iter().map(|value| (*value).to_owned()).collect()
        }
    }
}

fn grapheme_len(value: &str) -> usize {
    value.graphemes(true).count()
}

/// 第 `index` 個字素的位元組位移。
fn grapheme_byte_index(value: &str, index: usize) -> usize {
    value
        .grapheme_indices(true)
        .nth(index)
        .map_or(value.len(), |(offset, _)| offset)
}

// ── 顯示寬度工具（列表欄位排版用） ─────────────────────

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

/// 截斷並補齊到固定顯示寬度（列表欄位排版用；`width` 為 0 時為空字串）。
pub fn fit_display(text: &str, width: usize) -> String {
    let truncated = truncate_display(text, width);
    pad_display(&truncated, width)
}

#[cfg(test)]
#[path = "tests/text_test.rs"]
mod text_test;
