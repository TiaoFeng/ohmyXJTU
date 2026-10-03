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
        unwrap_envelope::<serde_json::Value>(&stringy, "查询学期").expect_err("字符串 code 应失败");
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
            assert!(message.contains("未登录"), "应保留服务端信息：{message}");
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
        "不应包含字段值：{message}"
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
    assert!(!message.contains("数组内容"), "不应包含字段值：{message}");
    assert!(
        !message.contains("期望字符串或数字"),
        "不应包含内部信息：{message}"
    );
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
        deserialize_value(serde_json::json!({ "text": "正文" }), "活动详情").expect("字符串应解析");
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

/// 列表欄位的寬容解析：缺欄位、非陣列與個別項目異常都不得讓整份回應失敗。
#[test]
fn lenient_array_tolerates_missing_and_malformed_values() {
    #[derive(serde::Deserialize)]
    struct Target {
        #[serde(default, deserialize_with = "lenient_array")]
        items: Vec<Item>,
    }

    #[derive(serde::Deserialize)]
    struct Item {
        #[serde(default, deserialize_with = "optional_string_lenient")]
        name: Option<String>,
    }

    let missing: Target = serde_json::from_str("{}").expect("缺少字段应可用");
    assert!(missing.items.is_empty());

    for json in [
        r#"{"items":null}"#,
        r#"{"items":{}}"#,
        r#"{"items":0}"#,
        r#"{"items":"a.pdf"}"#,
    ] {
        let target: Target = serde_json::from_str(json).expect("非数组应可用");
        assert!(target.items.is_empty(), "{json}");
    }

    // 個別項目型別異常時跳過該項，其餘照常解析（與 `parse_lenient` 同精神）。
    let target: Target =
        serde_json::from_str(r#"{"items":[{"name":"a"},7,null,{"name":"b"}]}"#).expect("可解析");
    let names: Vec<&str> = target
        .items
        .iter()
        .filter_map(|item| item.name.as_deref())
        .collect();
    assert_eq!(names, ["a", "b"]);
}

/// 數量欄位的寬容解析：只有數字與可解析的數字字串才算數。
#[test]
fn optional_u64_lenient_accepts_numbers_and_numeric_strings() {
    fn size(json: &str) -> Option<u64> {
        #[derive(serde::Deserialize)]
        struct Target {
            #[serde(default, deserialize_with = "optional_u64_lenient")]
            size: Option<u64>,
        }
        serde_json::from_str::<Target>(json)
            .unwrap_or_else(|err| panic!("{json}: {err}"))
            .size
    }

    assert_eq!(size(r#"{"size":2048}"#), Some(2048));
    assert_eq!(size(r#"{"size":"2048"}"#), Some(2048));
    assert_eq!(size(r#"{"size":" 512 "}"#), Some(512));
    // 缺欄位、非數字字串、負數、小數與非純量一律視為沒有這個數字。
    assert_eq!(size("{}"), None);
    assert_eq!(size(r#"{"size":"大小不明"}"#), None);
    assert_eq!(size(r#"{"size":-1}"#), None);
    assert_eq!(size(r#"{"size":1.5}"#), None);
    assert_eq!(size(r#"{"size":{"n":1}}"#), None);
    assert_eq!(size(r#"{"size":null}"#), None);
}
