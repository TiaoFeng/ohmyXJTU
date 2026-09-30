//! 回應解析錯誤訊息測試：只描述類別，不夾帶原始欄位值。

use super::*;

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
