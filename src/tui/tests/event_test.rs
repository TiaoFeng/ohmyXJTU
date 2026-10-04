//! 背景事件套用測試。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use crate::config::AccessPolicy;
use crate::domain::homework::{HomeworkGroup, HomeworkInput, HomeworkState, aggregate};
use crate::domain::semester::{TermCode, TermSource};
use crate::domain::todo::{Priority, Task};
use crate::model::{ActivityDetailView, FlowData, ScheduleData};
use crate::session::{AccessMode, SiteKind};
use crate::sites::lms::LmsCourse;
use crate::task::{CoursesData, Event, FailedTarget, HomeworkUpdate, Job};
use crate::tui::app::{
    AgreementState, App, FormState, HomeworkData, LmsLevel, LoginScreen, NavItem, Page, Screen,
    SettingsState, TaskFormState, TermPickerState,
};
use crate::tui::controller;
use crate::tui::text::InputLine;

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

/// 課表事件同步週次與總週數；換帳號清除（回到當前週），切換訪問模式保留。
#[test]
fn schedule_event_tracks_the_week_and_account_change_clears_it() {
    let mut app = app();
    apply_event(
        &mut app,
        Event::Schedule(Box::new(ScheduleData {
            semester: "2026-2027-1".to_owned(),
            week: 5,
            total_weeks: 23,
            lessons: Vec::new(),
            skipped: 0,
            notice: None,
        })),
    );
    assert_eq!(app.schedule_week, Some(5), "标题与边界应跟随课表事件");
    assert_eq!(app.schedule_total, Some(23));

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: false,
        },
    );
    assert_eq!(app.schedule_week, Some(5), "切换访问模式应保留周次");

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: true,
        },
    );
    assert_eq!(app.schedule_week, None, "换账号后回到当前周");
    assert_eq!(app.schedule_total, None);
    assert!(app.schedule.is_idle());
}

/// 套用一筆課表事件（`label` 用來辨識是哪一週的資料）。
fn apply_schedule_event(app: &mut App, week: u32, label: &str) {
    apply_event(
        app,
        Event::Schedule(Box::new(ScheduleData {
            semester: label.to_owned(),
            week,
            total_weeks: 23,
            lessons: Vec::new(),
            skipped: 0,
            notice: None,
        })),
    );
}

/// 切週待回期間丟棄舊週的遲到結果。
///
/// 課表載入是單步任務：切週指令要等已在執行中的舊週載入回報後才生效，那筆
/// 結果仍會送達介面。若照單全收，畫面會閃回舊週；而新週載入失敗時更會停在
/// 舊週的課程資料與標題（`Page::fail` 會把舊資料留在 `stale`）。
#[test]
fn stale_schedule_event_is_discarded_while_a_week_switch_is_pending() {
    let mut app = app();
    app.schedule_week = Some(5);
    app.schedule_total = Some(23);
    apply_schedule_event(&mut app, 5, "旧周资料");
    assert!(app.schedule.ready().is_some());

    // 使用者按 `]`：標題立即顯示目標週、內容清空，並送出切週指令。
    let (jobs, rx) = channel();
    controller::change_schedule_week(&mut app, &jobs, 1);
    assert_eq!(app.schedule_week, Some(6), "标题应立即显示目标周");
    assert_eq!(app.schedule_pending_week, Some(6));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetScheduleWeek { week: 6 })
    ));
    assert!(app.schedule.is_loading());

    // 舊週（第 5 週）的載入此時才回報：必須丟棄。
    apply_schedule_event(&mut app, 5, "旧周资料");
    assert_eq!(app.schedule_week, Some(6), "标题应维持在目标周");
    assert_eq!(app.schedule_pending_week, Some(6), "待回周次不应被清除");
    assert!(app.schedule.is_loading(), "应维持在加载中");
    assert!(app.schedule.ready().is_none(), "不得显示旧周课程");

    // 目標週的結果抵達：套用並清除待回週次。
    apply_schedule_event(&mut app, 6, "目标周资料");
    assert_eq!(app.schedule_week, Some(6));
    assert_eq!(app.schedule_pending_week, None);
    assert_eq!(
        app.schedule.ready().map(|data| data.semester.as_str()),
        Some("目标周资料")
    );
}

/// 沒有待回週次時，週次與上次顯示不同仍必須套用。
///
/// 使用者從未切週且學期已結束／尚未開始時，工作者會回應正規化後的週次
///（與上次顯示的週次不同）；拿 `schedule_week` 直接比對會把合法結果誤丟，
/// 頁面反而永久停在「載入中」。
#[test]
fn schedule_event_without_a_pending_switch_is_always_applied() {
    let mut app = app();
    app.schedule_week = Some(18);
    app.schedule_total = Some(18);

    apply_schedule_event(&mut app, 22, "学期已结束");

    assert_eq!(app.schedule_week, Some(22), "没有待回周次时不应丢弃");
    assert_eq!(app.schedule_pending_week, None);
    assert_eq!(
        app.schedule.ready().map(|data| data.semester.as_str()),
        Some("学期已结束")
    );
}

/// 待回週次的生命週期：換帳號清除（工作者一併重設週次），切換訪問模式保留
///（工作者同樣保留）。
#[test]
fn pending_week_cleared_on_account_change_but_kept_on_mode_change() {
    let mut app = app();
    app.schedule_week = Some(6);
    app.schedule_total = Some(23);
    app.schedule_pending_week = Some(6);

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: false,
        },
    );
    assert_eq!(
        app.schedule_pending_week,
        Some(6),
        "切换访问模式应保留待回周次"
    );

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: true,
        },
    );
    assert_eq!(app.schedule_pending_week, None, "换账号应清除待回周次");
    assert_eq!(app.schedule_week, None);
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
        "应显示验证码覆盖层"
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
            assert_eq!(error.as_deref(), Some("验证码错误"), "应保留上一次的错误");
            assert!(input.is_empty(), "换图后旧验证码应清空");
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
    assert!(app.login.is_none(), "登录成功后应关闭覆盖层");
    assert!(matches!(app.screen, Screen::Main));
    assert_eq!(app.message_text(), Some("登录成功"));
    // 目前頁面為課表（考勤站點）：底欄顯示實際訪問方式。
    assert_eq!(app.session_label(), "直连");
}

#[test]
fn data_events_fill_pages() {
    let mut app = app();

    apply_event(
        &mut app,
        Event::Schedule(Box::new(ScheduleData {
            semester: "2026-2027-1".to_owned(),
            week: 3,
            total_weeks: 23,
            lessons: Vec::new(),
            skipped: 0,
            notice: None,
        })),
    );
    assert!(app.schedule.ready().is_some());
    assert_eq!(app.schedule_week, Some(3), "应同步课表周次");
    assert_eq!(app.schedule_total, Some(23), "应同步总周数");
    assert!(matches!(app.screen, Screen::Main), "收到数据后应回到主画面");

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
        Event::Courses(CoursesData {
            courses: vec![LmsCourse {
                id: "42".to_owned(),
                name: "编译原理".to_owned(),
                course_code: None,
                instructors: Vec::new(),
                semester: None,
                academic_year: None,
            }],
            current_term: Some(TermCode::parse("2026-2027-1").expect("学期")),
        }),
    );
    assert_eq!(app.lms.courses.ready().map(Vec::len), Some(1));
    assert_eq!(
        app.lms.courses_term,
        Some(TermCode::parse("2026-2027-1").expect("学期")),
        "课程事件应带入当前学期提示"
    );
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
            description: None,
            submit_by_group: Some(false),
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
            courses_failed: 0,
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
        Some("已更新作业：未完成 1 项（用时 1.2s）")
    );
}

/// 資料任務失敗時：目標頁面標記失敗，且停在「正在登入」的覆蓋層必須收斂。
///
/// 離線時自動重登連開始都做不到，工作者會先要求重登（介面顯示「正在登入…」）
/// 再回報失敗；若覆蓋層留在進度畫面，按鍵全被它吃掉，使用者只能重啟程式。
#[test]
fn data_failure_settles_page_and_stuck_login_progress() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录考勤系统…".to_owned(),
    }));
    app.schedule.start_loading("正在加载…");

    apply_event(
        &mut app,
        Event::Failed {
            what: "课表".to_owned(),
            message: "网络连接失败（域名解析失败）".to_owned(),
            target: FailedTarget::Schedule,
            site: None,
            resource: None,
        },
    );

    assert!(matches!(app.screen, Screen::Main), "底层画面维持不变");
    assert!(
        matches!(app.schedule, Page::Failed { .. }),
        "目标页面必须收敛为失败，而不是停在加载中"
    );
    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Failed { .. })),
        "停在「正在登录」的覆盖层必须收敛为可重试的失败画面：{:?}",
        app.login
    );

    // 已在等待使用者輸入（驗證碼）的覆蓋層不受資料任務失敗影響。
    let mut waiting = App::new(AccessPolicy::Auto);
    waiting.set_screen(Screen::Main);
    waiting.login = Some(Box::new(LoginScreen::Captcha {
        path: PathBuf::from("/tmp/captcha.png"),
        input: crate::tui::text::InputLine::new(),
        error: None,
    }));
    apply_event(
        &mut waiting,
        Event::Failed {
            what: "课表".to_owned(),
            message: "网络连接失败".to_owned(),
            target: FailedTarget::Schedule,
            site: None,
            resource: None,
        },
    );
    assert!(
        matches!(waiting.login.as_deref(), Some(LoginScreen::Captcha { .. })),
        "等用户输入的覆盖层不应被数据失败替换：{:?}",
        waiting.login
    );
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
            site: Some(crate::session::SiteKind::Attendance),
            resource: None,
        },
    );

    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Failed { .. })),
        "登录失败应在覆盖层显示失败画面"
    );
    assert!(matches!(app.screen, Screen::Main), "底层画面维持不变");
}

#[test]
fn late_login_failure_does_not_reopen_dismissed_overlay() {
    // 使用者已在失敗畫面按 Esc 關閉覆蓋層；此時若又收到先前排隊的登入失敗
    // 事件，彈窗不得無預警重現。
    let mut app = app();
    app.set_screen(Screen::Main);
    app.login = None;

    apply_event(
        &mut app,
        Event::Failed {
            what: "登录".to_owned(),
            message: "网络连接失败".to_owned(),
            target: FailedTarget::Login,
            site: Some(crate::session::SiteKind::Attendance),
            resource: None,
        },
    );

    assert!(
        app.login.is_none(),
        "已关闭的登录弹窗不应被迟到的事件重新弹出"
    );
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
            site: None,
            resource: None,
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
            site: None,
            resource: None,
        },
    );

    let Screen::Unlock(form) = &app.screen else {
        panic!("应留在解锁表单");
    };
    assert!(!form.busy, "失败后应解除处理中");
    assert_eq!(form.error.as_deref(), Some("解锁凭证失败：加密口令错误"));
    assert!(form.fields[0].value.is_empty(), "口令字段应清空");
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
            site: None,
            resource: None,
        },
    );

    let Screen::Setup(form) = &app.screen else {
        panic!("应留在设置表单");
    };
    assert!(!form.busy);
    assert!(form.error.is_some());
    assert_eq!(form.value("账号"), "3120000001", "账号字段应保留");
    assert!(form.value("密码").is_empty(), "密码字段应清空");
    assert!(form.value("加密口令").is_empty(), "口令字段应清空");
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
        "成功后应回到设置菜单"
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
        "修改账号成功后应离开表单"
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
        site: crate::session::SiteKind::Attendance,
        form: FormState::login_retry(crate::session::SiteKind::Attendance),
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
        Event::LoginFailed {
            site: crate::session::SiteKind::Attendance,
            message: "登录失败：用户名或密码错误".to_owned(),
        },
    );

    match app.login.as_deref() {
        Some(LoginScreen::Credentials { form, .. }) => {
            assert_eq!(
                form.error.as_deref(),
                Some("登录失败：用户名或密码错误"),
                "账密被拒时应就地表单显示"
            );
            assert!(!form.busy, "失败后应恢复可输入");
            assert_eq!(
                form.fields[0].value.value(),
                "3120000001",
                "失败后应保留已输入的账号"
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
            site: Some(crate::session::SiteKind::Attendance),
            resource: None,
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
    assert!(app.message_text().is_some(), "状态列仍应显示错误");
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
        "网址应交给主循环开启"
    );
    assert!(app.message_text().is_some(), "应显示进行中提示");
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
    assert_eq!(app.session_label(), "WebVPN");

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
    assert_eq!(app.session_label(), "直连");

    // 解鎖、換帳號或切換訪問模式：全部清除。
    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: false,
        },
    );
    assert_eq!(app.session_label(), "未登录");
}

#[test]
fn account_change_discards_pages_and_policy_change_keeps_data() {
    // 更換帳號：舊帳號的資料一律清空，卡住的載入回到未載入。
    let mut app = app();
    app.schedule = Page::Ready(ScheduleData::default());
    app.attendance.start_loading("正在加载考勤流水…");
    app.lms.courses = Page::Ready(Vec::new());
    app.lms.level = LmsLevel::Activities;
    app.updated_at.schedule = Some("12:00".to_owned());

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: true,
        },
    );
    assert!(app.schedule.is_idle(), "换账号后课表资料应清空");
    assert!(app.attendance.is_idle(), "换账号后卡住的加载应回到未加载");
    assert!(app.lms.courses.is_idle(), "换账号后课程列表应清空");
    assert_eq!(
        app.lms.level,
        LmsLevel::Courses,
        "换账号后思源学堂应回到课程层"
    );
    assert!(app.updated_at.schedule.is_none(), "旧账号的更新时间应清除");

    // 切換訪問模式：既有資料仍有效，只把載入中的頁面收斂。
    let mut app = App::new(AccessPolicy::Auto);
    app.schedule = Page::Ready(ScheduleData::default());
    app.homework.start_loading("正在汇总作业…");

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: false,
        },
    );
    assert!(app.schedule.ready().is_some(), "切换模式后旧资料应保留");
    assert!(!app.homework.is_loading(), "卡住的加载状态应被解除");
}

#[test]
fn disabled_session_returns_to_the_unlock_screen() {
    // 無法建立乾淨的新會話時，必須清掉所有站點狀態與舊資料、關閉登入覆蓋層，
    // 並回到解鎖畫面；在成功解鎖前不得再顯示任何舊帳號的內容。
    let mut app = app();
    app.set_screen(Screen::Main);
    app.set_site_mode(SiteKind::Attendance, AccessMode::Direct);
    app.schedule = Page::Ready(ScheduleData::default());
    app.homework.start_loading("正在汇总作业…");
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录考勤系统…".to_owned(),
    }));

    apply_event(
        &mut app,
        Event::SessionDisabled("无法建立新的会话，已停用当前会话：连接失败".to_owned()),
    );

    assert!(app.login.is_none(), "登录覆盖层应关闭");
    assert_eq!(app.session_label(), "未登录", "站点登录状态应清除");
    assert!(app.schedule.is_idle(), "停用会话后旧数据应清空");
    assert!(app.homework.is_idle(), "卡住的加载状态应解除");
    match &app.screen {
        Screen::Unlock(form) => assert!(
            form.error
                .as_deref()
                .is_some_and(|error| error.contains("已停用当前会话")),
            "解锁表单应显示停用原因：{:?}",
            form.error
        ),
        other => panic!("应回到解锁画面，实际为 {other:?}"),
    }
    assert!(
        app.message
            .as_ref()
            .is_some_and(|(text, _)| text.contains("会话已停用")),
        "应提示用户重新解锁：{:?}",
        app.message
    );
}

#[test]
fn verification_retry_keeps_the_mfa_input() {
    // 簡訊驗證碼填錯：工作者仍保留登入流程，介面必須留在輸入畫面並就地顯示
    // 錯誤，否則使用者只能重新輸入帳號密碼（甚至重收簡訊）。
    let mut app = app();
    app.login = Some(Box::new(LoginScreen::Mfa {
        phone: Some("138****1234".to_owned()),
        sent: true,
        input: InputLine::with_value("000000"),
        error: None,
    }));

    apply_event(
        &mut app,
        Event::VerificationRetry {
            site: SiteKind::Attendance,
            message: "短信验证码不正确，请重试".to_owned(),
        },
    );

    match app.login.as_deref() {
        Some(LoginScreen::Mfa {
            sent,
            input,
            error,
            phone,
        }) => {
            assert!(*sent, "仍应维持「已发送」状态，不必重发短信");
            assert_eq!(phone.as_deref(), Some("138****1234"));
            assert!(input.is_empty(), "重输前应清空输入框：{:?}", input.value());
            assert_eq!(error.as_deref(), Some("短信验证码不正确，请重试"));
        }
        other => panic!("应留在短信验证画面，实际为 {other:?}"),
    }

    // 圖片驗證碼同樣保留輸入畫面。
    app.login = Some(Box::new(LoginScreen::Captcha {
        path: PathBuf::from("/tmp/captcha-1.png"),
        input: InputLine::with_value("a1b2"),
        error: None,
    }));
    apply_event(
        &mut app,
        Event::VerificationRetry {
            site: SiteKind::Attendance,
            message: "验证码不正确".to_owned(),
        },
    );
    match app.login.as_deref() {
        Some(LoginScreen::Captcha { input, error, .. }) => {
            assert!(input.is_empty(), "重输前应清空验证码");
            assert_eq!(error.as_deref(), Some("验证码不正确"));
        }
        other => panic!("应留在验证码画面，实际为 {other:?}"),
    }

    // 沒有驗證輸入畫面時（例如流程已被取消）：退回一般的失敗提示。
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录考勤系统…".to_owned(),
    }));
    apply_event(
        &mut app,
        Event::VerificationRetry {
            site: SiteKind::Attendance,
            message: "短信验证码不正确，请重试".to_owned(),
        },
    );
    assert!(
        matches!(
            app.login.as_deref(),
            Some(LoginScreen::Failed {
                site: SiteKind::Attendance,
                ..
            })
        ),
        "无输入画面时应显示失败画面：{:?}",
        app.login
    );
}

#[test]
fn homework_partial_updates_keep_page_loading_until_terminal() {
    let mut app = app();
    let update = |progress: Option<(usize, usize)>| {
        Event::Homework(HomeworkUpdate {
            term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
            term_source: Some(TermSource::Attendance),
            courses_included: 2,
            courses_skipped: 0,
            term_options: Vec::new(),
            items: Vec::new(),
            issues: Vec::new(),
            courses_failed: 0,
            progress,
            elapsed: Duration::from_millis(100),
            requests: 1,
        })
    };

    apply_event(&mut app, update(Some((0, 2))));
    assert!(app.homework.is_loading(), "部分结果仍属加载中");
    assert!(app.homework.ready().is_some(), "部分结果应可显示");
    assert!(
        app.homework
            .note()
            .is_some_and(|note| note.contains("已完成 0/2")),
        "加载说明应带进度：{:?}",
        app.homework.note()
    );

    apply_event(&mut app, update(None));
    assert!(!app.homework.is_loading(), "终态应结束加载");
    assert!(
        app.homework
            .ready()
            .is_some_and(|data| data.items.is_empty()),
        "终态数据应就绪"
    );
}

#[test]
fn loading_cancelled_settles_page_without_losing_partial_data() {
    let mut app = app();
    app.homework.start_loading("正在汇总作业…");
    apply_event(
        &mut app,
        Event::LoadingCancelled {
            target: FailedTarget::Homework,
        },
    );
    assert!(app.homework.is_idle(), "没有数据时取消应回到未加载");

    app.homework = Page::Loading {
        note: "正在汇总作业（已完成 1/2 门课程，累计 0 项）…".to_owned(),
        stale: Some(HomeworkData {
            term_label: None,
            term_source: None,
            courses_included: 2,
            courses_skipped: 0,
            term_options: Vec::new(),
            items: Vec::new(),
            issues: Vec::new(),
            courses_failed: 0,
            progress: Some((1, 2)),
        }),
    };
    apply_event(
        &mut app,
        Event::LoadingCancelled {
            target: FailedTarget::Homework,
        },
    );
    assert!(
        matches!(app.homework, Page::Ready(_)),
        "已有部分数据时取消应保留数据"
    );
}

#[test]
fn agreement_accepted_closes_gate() {
    let mut app = app();
    app.agreement = Some(Box::new(AgreementState::new()));
    apply_event(&mut app, Event::AgreementAccepted);
    assert!(app.agreement.is_none(), "同意成功应关闭协议阅读门");
}

#[test]
fn agreement_failure_keeps_gate_with_inline_error() {
    let mut app = app();
    let mut state = AgreementState::new();
    state.start_saving();
    app.agreement = Some(Box::new(state));
    apply_event(
        &mut app,
        Event::Failed {
            what: "用户协议".to_owned(),
            message: "写入配置文件失败".to_owned(),
            target: FailedTarget::Agreement,
            site: None,
            resource: None,
        },
    );
    let state = app.agreement.as_deref().expect("失败后阅读门应保留");
    assert!(!state.saving, "失败后应解除保存中");
    assert_eq!(state.error.as_deref(), Some("写入配置文件失败"));
}

#[test]
fn courses_event_does_not_pull_the_user_back_to_the_course_list() {
    // 使用者按 r 刷新課程後、回應抵達前已按 enter 進入活動層：資料更新不得
    // 把他拉回課程列表。
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities_course = Some("1".to_owned());

    apply_event(
        &mut app,
        Event::Courses(CoursesData {
            courses: vec![course("1")],
            current_term: None,
        }),
    );

    assert_eq!(
        app.lms.level,
        LmsLevel::Activities,
        "数据更新不应改变目前的浏览层级"
    );
    assert!(app.lms.courses.ready().is_some(), "课程数据仍应更新");
}

/// 課程列表的順序可能改變：目前課程必須以識別碼而非索引來維持。
#[test]
fn courses_event_reanchors_the_current_course_by_id() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities_course = Some("A".to_owned());
    app.lms.course_index = 0;

    // 重新查詢後順序由 [A, B] 變成 [B, A]。
    apply_event(
        &mut app,
        Event::Courses(CoursesData {
            courses: vec![course("B"), course("A")],
            current_term: None,
        }),
    );

    assert_eq!(app.lms.level, LmsLevel::Activities);
    assert_eq!(app.lms.course_index, 1, "应以课程识别码重新定位目前课程");
    assert_eq!(
        app.course_state.selected(),
        Some(1),
        "课程层的选取也应跟着移动"
    );
}

/// 目前課程已不在新的清單中：不得讓活動層停留在一個不存在的課程上。
#[test]
fn courses_event_returns_to_the_list_when_the_current_course_is_gone() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities_course = Some("A".to_owned());

    apply_event(
        &mut app,
        Event::Courses(CoursesData {
            courses: vec![course("B")],
            current_term: None,
        }),
    );

    assert_eq!(app.lms.level, LmsLevel::Courses, "应回到课程列表");
    assert_eq!(app.lms.course_index, 0);
}

/// 選定學期後介面會收到分區提示的更新。
#[test]
fn courses_term_event_updates_the_partition_hint() {
    let mut app = app();
    let term = TermCode::parse("2025-2026-2").expect("学期");
    app.lms.courses_term = TermCode::parse("2026-2027-1");

    apply_event(&mut app, Event::CoursesTerm(Some(term)));
    assert_eq!(app.lms.courses_term, Some(term));

    apply_event(&mut app, Event::CoursesTerm(None));
    assert_eq!(app.lms.courses_term, None);
}

/// 測試用課程。
fn course(id: &str) -> LmsCourse {
    LmsCourse {
        id: id.to_owned(),
        name: format!("课程{id}"),
        course_code: None,
        instructors: Vec::new(),
        semester: None,
        academic_year: None,
    }
}

/// 測試用作業更新（空清單）。
fn homework_update(progress: Option<(usize, usize)>) -> HomeworkUpdate {
    HomeworkUpdate {
        term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
        term_source: Some(TermSource::Attendance),
        courses_included: 0,
        courses_skipped: 0,
        term_options: Vec::new(),
        items: Vec::new(),
        issues: Vec::new(),
        courses_failed: 0,
        progress,
        elapsed: Duration::from_millis(10),
        requests: 1,
    }
}

/// 背景資料更新不得關閉使用者正在操作的學期選擇器。
#[test]
fn term_picker_survives_background_data_updates() {
    let mut app = app();
    app.set_screen(Screen::TermPicker(TermPickerState::new(
        vec![TermCode::parse("2026-2027-1").expect("学期")],
        None,
        "无法判定本学期".to_owned(),
    )));
    app.lms.activities_course = Some("1".to_owned());

    // 作業進度（部分結果與終態）都不得把選擇器換成主畫面。
    apply_event(&mut app, Event::Homework(homework_update(Some((0, 2)))));
    assert!(
        matches!(app.screen, Screen::TermPicker(_)),
        "作业进度不得关闭学期选择器"
    );
    apply_event(&mut app, Event::Homework(homework_update(None)));
    assert!(matches!(app.screen, Screen::TermPicker(_)));

    // 其他頁面的資料事件同理。
    apply_event(&mut app, Event::Flow(Box::default()));
    apply_event(
        &mut app,
        Event::Courses(CoursesData {
            courses: vec![course("1")],
            current_term: None,
        }),
    );
    apply_event(
        &mut app,
        Event::Activities {
            course_id: "1".to_owned(),
            activities: Vec::new(),
        },
    );
    assert!(
        matches!(app.screen, Screen::TermPicker(_)),
        "其他数据事件也不得关闭学期选择器"
    );
}

/// 切到新課程後，前一門課遲到的失敗不得把新課程的活動頁標成失敗。
#[test]
fn stale_activities_failure_does_not_pollute_the_new_course() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities_course = Some("B".to_owned());
    app.lms.activities.start_loading("正在加载课程活动…");

    // A 課（已離開）的失敗：忽略。
    apply_event(
        &mut app,
        Event::Failed {
            what: "课程活动".to_owned(),
            message: "连接失败".to_owned(),
            target: FailedTarget::Activities,
            site: Some(SiteKind::Lms),
            resource: Some("A".to_owned()),
        },
    );
    assert!(
        app.lms.activities.is_loading(),
        "旧课程的失败不得影响目前课程：{:?}",
        app.lms.activities
    );

    // 目前課程（B）的失敗：照常標記。
    apply_event(
        &mut app,
        Event::Failed {
            what: "课程活动".to_owned(),
            message: "连接失败".to_owned(),
            target: FailedTarget::Activities,
            site: Some(SiteKind::Lms),
            resource: Some("B".to_owned()),
        },
    );
    assert!(matches!(app.lms.activities, Page::Failed { .. }));
}

/// 活動詳情同理：遲到的舊活動失敗不得污染目前詳情。
#[test]
fn stale_activity_detail_failure_is_ignored() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail_activity = Some("b".to_owned());
    app.lms.detail.start_loading("正在加载活动详情…");

    apply_event(
        &mut app,
        Event::Failed {
            what: "活动详情".to_owned(),
            message: "连接失败".to_owned(),
            target: FailedTarget::ActivityDetail,
            site: Some(SiteKind::Lms),
            resource: Some("a".to_owned()),
        },
    );
    assert!(app.lms.detail.is_loading(), "旧活动的失败不得影响目前详情");
}

/// 帳號驗證成功但憑證保存失敗：解除表單處理中並就地顯示錯誤（不再卡住）。
#[test]
fn credential_save_failure_clears_busy_settings_form() {
    let mut app = app();
    let mut form = FormState::change_account();
    form.busy = true;
    app.set_screen(Screen::SettingsForm(form));

    apply_event(
        &mut app,
        Event::CredentialSaveFailed("登录成功，但凭据保存失败：磁盘只读".to_owned()),
    );

    let Screen::SettingsForm(form) = &app.screen else {
        panic!("保存失败应留在表单");
    };
    assert!(!form.busy, "保存失败后必须解除处理中，否则连 Esc 都被忽略");
    assert_eq!(
        form.error.as_deref(),
        Some("登录成功，但凭据保存失败：磁盘只读")
    );
}

/// 「重新輸入帳密」覆蓋層的保存失敗：同樣解除處理中並顯示錯誤。
#[test]
fn credential_save_failure_clears_busy_login_form() {
    let mut app = app();
    app.set_screen(Screen::Main);
    let mut form = FormState::login_retry(SiteKind::Attendance);
    form.busy = true;
    app.login = Some(Box::new(LoginScreen::Credentials {
        site: SiteKind::Attendance,
        form,
        message: String::new(),
    }));

    apply_event(&mut app, Event::CredentialSaveFailed("保存失败".to_owned()));

    let Some(LoginScreen::Credentials { form, .. }) = app.login.as_deref() else {
        panic!("应留在重新输入表单");
    };
    assert!(!form.busy);
    assert_eq!(form.error.as_deref(), Some("保存失败"));
}

#[test]
fn late_activities_response_is_dropped() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    // 使用者已改看課程 2；課程 1 的回應遲到了。
    app.lms.activities_course = Some("2".to_owned());

    apply_event(
        &mut app,
        Event::Activities {
            course_id: "1".to_owned(),
            activities: Vec::new(),
        },
    );
    assert!(
        app.lms.activities.ready().is_none(),
        "迟到且不属于目前课程的响应应丢弃"
    );

    apply_event(
        &mut app,
        Event::Activities {
            course_id: "2".to_owned(),
            activities: Vec::new(),
        },
    );
    assert!(app.lms.activities.ready().is_some(), "目前课程的响应应套用");
}

#[test]
fn late_activity_detail_response_is_dropped() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail_activity = Some("2".to_owned());

    let detail = |id: &str| {
        Box::new(ActivityDetailView {
            id: id.to_owned(),
            ..ActivityDetailView::default()
        })
    };

    apply_event(&mut app, Event::ActivityDetail(detail("1")));
    assert!(app.lms.detail.ready().is_none(), "上一个活动的详情应丢弃");

    apply_event(&mut app, Event::ActivityDetail(detail("2")));
    assert!(app.lms.detail.ready().is_some(), "目前活动的详情应套用");
}

/// 遲到的舊資源失敗雖不標記目前頁面，仍必須收斂卡住的登入進度覆蓋層。
///
/// 重現：A 課查詢觸發自動重登，使用者已切到 B 課；重登的首次請求逾時，A 的
/// 失敗被視為遲到，若連登入終態處理都被略過，「正在登入」會永遠留著。
#[test]
fn stale_resource_failure_still_settles_login_progress() {
    let mut app = app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities_course = Some("B".to_owned());
    app.lms.activities.start_loading("正在加载课程活动…");
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录考勤系统…".to_owned(),
    }));

    apply_event(
        &mut app,
        Event::Failed {
            what: "课程活动".to_owned(),
            message: "网络连接失败".to_owned(),
            target: FailedTarget::Activities,
            site: Some(SiteKind::Lms),
            resource: Some("A".to_owned()),
        },
    );

    assert!(
        app.lms.activities.is_loading(),
        "旧资源的失败不得标记目前页面"
    );
    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Failed { .. })),
        "覆盖层仍须离开「正在登录」：{:?}",
        app.login
    );
}

/// 保存失敗的通知必須在登入成功之後仍看得見。
///
/// 工作者先發 `LoginSucceeded`（主要結果）、再發 `CredentialSaveFailed`
/// （附帶副作用）；介面訊息是後到者覆蓋先前的，故保存失敗不能被清掉。
#[test]
fn credential_save_failure_survives_login_success() {
    let mut app = app();
    app.set_screen(Screen::Main);
    let mut form = FormState::login_retry(SiteKind::Attendance);
    form.busy = true;
    app.login = Some(Box::new(LoginScreen::Credentials {
        site: SiteKind::Attendance,
        form,
        message: String::new(),
    }));

    apply_event(
        &mut app,
        Event::LoginSucceeded {
            site: SiteKind::Attendance,
            mode: Some(AccessMode::Direct),
        },
    );
    assert!(app.login.is_none(), "登录成功应关闭覆盖层");

    apply_event(
        &mut app,
        Event::CredentialSaveFailed("登录成功，但凭据保存失败：磁盘只读".to_owned()),
    );
    assert_eq!(
        app.message_text(),
        Some("登录成功，但凭据保存失败：磁盘只读"),
        "保存失败的提醒不得被「登录成功」盖掉"
    );
}

/// esc 取消登入後（等待取消完成期間），遲到的登入事件不得重開覆蓋層；
/// 收到取消完成後等待狀態清除，新的登入事件才能再開窗。
#[test]
fn dismissed_login_overlay_ignores_late_login_events_until_cancelled() {
    let mut app = app();
    app.set_screen(Screen::Main);
    // 模擬使用者已按 esc：覆蓋層關閉、等待工作者回報取消完成。
    app.login_cancel_pending = true;

    apply_event(
        &mut app,
        Event::LoginNeedsMfa {
            phone: Some("138****1234".to_owned()),
            sent: false,
        },
    );
    assert!(app.login.is_none(), "取消中的迟到短信事件不得重开覆盖层");

    apply_event(
        &mut app,
        Event::LoginProgress("正在登录考勤系统…".to_owned()),
    );
    assert!(app.login.is_none(), "取消中的迟到进度事件不得重开覆盖层");

    apply_event(
        &mut app,
        Event::LoginNeedsCaptcha(PathBuf::from("/tmp/captcha-late.png")),
    );
    assert!(app.login.is_none(), "取消中的迟到验证码事件不得重开覆盖层");
    assert!(app.captcha_path.is_none(), "被忽略的验证码不应残留路径");

    // 取消完成：清除等待狀態。
    apply_event(&mut app, Event::LoginCancelled);
    assert!(!app.login_cancel_pending, "取消完成后应清除等待状态");

    // 之後的新登入事件照常開啟覆蓋層。
    apply_event(
        &mut app,
        Event::LoginNeedsMfa {
            phone: None,
            sent: false,
        },
    );
    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Mfa { .. })),
        "取消完成后的登录事件应正常显示：{:?}",
        app.login
    );
}

/// 取消完成必須關閉任何已出現的登入覆蓋層（例如遲到事件已把它重開）。
#[test]
fn login_cancelled_closes_the_overlay() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Mfa {
        phone: Some("138****1234".to_owned()),
        sent: false,
        input: InputLine::new(),
        error: None,
    }));
    app.login_cancel_pending = true;

    apply_event(&mut app, Event::LoginCancelled);

    assert!(app.login.is_none(), "取消完成应关闭登录覆盖层");
    assert!(!app.login_cancel_pending, "取消完成应清除等待状态");
}

/// 登入成功、憑證就緒與會話停用都會清除取消等待狀態，避免永久壓抑新事件。
#[test]
fn terminal_login_events_clear_the_cancel_pending_state() {
    let mut app = app();
    app.set_screen(Screen::Main);

    app.login_cancel_pending = true;
    apply_event(
        &mut app,
        Event::LoginSucceeded {
            site: SiteKind::Attendance,
            mode: Some(AccessMode::Direct),
        },
    );
    assert!(!app.login_cancel_pending, "登录成功后应清除等待状态");

    app.login_cancel_pending = true;
    apply_event(&mut app, Event::VaultReady);
    assert!(!app.login_cancel_pending, "凭证就绪后应清除等待状态");

    app.login_cancel_pending = true;
    apply_event(
        &mut app,
        Event::SessionDisabled("无法建立新的会话".to_owned()),
    );
    assert!(!app.login_cancel_pending, "会话停用后应清除等待状态");
}

/// 詳情內容被取代時捲動回到頂端；部分結果不清位移（使用者可能正在讀）。
#[test]
fn detail_updates_reset_scroll() {
    let mut app = app();
    app.homework_scroll.sync(5, 20);
    app.homework_scroll.to_bottom();
    assert_eq!(app.homework_scroll.offset(), 15);

    apply_event(&mut app, Event::Homework(homework_update(Some((1, 2)))));
    assert_eq!(app.homework_scroll.offset(), 15, "部分结果不应打断阅读位置");

    apply_event(&mut app, Event::Homework(homework_update(None)));
    assert_eq!(app.homework_scroll.offset(), 0, "终态更新应回到顶端");

    app.lms.detail_activity = Some("1".to_owned());
    app.lms.detail_scroll.sync(5, 20);
    app.lms.detail_scroll.to_bottom();
    assert_eq!(app.lms.detail_scroll.offset(), 15);
    apply_event(
        &mut app,
        Event::ActivityDetail(Box::new(ActivityDetailView {
            id: "1".to_owned(),
            title: "作业A".to_owned(),
            kind: crate::sites::lms::ActivityKind::Homework,
            description: None,
            end_time: None,
            submit_by_group: Some(false),
            submissions: None,
            note: None,
        })),
    );
    assert_eq!(app.lms.detail_scroll.offset(), 0, "详情更新应回到顶端");
}

// ── 任務快照（自訂義任務） ───────────────────────────────

/// 測試用任務。
fn todo_task(id: u64, content: &str, completed: bool) -> Task {
    Task {
        id,
        content: content.to_owned(),
        description: None,
        deadline: None,
        priority: Priority::Low,
        completed,
    }
}

#[test]
fn tasks_event_replaces_the_list_and_anchors_the_selection() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.tasks = vec![todo_task(1, "甲", false), todo_task(2, "乙", false)];
    app.homework_state.select(Some(1));

    // 背景端依排序鍵重新排列後回報：選取應跟著識別碼，而不是位置。
    apply_event(
        &mut app,
        Event::Tasks(vec![todo_task(2, "乙", false), todo_task(1, "甲", false)]),
    );
    let unfinished = app.task_group_items(HomeworkGroup::Unfinished);
    assert_eq!(unfinished.len(), 2);
    assert_eq!(
        unfinished[app.page_selection()].id,
        2,
        "选取应锚定在同一任务"
    );

    // 目前任務移到已完成分組（不在目前分組就找不到）：索引夾在新長度內。
    apply_event(
        &mut app,
        Event::Tasks(vec![todo_task(1, "甲", false), todo_task(2, "乙", true)]),
    );
    assert_eq!(app.page_selection(), 0, "找不到原本的任务时应夹取索引");
    assert_eq!(app.task_group_items(HomeworkGroup::Unfinished)[0].id, 1);
    assert_eq!(
        app.page_group_count(HomeworkGroup::Completed),
        1,
        "分组计数应包含自訂義任务"
    );
}

#[test]
fn tasks_failure_keeps_the_form_and_shows_the_error_in_place() {
    let mut app = app();
    app.set_screen(Screen::TaskForm(Box::new(TaskFormState::add())));
    if let Screen::TaskForm(form) = &mut app.screen {
        form.busy = true;
    }

    apply_event(
        &mut app,
        Event::Failed {
            what: "任务".to_owned(),
            message: "无法写入任务文件".to_owned(),
            target: FailedTarget::Tasks,
            site: None,
            resource: None,
        },
    );

    let Screen::TaskForm(form) = &app.screen else {
        panic!("保存失败应留在任务表单，而不是关闭它");
    };
    assert!(!form.busy, "失败应解除保存中");
    assert_eq!(form.error.as_deref(), Some("任务失败：无法写入任务文件"));
    assert_eq!(app.message_text(), Some("任务失败：无法写入任务文件"));
}

#[test]
fn tasks_event_keeps_the_task_form_open() {
    let mut app = app();
    app.set_screen(Screen::TaskForm(Box::new(TaskFormState::add())));
    apply_event(&mut app, Event::Tasks(vec![todo_task(1, "甲", false)]));
    assert!(
        matches!(app.screen, Screen::TaskForm(_)),
        "任务表单属于主画面，不得被背景快照关闭"
    );
}

#[test]
fn account_change_keeps_local_tasks_but_clears_task_page_state() {
    let mut app = app();
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.tasks = vec![todo_task(1, "甲", false)];
    app.task_filter = Some("甲".to_owned());
    app.task_multi = Some(HashSet::from([1]));
    app.task_pending_delete = Some((1, "甲".to_owned()));

    apply_event(
        &mut app,
        Event::SessionsCleared {
            account_changed: true,
        },
    );

    assert_eq!(app.tasks.len(), 1, "本地任务与账号无关，换账号应保留");
    assert!(app.task_filter.is_none(), "筛选属于暂时状态，应清除");
    assert!(app.task_multi.is_none(), "多选属于暂时状态，应清除");
    assert!(app.task_pending_delete.is_none(), "待确认删除应清除");
    assert!(matches!(app.screen, Screen::Main));
}
