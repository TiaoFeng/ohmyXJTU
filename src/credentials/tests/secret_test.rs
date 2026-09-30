//! [`Secret`] 的行為測試：遮罩、檢視、清空與轉換。

use super::*;

#[test]
fn debug_is_masked_and_never_contains_the_value() {
    let secret = Secret::from("super-secret-passphrase");
    let debug = format!("{secret:?}");
    assert_eq!(debug, "Secret(<redacted>)");
    assert!(
        !debug.contains("super-secret"),
        "Debug 不得洩漏內容：{debug}"
    );

    // 巢狀結構的 Debug 輸出也不含內容。
    let nested = format!("{:?}", vec![Secret::from("another-secret")]);
    assert!(!nested.contains("another-secret"), "嵌套 Debug：{nested}");
}

#[test]
fn view_conversions_and_equality() {
    let from_string = Secret::from("secret123".to_owned());
    let from_str = Secret::from("secret123");
    assert_eq!(from_string.as_str(), "secret123");
    assert_eq!(from_string, from_str, "相同內容應相等");
    assert_eq!(from_string, "secret123", "應可與 &str 比較");
    assert_ne!(from_string, Secret::from("other"));

    // Deref 讓 &Secret 可直接作為 &str 使用。
    let as_str: &str = &from_string;
    assert_eq!(as_str.len(), 9);
}

#[test]
fn clear_overwrites_and_empties() {
    let mut secret = Secret::from("secret123");
    assert!(!secret.is_empty());

    secret.clear();
    assert!(secret.is_empty());
    assert_eq!(secret.as_str(), "");
}

#[test]
fn clone_is_independent() {
    let original = Secret::from("secret123");
    let copy = original.clone();
    assert_eq!(copy, "secret123");
    // 修改（清空）副本不影響原件。
    let mut copy = copy;
    copy.clear();
    assert_eq!(original, "secret123");
}
