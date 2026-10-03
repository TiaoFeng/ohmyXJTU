//! 活動說明（HTML）轉純文字。
//!
//! 思源學堂把活動正文放在詳情回應的 `data.description`（作業、資料）或
//! `data.content`（頁面型活動）；兩者都是 HTML，終端無法呈現富文本，因此
//! 先轉成純文字再顯示。規則對齊參考實作（`LmsActivityDetail` 以 Jsoup 去
//! 標籤）：`<br>` 與區塊元素產生換行、其餘標籤只保留文字、HTML 實體由解析
//! 器解碼。
//!
//! 原始碼中的換行與縮排一律視為空白（不當作段落），因此相鄰段落之間恰好
//! 一個換行。沒有可見文字時回傳 `None`（呼叫端據此隱藏整個描述區塊）。

use scraper::node::Node;
use scraper::{ElementRef, Html};

/// 純文字長度上限（字元）；超出時截斷並以「…」結尾。
///
/// 這是防護上限：描述會隨作業清單一起快取並參與重繪，異常巨大的回應不應
/// 拖垮記憶體或畫面。
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
const SKIPPED_ELEMENTS: &[&str] = &["script", "style", "template"];

/// 無法以文字呈現的元素（圖片、影片等）：略過子樹並標記。
const MEDIA_ELEMENTS: &[&str] = &[
    "audio", "canvas", "embed", "iframe", "img", "object", "svg", "video",
];

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
    /// 遞迴進入子樹；`true` 代表是區塊元素（前後補換行）。
    Nested(bool),
}

/// HTML 轉換結果。
///
/// 純文字只能呈現正文的一部分：作業說明可能整份就是一張圖片。`has_media`
/// 讓上層能標註「說明含圖片」，使用者才會知道要開網頁看原本的內容。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Content {
    /// 純文字內容（沒有可見文字時為 `None`）。
    pub text: Option<String>,
    /// 是否含圖片、影片等無法以文字呈現的元素。
    pub has_media: bool,
}

/// 把 HTML 片段轉成純文字，並回報是否含圖片等無法以文字呈現的內容。
pub(super) fn convert(html: &str) -> Content {
    let document = Html::parse_fragment(html);
    let mut raw = String::new();
    let mut has_media = false;
    // 片段根不是元素（`Node::Fragment`），直接走訪其子節點。
    for child in document.tree.root().children() {
        if child.value().is_element() {
            if let Some(element) = ElementRef::wrap(child) {
                walk_element(element, &mut raw, &mut has_media, 0);
            }
        } else if let Node::Text(text) = child.value() {
            push_text(&text.text, &mut raw);
        }
    }
    Content {
        text: normalize(&raw),
        has_media,
    }
}

/// 走訪元素的子節點：文字原樣保留（空白折疊）、`<br>` 與區塊元素補換行。
fn walk_element(element: ElementRef<'_>, out: &mut String, has_media: &mut bool, depth: usize) {
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
                    Step::Nested(BLOCK_ELEMENTS.contains(&name))
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
            Step::Nested(block) => {
                if block {
                    out.push('\n');
                }
                if depth < MAX_DEPTH
                    && let Some(nested) = ElementRef::wrap(child)
                {
                    walk_element(nested, out, has_media, depth + 1);
                }
                if block {
                    out.push('\n');
                }
            }
        }
    }
}

/// 寫入文字節點內容：連續空白（含原始碼換行、縮排與 `&nbsp;`）折成單一空格。
fn push_text(text: &str, out: &mut String) {
    for character in text.chars() {
        if character.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(character);
        }
    }
}

/// 收斂換行：逐行去空白、丟棄空行、以單一換行連接，並套用長度上限。
fn normalize(raw: &str) -> Option<String> {
    let mut result = String::new();
    for line in raw.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
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
