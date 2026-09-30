//! 用户协议：內嵌 `PRIVACY.md` 全文、版本與輕量排版。
//!
//! 協議全文以 `include_str!` 內嵌進執行檔，散布的二進位檔不需外部檔案也能
//! 完整顯示；[`VERSION`] 必須與文件標頭的版本一致（由測試把關）。解析與換行
//! 皆為純函式，不新增任何 Markdown 或排版依賴。
//!
//! 排版為「輕度整理」：去除 `**`、反引號等行內標記，標題、清單、引用與表格
//! 改以 [`LineKind`] 標示意義（供介面上色），表格去除外框豎線並略過對齊列，
//! 長行依顯示寬度換行（`-`／`>` 等原始符號不會出現在畫面上）。

use std::sync::OnceLock;

use unicode_segmentation::UnicodeSegmentation as _;

use crate::tui::text::display_width;

/// 本版本要求的協議版本（對應 `PRIVACY.md` 標頭的「版本」）。
pub const VERSION: &str = "1.2";

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
    /// 表格內容列。
    Table,
    /// 水平分隔線（文字留空，由介面依寬度鋪滿）。
    Rule,
}

/// 文件中的一行（已去除行內 Markdown 標記）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocLine {
    /// 顯示文字。
    pub text: String,
    /// 呈現種類。
    pub kind: LineKind,
}

impl DocLine {
    /// 建立文件列。
    pub fn new(text: impl Into<String>, kind: LineKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
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

/// 解析後的協議文件（只解析一次；文字內嵌，內容固定）。
pub fn document() -> &'static [DocLine] {
    static DOCUMENT: OnceLock<Vec<DocLine>> = OnceLock::new();
    DOCUMENT.get_or_init(|| parse(TEXT))
}

/// 取出文件標頭宣告的版本（供測試校驗與 [`VERSION`] 一致）。
pub fn embedded_version(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let rest = line.split("**版本**：").nth(1)?;
        rest.split_whitespace().next()
    })
}

/// 將 Markdown 原文轉為可顯示的文件列。
///
/// 辨識範圍刻意維持最小：標題、清單、引用、表格、分隔線與行內記號；
/// 其餘語法（巢狀清單、程式碼區塊等）目前文本未使用，原樣視為段落。
pub fn parse(text: &str) -> Vec<DocLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            lines.push(DocLine::blank());
        } else if let Some(rest) = trimmed.strip_prefix("## ") {
            lines.push(DocLine::new(inline(rest), LineKind::Heading));
        } else if let Some(rest) = trimmed.strip_prefix("# ") {
            lines.push(DocLine::new(inline(rest), LineKind::Title));
        } else if is_rule(trimmed) {
            lines.push(DocLine::rule());
        } else if trimmed.starts_with('|') && trimmed.ends_with('|') {
            if is_table_separator(trimmed) {
                lines.push(DocLine::rule());
            } else {
                lines.push(DocLine::new(table_row(trimmed), LineKind::Table));
            }
        } else if let Some(rest) = trimmed.strip_prefix('>') {
            let content = inline(rest.trim());
            let text = if content.is_empty() {
                "│".to_owned()
            } else {
                format!("│ {content}")
            };
            lines.push(DocLine::new(text, LineKind::Quote));
        } else if let Some(rest) = trimmed.strip_prefix("- ") {
            lines.push(DocLine::new(
                format!("• {}", inline(rest)),
                LineKind::Bullet,
            ));
        } else {
            lines.push(DocLine::new(inline(trimmed), LineKind::Body));
        }
    }
    lines
}

/// 依顯示寬度將文件換行；續行沿用原列的呈現種類，空行與分隔線原樣保留。
pub fn wrap(lines: &[DocLine], width: usize) -> Vec<DocLine> {
    if width == 0 {
        return Vec::new();
    }
    let mut wrapped = Vec::with_capacity(lines.len());
    for line in lines {
        if line.text.is_empty() {
            wrapped.push(line.clone());
        } else {
            wrap_line(line, width, &mut wrapped);
        }
    }
    wrapped
}

/// 單列換行：先以空白切詞，詞內再切為最小原子後貪婪填充。
///
/// 原子規則：連續 ASCII 字母與數字視為不可分割的一段（避免長網址或專有名詞
/// 被攔腰截斷），其他每個字素自成一個原子（中文可任意斷行）。原子本身仍比
/// 整行寬時才會逐字素硬切。
fn wrap_line(line: &DocLine, width: usize, out: &mut Vec<DocLine>) {
    let mut current = String::new();
    let mut current_width = 0;
    for (index, word) in line.text.split_whitespace().enumerate() {
        let mut needs_space = index > 0;
        for atom in atoms(word) {
            let atom_width = display_width(&atom);
            let gap = usize::from(needs_space && !current.is_empty());
            needs_space = false;
            if current_width + gap + atom_width <= width {
                if gap == 1 {
                    current.push(' ');
                    current_width += 1;
                }
                current.push_str(&atom);
                current_width += atom_width;
                continue;
            }
            // 放不下：先收尾目前行（沒有內容時不動作）。
            if !current.is_empty() {
                push_wrapped(out, line.kind, &mut current, &mut current_width);
            }
            if atom_width <= width {
                current.push_str(&atom);
                current_width = atom_width;
            } else {
                // 極窄畫面：單一原子仍超寬，逐字素硬切。
                for grapheme in atom.graphemes(true) {
                    let grapheme_width = display_width(grapheme);
                    if current_width + grapheme_width > width && !current.is_empty() {
                        push_wrapped(out, line.kind, &mut current, &mut current_width);
                    }
                    current.push_str(grapheme);
                    current_width += grapheme_width;
                }
            }
        }
    }
    if !current.is_empty() {
        out.push(DocLine::new(current.trim_end(), line.kind));
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

/// 收尾目前行並重置緩衝。
fn push_wrapped(out: &mut Vec<DocLine>, kind: LineKind, current: &mut String, width: &mut usize) {
    out.push(DocLine::new(current.trim_end(), kind));
    current.clear();
    *width = 0;
}

/// 水平分隔線（只含 `-`，至少三個）。
fn is_rule(line: &str) -> bool {
    line.len() >= 3 && line.chars().all(|ch| ch == '-')
}

/// 表格對齊列（每一格只含 `-` 與 `:`）。
fn is_table_separator(line: &str) -> bool {
    line.trim_matches('|').split('|').all(|cell| {
        let cell = cell.trim();
        !cell.is_empty() && cell.chars().all(|ch| ch == '-' || ch == ':')
    })
}

/// 表格內容列：去除外框豎線，保留格與格之間的 ` | ` 分隔。
fn table_row(line: &str) -> String {
    line.trim_matches('|')
        .split('|')
        .map(|cell| inline(cell.trim()))
        .collect::<Vec<_>>()
        .join(" | ")
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
