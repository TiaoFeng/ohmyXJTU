//! 用户协议：內嵌 `PRIVACY.md` 全文、版本與輕量排版。
//!
//! 協議全文以 `include_str!` 內嵌進執行檔，散布的二進位檔不需外部檔案也能
//! 完整顯示；[`VERSION`] 必須與文件標頭的版本一致（由測試把關）。解析與換行
//! 皆為純函式，不新增任何 Markdown 或排版依賴。
//!
//! 排版為「輕度整理」：去除 `**`、反引號等行內標記，標題、清單、引用與表格
//! 改以 [`LineKind`] 標示意義（供介面上色）；連續的表格列收成一個 [`Table`]，
//! 由 [`table`] 模組依可用寬度排成對齊表格或以卡片呈現，長行依顯示寬度換行
//! （`-`／`>`／`|` 等原始符號不會出現在畫面上）。

use std::sync::OnceLock;

use unicode_segmentation::UnicodeSegmentation as _;

use crate::text::display_width;

mod table;

pub use table::Table;

/// 本版本要求的協議版本（對應 `PRIVACY.md` 標頭的「版本」）。
pub const VERSION: &str = "1.7";

/// 內嵌的協議全文（Markdown 原始內容）。
pub const TEXT: &str = include_str!("../PRIVACY.md");

/// 文件列的呈現種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 文件標題（`#`）。
    Title,
    /// 章節標題（`##`）。
    Heading,
    /// 一般段落。
    Body,
    /// 清單項目。
    Bullet,
    /// 引用區塊。
    Quote,
    /// 水平分隔線（文字留空，由介面依寬度鋪滿）。
    Rule,
    /// 對齊表格的標頭列。
    TableHeader,
    /// 對齊表格標頭下的細線（文字為該線本身）。
    TableRule,
    /// 對齊表格的資料列。
    TableRow,
    /// 卡片標題（表格首欄）。
    TableTitle,
    /// 卡片欄位（標籤：值）。
    TableField,
}

/// 文件中的一行（已去除行內 Markdown 標記）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocLine {
    /// 顯示文字。
    pub text: String,
    /// 呈現種類。
    pub kind: LineKind,
    /// 列首以標籤樣式（次要色）呈現的顯示欄數；0 表示整列同一樣式。
    pub label_len: usize,
}

impl DocLine {
    /// 建立文件列。
    pub fn new(text: impl Into<String>, kind: LineKind) -> Self {
        Self {
            text: text.into(),
            kind,
            label_len: 0,
        }
    }

    /// 標記列首的標籤欄數（見 [`DocLine::label_len`]）。
    pub fn with_label(mut self, label_len: usize) -> Self {
        self.label_len = label_len;
        self
    }

    /// 空白列。
    fn blank() -> Self {
        Self::new(String::new(), LineKind::Body)
    }

    /// 分隔線。
    fn rule() -> Self {
        Self::new(String::new(), LineKind::Rule)
    }
}

/// 文件區塊：一般文字列或結構化表格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// 一般文字列。
    Line(DocLine),
    /// 表格（連續的 `|` 列；對齊列已被消耗）。
    Table(Table),
}

/// 解析後的協議文件（只解析一次；文字內嵌，內容固定）。
pub fn document() -> &'static [Block] {
    static DOCUMENT: OnceLock<Vec<Block>> = OnceLock::new();
    DOCUMENT.get_or_init(|| parse(TEXT))
}

/// 取出文件標頭宣告的版本（僅供測試校驗與 [`VERSION`] 一致）。
#[cfg(test)]
pub fn embedded_version(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let rest = line.split("**版本**：").nth(1)?;
        rest.split_whitespace().next()
    })
}

/// 將 Markdown 原文轉為文件區塊。
///
/// 辨識範圍刻意維持最小：標題、清單、引用、表格、分隔線與行內記號；
/// 其餘語法（巢狀清單、程式碼區塊等）目前文本未使用，原樣視為段落。
pub fn parse(text: &str) -> Vec<Block> {
    let source: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if is_table_line(source[index].trim()) {
            let start = index;
            while index < source.len() && is_table_line(source[index].trim()) {
                index += 1;
            }
            match Table::parse(&source[start..index]) {
                Some(table) => blocks.push(Block::Table(table)),
                // 無法構成表格時退回一般段落，不吞掉原文。
                None => blocks.extend(
                    source[start..index]
                        .iter()
                        .map(|raw| Block::Line(DocLine::new(inline(raw.trim()), LineKind::Body))),
                ),
            }
            continue;
        }
        blocks.push(Block::Line(parse_line(source[index].trim())));
        index += 1;
    }
    blocks
}

/// 解析單列文字：標題、清單、引用、分隔線或一般段落。
fn parse_line(trimmed: &str) -> DocLine {
    if trimmed.is_empty() {
        DocLine::blank()
    } else if let Some(rest) = trimmed.strip_prefix("## ") {
        DocLine::new(inline(rest), LineKind::Heading)
    } else if let Some(rest) = trimmed.strip_prefix("# ") {
        DocLine::new(inline(rest), LineKind::Title)
    } else if is_rule(trimmed) {
        DocLine::rule()
    } else if let Some(rest) = trimmed.strip_prefix('>') {
        let content = inline(rest.trim());
        let text = if content.is_empty() {
            "│".to_owned()
        } else {
            format!("│ {content}")
        };
        DocLine::new(text, LineKind::Quote)
    } else if let Some(rest) = trimmed.strip_prefix("- ") {
        DocLine::new(format!("• {}", inline(rest)), LineKind::Bullet)
    } else {
        DocLine::new(inline(trimmed), LineKind::Body)
    }
}

/// 是否為表格列（以 `|` 開頭與結尾，且不只一個字元）。
fn is_table_line(trimmed: &str) -> bool {
    trimmed.len() > 1 && trimmed.starts_with('|') && trimmed.ends_with('|')
}

/// 依顯示寬度將文件區塊排成可繪製的列。
///
/// 一般文字依寬度換行（續行沿用原列的呈現種類，空行與分隔線原樣保留）；
/// 表格交由 [`Table::layout`] 依寬度選擇對齊表格或卡片；`width` 為 0 時
/// 回傳空清單。
pub fn wrap(blocks: &[Block], width: usize) -> Vec<DocLine> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Line(line) => {
                if line.text.is_empty() {
                    wrapped.push(line.clone());
                } else {
                    wrapped.extend(wrap_text(&line.text, line.kind, width, "", ""));
                }
            }
            Block::Table(table) => wrapped.extend(table.layout(width)),
        }
    }
    wrapped
}

/// 將一段文字依顯示寬度換行：先以空白切詞，詞內再切為最小原子後貪婪填充。
///
/// `first_prefix`／`continuation_prefix` 為首列與續列的前綴（懸掛縮排），
/// 兩者皆計入可用寬度，並成為輸出列的 [`DocLine::label_len`]（介面以標籤
/// 樣式呈現）。
///
/// 原子規則：連續 ASCII 字母與數字視為不可分割的一段（避免長網址或專有名詞
/// 被攔腰截斷），其他每個字素自成一個原子（中文可任意斷行）。原子本身仍比
/// 整列寬時才會逐字素硬切。
fn wrap_text(
    text: &str,
    kind: LineKind,
    width: usize,
    first_prefix: &str,
    continuation_prefix: &str,
) -> Vec<DocLine> {
    let mut wrapper = Wrapper {
        kind,
        width,
        first_prefix,
        continuation_prefix,
        text: String::new(),
        text_width: 0,
        line_index: 0,
        lines: Vec::new(),
    };
    for (index, word) in text.split_whitespace().enumerate() {
        let mut needs_space = index > 0;
        for atom in atoms(word) {
            wrapper.push_atom(&atom, needs_space);
            needs_space = false;
        }
    }
    wrapper.finish()
}

/// 換行緩衝：追蹤目前列的內容、顯示寬度與前綴。
struct Wrapper<'a> {
    kind: LineKind,
    width: usize,
    first_prefix: &'a str,
    continuation_prefix: &'a str,
    text: String,
    text_width: usize,
    line_index: usize,
    lines: Vec<DocLine>,
}

impl<'a> Wrapper<'a> {
    /// 目前列的前綴（首列與續列不同）。
    fn prefix(&self) -> &'a str {
        if self.line_index == 0 {
            self.first_prefix
        } else {
            self.continuation_prefix
        }
    }

    /// 目前列可用的顯示欄數。
    fn budget(&self) -> usize {
        self.width.saturating_sub(display_width(self.prefix()))
    }

    /// 放入一個原子；放不下時先收尾目前列再另起一列。
    fn push_atom(&mut self, atom: &str, space: bool) {
        let atom_width = display_width(atom);
        let gap = usize::from(space && !self.text.is_empty());
        if self.text_width + gap + atom_width <= self.budget() {
            if gap == 1 {
                self.text.push(' ');
                self.text_width += 1;
            }
            self.text.push_str(atom);
            self.text_width += atom_width;
            return;
        }
        // 放不下：先收尾目前列（沒有內容時不動作）。
        if !self.text.is_empty() {
            self.flush();
        }
        if atom_width <= self.budget() {
            self.text.push_str(atom);
            self.text_width = atom_width;
            return;
        }
        // 極窄畫面：單一原子仍超寬，逐字素硬切。
        for grapheme in atom.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if self.text_width + grapheme_width > self.budget() && !self.text.is_empty() {
                self.flush();
            }
            self.text.push_str(grapheme);
            self.text_width += grapheme_width;
        }
    }

    /// 收尾目前列（加上該列前綴）。
    fn flush(&mut self) {
        let prefix = self.prefix();
        let text = format!("{prefix}{}", self.text.trim_end());
        self.lines
            .push(DocLine::new(text, self.kind).with_label(display_width(prefix)));
        self.text.clear();
        self.text_width = 0;
        self.line_index += 1;
    }

    /// 結束換行，回傳所有列。
    fn finish(mut self) -> Vec<DocLine> {
        if !self.text.is_empty() {
            self.flush();
        }
        self.lines
    }
}

/// 將一個詞切為最小原子（見 [`wrap_line`]）。
fn atoms(word: &str) -> Vec<String> {
    let mut atoms = Vec::new();
    let mut ascii_run = String::new();
    for grapheme in word.graphemes(true) {
        let keep_together = grapheme
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric());
        if keep_together {
            ascii_run.push_str(grapheme);
        } else {
            if !ascii_run.is_empty() {
                atoms.push(std::mem::take(&mut ascii_run));
            }
            atoms.push(grapheme.to_owned());
        }
    }
    if !ascii_run.is_empty() {
        atoms.push(ascii_run);
    }
    atoms
}

/// 水平分隔線（只含 `-`，至少三個）。
fn is_rule(line: &str) -> bool {
    line.len() >= 3 && line.chars().all(|ch| ch == '-')
}

/// 去除行內標記：連結只留顯示文字（文字為空時保留網址），
/// `**粗體**` 與行內程式碼的反引號一併移除。
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after
            .split_once("](")
            .and_then(|(label, tail)| tail.split_once(')').map(|(url, next)| (label, url, next)))
        {
            Some((label, url, next)) => {
                out.push_str(if label.is_empty() { url } else { label });
                rest = next;
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "")
}

#[cfg(test)]
#[path = "tests/privacy_test.rs"]
mod privacy_test;
