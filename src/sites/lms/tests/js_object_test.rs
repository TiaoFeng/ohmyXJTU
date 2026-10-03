//! 寬容 JavaScript 物件解析測試（使用脫敏的固定樣本）。
//!
//! 對應參考實作 `ref/lms/lms.py::_parse_js_object` 的語義：未加引號的鍵、
//! `None`、尾逗號都必須能解析；名稱比對必須是獨立識別字（不得命中 `userNo`）。

use serde_json::json;

use super::*;

#[test]
fn parses_loose_javascript_objects() {
    // 未加引號的鍵、None、尾逗號（真實頁面的 globalData 形態）。
    let page = "{ user: { id: 7788, name: \"张三\", role: Student, dept: None, }, \
                dept: { id: 7 }, locale: \"zh\" }";
    let user = parse_js_object(page, "user", "dept").expect("应解析 user 子对象");
    assert_eq!(user["id"], json!(7788));
    assert_eq!(user["name"], json!("张三"));
    assert_eq!(user["role"], json!("Student"));
    assert_eq!(user["dept"], json!(null));
}

#[test]
fn parses_quoted_keys_and_escapes() {
    // 嚴格 JSON 的寫法也要支援（字串中的括號與轉義引號不得干擾配對）。
    let page = r#"{ "user": {"id":7788,"name":"张三","note":"} 不是结尾 \" 引号"}, "dept": {} }"#;
    let user = parse_js_object(page, "user", "dept").expect("应解析");
    assert_eq!(user["id"], json!(7788));
    assert_eq!(user["note"], json!("} 不是结尾 \" 引号"));
}

#[test]
fn requires_boundary_between_members() {
    // `dept` 不在 user 之後：保守回 None，不得命中其他位置的同名物件。
    let page = "{ dept: { id: 1 }, user: { id: \"abc\" } }";
    assert!(parse_js_object(page, "user", "dept").is_none());
}

#[test]
fn parses_nested_arrays_and_unicode_escapes() {
    let page = "{ user: { id: 1, roles: [\"student\", 2], tag: \"a\\u0031\" }, dept: {} }";
    let user = parse_js_object(page, "user", "dept").expect("应解析");
    assert_eq!(user["roles"], json!(["student", 2]));
    assert_eq!(user["tag"], json!("a1"));
}

#[test]
fn find_named_value_accepts_assignment_and_member() {
    let page = "var other = 1; var globalData = {\"user\":{\"id\":42}};";
    let value = find_named_value(page, "globalData").expect("应找到 globalData");
    assert_eq!(value["user"]["id"], json!(42));

    // 物件成員寫法（`globalData: {…}`）亦可。
    let page = "window.globalData: { user: { id: 7 } }";
    assert_eq!(
        find_named_value(page, "globalData").expect("成员写法")["user"]["id"],
        json!(7)
    );
}

#[test]
fn rejects_invalid_or_partial_input() {
    assert!(parse_js_object("user: { id: 1", "user", "dept").is_none());
    assert!(parse_js_object("no identifiers here", "user", "dept").is_none());
    // 非物件值（`user: 42`）不視為使用者物件。
    assert!(parse_js_object("user: 42, dept: {}", "user", "dept").is_none());
    assert!(find_named_value("var globalData = ", "globalData").is_none());
    // 名稱必須是獨立識別字，不得命中更長的鍵名。
    assert!(find_named_value("var globalDataExtra = {};", "globalData").is_none());
}

#[test]
fn handles_identity_escapes_with_multibyte_characters() {
    // `\` 緊接多位元組字元（JS identity escape）：`\中` 等同 `中`。
    // 修復前解析器只前進一個位元組，會對非字元邊界切片而 panic。
    let page = "{ user: { name: \"a\\中b\", tag: \"x\\🙂y\" }, dept: {} }";
    let user = parse_js_object(page, "user", "dept").expect("应解析");
    assert_eq!(user["name"], json!("a中b"));
    assert_eq!(user["tag"], json!("x🙂y"));
}

#[test]
fn keeps_ascii_escapes_and_rejects_trailing_backslash() {
    let page =
        r#"{ user: { path: "a\/b", win: "c:\\d", quote: "e\"f", esc: "\u4e2d" }, dept: {} }"#;
    let user = parse_js_object(page, "user", "dept").expect("应解析");
    assert_eq!(user["path"], json!("a/b"));
    assert_eq!(user["win"], json!("c:\\d"));
    assert_eq!(user["quote"], json!("e\"f"));
    assert_eq!(user["esc"], json!("中"));

    // 結尾反斜線未閉合：保守回 None，不得 panic。
    assert!(parse_js_object("{ user: { a: \"abc\\", "user", "dept").is_none());
    // 非法 `\u` 跳脫：解析失敗回 None。
    assert!(parse_js_object("{ user: { a: \"\\uZZZZ\" }, dept: {} }", "user", "dept").is_none());
}

/// 巢狀深度上限：超限回 `None`，不得遞迴到堆疊溢位。
///
/// 修復前這裡的輸入會讓解析器遞迴上萬層（工作執行緒堆疊 2 MiB）而直接
/// abort；上限生效後只有前 `MAX_DEPTH` 層會被走訪，回應極快。
#[test]
fn refuses_values_nested_deeper_than_the_limit() {
    const DEPTH: usize = 50_000;

    // 深層巢狀陣列（`globalData` 的指派語句）。
    let deep_arrays = format!(
        "var globalData = {}{};",
        "[".repeat(DEPTH),
        "]".repeat(DEPTH)
    );
    assert!(find_named_value(&deep_arrays, "globalData").is_none());

    // 深層巢狀物件（`user` 子物件）。
    let deep_objects = format!("{}1{}", "{ a: ".repeat(DEPTH), " }".repeat(DEPTH));
    let page = format!("{{ user: {deep_objects}, dept: {{}} }}");
    assert!(parse_js_object(&page, "user", "dept").is_none());
}

/// 上限之內的巢狀結構仍要能完整解析。
#[test]
fn still_parses_nesting_within_the_limit() {
    let nested = "{ a: { b: [ { c: 1 } ] } }";
    let page = format!("{{ user: {nested}, dept: {{}} }}");
    let user = parse_js_object(&page, "user", "dept").expect("应解析");
    assert_eq!(user["a"]["b"][0]["c"], json!(1));
}
