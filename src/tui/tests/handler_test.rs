//! 按鍵處理測試：表單驗證、任務送出與導航觸發載入。

use std::sync::mpsc::{Sender, channel};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::AccessPolicy;
use crate::domain::activity::ActivityGroup;
use crate::domain::homework::{HomeworkInput, aggregate};
use crate::sites::lms::{ActivityKind, LmsActivity, LmsCourse};
use crate::task::Job;
use crate::tui::app::{
    ActivityDetailView, AgreementState, App, FormKind, FormState, HomeworkData, LmsLevel,
    LoginScreen, NavItem, Page, ScheduleData, Screen, SettingsState,
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
    let Screen::Setup(form) = &app.screen else {
        panic!("提交后应停留在设定表单");
    };
    assert!(form.busy, "提交后应显示处理中");
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
fn setup_form_passphrase_minimum_is_eight_characters() {
    // 7 個字元：拒絕。
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));
    fill_setup(&mut app, &jobs, "1234567", "1234567", "pw-12345");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "7 个字符的口令应被拒绝");

    // 8 個字元：接受。
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));
    fill_setup(&mut app, &jobs, "12345678", "12345678", "pw-12345");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(
        matches!(rx.try_recv(), Ok(Job::CreateVault { .. })),
        "8 个字符的口令应被接受"
    );
}

#[test]
fn change_passphrase_minimum_applies_to_new_passphrase_only() {
    // 新口令過短（7 字元）：拒絕；原口令長度不檢查（可少於下限）。
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SettingsForm(FormState::change_passphrase()));
    type_text(&mut app, &jobs, "old6ch");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "1234567");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "1234567");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "7 个字符的新口令应被拒绝");

    // 新口令 8 字元：接受（原口令仅 6 字元，属旧凭据，不受下限限制）。
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SettingsForm(FormState::change_passphrase()));
    type_text(&mut app, &jobs, "old6ch");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "12345678");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "12345678");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(
        matches!(rx.try_recv(), Ok(Job::ChangePassphrase { old, new }) if old == "old6ch" && new == "12345678"),
        "新口令 8 字元、原口令 6 字元应被接受"
    );
}

#[test]
fn unlock_accepts_short_passphrases_of_existing_vaults() {
    // 解鎖既有憑證不檢查長度：舊使用者的 6 字元口令必須能繼續使用。
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);

    type_text(&mut app, &jobs, "old6ch");
    press(&mut app, &jobs, KeyCode::Enter);

    assert!(
        matches!(rx.try_recv(), Ok(Job::Unlock { passphrase }) if passphrase == "old6ch"),
        "旧凭据的短口令应能解锁"
    );
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
    let Screen::Unlock(form) = &app.screen else {
        panic!("提交后应停留在解锁表单");
    };
    assert!(form.busy, "提交后应显示处理中");
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
fn busy_form_ignores_input_until_reply() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));

    fill_setup(&mut app, &jobs, "secret123", "secret123", "pw-12345");
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::CreateVault { .. })));

    // 送出後忽略輸入與重複提交。
    press(&mut app, &jobs, KeyCode::Char('x'));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "處理中不應重複送出");
    let Screen::Setup(form) = &app.screen else {
        panic!("应停留在设定表单");
    };
    assert_eq!(form.value("账号"), "3120000001", "處理中輸入不應被改動");
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
    app.login = Some(Box::new(crate::tui::app::LoginScreen::Captcha {
        path: std::path::PathBuf::from("/tmp/captcha.png"),
        input: crate::tui::text::InputLine::new(),
        error: None,
    }));

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
    app.login = Some(Box::new(crate::tui::app::LoginScreen::Mfa {
        phone: Some("138****8888".to_owned()),
        sent: true,
        input: crate::tui::text::InputLine::new(),
        error: None,
    }));

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

/// 登入失敗畫面（覆蓋在空的主畫面上）。
fn failed_app(message: &str) -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Failed {
        message: message.to_owned(),
    }));
    app
}

/// 目前登入憑證表單（測試輔助）。
fn credentials_form(app: &App) -> &FormState {
    let Some(screen) = app.login.as_deref() else {
        panic!("应停留在登录画面");
    };
    let LoginScreen::Credentials { form, .. } = screen else {
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
        matches!(app.login.as_deref(), Some(LoginScreen::Progress { .. })),
        "重試時應顯示進度覆蓋層"
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
    match app.login.as_deref() {
        Some(LoginScreen::Failed { message }) => {
            assert_eq!(message, "登录失败：用户名或密码错误");
        }
        other => panic!("应回到失败画面，实际为 {other:?}"),
    }
}

#[test]
fn login_overlay_takes_keys_and_keeps_underlying_screen() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Captcha {
        path: std::path::PathBuf::from("/tmp/captcha.png"),
        input: crate::tui::text::InputLine::new(),
        error: None,
    }));

    // 主畫面按鍵（右鍵切頁）在覆蓋層開啟時不生效。
    press(&mut app, &jobs, KeyCode::Right);
    assert_eq!(app.nav, NavItem::Schedule, "覆蓋層開啟時不應切換頁面");
    assert!(matches!(app.screen, Screen::Main), "底層畫面維持不變");

    // 輸入進驗證碼框。
    type_text(&mut app, &jobs, "9f9f");
    let Some(LoginScreen::Captcha { input, .. }) = app.login.as_deref() else {
        panic!("应停留在验证码画面");
    };
    assert_eq!(input.value(), "9f9f");

    // Ctrl+P 在覆蓋層開啟時不應在底層打開設定。
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &jobs,
    );
    assert!(matches!(app.screen, Screen::Main), "覆蓋層期間不應開啟設定");
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

/// 思源學堂測試用活動。
fn lms_activity(id: &str, kind: &str) -> LmsActivity {
    LmsActivity {
        id: id.to_owned(),
        course_id: None,
        kind: kind.to_owned(),
        title: Some(format!("活动 {id}")),
        start_time: None,
        end_time: None,
        submit_by_group: None,
        group_id: None,
        description: None,
        user_submit_count: None,
        published: None,
    }
}

#[test]
fn bracket_keys_switch_activity_group_on_lms_activities() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities = Page::Ready(vec![
        lms_activity("1", "lesson"),
        lms_activity("2", "homework"),
    ]);
    app.activity_state.select(Some(1));

    // 直播（空組）→ 課程內容：選取重設、不發任務。
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.lms.activity_group, ActivityGroup::Lesson);
    assert_eq!(app.page_selection(), 0);
    assert!(rx.try_recv().is_err(), "切換分組不應送出網路任務");

    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.lms.activity_group, ActivityGroup::Homework);
    press(&mut app, &jobs, KeyCode::Char('['));
    assert_eq!(app.lms.activity_group, ActivityGroup::Lesson);

    // 其他層級不生效。
    app.lms.level = LmsLevel::Courses;
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.lms.activity_group, ActivityGroup::Lesson);
}

#[test]
fn o_key_sends_open_activity_for_selected_item() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activity_group = ActivityGroup::Homework;
    app.lms.activities = Page::Ready(vec![
        lms_activity("1", "lesson"),
        lms_activity("2", "homework"),
    ]);

    // 附上目前課程識別碼：作業需要它組出課程作業列表網址。
    app.lms.courses = Page::Ready(vec![course_with_term("7", Some("2026-1"))]);
    app.lms.course_index = 0;

    // 過濾後清單的第一項為作業 2。
    press(&mut app, &jobs, KeyCode::Char('o'));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::OpenActivity {
            activity_id,
            course_id,
            kind
        }) if activity_id == "2"
            && course_id.as_deref() == Some("7")
            && kind == ActivityKind::Homework
    ));

    // 詳情層沿用目前活動。
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "2".to_owned(),
        title: "作业".to_owned(),
        kind: ActivityKind::Homework,
        end_time: None,
        submit_by_group: false,
        submissions: None,
        note: None,
    });
    press(&mut app, &jobs, KeyCode::Char('o'));
    assert!(
        matches!(rx.try_recv(), Ok(Job::OpenActivity { activity_id, .. }) if activity_id == "2")
    );

    // 其他頁面不生效。
    app.nav = NavItem::Schedule;
    press(&mut app, &jobs, KeyCode::Char('o'));
    assert!(rx.try_recv().is_err());
}

/// 建構作業頁資料（單一課程、單一作業）。
fn homework_page(course_id: &str, activity_id: &str, submitted: usize) -> HomeworkData {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let input = HomeworkInput {
        course_id: course_id.to_owned(),
        course_name: "编译原理".to_owned(),
        activity_id: activity_id.to_owned(),
        title: "第一次作业".to_owned(),
        end_time: Some("2026-10-01 23:59:59".to_owned()),
        submit_by_group: false,
        submission_count: Some(submitted),
        note: None,
    };
    HomeworkData {
        term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
        term_source: Some("考勤系统"),
        courses_included: 1,
        courses_skipped: 0,
        term_options: Vec::new(),
        items: aggregate(&[input], now),
        issues: Vec::new(),
        courses_failed: 0,
        progress: None,
    }
}

#[test]
fn o_key_on_homework_page_opens_selected_homework_course() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;

    // 尚未載入作業：提示而不送任務。
    press(&mut app, &jobs, KeyCode::Char('o'));
    assert!(rx.try_recv().is_err(), "未載入時不應送出任務");
    assert_eq!(app.message_text(), Some("请先选择要打开的作业"));

    app.homework = Page::Ready(homework_page("42", "a-1", 0));
    press(&mut app, &jobs, KeyCode::Char('o'));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::OpenActivity {
            activity_id,
            course_id,
            kind
        }) if activity_id == "a-1"
            && course_id.as_deref() == Some("42")
            && kind == ActivityKind::Homework
    ));

    // 分組過濾後沒有項目：同樣只提示，不送任務。
    use crate::domain::homework::HomeworkGroup;
    app.homework_group = HomeworkGroup::Completed;
    app.set_selection(0);
    press(&mut app, &jobs, KeyCode::Char('o'));
    assert!(rx.try_recv().is_err(), "空清單不應送出任務");
}

#[test]
fn enter_on_lms_activities_uses_filtered_selection() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activity_group = ActivityGroup::Homework;
    app.lms.activities = Page::Ready(vec![
        lms_activity("1", "lesson"),
        lms_activity("2", "homework"),
    ]);

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadActivityDetail { activity_id }) if activity_id == "2"
    ));
    assert_eq!(app.lms.level, LmsLevel::Detail);
    assert!(app.lms.detail.is_loading(), "應切換到詳情載入中");
}

fn course_with_term(id: &str, code: Option<&str>) -> LmsCourse {
    LmsCourse {
        id: id.to_owned(),
        name: format!("课程{id}"),
        course_code: None,
        instructors: Vec::new(),
        semester: code.map(|code| crate::sites::lms::models::LmsSemester {
            id: None,
            code: Some(code.to_owned()),
            name: None,
            real_name: None,
        }),
        academic_year: None,
    }
}

#[test]
fn enter_on_lms_courses_uses_real_index_after_partition_headers() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    app.lms.courses = Page::Ready(vec![
        course_with_term("1", Some("2025-2")), // 歷史（列模型中有標題列）
        course_with_term("2", Some("2026-1")), // 當前學期
    ]);
    app.lms.courses_term =
        Some(crate::domain::semester::TermCode::parse("2026-2027-1").expect("学期"));

    // 選取真實索引 0 的歷史課程：即使列模型插入了標題列，也必須開啟正確課程。
    app.course_state.select(Some(0));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadActivities { course_id, .. }) if course_id == "1"
    ));
    assert_eq!(app.lms.course_index, 0);
    assert_eq!(app.lms.level, LmsLevel::Activities);
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

// ── 用户协议閱讀門 ───────────────────────────────────

/// 建立帶協議閱讀門的 App（底層為主畫面）。
fn app_with_agreement() -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.agreement = Some(Box::new(AgreementState::new()));
    app
}

/// 目前協議狀態。
fn agreement_state(app: &App) -> &AgreementState {
    app.agreement.as_deref().expect("協议閱讀門应开启")
}

#[test]
fn agreement_gate_blocks_settings_and_main_shortcuts() {
    let (jobs, _rx) = channel();
    let mut app = app_with_agreement();

    // Ctrl+P 不開啟設定。
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &jobs,
    );
    assert!(
        matches!(app.screen, Screen::Main),
        "協議開啟時 ctrl+p 不作用"
    );

    // Ctrl+U 不清空底層表單。
    app.set_screen(Screen::Setup(FormState::setup()));
    if let Screen::Setup(form) = &mut app.screen {
        form.fields[0].value.set("secret");
    }
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &jobs,
    );
    if let Screen::Setup(form) = &app.screen {
        assert_eq!(
            form.fields[0].value.value(),
            "secret",
            "ctrl+u 不得穿透協議畫面"
        );
    }

    // 主畫面快捷鍵不作用：`s` 不開學期選擇器、`]` 不切換分組。
    app.set_screen(Screen::Main);
    let group = app.homework_group;
    press(&mut app, &jobs, KeyCode::Char('s'));
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(matches!(app.screen, Screen::Main), "協議開啟時 s 不作用");
    assert_eq!(app.homework_group, group, "協議開啟時 ] 不作用");
}

#[test]
fn agreement_scroll_keys_move_document() {
    let (jobs, _rx) = channel();
    let mut app = app_with_agreement();
    app.agreement.as_mut().expect("閱讀門").sync_layout(10, 50);

    press(&mut app, &jobs, KeyCode::Char('j'));
    assert_eq!(agreement_state(&app).scroll(), 1);
    press(&mut app, &jobs, KeyCode::Char('k'));
    assert_eq!(agreement_state(&app).scroll(), 0);
    press(&mut app, &jobs, KeyCode::PageDown);
    assert_eq!(agreement_state(&app).scroll(), 10);
    press(&mut app, &jobs, KeyCode::Char(' '));
    assert_eq!(agreement_state(&app).scroll(), 20);
    press(&mut app, &jobs, KeyCode::PageUp);
    assert_eq!(agreement_state(&app).scroll(), 10);
    press(&mut app, &jobs, KeyCode::Char('g'));
    assert_eq!(agreement_state(&app).scroll(), 0);
    press(&mut app, &jobs, KeyCode::Char('G'));
    assert_eq!(agreement_state(&app).scroll(), 40);
}

#[test]
fn agreement_enter_requires_bottom_and_sends_once() {
    let (jobs, rx) = channel();
    let mut app = app_with_agreement();
    app.agreement.as_mut().expect("閱讀門").sync_layout(10, 50);

    // 未讀到底部：enter 不送任務。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "未到底部不得送出同意");
    assert!(!agreement_state(&app).saving);

    // 跳到結尾後送出一次同意。
    press(&mut app, &jobs, KeyCode::End);
    press(&mut app, &jobs, KeyCode::Enter);
    match rx.try_recv() {
        Ok(Job::AcceptAgreement) => {}
        other => panic!("应为同意协议任务，实际为 {other:?}"),
    }
    assert!(agreement_state(&app).saving, "送出后应进入保存中");

    // 保存中：重複 enter 不再送出，esc 也不退出。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "保存中不得重复送出");
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(!app.quit, "保存中忽略退出键");
}

#[test]
fn agreement_quit_keys_exit_without_accepting() {
    let (jobs, rx) = channel();
    let mut app = app_with_agreement();

    press(&mut app, &jobs, KeyCode::Char('q'));
    assert!(app.quit, "q 應直接退出");
    assert!(rx.try_recv().is_err(), "退出不得送出同意");

    let mut app = app_with_agreement();
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(app.quit, "esc 應直接退出");
    assert!(rx.try_recv().is_err(), "退出不得送出同意");

    let mut app = app_with_agreement();
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &jobs,
    );
    assert!(app.quit, "ctrl+c 一律可退出");
}
