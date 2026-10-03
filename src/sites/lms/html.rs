//! 活動說明（HTML）轉純文字。
//!
//! 思源學堂把活動正文放在詳情回應的 `data.description`（作業、資料）或
//! `data.content`（頁面型活動）；兩者都是 HTML，終端無法呈現富文本，因此
//! 先轉成純文字再顯示。規則對齊參考實作（`LmsActivityDetail` 以 Jsoup 去
//! 標籤）：`<br>` 與區塊元素產生換行、其餘標籤只保留文字、HTML 實體由解析
//! 器解碼。
//!
//! 原始碼中的 **ASCII** 空白（換行與縮排）折成單一空格，不當作段落（`<pre>`
//! 內的排版也因此會被壓平），因此相鄰段落之間恰好一個換行。非 ASCII 空白
//! （全形空格 `U+3000`、`&nbsp;` `U+00A0`…）在瀏覽器中是可見字元、中文排版
//! 常以它們做縮排，因此原樣保留——與 [`crate::text::wrap_display`] 的規則一致。
//!
//! 表格以「列」為單位換行，同一列的各儲存格之間補一個空白（否則會出現
//! 「第一题10」這種黏在一起的內容）；儲存格內的區塊元素同樣降級成空白（所見即
//! 所得的編輯器會把儲存格內容包在 `<p>` 裡，換行會讓同一列的欄位散開）。沒有
//! 可見文字時回傳空的 `text`（呼叫端據此隱藏整個描述區塊）。

use scraper::node::Node;
use scraper::{ElementRef, Html};

use super::models::ActivityText;

/// 純文字長度上限（字元）；超出時截斷並以「…」結尾。
///
/// 這是呈現與快取的防護上限：說明會隨作業清單一起快取並參與重繪，過長的正文
/// 也沒有閱讀價值。上限只約束轉換結果，走訪過程仍會看完整棵樹（佔用的記憶體
/// 與回應本身同量級）。
pub(super) const MAX_TEXT_CHARS: usize = 4096;

/// 巢狀深度上限（與 [`super::js_object`] 的寬容解析器同慣例）。
const MAX_DEPTH: usize = 64;

/// 產生換行邊界的元素。
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "blockquote",
    "div",
    "dl",
    "figure",
    "footer",
    "header",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "li",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tr",
    "ul",
];

/// 整棵子樹都略過的元素。
///
/// `title` 只在整份文件才有意義，但片段解析仍可能把它放進 `head`；不略過就會
/// 讓標題文字混進正文。
const SKIPPED_ELEMENTS: &[&str] = &["script", "style", "template", "title"];

/// 無法以文字呈現的元素（圖片、影片等）：略過子樹並標記。
const MEDIA_ELEMENTS: &[&str] = &[
    "audio", "canvas", "embed", "iframe", "img", "object", "svg", "video",
];

/// 表格儲存格：同一列的各格之間補一個空白（不分行，保留列結構）。
const CELL_ELEMENTS: &[&str] = &["td", "th"];

/// 走訪單一子節點時採取的動作。
#[derive(Clone, Copy)]
enum Step {
    /// 文字節點。
    Text,
    /// `<br>`：強制換行。
    Break,
    /// 不產生可見文字。
    Skip,
    /// 圖片、影片等無法以文字呈現的元素。
    Media,
    /// 遞迴進入子樹。`block` 代表區塊元素（前後補換行）；`cell` 代表表格儲存格
    /// （只在前面補一個空白）。
    Nested { block: bool, cell: bool },
}

/// 把 HTML 片段轉成純文字。
///
/// 回傳 [`ActivityText`]：純文字只能呈現正文的一部分（作業說明可能整份就是一張
/// 圖片），`has_media` 讓上層能標註「說明含圖片」，使用者才會知道要開網頁看原本
/// 的內容；沒有可見文字時 `text` 為 `None`。
pub(super) fn convert(html: &str) -> ActivityText {
    let document = Html::parse_fragment(html);
    let mut raw = String::new();
    let mut has_media = false;
    // 片段根不是元素（`Node::Fragment`），直接走訪其子節點。
    for child in document.tree.root().children() {
        if child.value().is_element() {
            if let Some(element) = ElementRef::wrap(child) {
                walk_element(element, &mut raw, &mut has_media, 0, false);
            }
        } else if let Node::Text(text) = child.value() {
            push_text(&text.text, &mut raw);
        }
    }
    ActivityText {
        text: normalize(&raw),
        has_media,
    }
}

/// 走訪元素的子節點：文字原樣保留（ASCII 空白折疊）、`<br>` 與區塊元素補換行、
/// 表格儲存格之間補一個空白（保留同一列的欄位對應）。
///
/// `in_cell` 表示目前位於表格儲存格內：儲存格裡的換行會讓同一列的欄位散開，
/// 因此把區塊元素降級成空白分隔。
fn walk_element(
    element: ElementRef<'_>,
    out: &mut String,
    has_media: &mut bool,
    depth: usize,
    in_cell: bool,
) {
    for child in element.children() {
        let step = match child.value() {
            Node::Text(_) => Step::Text,
            Node::Element(node) => {
                let name = node.name();
                if SKIPPED_ELEMENTS.contains(&name) {
                    Step::Skip
                } else if name == "br" {
                    Step::Break
                } else if MEDIA_ELEMENTS.contains(&name) {
                    Step::Media
                } else {
                    Step::Nested {
                        block: BLOCK_ELEMENTS.contains(&name),
                        cell: CELL_ELEMENTS.contains(&name),
                    }
                }
            }
            _ => Step::Skip,
        };

        match step {
            Step::Text => {
                if let Node::Text(text) = child.value() {
                    push_text(&text.text, out);
                }
            }
            Step::Break => out.push('\n'),
            Step::Skip => {}
            Step::Media => *has_media = true,
            Step::Nested { block, cell } => {
                if cell || (in_cell && block) {
                    push_separator(out);
                } else if block {
                    out.push('\n');
                }
                if depth < MAX_DEPTH
                    && let Some(nested) = ElementRef::wrap(child)
                {
                    walk_element(nested, out, has_media, depth + 1, in_cell || cell);
                }
                if block && !in_cell {
                    out.push('\n');
                }
            }
        }
    }
}

/// 補一個分隔空白（前一個字元已是空白時不重複補）。
fn push_separator(out: &mut String) {
    if !out.ends_with(|character: char| character.is_whitespace()) {
        out.push(' ');
    }
}

/// 寫入文字節點內容：連續的 **ASCII** 空白（原始碼換行與縮排）折成單一空格。
///
/// 只折疊 ASCII 空白：全形空格與 `&nbsp;`（`U+00A0`）在瀏覽器中是可見字元，
/// 折成半形會讓作者刻意排出的縮排走樣（見模組文件）。
fn push_text(text: &str, out: &mut String) {
    for character in text.chars() {
        if character.is_ascii_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(character);
        }
    }
}

/// 收斂換行：逐行去 ASCII 空白、丟棄空行、以單一換行連接，並套用長度上限。
///
/// 兩次修剪的語意不同：判斷空行用完整的空白定義（編輯器會以 `&nbsp;` 表示空
/// 段落，那不算內容），修剪行首行尾則只去 ASCII 空白（非 ASCII 空白是排版）。
fn normalize(raw: &str) -> Option<String> {
    let mut result = String::new();
    for line in raw.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let line = line.trim_matches(|character: char| character.is_ascii_whitespace());
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(line);
    }
    if result.is_empty() {
        return None;
    }
    Some(truncate(result))
}

/// 依字元數截斷；超出上限時補上省略號。
fn truncate(text: String) -> String {
    let mut characters = text.chars();
    let limited: String = characters.by_ref().take(MAX_TEXT_CHARS).collect();
    if characters.next().is_some() {
        format!("{limited}…")
    } else {
        limited
    }
}

#[cfg(test)]
#[path = "tests/html_test.rs"]
mod html_test;
