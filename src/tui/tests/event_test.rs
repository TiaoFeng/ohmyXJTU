//! 背景事件套用測試。

use std::path::PathBuf;

use crate::config::AccessPolicy;
use crate::domain::homework::{HomeworkInput, HomeworkState, aggregate};
use crate::sites::lms::LmsCourse;
use crate::task::Event;
use crate::tui::app::{App, FlowData, FormState, LoginScreen, Page, ScheduleData, Screen};

use super::apply_event;

fn app() -> App {
    App::new(AccessPolicy::Auto)
}

#[test]
fn vault_ready_moves_to_login() {
    let mut app = app();
    apply_event(&mut app, Event::VaultReady);
    assert!(matches!(app.screen, Screen::Login(_)));
}

#[test]
fn captcha_event_resets_input_but_keeps_error() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::LoginNeedsCaptcha(PathBuf::from("/tmp/captcha-1.png")),
    );
    let Screen::Login(screen) = &app.screen else {
        panic!("应进入登录画面");
    };
    assert!(matches!(screen.as_ref(), LoginScreen::Captcha { .. }));
    assert_eq!(
        app.captcha_path.as_deref(),
        Some(std::path::Path::new("/tmp/captcha-1.png"))
    );

    if let Screen::Login(screen) = &mut app.screen
        && let LoginScreen::Captcha { input, error, .. } = screen.as_mut()
    {
        input.set("a1b2");
        *error = Some("验证码错误".to_owned());
    }

    // 換一張驗證碼圖片後，舊的驗證碼自然失效，但錯誤提示要保留。
    apply_event(
        &mut app,
        Event::LoginNeedsCaptcha(PathBuf::from("/tmp/captcha-2.png")),
    );

    let Screen::Login(screen) = &app.screen else {
        panic!("应进入登录画面");
    };
    match screen.as_ref() {
        LoginScreen::Captcha { input, error, path } => {
            assert_eq!(error.as_deref(), Some("验证码错误"), "應保留上一次的錯誤");
            assert!(input.is_empty(), "換圖後舊驗證碼應清空");
            assert_eq!(path, &PathBuf::from("/tmp/captcha-2.png"));
        }
        other => panic!("应停留在验证码画面，实际为 {other:?}"),
    }
}

#[test]
fn mfa_event_preserves_typed_code() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::LoginNeedsMfa {
            phone: Some("138****8888".to_owned()),
            sent: false,
        },
    );
    if let Screen::Login(screen) = &mut app.screen
        && let LoginScreen::Mfa { input, .. } = screen.as_mut()
    {
        input.set("123456");
    }

    // 使用者按 s 發送驗證碼後，輸入框內容不應被清掉。
    apply_event(
        &mut app,
        Event::LoginNeedsMfa {
            phone: Some("138****8888".to_owned()),
            sent: true,
        },
    );

    let Screen::Login(screen) = &app.screen else {
        panic!("应进入登录画面");
    };
    match screen.as_ref() {
        LoginScreen::Mfa { input, sent, .. } => {
            assert!(*sent);
            assert_eq!(input.value(), "123456");
        }
        other => panic!("应停留在短信验证画面，实际为 {other:?}"),
    }
}

#[test]
fn login_success_returns_to_main() {
    let mut app = app();
    app.set_screen(Screen::Login(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    })));
    apply_event(&mut app, Event::LoginSucceeded);
    assert!(matches!(app.screen, Screen::Main));
    assert_eq!(app.message_text(), Some("登录成功"));
}

#[test]
fn data_events_fill_pages() {
    let mut app = app();

    apply_event(
        &mut app,
        Event::Schedule(Box::new(ScheduleData {
            semester: "2026-2027-1".to_owned(),
            week: 3,
            lessons: Vec::new(),
            skipped: 0,
        })),
    );
    assert!(app.schedule.ready().is_some());
    assert!(matches!(app.screen, Screen::Main), "收到資料後應回到主畫面");

    apply_event(
        &mut app,
        Event::Flow(Box::new(FlowData {
            records: Vec::new(),
            page: 2,
            total_pages: 4,
            total: 0,
        })),
    );
    assert_eq!(app.attendance.ready().map(|data| data.page), Some(2));

    apply_event(
        &mut app,
        Event::Courses(vec![LmsCourse {
            id: "42".to_owned(),
            name: "编译原理".to_owned(),
            course_code: None,
            instructors: Vec::new(),
            semester: None,
            academic_year: None,
        }]),
    );
    assert_eq!(app.lms.courses.ready().map(Vec::len), Some(1));
    assert!(app.message_text().is_some());
}

#[test]
fn homework_event_reports_count() {
    let mut app = app();
    let items = aggregate(
        &[HomeworkInput {
            course_id: "1".to_owned(),
            course_name: "编译原理".to_owned(),
            activity_id: "9".to_owned(),
            title: "第三次作业".to_owned(),
            end_time: None,
            submit_by_group: false,
            submission_count: Some(0),
        }],
        chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间"),
    );
    assert_eq!(items[0].state, HomeworkState::Pending);

    apply_event(&mut app, Event::Homework(items));
    assert!(app.homework.ready().is_some());
    assert_eq!(app.message_text(), Some("共 1 项待处理作业"));
}

#[test]
fn failure_leaves_login_screen_and_marks_pages() {
    let mut app = app();
    app.set_screen(Screen::Login(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    })));
    app.schedule.start_loading("正在加载…");

    apply_event(
        &mut app,
        Event::Failed {
            what: "课表".to_owned(),
            message: "网络连接失败".to_owned(),
        },
    );

    // 不可停在「正在登录…」：使用者需要能按 enter 重試。
    match &app.screen {
        Screen::Login(screen) => assert!(matches!(screen.as_ref(), LoginScreen::Failed { .. })),
        _ => panic!("应停留在登录画面并显示失败原因"),
    }
    assert!(matches!(app.schedule, Page::Failed(_)));
}

#[test]
fn access_policy_event_updates_state() {
    let mut app = app();
    apply_event(&mut app, Event::AccessPolicyUpdated(AccessPolicy::WebVpn));
    assert_eq!(app.access_policy, AccessPolicy::WebVpn);
    assert!(app.message_text().is_some());
}

#[test]
fn notice_event_only_sets_message() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::Notice("已跳过 2 项无法解析的思源学堂数据".to_owned()),
    );
    assert_eq!(
        app.message_text(),
        Some("已跳过 2 项无法解析的思源学堂数据")
    );
    assert!(app.homework.is_idle());
}

/// 帶著已輸入內容的憑證表單。
fn credentials_app(typed: &str) -> App {
    let mut app = app();
    app.set_screen(Screen::Login(Box::new(LoginScreen::Credentials {
        form: FormState::login_retry(),
        message: "登录失败：用户名或密码错误".to_owned(),
    })));
    if let Screen::Login(screen) = &mut app.screen
        && let LoginScreen::Credentials { form, .. } = screen.as_mut()
    {
        form.busy = true;
        form.fields[0].value.set(typed);
    }
    app
}

#[test]
fn login_failed_stays_on_credentials_form() {
    let mut app = credentials_app("3120000001");

    apply_event(
        &mut app,
        Event::LoginFailed("登录失败：用户名或密码错误".to_owned()),
    );

    let Screen::Login(screen) = &app.screen else {
        panic!("应停留在登录画面");
    };
    match screen.as_ref() {
        LoginScreen::Credentials { form, .. } => {
            assert_eq!(
                form.error.as_deref(),
                Some("登录失败：用户名或密码错误"),
                "帳密被拒時應就地表單顯示"
            );
            assert!(!form.busy, "失敗後應恢復可輸入");
            assert_eq!(
                form.fields[0].value.value(),
                "3120000001",
                "失敗後應保留已輸入的帳號"
            );
        }
        other => panic!("应停留在凭证表单，实际为 {other:?}"),
    }
}

#[test]
fn task_failure_stays_on_credentials_form() {
    let mut app = credentials_app("3120000001");

    apply_event(
        &mut app,
        Event::Failed {
            what: "账户设置".to_owned(),
            message: "口令错误或凭证文件已损坏".to_owned(),
        },
    );

    let Screen::Login(screen) = &app.screen else {
        panic!("应停留在登录画面");
    };
    match screen.as_ref() {
        LoginScreen::Credentials { form, .. } => {
            assert_eq!(
                form.error.as_deref(),
                Some("账户设置失败：口令错误或凭证文件已损坏")
            );
            assert!(!form.busy);
            assert_eq!(form.fields[0].value.value(), "3120000001");
        }
        other => panic!("应停留在凭证表单，实际为 {other:?}"),
    }
    assert!(app.message_text().is_some(), "狀態列仍應顯示錯誤");
}
