//! `split_envelope` 的外殼解碼測試：缺 code、業務錯誤與成功三態。

use super::split_envelope;
use crate::error::AppError;
use crate::http::HttpResponse;

#[test]
fn success_returns_data_value() {
    let response = HttpResponse::new(200, "https://example", r#"{"code":0,"data":{"status":2}}"#);
    let data = split_envelope(&response, "核验短信验证码").expect("code 0 应成功");
    assert_eq!(
        data.get("status").and_then(serde_json::Value::as_i64),
        Some(2)
    );
}

#[test]
fn missing_code_is_a_protocol_error() {
    let response = HttpResponse::new(200, "https://example", r#"{"data":{}}"#);
    let err = split_envelope(&response, "核验短信验证码").expect_err("缺少 code 应失败");
    assert!(matches!(err, AppError::Protocol(_)), "应为协定错误：{err}");
    let message = err.to_string();
    assert!(
        message.contains("核验短信验证码"),
        "应保留上下文：{message}"
    );
}

#[test]
fn non_integer_code_is_a_protocol_error() {
    let response = HttpResponse::new(200, "https://example", r#"{"code":"0","data":{}}"#);
    let err = split_envelope(&response, "查询学期").expect_err("字串 code 应失败");
    assert!(matches!(err, AppError::Protocol(_)), "应为协定错误：{err}");
}

#[test]
fn business_error_keeps_code_and_prefixed_message() {
    let response = HttpResponse::new(
        200,
        "https://example",
        r#"{"code":7,"message":"验证码错误"}"#,
    );
    let err = split_envelope(&response, "核验短信验证码").expect_err("业务错误应失败");
    match err {
        AppError::Server { code, message } => {
            assert_eq!(code, 7);
            assert!(message.contains("核验短信验证码"), "应带上下文：{message}");
            assert!(
                message.contains("验证码错误"),
                "应保留服务端訊息：{message}"
            );
        }
        other => panic!("应为服务器业务错误，实际：{other}"),
    }
}

#[test]
fn business_error_without_message_uses_placeholder() {
    let response = HttpResponse::new(200, "https://example", r#"{"code":-1}"#);
    let err = split_envelope(&response, "查询学期").expect_err("业务错误应失败");
    let AppError::Server { code, message } = err else {
        panic!("应为服务器业务错误");
    };
    assert_eq!(code, -1);
    assert!(
        message.contains("未知错误"),
        "缺 message 时应使用占位：{message}"
    );
}

#[test]
fn http_error_is_reported_as_http() {
    let response = HttpResponse::new(502, "https://example", "bad gateway");
    let err = split_envelope(&response, "查询学期").expect_err("非 2xx 应失败");
    assert!(matches!(err, AppError::Http { status: 502 }));
}

#[test]
fn invalid_json_is_a_protocol_error() {
    let response = HttpResponse::new(200, "https://example", "not json");
    let err = split_envelope(&response, "查询学期").expect_err("非 JSON 应失败");
    assert!(matches!(err, AppError::Protocol(_)), "应为协定错误：{err}");
}
