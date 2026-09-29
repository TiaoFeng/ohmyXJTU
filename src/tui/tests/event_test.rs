//! 背景事件套用測試。

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use crate::config::AccessPolicy;
use crate::domain::homework::{HomeworkGroup, HomeworkInput, HomeworkState, aggregate};
use crate::domain::semester::{TermCode, TermSource};
use crate::session::{AccessMode, SiteKind};
use crate::sites::lms::LmsCourse;
use crate::task::{Event, FailedTarget, HomeworkUpdate};
use crate::tui::app::{
    App, FlowData, FormState, LoginScreen, NavItem, Page, ScheduleData, Screen, SettingsState,
};

use super::apply_event as apply_event_with_jobs;

/// 套用事件（不需要檢查送出的任務時使用；任務會被丟棄）。
fn apply_event(app: &mut App, event: Event) {
    let (jobs, _rx) = channel();
    apply_event_with_jobs(app, event, &jobs);
}

fn app() -> App {
    App::new(AccessPolicy::Auto)
}

#[test]
fn vault_ready_moves_to_main_and_starts_loading() {
    let mut app = app();
    apply_event(&mut app, Event::VaultReady);
    assert!(matches!(app.screen, Screen::Main), "解锁后直接进入主画面");
    assert!(app.schedule.is_loading(), "应触发当前页面的首次加载");
}

#[test]
fn captcha_event_resets_input_but_keeps_error() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::LoginNeedsCaptcha(PathBuf::from("/tmp/captcha-1.png")),
    );
    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Captcha { .. })),
        "應顯示驗證碼覆蓋層"
    );
    assert_eq!(
        app.captcha_path.as_deref(),
        Some(std::path::Path::new("/tmp/captcha-1.png"))
    );

    if let Some(LoginScreen::Captcha { input, error, .. }) = app.login.as_deref_mut() {
        input.set("a1b2");
        *error = Some("验证码错误".to_owned());
    }

    // 換一張驗證碼圖片後，舊的驗證碼自然失效，但錯誤提示要保留。
    apply_event(
        &mut app,
        Event::LoginNeedsCaptcha(PathBuf::from("/tmp/captcha-2.png")),
    );

    match app.login.as_deref() {
        Some(LoginScreen::Captcha { input, error, path }) => {
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
    if let Some(LoginScreen::Mfa { input, .. }) = app.login.as_deref_mut() {
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

    match app.login.as_deref() {
        Some(LoginScreen::Mfa { input, sent, .. }) => {
            assert!(*sent);
            assert_eq!(input.value(), "123456");
        }
        other => panic!("应停留在短信验证画面，实际为 {other:?}"),
    }
}

#[test]
fn login_success_returns_to_main_and_updates_site_mode() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    }));
    apply_event(
        &mut app,
        Event::LoginSucceeded {
            site: SiteKind::Attendance,
            mode: Some(AccessMode::Direct),
        },
    );
    assert!(app.login.is_none(), "登入成功後應關閉覆蓋層");
    assert!(matches!(app.screen, Screen::Main));
    assert_eq!(app.message_text(), Some("登录成功"));
    // 目前頁面為課表（考勤站點）：底欄顯示實際訪問方式。
    assert_eq!(app.session_label(), "已登录 · 直连");
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
fn homework_event_updates_groups_and_counts() {
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
            note: None,
        }],
        chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间"),
    );
    assert_eq!(items[0].state, HomeworkState::Pending);

    apply_event(
        &mut app,
        Event::Homework(HomeworkUpdate {
            term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
            term_source: Some(TermSource::Attendance),
            courses_included: 2,
            courses_skipped: 1,
            term_options: Vec::new(),
            items,
            issues: Vec::new(),
            progress: None,
            elapsed: Duration::from_millis(1200),
            requests: 7,
        }),
    );

    let data = app.homework.ready().expect("应有作业资料");
    assert_eq!(data.group_count(HomeworkGroup::Unfinished), 1);
    assert_eq!(data.group_count(HomeworkGroup::Completed), 0);
    assert_eq!(
        app.message_text(),
        Some("作业已更新：未完成 1 项（用时 1.2s）")
    );
}

#[test]
fn data_failure_marks_target_page_without_touching_login_overlay() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    }));
    app.schedule.start_loading("正在加载…");

    apply_event(
        &mut app,
        Event::Failed {
            what: "课表".to_owned(),
            message: "网络连接失败".to_owned(),
            target: FailedTarget::Schedule,
        },
    );

    // 資料任務失敗：失敗標記只落在課表頁；底層畫面與登入覆蓋層皆不變。
    assert!(matches!(app.screen, Screen::Main));
    assert!(app.login.is_some(), "資料失敗不應關閉或替換登入覆蓋層");
    assert!(matches!(app.schedule, Page::Failed { .. }));
}

#[test]
fn login_failure_shows_failed_overlay() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    }));

    apply_event(
        &mut app,
        Event::Failed {
            what: "登录".to_owned(),
            message: "网络连接失败".to_owned(),
            target: FailedTarget::Login,
        },
    );

    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Failed { .. })),
        "登入失敗應在覆蓋層顯示失敗畫面"
    );
    assert!(matches!(app.screen, Screen::Main), "底層畫面維持不變");
}

#[test]
fn settings_failure_keeps_popup_and_draft() {
    let mut app = app();
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));
    if let Screen::Settings(state) = &mut app.screen {
        state.draft = Some(AccessPolicy::WebVpn);
        state.saving = true;
    }

    apply_event(
        &mut app,
        Event::Failed {
            what: "访问模式".to_owned(),
            message: "配置错误".to_owned(),
            target: FailedTarget::Settings,
        },
    );

    let Screen::Settings(state) = app.screen else {
        panic!("设置保存失败时应保留弹窗");
    };
    assert!(!state.saving, "失败后应解除保存中");
    assert_eq!(
        state.policy(app.access_policy),
        AccessPolicy::WebVpn,
        "应保留草稿"
    );
    assert_eq!(
        app.access_policy,
        AccessPolicy::Auto,
        "失败时不得更新已生效值"
    );
}

#[test]
fn credential_failure_restores_unlock_form_with_error() {
    let mut app = app();
    let mut form = FormState::unlock();
    form.fields[0].value.set("secret123");
    form.busy = true;
    app.set_screen(Screen::Unlock(form));

    apply_event(
        &mut app,
        Event::Failed {
            what: "解锁凭证".to_owned(),
            message: "加密口令错误".to_owned(),
            target: FailedTarget::Credentials,
        },
    );

    let Screen::Unlock(form) = &app.screen else {
        panic!("應留在解鎖表單");
    };
    assert!(!form.busy, "失敗後應解除處理中");
    assert_eq!(form.error.as_deref(), Some("解锁凭证失败：加密口令错误"));
    assert!(form.fields[0].value.is_empty(), "口令欄位應清空");
}

#[test]
fn credential_failure_keeps_setup_account_but_clears_secrets() {
    let mut app = app();
    let mut form = FormState::setup();
    form.fields[0].value.set("secret123");
    form.fields[1].value.set("secret123");
    form.fields[2].value.set("3120000001");
    form.fields[3].value.set("pw-12345");
    form.fields[4].value.set("pw-12345");
    form.busy = true;
    app.set_screen(Screen::Setup(form));

    apply_event(
        &mut app,
        Event::Failed {
            what: "创建凭证".to_owned(),
            message: "凭证文件写入失败".to_owned(),
            target: FailedTarget::Credentials,
        },
    );

    let Screen::Setup(form) = &app.screen else {
        panic!("應留在設定表單");
    };
    assert!(!form.busy);
    assert!(form.error.is_some());
    assert_eq!(form.value("账号"), "3120000001", "帳號欄位應保留");
    assert!(form.value("密码").is_empty(), "密碼欄位應清空");
    assert!(form.value("加密口令").is_empty(), "口令欄位應清空");
}

#[test]
fn passphrase_updated_returns_to_settings_list() {
    let mut app = app();
    let mut form = FormState::change_passphrase();
    form.busy = true;
    app.set_screen(Screen::SettingsForm(form));

    apply_event(&mut app, Event::PassphraseUpdated);

    assert!(
        matches!(app.screen, Screen::Settings(_)),
        "成功後應回到設定選單"
    );
    assert_eq!(app.message_text(), Some("加密口令已更新"));
}

#[test]
fn account_updated_leaves_form_for_main() {
    let mut app = app();
    let mut form = FormState::change_account();
    form.busy = true;
    app.set_screen(Screen::SettingsForm(form));

    apply_event(&mut app, Event::AccountUpdated);

    assert!(
        matches!(app.screen, Screen::Main),
        "修改帳號成功後應離開表單"
    );
}

#[test]
fn needs_term_opens_picker_and_marks_homework() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::HomeworkNeedsTerm {
            options: vec![TermCode::parse("2026-2027-1").expect("学期")],
            suggestion: None,
            reason: "考勤系统不可用".to_owned(),
        },
    );

    assert!(matches!(app.screen, Screen::TermPicker(_)));
    assert!(matches!(app.homework, Page::Failed { .. }));
    assert_eq!(app.term_options.len(), 1);
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
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Credentials {
        form: FormState::login_retry(),
        message: "登录失败：用户名或密码错误".to_owned(),
    }));
    if let Some(LoginScreen::Credentials { form, .. }) = app.login.as_deref_mut() {
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

    match app.login.as_deref() {
        Some(LoginScreen::Credentials { form, .. }) => {
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
            what: "重新输入账户".to_owned(),
            message: "口令错误或凭证文件已损坏".to_owned(),
            target: FailedTarget::Login,
        },
    );

    match app.login.as_deref() {
        Some(LoginScreen::Credentials { form, .. }) => {
            assert_eq!(
                form.error.as_deref(),
                Some("重新输入账户失败：口令错误或凭证文件已损坏")
            );
            assert!(!form.busy);
            assert_eq!(form.fields[0].value.value(), "3120000001");
        }
        other => panic!("应停留在凭证表单，实际为 {other:?}"),
    }
    assert!(app.message_text().is_some(), "狀態列仍應顯示錯誤");
}

#[test]
fn open_url_event_queues_browser_open() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::OpenUrl("https://lms.xjtu.edu.cn".to_owned()),
    );
    assert_eq!(
        app.pending_open.as_deref(),
        Some("https://lms.xjtu.edu.cn"),
        "網址應交給主迴圈開啟"
    );
    assert!(app.message_text().is_some(), "應顯示進行中提示");
}

#[test]
fn session_state_is_tracked_per_site() {
    let mut app = app();

    // 未登入：底欄顯示未登录。
    app.nav = NavItem::Lms;
    assert_eq!(app.session_label(), "未登录");

    apply_event(
        &mut app,
        Event::LoginSucceeded {
            site: SiteKind::Lms,
            mode: Some(AccessMode::WebVpn),
        },
    );
    assert_eq!(app.session_label(), "已登录 · WebVPN");

    // 另一站點（考勤）不受影響。
    app.nav = NavItem::Schedule;
    assert_eq!(app.session_label(), "未登录");

    // 會話失效只清除該站點。
    app.set_site_mode(SiteKind::Attendance, AccessMode::Direct);
    apply_event(
        &mut app,
        Event::SessionExpired {
            site: SiteKind::Lms,
        },
    );
    app.nav = NavItem::Lms;
    assert_eq!(app.session_label(), "未登录");
    app.nav = NavItem::Attendance;
    assert_eq!(app.session_label(), "已登录 · 直连");

    // 解鎖、換帳號或切換訪問模式：全部清除。
    apply_event(&mut app, Event::SessionsCleared);
    assert_eq!(app.session_label(), "未登录");
}
