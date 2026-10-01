//! Markdown 表格的結構化模型與排版。
//!
//! 終端足夠寬時排成對齊表格（標頭、細線、資料列）；內容欄過寬而排不進畫面時
//! 改以卡片呈現：首欄當標題，其餘欄位各成一列「標籤：值」並以懸掛縮排對齊，
//! 讓長內容（例如 §5.1 的「会上传的内容」欄）仍看得出所屬欄位。排版全部是
//! 純函式，未新增任何依賴。

use crate::text::{display_width, pad_display};

use super::{DocLine, LineKind, wrap_text};

/// 對齊表格的欄位分隔（左右各一欄空白）。
const COLUMN_SEPARATOR: &str = " │ ";
/// 卡片欄位相對標題的縮排欄數。
const CARD_INDENT: usize = 2;
/// 卡片標題前綴（表格首欄）。
const TITLE_PREFIX: &str = "▸ ";
/// 卡片標籤與值之間的分隔。
const LABEL_SEPARATOR: &str = "：";
/// 卡片欄位至少保留給值的顯示欄數；不足時標籤獨立成列。
const MIN_VALUE_WIDTH: usize = 8;
/// 對齊表格標頭下的細線字元。
const RULE_CHAR: char = '─';

/// 表格：標頭（可為空）與資料列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// 欄數（標頭與每一列都補齊到此寬度）。
    columns: usize,
    /// 標頭列；沒有標頭時為空。
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// 建立表格；欄數不一致時以最寬的一列為準，缺格補空字串。
    pub fn new(headers: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        let columns = headers
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        let mut headers = headers;
        if !headers.is_empty() {
            headers.resize(columns, String::new());
        }
        let rows = rows
            .into_iter()
            .map(|mut row| {
                row.resize(columns, String::new());
                row
            })
            .collect();
        Self {
            columns,
            headers,
            rows,
        }
    }

    /// 解析連續的表格列；無法構成表格時回傳 `None`。
    ///
    /// 對齊列（`| --- |`）只作為「首列是標頭」的標記，本身不成為資料列；
    /// 沒有對齊列時全部視為資料（沒有標頭）。
    pub fn parse(lines: &[&str]) -> Option<Self> {
        let mut cells: Vec<Vec<String>> = Vec::new();
        let mut header_marked = false;
        for (index, raw) in lines.iter().enumerate() {
            let line = raw.trim();
            if is_separator(line) {
                header_marked |= index == 1;
                continue;
            }
            let row = row_cells(line);
            if row.is_empty() {
                return None;
            }
            cells.push(row);
        }
        let headers = match header_marked {
            true if !cells.is_empty() => cells.remove(0),
            _ => Vec::new(),
        };
        Some(Self::new(headers, cells))
    }

    /// 標頭列（可能為空）。
    pub fn headers(&self) -> &[String] {
        &self.headers
    }

    /// 資料列。
    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }

    /// 欄數。
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// 依可用寬度排版：排得下就對齊表格，否則改用卡片。
    pub fn layout(&self, width: usize) -> Vec<DocLine> {
        if width == 0 {
            return Vec::new();
        }
        match self.grid_width(width) {
            Some(total) => self.grid(total),
            None => self.cards(width),
        }
    }

    /// 對齊表格所需的總寬；欄數為 0 或排不進 `width` 時回傳 `None`。
    fn grid_width(&self, width: usize) -> Option<usize> {
        let widths = self.natural_widths();
        if widths.is_empty() {
            return None;
        }
        let separator = display_width(COLUMN_SEPARATOR);
        let total = widths.iter().sum::<usize>() + separator * (widths.len() - 1);
        (total <= width).then_some(total)
    }

    /// 各欄的自然寬度（標頭與所有儲存格的最大顯示寬）。
    fn natural_widths(&self) -> Vec<usize> {
        let mut widths = vec![0; self.columns];
        for (index, cell) in self.headers.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
        widths
    }

    /// 對齊表格：標頭列（粗體）＋細線＋資料列。
    fn grid(&self, total: usize) -> Vec<DocLine> {
        let widths = self.natural_widths();
        let mut lines = Vec::with_capacity(self.rows.len() + 2);
        if !self.headers.is_empty() {
            lines.push(self.grid_row(&self.headers, &widths, LineKind::TableHeader));
            lines.push(DocLine::new(
                RULE_CHAR.to_string().repeat(total),
                LineKind::TableRule,
            ));
        }
        for row in &self.rows {
            lines.push(self.grid_row(row, &widths, LineKind::TableRow));
        }
        lines
    }

    /// 對齊表格的一列（最後一欄不補白，避免尾端多餘空白）。
    fn grid_row(&self, cells: &[String], widths: &[usize], kind: LineKind) -> DocLine {
        let mut text = String::new();
        for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
            if index > 0 {
                text.push_str(COLUMN_SEPARATOR);
            }
            if index + 1 == cells.len() {
                text.push_str(cell);
            } else {
                text.push_str(&pad_display(cell, *width));
            }
        }
        DocLine::new(text, kind)
    }

    /// 卡片：首欄為標題，其餘欄位各成「標籤：值」一列（值以懸掛縮排對齊）。
    fn cards(&self, width: usize) -> Vec<DocLine> {
        let label_width = self.label_width();
        let mut lines = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                lines.push(DocLine::blank());
            }
            self.card(row, label_width, width, &mut lines);
        }
        lines
    }

    /// 一筆資料的卡片：標題（首欄）與其餘欄位。
    fn card(&self, row: &[String], label_width: usize, width: usize, out: &mut Vec<DocLine>) {
        if let Some(title) = row.first()
            && !title.is_empty()
        {
            let indent = " ".repeat(display_width(TITLE_PREFIX));
            out.extend(wrap_text(
                title,
                LineKind::TableTitle,
                width,
                TITLE_PREFIX,
                &indent,
            ));
        }
        for (column, value) in row.iter().enumerate().skip(1) {
            self.card_field(column, value, label_width, width, out);
        }
    }

    /// 卡片欄位：預設標籤與值同列並以懸掛縮排對齊；標籤欄過寬時標籤獨立成列。
    fn card_field(
        &self,
        column: usize,
        value: &str,
        label_width: usize,
        width: usize,
        out: &mut Vec<DocLine>,
    ) {
        let indent = " ".repeat(CARD_INDENT);
        let label = self.label(column);
        if label.is_empty() {
            self.push_value(value, &indent, &indent, width, out);
            return;
        }
        let prefix = format!(
            "{indent}{}{LABEL_SEPARATOR}",
            pad_display(&label, label_width)
        );
        let prefix_width = display_width(&prefix);
        if prefix_width + MIN_VALUE_WIDTH <= width {
            let hanging = " ".repeat(prefix_width);
            self.push_value(value, &prefix, &hanging, width, out);
            return;
        }
        // 空間不足：標籤獨占一列，值由列首開始換行（不再被標籤欄擠壓）。
        if !value.is_empty() {
            out.push(DocLine::new(prefix, LineKind::TableField).with_label(prefix_width));
        }
        self.push_value(value, "", "", width, out);
    }

    /// 輸出一段值（值為空時只保留標籤）。
    fn push_value(
        &self,
        value: &str,
        first_prefix: &str,
        continuation_prefix: &str,
        width: usize,
        out: &mut Vec<DocLine>,
    ) {
        if value.is_empty() {
            if !first_prefix.trim().is_empty() {
                out.push(
                    DocLine::new(first_prefix, LineKind::TableField)
                        .with_label(display_width(first_prefix)),
                );
            }
            return;
        }
        out.extend(wrap_text(
            value,
            LineKind::TableField,
            width,
            first_prefix,
            continuation_prefix,
        ));
    }

    /// 該欄的標籤（沒有標頭時為空字串）。
    fn label(&self, column: usize) -> String {
        self.headers.get(column).cloned().unwrap_or_default()
    }

    /// 標籤欄寬（所有標籤的最大顯示寬）。
    fn label_width(&self) -> usize {
        self.headers
            .iter()
            .skip(1)
            .map(|label| display_width(label))
            .max()
            .unwrap_or(0)
    }
}

/// 表格列的一格一格（去除外框豎線並清理行內標記）。
fn row_cells(line: &str) -> Vec<String> {
    line.trim_matches('|')
        .split('|')
        .map(|cell| super::inline(cell.trim()))
        .collect()
}

/// 對齊列（每一格只含 `-` 與 `:`）。
fn is_separator(line: &str) -> bool {
    !line.trim_matches('|').is_empty()
        && line.trim_matches('|').split('|').all(|cell| {
            let cell = cell.trim();
            !cell.is_empty() && cell.chars().all(|ch| ch == '-' || ch == ':')
        })
}

#[cfg(test)]
#[path = "tests/table_test.rs"]
mod table_test;
