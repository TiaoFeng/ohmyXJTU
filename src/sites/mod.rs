//! 站點客戶端：考勤系統與思源學堂。

pub mod attendance;
pub mod lms;

use serde::Deserialize as _;
use serde::de::DeserializeOwned;

use crate::error::{AppError, AppResult};
use crate::http::HttpResponse;

/// 確認登入收尾取得的是有效回應（非維護頁或登入頁）。
///
/// 站點的登入收尾（例如思源學堂的 `/user/index`）即使遇到 5xx 維護頁或被導回
/// 登入頁，仍會回傳可讀的 HTML。不檢查就會把站點標記為已登入：之後的查詢
/// 全部以「待核实」收場，使用者卻只看到「登入成功」。
pub fn ensure_authenticated(response: &HttpResponse) -> AppResult<()> {
    if response.status >= 500 {
        return Err(AppError::Http {
            status: response.status,
        });
    }
    if crate::session::site::is_auth_failure(response) {
        return Err(AppError::SessionExpired);
    }
    response.error_for_status()
}

/// 解開 `{code, message, data}` 外殼，並把 `data` 解析為指定型別。
///
/// 外殼解碼規則（嚴格度）集中於 [`crate::json::split_envelope`]；缺 `code`
/// 或型別不符一律視為協定格式錯誤，不可用假業務碼回報。
pub fn unwrap_envelope<T: DeserializeOwned>(
    response: &HttpResponse,
    context: &str,
) -> AppResult<T> {
    let data = crate::json::split_envelope(response, context)?;
    deserialize_value(data, context)
}

/// 解析純 JSON 回應（無外殼）。
pub fn parse_json<T: DeserializeOwned>(response: &HttpResponse, context: &str) -> AppResult<T> {
    response.error_for_status()?;
    let value: serde_json::Value = response.json()?;
    deserialize_value(value, context)
}

/// 將 JSON 值解析為指定型別。
///
/// 錯誤訊息只描述失敗類別（不含原始欄位值），避免回應內容出現在介面提示中。
pub fn deserialize_value<T: DeserializeOwned>(
    value: serde_json::Value,
    context: &str,
) -> AppResult<T> {
    serde_json::from_value(value).map_err(|err| {
        AppError::protocol(format!(
            "{context} 响应格式不符（{}）",
            crate::error::describe_json_failure(err.classify())
        ))
    })
}

/// JSON 值的型別名稱（不含內容）。
fn value_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "空值",
        serde_json::Value::Bool(_) => "布尔值",
        serde_json::Value::Number(_) => "数字",
        serde_json::Value::String(_) => "字符串",
        serde_json::Value::Array(_) => "数组",
        serde_json::Value::Object(_) => "对象",
    }
}

/// 逐項解析列表，回傳（成功項目, 被跳過的項目數）。
///
/// 伺服器對列表項的欄位並不總是完整（例如缺少活動 ID），
/// 個別項目解析失敗時跳過該項，而不是讓整份列表查詢失敗。
pub fn parse_lenient<T: DeserializeOwned>(
    items: serde_json::Value,
    context: &str,
) -> AppResult<(Vec<T>, usize)> {
    let serde_json::Value::Array(items) = items else {
        return Err(AppError::protocol(format!("{context} 响应不是列表")));
    };

    let mut parsed = Vec::with_capacity(items.len());
    let mut skipped = 0_usize;
    for item in items {
        match serde_json::from_value::<T>(item) {
            Ok(value) => parsed.push(value),
            Err(_) => skipped += 1,
        }
    }
    Ok((parsed, skipped))
}

/// 接受字串或數字的欄位（伺服器對識別碼與節次的型別並不統一）。
pub(crate) fn string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "期望字符串或数字，实际为{}",
            value_type_name(&other)
        ))),
    }
}

/// 接受數字或可解析為數字的字串。
pub(crate) fn u32_or_string<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match &value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(|| serde::de::Error::custom("数值超出范围")),
        serde_json::Value::String(text) => text
            .trim()
            .parse::<u32>()
            .map_err(|_| serde::de::Error::custom("无法解析为数字")),
        other => Err(serde::de::Error::custom(format!(
            "期望数字或字符串，实际为{}",
            value_type_name(other)
        ))),
    }
}

/// 缺欄位時回傳 `None`，型別不符時才報錯的可選字串。
pub(crate) fn optional_string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        Some(serde_json::Value::Number(number)) => Ok(Some(number.to_string())),
        Some(other) => Err(serde::de::Error::custom(format!(
            "期望字符串或数字，实际为{}",
            value_type_name(&other)
        ))),
    }
}

/// 缺欄位或型別不符時回傳 `None` 的可選字串。
///
/// 只接受字串：非字串（數字、物件、陣列…）一律視為「沒有這段內容」。用於純展示
/// 用的文字欄位（例如活動說明）：這些欄位型別異常時只應損失該段說明，不應讓整份
/// 回應解析失敗——詳情解析失敗會使該課程的作業全部退回「待核实」。
pub(crate) fn optional_string_lenient<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        _ => Ok(None),
    }
}

/// 缺欄位或內容不是物件時回傳 `None` 的可選物件欄位。
///
/// 伺服器對同一欄位的型別並不總是穩定（例如活動正文的 `data`）：把非物件的內容
/// 一律視為「沒有這個區塊」，比讓整份回應解析失敗安全——詳情解析失敗會使該課程
/// 的作業全部退回「待核实」，而此處損失的只是一段說明。
pub(crate) fn optional_object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(value @ serde_json::Value::Object(_)) => serde_json::from_value(value)
            .map(Some)
            .map_err(|_| serde::de::Error::custom("物件内容无法解析")),
        _ => Ok(None),
    }
}

#[cfg(test)]
#[path = "tests/mod_test.rs"]
mod mod_test;
