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

    /// 直接把游標設在指定字素位置（超出內容時夾到尾端）。
    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.len());
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

/// 多行文字輸入框（任務描述用）。
///
/// 每個邏輯行各由一個 [`InputLine`] 維護（含游標），因此插入、刪除、字素級
/// 游標與水平捲動等行為都與單行輸入完全一致；畫面只顯示游標附近的行，不做
/// 自動軟換行（過寬的行由 `input_window` 水平捲動）。
#[derive(Debug, Clone)]
pub struct TextArea {
    lines: Vec<InputLine>,
    row: usize,
}

impl TextArea {
    /// 以初值建立（依顯式換行分段）。
    pub fn new(value: &str) -> Self {
        let lines: Vec<InputLine> = if value.is_empty() {
            vec![InputLine::new()]
        } else {
            value.split('\n').map(InputLine::with_value).collect()
        };
        let row = lines.len().saturating_sub(1);
        Self { lines, row }
    }

    /// 目前內容（以 `\n` 連接）。
    pub fn value(&self) -> String {
        self.lines
            .iter()
            .map(InputLine::value)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 內容是否為空（所有行皆空）。
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(InputLine::is_empty)
    }

    /// 游標所在行的索引。
    pub fn row(&self) -> usize {
        self.row
    }

    /// 邏輯行數。
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// 指定行（供繪製）。
    pub fn line(&self, index: usize) -> Option<&InputLine> {
        self.lines.get(index)
    }

    /// 顯示視窗的起始行（讓游標所在行保持可見）。
    pub fn window_start(&self, rows: usize) -> usize {
        if rows == 0 {
            return 0;
        }
        self.row.saturating_sub(rows - 1)
    }

    /// 游標所在行（可變）。
    pub fn focused_line_mut(&mut self) -> &mut InputLine {
        &mut self.lines[self.row]
    }

    /// 游標所在行。
    pub fn focused_line(&self) -> &InputLine {
        &self.lines[self.row]
    }

    /// 在游標處插入字元（換行字元會分段）。
    pub fn insert(&mut self, character: char) {
        if character == '\n' {
            self.new_line();
            return;
        }
        self.focused_line_mut().insert(character);
    }

    /// 在游標處斷行。
    pub fn new_line(&mut self) {
        let line = self.focused_line_mut();
        let tail = line.value()[byte_index(line, line.cursor())..].to_owned();
        let head = line.value()[..byte_index(line, line.cursor())].to_owned();
        line.set(head);
        self.lines.insert(self.row + 1, InputLine::with_value(tail));
        self.row += 1;
    }

    /// 刪除游標前的字元；行首時與前一行合併。
    pub fn backspace(&mut self) {
        if self.focused_line_mut().backspace() {
            return;
        }
        if self.row == 0 {
            return;
        }
        let joined = self.lines.remove(self.row);
        self.row -= 1;
        let previous = self.focused_line_mut();
        let cursor = previous.len();
        let mut merged = previous.value().to_owned();
        merged.push_str(joined.value());
        previous.set(merged);
        previous.set_cursor(cursor);
    }

    /// 刪除游標後的字元；行尾時把下一行併入。
    pub fn delete(&mut self) {
        if self.focused_line_mut().delete() {
            return;
        }
        if self.row + 1 >= self.lines.len() {
            return;
        }
        let next = self.lines.remove(self.row + 1);
        let line = self.focused_line_mut();
        let mut merged = line.value().to_owned();
        merged.push_str(next.value());
        line.set(merged);
        line.move_end();
    }

    /// 游標左移（行首會移到上一行尾端）。
    pub fn move_left(&mut self) {
        if self.focused_line_mut().move_left() {
            return;
        }
        if self.row > 0 {
            self.row -= 1;
            self.focused_line_mut().move_end();
        }
    }

    /// 游標右移（行尾會移到下一行開頭）。
    pub fn move_right(&mut self) {
        if self.focused_line_mut().move_right() {
            return;
        }
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.focused_line_mut().move_home();
        }
    }

    /// 游標移到本行開頭。
    pub fn move_home(&mut self) {
        self.focused_line_mut().move_home();
    }

    /// 游標移到本行尾端。
    pub fn move_end(&mut self) {
        self.focused_line_mut().move_end();
    }

    /// 游標上移一行（同一欄位，越界時夾取）。
    pub fn move_up(&mut self) {
        if self.row == 0 {
            return;
        }
        let column = self.focused_line().cursor();
        self.row -= 1;
        let target = self.focused_line_mut();
        let column = column.min(target.len());
        target.set_cursor(column);
    }

    /// 游標下移一行（同一欄位，越界時夾取）。
    pub fn move_down(&mut self) {
        if self.row + 1 >= self.lines.len() {
            return;
        }
        let column = self.focused_line().cursor();
        self.row += 1;
        let target = self.focused_line_mut();
        let column = column.min(target.len());
        target.set_cursor(column);
    }
}

fn byte_index(line: &InputLine, cursor: usize) -> usize {
    grapheme_byte_index(line.value(), cursor)
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
