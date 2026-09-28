//! 登入頁解析測試（使用 `tests/fixtures` 下的脫敏樣本）。

use super::*;
use crate::auth::state::AccountType;

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).expect("读取 fixture")
}

#[test]
fn extracts_execution_value() {
    let html = fixture("login_page.html");
    assert_eq!(
        execution_value(&html).as_deref(),
        Some("e1s1-fixture-execution")
    );
    assert_eq!(input_value(&html, "_eventId").as_deref(), Some("submit"));
    assert_eq!(input_value(&html, "not-exist"), None);
}

#[test]
fn target_page_has_no_execution_value() {
    let html = fixture("target_page.html");
    assert_eq!(execution_value(&html), None);
    assert!(!is_safety_verify_page(&html));
    assert_eq!(account_choices(&html), None);
    assert_eq!(alert_message(&html), None);
}

#[test]
fn parses_mfa_enabled_both_values() {
    assert!(mfa_enabled(&fixture("login_page.html")));
    assert!(!mfa_enabled(&fixture("account_choice_page.html")));
    // 找不到設定時必須保守地視為需要 MFA。
    assert!(mfa_enabled(&fixture("target_page.html")));
    // 字串形式的 "true" 也算啟用。
    let html =
        r#"<script>var globalConfig = eval('(' + "{\"mfaEnabled\":\"true\"}" + ')');</script>"#;
    assert!(mfa_enabled(html));
}

#[test]
fn parses_account_choices() {
    let choices = account_choices(&fixture("account_choice_page.html")).expect("应有身份选项");
    assert_eq!(choices.len(), 2);
    assert_eq!(choices[0].name, "本科生");
    assert_eq!(choices[0].label, "3120000001-1");
    assert_eq!(choices[1].name, "研究生");
}

#[test]
fn selects_undergraduate_label() {
    let choices = account_choices(&fixture("account_choice_page.html")).unwrap();
    assert_eq!(
        AccountType::Undergraduate.select(&choices),
        Some("3120000001-1")
    );
    assert_eq!(
        AccountType::Postgraduate.select(&choices),
        Some("3120000001-2")
    );
    assert_eq!(AccountType::Undergraduate.select(&[]), None);
}

#[test]
fn parses_alert_message() {
    let alert = alert_message(&fixture("login_failed.html")).expect("应有错误提示");
    assert_eq!(alert.text(), "用户名或密码错误");
    assert_eq!(alert.title, "用户名或密码错误");
}

#[test]
fn detects_safety_verify_page() {
    assert!(is_safety_verify_page(&fixture("safety_verify_page.html")));
    assert!(!is_safety_verify_page(&fixture("login_page.html")));
    // 有 secState 表單但沒有二次認證特徵時不算。
    let html = r#"<form id="fm1"><input name="secState" value="s" />
        <input name="execution" value="e" /><input name="_eventId" value="submit" /></form>"#;
    assert!(!is_safety_verify_page(html));
}

#[test]
fn malformed_html_does_not_panic() {
    assert_eq!(execution_value("<input name="), None);
    assert!(!is_safety_verify_page("<<<>>>"));
    assert_eq!(account_choices("<div class=\"account-wrap\">"), None);
    assert_eq!(alert_message(""), None);
}
