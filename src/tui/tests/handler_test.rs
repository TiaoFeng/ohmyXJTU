//! 按鍵處理測試：表單驗證、任務送出與導航觸發載入。

use std::sync::mpsc::{Sender, channel};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::AccessPolicy;
use crate::task::Job;
use crate::tui::app::{
    App, FormKind, FormState, LoginScreen, NavItem, Page, ScheduleData, Screen, SettingsState,
};

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
    assert!(matches!(rx.try_recv(), Ok(Job::LoadHomework { .. })));
    assert!(app.homework.is_loading());

    press(&mut app, &jobs, KeyCode::Right);
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 1 })));

    press(&mut app, &jobs, KeyCode::Right);
    assert!(matches!(rx.try_recv(), Ok(Job::LoadCourses { .. })));
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
fn settings_policy_draft_updates_locally_without_tasks() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));
    // 選到「訪問模式」。
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Down);

    // 右鍵：草稿即時變為 Direct，不送任務。
    press(&mut app, &jobs, KeyCode::Right);
    let Screen::Settings(state) = app.screen else {
        panic!("应停留在设定弹窗");
    };
    assert_eq!(state.policy(app.access_policy), AccessPolicy::Direct);
    assert!(rx.try_recv().is_err(), "调整草稿不应送出任务");

    // 左鍵回到 Auto。
    press(&mut app, &jobs, KeyCode::Left);
    let Screen::Settings(state) = app.screen else {
        panic!("应停留在设定弹窗");
    };
    assert_eq!(state.policy(app.access_policy), AccessPolicy::Auto);
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_policy_enter_submits_once_then_locks() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Right);
    press(&mut app, &jobs, KeyCode::Enter);

    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetAccessPolicy(AccessPolicy::Direct))
    ));

    // 保存中：重複 enter 不重送。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "保存中不应重复提交");

    // esc 關閉且不送任務。
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Main));
    assert!(rx.try_recv().is_err());
}

#[test]
fn settings_policy_enter_without_changes_sends_nothing() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "未变更时不应送出任务");
}

#[test]
fn settings_opens_account_forms() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));

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

/// 登入失敗畫面。
fn failed_app(message: &str) -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Login(Box::new(LoginScreen::Failed {
        message: message.to_owned(),
    })));
    app
}

/// 目前登入憑證表單（測試輔助）。
fn credentials_form(app: &App) -> &FormState {
    let Screen::Login(screen) = &app.screen else {
        panic!("应停留在登录画面");
    };
    let LoginScreen::Credentials { form, .. } = screen.as_ref() else {
        panic!("应处于凭证表单");
    };
    form
}

#[test]
fn failed_screen_retries_with_saved_credentials_or_quits() {
    let (jobs, rx) = channel();
    let mut app = failed_app("登录失败：用户名或密码错误");

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::RetryLogin)));
    assert!(
        matches!(app.screen, Screen::Login(_)),
        "重試時應顯示進度畫面"
    );

    press(&mut app, &jobs, KeyCode::Char('q'));
    assert!(app.quit);
}

#[test]
fn failed_screen_opens_credentials_form() {
    let (jobs, _rx) = channel();
    let mut app = failed_app("登录失败：用户名或密码错误");

    press(&mut app, &jobs, KeyCode::Char('e'));

    let form = credentials_form(&app);
    assert_eq!(form.kind, FormKind::LoginRetry);
    assert!(
        form.fields.iter().all(|field| field.value.is_empty()),
        "重新输入时字段必须为空"
    );
    assert!(!form.fields[0].value.is_masked(), "账号不必遮蔽");
    assert!(form.fields[1].value.is_masked(), "密码必须遮蔽");
    assert!(form.fields[2].value.is_masked(), "加密口令必须遮蔽");

    // esc 回到失敗畫面，並保留原本的錯誤訊息。
    press(&mut app, &jobs, KeyCode::Esc);
    match &app.screen {
        Screen::Login(screen) => match screen.as_ref() {
            LoginScreen::Failed { message } => {
                assert_eq!(message, "登录失败：用户名或密码错误");
            }
            other => panic!("应回到失败画面，实际为 {other:?}"),
        },
        _ => panic!("应停留在登录画面"),
    }
}

#[test]
fn bracket_keys_switch_homework_group_only_on_homework_page() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;

    use crate::domain::homework::HomeworkGroup;
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.homework_group, HomeworkGroup::Completed);
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.homework_group, HomeworkGroup::Unknown);
    press(&mut app, &jobs, KeyCode::Char('['));
    assert_eq!(app.homework_group, HomeworkGroup::Completed);

    // 非作業頁不生效。
    app.nav = NavItem::Schedule;
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.homework_group, HomeworkGroup::Completed);
}

#[test]
fn s_key_opens_term_picker_and_submits_choice() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;

    // 尚未得知任何学期：提示而不开弹窗。
    press(&mut app, &jobs, KeyCode::Char('s'));
    assert!(matches!(app.screen, Screen::Main));
    assert!(rx.try_recv().is_err());

    use crate::domain::semester::TermCode;
    app.term_options = vec![
        TermCode::parse("2026-2027-2").expect("学期"),
        TermCode::parse("2026-2027-1").expect("学期"),
    ];
    press(&mut app, &jobs, KeyCode::Char('s'));
    assert!(matches!(app.screen, Screen::TermPicker(_)));

    // 上下選擇後 enter 送出 SetHomeworkTerm。
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetHomeworkTerm { term }) if term == "2026-2027-1"
    ));
    assert!(matches!(app.screen, Screen::Main));

    // esc 取消不送任務。
    press(&mut app, &jobs, KeyCode::Char('s'));
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(rx.try_recv().is_err());
    assert!(matches!(app.screen, Screen::Main));
}

#[test]
fn credentials_form_validates_before_sending() {
    let (jobs, rx) = channel();
    let mut app = failed_app("登录失败");
    press(&mut app, &jobs, KeyCode::Char('e'));

    // 全部為空時不送出任務，錯誤就地顯示。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "字段为空时不应送出任务");
    assert!(credentials_form(&app).error.is_some());
    assert!(!credentials_form(&app).busy);

    // 依序填入帳號 / 密碼 / 加密口令。
    type_text(&mut app, &jobs, "3120000001");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "pw-12345");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "secret123");
    press(&mut app, &jobs, KeyCode::Enter);

    match rx.try_recv() {
        Ok(Job::RetryWithAccount {
            credentials,
            passphrase,
        }) => {
            assert_eq!(credentials.username, "3120000001");
            assert_eq!(credentials.password, "pw-12345");
            assert_eq!(passphrase, "secret123");
        }
        other => panic!("应为凭证重输任务，实际为 {other:?}"),
    }

    // 送出後留在表單上等待事件（busy 期間不接受輸入）。
    assert!(credentials_form(&app).busy);
    assert!(credentials_form(&app).error.is_none());
    type_text(&mut app, &jobs, "x");
    assert_eq!(credentials_form(&app).fields[0].value.value(), "3120000001");
}
