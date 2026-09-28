//! 回應解析錯誤訊息測試：只描述類別，不夾帶原始欄位值。

use super::*;
use crate::error::AppError;
use crate::http::HttpResponse;

#[test]
fn envelope_without_integer_code_is_a_protocol_error() {
    // 缺 `code`：不應回報成假業務錯誤（例如 -1），應是協定格式錯誤。
    let missing = HttpResponse::new(200, "https://example", r#"{"data":{}}"#);
    let err =
        unwrap_envelope::<serde_json::Value>(&missing, "查询学期").expect_err("缺少 code 应失败");
    assert!(matches!(err, AppError::Protocol(_)), "应为协定错误：{err}");

    // 字串碼（學校可能以字串序列化）同樣是協定不符，不可假裝成業務錯誤。
    let stringy = HttpResponse::new(200, "https://example", r#"{"code":"0","data":{}}"#);
    let err =
        unwrap_envelope::<serde_json::Value>(&stringy, "查询学期").expect_err("字串 code 应失败");
    assert!(matches!(err, AppError::Protocol(_)), "应为协定错误：{err}");
}

#[test]
fn envelope_business_error_keeps_code_and_message() {
    let response = HttpResponse::new(200, "https://example", r#"{"code":12,"message":"未登录"}"#);
    let err = unwrap_envelope::<serde_json::Value>(&response, "查询课表").expect_err("业务错误");
    match err {
        AppError::Server { code, message } => {
            assert_eq!(code, 12);
            assert!(message.contains("查询课表"), "应保留上下文：{message}");
            assert!(message.contains("未登录"), "应保留服务端訊息：{message}");
        }
        other => panic!("应为服务器业务错误，实际：{other}"),
    }
}

#[test]
fn deserialize_error_does_not_include_field_values() {
    #[derive(serde::Deserialize, Debug)]
    struct Target {
        #[allow(dead_code)]
        id: u32,
    }

    let value = serde_json::json!({ "id": "示例课程名称" });
    let err = deserialize_value::<Target>(value, "课程列表").expect_err("应解析失败");
    let message = err.to_string();
    assert!(message.contains("课程列表"), "应保留上下文：{message}");
    assert!(
        message.contains("数据类型不符"),
        "应说明失败类别：{message}"
    );
    assert!(
        !message.contains("示例课程名称"),
        "不应包含欄位值：{message}"
    );
}

#[test]
fn tolerant_field_errors_describe_type_only() {
    #[derive(serde::Deserialize, Debug)]
    struct Target {
        #[serde(deserialize_with = "string_or_number")]
        #[allow(dead_code)]
        id: String,
    }

    let value = serde_json::json!({ "id": ["数组内容"] });
    let err = deserialize_value::<Target>(value, "课程").expect_err("应解析失败");
    let message = err.to_string();
    assert!(message.contains("课程"), "应保留上下文：{message}");
    assert!(!message.contains("数组内容"), "不应包含欄位值：{message}");
    assert!(
        !message.contains("期望字符串或数字"),
        "不应包含内部訊息：{message}"
    );
}
