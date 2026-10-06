//! 學校 API 共用的 JSON 外殼解碼。
//!
//! 統一身份認證（`login.xjtu.edu.cn/attest/**`）與考勤 API 都使用
//! `{code, message, data}` 外殼：`code` 為 0 時 `data` 是實際負載，非 0 時
//! 代表業務錯誤。兩處呼叫端共用同一份嚴格解碼規則：缺 `code` 或型別不符
//! 一律視為協定格式錯誤，不得以假業務碼（例如 -1）回報，否則真正的格式
//! 問題會被誤認為學校端的業務錯誤、歸因錯誤。

use crate::error::{AppError, AppResult};
use crate::http::HttpResponse;
use crate::text::{MAX_INLINE_CHARS, sanitize_inline};

/// 解開 `{code, message, data}` 外殼，回傳 `data` 的值。
///
/// `context` 用於錯誤訊息前缀（例如「核验短信验证码」），讓使用者知道是
/// 哪個階段的回應格式不符；錯誤訊息不含回應內容。
pub fn split_envelope(response: &HttpResponse, context: &str) -> AppResult<serde_json::Value> {
    response.error_for_status()?;
    let value: serde_json::Value = response.json()?;
    match value.get("code").and_then(serde_json::Value::as_i64) {
        None => Err(AppError::protocol(format!(
            "{context} 响应缺少整数 code 字段"
        ))),
        Some(0) => Ok(value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
        Some(code) => {
            let message = value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("未知错误");
            // 伺服器訊息直接進入介面（通知列、作業狀態文字）：先清理控制字元
            // 並限制長度，不讓異常回應污染畫面。
            Err(AppError::Server {
                code,
                message: format!("{context}：{}", sanitize_inline(message, MAX_INLINE_CHARS)),
            })
        }
    }
}

#[cfg(test)]
#[path = "tests/json_test.rs"]
mod json_test;
