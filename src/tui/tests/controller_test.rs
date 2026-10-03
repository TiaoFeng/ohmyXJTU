//! controller 的表單語意測試：欄位角色到 FormValues 的對應。

use super::*;
use crate::session::SiteKind;
use crate::tui::app::FormState;

/// 依畫面順序設定每個欄位的值。
fn set_values(form: &mut FormState, values: &[&str]) {
    assert_eq!(form.fields.len(), values.len(), "字段数应与测试数据相符");
    for (field, value) in form.fields.iter_mut().zip(values) {
        field.value.set(*value);
    }
}

#[test]
fn setup_values_map_by_role() {
    let mut form = FormState::setup();
    set_values(&mut form, &["p1", "p2", "user", "pw1", "pw2"]);
    let values = FormValues::from_form(&form);
    assert_eq!(values.passphrase, "p1");
    assert_eq!(values.passphrase_confirm, "p2");
    assert_eq!(values.username, "user");
    assert_eq!(values.password, "pw1");
    assert_eq!(values.password_confirm, "pw2");
}

#[test]
fn login_retry_values_map_by_role() {
    // 登入重試表單的順序與其他表單不同（帳號在最前），對應仍以角色為準。
    let mut form = FormState::login_retry(SiteKind::Attendance);
    set_values(&mut form, &["user", "pw", "pass"]);
    let values = FormValues::from_form(&form);
    assert_eq!(values.username, "user");
    assert_eq!(values.password, "pw");
    assert_eq!(values.passphrase, "pass");
}

#[test]
fn account_and_passphrase_forms_map_by_role() {
    let mut form = FormState::change_account();
    set_values(&mut form, &["old-pass", "new-user", "pw1", "pw2"]);
    let values = FormValues::from_form(&form);
    assert_eq!(values.passphrase, "old-pass");
    assert_eq!(values.username, "new-user");
    assert_eq!(values.password, "pw1");
    assert_eq!(values.password_confirm, "pw2");

    let mut form = FormState::change_passphrase();
    set_values(&mut form, &["old-pass", "new-pass", "new-pass"]);
    let values = FormValues::from_form(&form);
    assert_eq!(values.passphrase, "old-pass");
    assert_eq!(values.password, "new-pass");
    assert_eq!(values.password_confirm, "new-pass");
}

#[test]
fn change_passphrase_form_builds_job_with_role_values() {
    let mut form = FormState::change_passphrase();
    set_values(
        &mut form,
        &["old-passphrase", "new-passphrase", "new-passphrase"],
    );
    let values = FormValues::from_form(&form);
    let job = build_job(form.kind, &values).expect("应可构建任务");
    let Job::ChangePassphrase { old, new } = job else {
        panic!("应为修改口令任务：{job:?}");
    };
    assert_eq!(old, "old-passphrase");
    assert_eq!(new, "new-passphrase");
}
