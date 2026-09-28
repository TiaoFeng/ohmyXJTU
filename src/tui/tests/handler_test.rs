//! 按鍵處理測試：表單驗證、任務送出與導航觸發載入。

use std::sync::mpsc::{Sender, channel};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::AccessPolicy;
use crate::task::Job;
use crate::tui::app::{App, FormState, NavItem, Page, ScheduleData, Screen};

use super::handle_key;

fn press(app: &mut App, jobs: &Sender<Job>, code: KeyCode) {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE), jobs);
}

fn type_text(app: &mut App, jobs: &Sender<Job>, text: &str) {
    for character in text.chars() {
        press(app, jobs, KeyCode::Char(character));
    }
}

/// 依序填入設定表單的所有欄位。
fn fill_setup(app: &mut App, jobs: &Sender<Job>, passphrase: &str, confirm: &str, password: &str) {
    type_text(app, jobs, passphrase);
    press(app, jobs, KeyCode::Tab);
    type_text(app, jobs, confirm);
    press(app, jobs, KeyCode::Tab);
    type_text(app, jobs, "3120000001");
    press(app, jobs, KeyCode::Tab);
    type_text(app, jobs, password);
    press(app, jobs, KeyCode::Tab);
    type_text(app, jobs, password);
}

#[test]
fn setup_form_creates_vault() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));

    fill_setup(&mut app, &jobs, "secret123", "secret123", "pw-12345");
    press(&mut app, &jobs, KeyCode::Enter);

    match rx.try_recv() {
        Ok(Job::CreateVault {
            passphrase,
            credentials,
        }) => {
            assert_eq!(passphrase, "secret123");
            assert_eq!(credentials.username, "3120000001");
            assert_eq!(credentials.password, "pw-12345");
        }
        other => panic!("应为创建凭证任务，实际为 {other:?}"),
    }
    assert!(matches!(app.screen, Screen::Login(_)));
}

#[test]
fn setup_form_rejects_short_passphrase() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));

    fill_setup(&mut app, &jobs, "123", "123", "pw-12345");
    press(&mut app, &jobs, KeyCode::Enter);

    assert!(rx.try_recv().is_err(), "口令过短时不应送出任务");
    let Screen::Setup(form) = &app.screen else {
        panic!("应停留在设定画面");
    };
    assert!(form.error.is_some());
}

#[test]
fn setup_form_rejects_mismatched_passphrase() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));

    fill_setup(&mut app, &jobs, "secret123", "secret999", "pw-12345");
    press(&mut app, &jobs, KeyCode::Enter);

    assert!(rx.try_recv().is_err(), "两次口令不一致时不应送出任务");
}

#[test]
fn setup_form_rejects_mismatched_password() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));

    type_text(&mut app, &jobs, "secret123");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "secret123");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "3120000001");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "pw-12345");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "pw-99999");
    press(&mut app, &jobs, KeyCode::Enter);

    assert!(rx.try_recv().is_err(), "两次密码不一致时不应送出任务");
}

#[test]
fn unlock_form_sends_unlock_job() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);

    type_text(&mut app, &jobs, "secret123");
    press(&mut app, &jobs, KeyCode::Enter);

    assert!(matches!(
        rx.try_recv(),
        Ok(Job::Unlock { passphrase }) if passphrase == "secret123"
    ));
    assert!(matches!(app.screen, Screen::Login(_)));
}

#[test]
fn unlock_form_requires_passphrase() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err());
    let Screen::Unlock(form) = &app.screen else {
        panic!("应停留在解锁画面");
    };
    assert_eq!(form.error.as_deref(), Some("请输入加密口令"));
}

#[test]
fn control_u_clears_the_focused_field() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    type_text(&mut app, &jobs, "draft");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &jobs,
    );

    let Screen::Unlock(form) = &app.screen else {
        panic!("应停留在解锁画面");
    };
    assert!(form.focused().expect("聚焦字段").value.is_empty());
}

#[test]
fn changing_page_requests_data() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);

    press(&mut app, &jobs, KeyCode::Right);
    assert_eq!(app.nav, NavItem::Homework);
    assert!(matches!(rx.try_recv(), Ok(Job::LoadHomework)));
    assert!(app.homework.is_loading());

    press(&mut app, &jobs, KeyCode::Right);
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 1 })));

    press(&mut app, &jobs, KeyCode::Right);
    assert!(matches!(rx.try_recv(), Ok(Job::LoadCourses)));
}

#[test]
fn refresh_keeps_flow_page() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::tui::app::FlowData {
        records: Vec::new(),
        page: 3,
        total_pages: 5,
        total: 0,
    });

    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 3 })));
}

#[test]
fn flow_paging_respects_bounds() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::tui::app::FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 2,
        total: 0,
    });

    // 已在第一頁，往上一頁不應送出任務。
    press(&mut app, &jobs, KeyCode::Char('p'));
    assert!(rx.try_recv().is_err());

    press(&mut app, &jobs, KeyCode::Char('n'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 2 })));
}

#[test]
fn enter_toggles_details_and_escape_closes() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.schedule = Page::Ready(ScheduleData::default());

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(app.schedule_detail);

    press(&mut app, &jobs, KeyCode::Esc);
    assert!(!app.schedule_detail);
}

#[test]
fn control_p_opens_and_closes_settings() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &jobs,
    );
    assert!(matches!(app.screen, Screen::Settings(_)));

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &jobs,
    );
    assert!(matches!(app.screen, Screen::Main));
}

#[test]
fn access_policy_can_be_changed_from_settings() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(crate::tui::app::SettingsState {
        index: 2,
    }));

    press(&mut app, &jobs, KeyCode::Right);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetAccessPolicy(AccessPolicy::Direct))
    ));

    // enter 亦能循環切換訪問模式。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetAccessPolicy(AccessPolicy::Direct))
    ));
}

#[test]
fn settings_opens_account_forms() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(crate::tui::app::SettingsState {
        index: 0,
    }));

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::SettingsForm(_)));

    // esc 回到設定選單，而不是主畫面。
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Settings(_)));
}

#[test]
fn login_captcha_submission_sends_job() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Login(Box::new(
        crate::tui::app::LoginScreen::Captcha {
            path: std::path::PathBuf::from("/tmp/captcha.png"),
            input: crate::tui::text::InputLine::new(),
            error: None,
        },
    )));

    type_text(&mut app, &jobs, "a1b2");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SubmitCaptcha(code)) if code == "a1b2"
    ));

    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(rx.try_recv(), Ok(Job::RefreshCaptcha)));
}

#[test]
fn login_mfa_requires_code_then_sends() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Login(Box::new(crate::tui::app::LoginScreen::Mfa {
        phone: Some("138****8888".to_owned()),
        sent: true,
        input: crate::tui::text::InputLine::new(),
        error: None,
    })));

    // 空輸入按 enter 不應送出。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err());

    type_text(&mut app, &jobs, "123456");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::VerifyMfaCode(code)) if code == "123456"
    ));
}

#[test]
fn quitting_sets_flag() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    press(&mut app, &jobs, KeyCode::Char('q'));
    assert!(app.quit);
}
