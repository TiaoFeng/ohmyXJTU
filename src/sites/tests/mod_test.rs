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

/// 可選物件欄位：缺欄位或非物件一律視為「沒有這個區塊」。
///
/// 伺服器對同一欄位的型別並不總是穩定（例如活動正文的 `data`）：一段可選內容
/// 不應該毀掉整份解析。
#[test]
fn optional_object_tolerates_non_object_values() {
    #[derive(serde::Deserialize, Debug)]
    struct Body {
        #[serde(default)]
        text: Option<String>,
    }

    #[derive(serde::Deserialize, Debug)]
    struct Target {
        #[serde(default, deserialize_with = "optional_object")]
        data: Option<Body>,
    }

    for value in [
        serde_json::json!({}),
        serde_json::json!({ "data": null }),
        serde_json::json!({ "data": "" }),
        serde_json::json!({ "data": "<p>整份是字串</p>" }),
        serde_json::json!({ "data": [1, 2] }),
        serde_json::json!({ "data": 12 }),
    ] {
        let target: Target = deserialize_value(value.clone(), "活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert!(target.data.is_none(), "{value}");
    }

    let target: Target = deserialize_value(
        serde_json::json!({ "data": { "text": "正文" } }),
        "活动详情",
    )
    .expect("物件应正常解析");
    assert_eq!(
        target.data.and_then(|body| body.text).as_deref(),
        Some("正文")
    );

    // 物件「內部」型別不符仍是協定錯誤：可見的失敗，不得靜默吞掉。
    let err =
        deserialize_value::<Target>(serde_json::json!({ "data": { "text": 42 } }), "活动详情")
            .expect_err("內部型別不符应失败");
    let message = err.to_string();
    assert!(message.contains("数据类型不符"), "{message}");
    assert!(!message.contains("42"), "不应包含欄位值：{message}");
}

/// 可選字串欄位：只接受字串，其餘型別一律視為「沒有這段內容」。
///
/// 用於純展示用的文字欄位（活動說明）：型別異常時只應損失該段說明，不應讓整份
/// 回應解析失敗。
#[test]
fn optional_string_lenient_ignores_non_string_values() {
    #[derive(serde::Deserialize, Debug)]
    struct Target {
        #[serde(default, deserialize_with = "optional_string_lenient")]
        text: Option<String>,
    }

    // 字串正常解析。
    let target: Target =
        deserialize_value(serde_json::json!({ "text": "正文" }), "活动详情").expect("字串应解析");
    assert_eq!(target.text.as_deref(), Some("正文"));

    // 缺欄位或非字串（含 null）一律回 None，且不得報錯。
    for value in [
        serde_json::json!({}),
        serde_json::json!({ "text": null }),
        serde_json::json!({ "text": 42 }),
        serde_json::json!({ "text": true }),
        serde_json::json!({ "text": [1, 2] }),
        serde_json::json!({ "text": { "a": 1 } }),
    ] {
        let target: Target = deserialize_value(value.clone(), "活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert!(target.text.is_none(), "{value}");
    }
}
