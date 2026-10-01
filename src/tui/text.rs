//! 單行文字輸入。
//!
//! 游標以「字素」為單位移動，因此中文與表情符號都不會被切成半個字；
//! 密碼欄位以 [`InputLine::masked`] 標記，繪製時統一以 `•` 呈現。
//!
//! 顯示寬度與欄位對齊工具見 [`crate::text`]（與終端繪製無關，文件排版亦共用）。

use unicode_segmentation::UnicodeSegmentation as _;
use zeroize::Zeroize as _;

/// 遮罩字元。
pub const MASK_CHAR: char = '•';

/// 單行輸入框的內容與游標。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct InputLine {
    value: String,
    cursor: usize,
    masked: bool,
}

impl std::fmt::Debug for InputLine {
    /// 只輸出長度：任何 `{:?}` 都不會洩漏輸入內容（口令、密碼或帳號）。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputLine")
            .field("len", &self.len())
            .field("cursor", &self.cursor)
            .field("masked", &self.masked)
            .finish()
    }
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

    /// 清空內容（覆寫底層緩衝，不只截斷長度）。
    pub fn clear(&mut self) {
        self.value.zeroize();
        self.cursor = 0;
    }

    /// 取代全部內容（先覆寫舊緩衝，避免明文殘留）。
    pub fn set(&mut self, value: impl Into<String>) {
        self.value.zeroize();
        self.value = value.into();
        self.cursor = grapheme_len(&self.value);
    }

    /// 在游標處插入字元。
    pub fn insert(&mut self, character: char) {
        let byte = grapheme_byte_index(&self.value, self.cursor);
        self.value.insert(byte, character);
        // 新字元可能與前一個字素合併（例如組合重音），此時游標不應前進。
        // 以字素重新計算游標位置，而非固定加一。
        let inserted_end = byte + character.len_utf8();
        self.cursor = grapheme_len(&self.value[..inserted_end]);
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

impl Drop for InputLine {
    /// 丟棄前覆寫底層緩衝：表單被換掉或離開畫面時，明文口令／密碼不會
    /// 留在已釋放的堆積上（`clear()` 只在顯式呼叫時執行）。
    fn drop(&mut self) {
        self.value.zeroize();
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

#[cfg(test)]
#[path = "tests/text_test.rs"]
mod text_test;
