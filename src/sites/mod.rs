//! 站點客戶端：考勤系統與思源學堂。

pub mod attendance;
pub mod lms;

use serde::Deserialize as _;
use serde::de::DeserializeOwned;

use crate::error::{AppError, AppResult};
use crate::http::HttpResponse;

/// 解開 `{code, message, data}` 外殼，並把 `data` 解析為指定型別。
pub fn unwrap_envelope<T: DeserializeOwned>(
    response: &HttpResponse,
    context: &str,
) -> AppResult<T> {
    response.error_for_status()?;
    let value: serde_json::Value = response.json()?;
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    if code != 0 {
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("未知错误");
        return Err(AppError::Server {
            code,
            message: format!("{context}：{message}"),
        });
    }
    deserialize_value(
        value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        context,
    )
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
            describe_parse_failure(err.classify())
        ))
    })
}

/// 解析失敗的類別描述（不含原始欄位值）。
fn describe_parse_failure(category: serde_json::error::Category) -> &'static str {
    match category {
        serde_json::error::Category::Io => "读取失败",
        serde_json::error::Category::Syntax => "语法错误",
        serde_json::error::Category::Data => "数据类型不符",
        serde_json::error::Category::Eof => "内容不完整",
    }
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

#[cfg(test)]
#[path = "tests/mod_test.rs"]
mod mod_test;
