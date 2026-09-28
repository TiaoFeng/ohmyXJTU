//! 統一認證登入頁的 HTML 解析。
//!
//! 對應參考實作中以 lxml XPath 完成的查詢。所有解析失敗都回傳 `None`／預設值，
//! 不以 panic 中斷登入流程。

use scraper::{ElementRef, Html, Selector};

/// 帳號身份選項（「本科生」「研究生」…）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountChoice {
    /// 顯示名稱。
    pub name: String,
    /// 表單提交用的 `label` 值。
    pub label: String,
}

/// `el-alert` 元件顯示的錯誤提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertMessage {
    /// `title` 屬性。
    pub title: String,
    /// 元素內的文字。
    pub content: String,
}

impl AlertMessage {
    /// 可直接顯示的錯誤訊息：優先取 `title`，其次取元素文字。
    pub fn text(&self) -> String {
        if !self.title.is_empty() {
            self.title.clone()
        } else {
            self.content.clone()
        }
    }
}

/// 取得指定 `name` 的 `input` 元素之 `value`。
pub fn input_value(html: &str, name: &str) -> Option<String> {
    let document = Html::parse_document(html);
    first(&document, &format!("input[name=\"{name}\"]"))?
        .value()
        .attr("value")
        .map(str::to_owned)
}

/// 取得隱藏欄位 `execution` 的值（登入表單的必要欄位）。
pub fn execution_value(html: &str) -> Option<String> {
    input_value(html, "execution")
}

/// 判斷是否為 Safety Verify（二次認證）頁面。
pub fn is_safety_verify_page(html: &str) -> bool {
    let document = Html::parse_document(html);
    let has_title = first(&document, "title")
        .map(|element| text_of(&element))
        .is_some_and(|title| title.contains("Safety Verify"));
    let has_field = |name: &str| {
        first(&document, &format!("#fm1 input[name=\"{name}\"]"))
            .and_then(|element| element.value().attr("value").map(str::to_owned))
            .is_some()
    };
    let has_sec_api =
        html.contains("/cas/sec/initByType") || html.contains("\\/cas\\/sec\\/initByType");
    let has_safety_text = html.contains("选择安全认证") || html.contains("二次认证");

    has_field("secState")
        && has_field("execution")
        && has_field("_eventId")
        && (has_title || has_sec_api || has_safety_text)
}

/// 解析帳號身份選項；非帳號選擇頁時回傳 `None`。
pub fn account_choices(html: &str) -> Option<Vec<AccountChoice>> {
    let document = Html::parse_document(html);
    let wraps = selector("div.account-wrap")?;
    let name_selector = selector("div.name")?;
    let radio_selector = selector("el-radio.checkbox-radio")?;

    let choices: Vec<AccountChoice> = document
        .select(&wraps)
        .filter_map(|wrap| {
            let name = wrap.select(&name_selector).next().map(|el| text_of(&el))?;
            let label = wrap
                .select(&radio_selector)
                .next()
                .and_then(|el| el.value().attr("label").map(str::to_owned))?;
            Some(AccountChoice { name, label })
        })
        .collect();

    if choices.is_empty() {
        None
    } else {
        Some(choices)
    }
}

/// 解析 `el-alert` 錯誤提示。
pub fn alert_message(html: &str) -> Option<AlertMessage> {
    let document = Html::parse_document(html);
    let alert = first(&document, "el-alert")?;
    Some(AlertMessage {
        title: alert.value().attr("title").unwrap_or_default().to_owned(),
        content: text_of(&alert),
    })
}

/// 解析 `globalConfig.mfaEnabled`。
///
/// 與參考實作一致：**找不到時視為需要 MFA**，避免漏做兩步驗證而反覆登入失敗。
pub fn mfa_enabled(html: &str) -> bool {
    global_config(html)
        .and_then(|config| config.get("mfaEnabled").cloned())
        .is_none_or(|value| {
            value == serde_json::Value::Bool(true)
                || value == serde_json::Value::String("true".into())
        })
}

/// 解析登入頁中的 `globalConfig = eval('(' + "…" + ')');`。
fn global_config(html: &str) -> Option<serde_json::Value> {
    let escaped = escaped_string_after(html, "globalConfig", "eval(")?;
    // 以 JSON 字串文法反轉義，可正確處理 `\"`、`\\`、`\/`、`\uXXXX` 等序列。
    let unescaped = serde_json::from_str::<String>(&format!("\"{escaped}\"")).ok()?;
    serde_json::from_str(&unescaped).ok()
}

/// 取出 `anchor` 之後、`marker` 之後的第一個雙引號字串（保留轉義序列）。
fn escaped_string_after<'a>(haystack: &'a str, anchor: &str, marker: &str) -> Option<&'a str> {
    let rest = &haystack[haystack.find(anchor)? + anchor.len()..];
    let rest = &rest[rest.find(marker)? + marker.len()..];
    let rest = &rest[rest.find('"')? + 1..];

    let mut escaped = false;
    for (index, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(&rest[..index]),
            _ => {}
        }
    }
    None
}

fn selector(pattern: &str) -> Option<Selector> {
    Selector::parse(pattern).ok()
}

fn first<'a>(document: &'a Html, pattern: &str) -> Option<ElementRef<'a>> {
    document.select(&selector(pattern)?).next()
}

fn text_of(element: &ElementRef<'_>) -> String {
    element.text().collect::<String>().trim().to_owned()
}

#[cfg(test)]
#[path = "tests/html_test.rs"]
mod html_test;
