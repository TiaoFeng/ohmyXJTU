//! 按鍵處理測試：表單驗證、任務送出與導航觸發載入。

use std::collections::HashSet;
use std::sync::mpsc::{Sender, channel};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::AccessPolicy;
use crate::domain::activity::ActivityGroup;
use crate::domain::homework::{HomeworkInput, aggregate};
use crate::domain::semester::TermCode;
use crate::domain::todo::{Priority, SortMode, Task};
use crate::model::{ActivityDetailView, ScheduleData};
use crate::sites::lms::{ActivityKind, LmsActivity, LmsCourse};
use crate::task::Job;
use crate::tone::Tone;
use crate::tui::app::{
    AgreementState, App, FormKind, FormState, HomeworkData, LmsLevel, LoginScreen, NavItem, Page,
    Screen, SettingsState, SyncImportState, TaskConfirmState, TaskEntry, TaskField, TaskFormMode,
    TermPickerState,
};
use crate::tui::controller::FormValues;

use super::{handle_key, handle_paste};

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
        panic!("提交后应停留在设置表单");
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
        panic!("应停留在设置画面");
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
        "新口令 8 字符、原口令 6 字符应被接受"
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
    assert!(rx.try_recv().is_err(), "处理中不应重复送出");
    let Screen::Setup(form) = &app.screen else {
        panic!("应停留在设置表单");
    };
    assert_eq!(form.value("账号"), "3120000001", "处理中输入不应被改动");
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
    app.attendance = Page::Ready(crate::model::FlowData {
        records: Vec::new(),
        page: 3,
        total_pages: 5,
        total: 0,
    });

    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 3 })));
}

/// 連續翻頁：目標頁以「使用者最後選定的頁碼」計算，而不是畫面上的舊頁。
///
/// `Page::start_loading` 會保留舊資料，所以載入期間 `ready()` 仍是上一頁；
/// 拿它計算會讓連按兩次 `]` 都算成同一頁，只前進一頁。
#[test]
fn flow_paging_counts_from_the_pending_page() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::model::FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 5,
        total: 0,
    });

    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 2 })));
    assert_eq!(app.flow_pending_page, Some(2), "应记下待回的目标页码");

    // 第 2 頁還沒回來就再按一次：應以第 2 頁為基準前進到第 3 頁。
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 3 })));
    assert_eq!(app.flow_pending_page, Some(3));

    // 往回一頁同樣以最後選定的頁碼為基準。
    press(&mut app, &jobs, KeyCode::Char('['));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 2 })));
}

#[test]
fn flow_paging_respects_bounds() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::model::FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 2,
        total: 0,
    });

    // 已在第一頁，往上一頁不應送出任務。
    press(&mut app, &jobs, KeyCode::Char('['));
    assert!(rx.try_recv().is_err());

    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 2 })));
}

/// 考勤流水頁的 `[`／`]` 翻頁（與其他頁面統一）；`n`／`p` 已不再是翻頁鍵。
#[test]
fn bracket_keys_page_the_flow_on_attendance_page() {
    use crate::domain::homework::HomeworkGroup;

    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::model::FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 3,
        total: 0,
    });

    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 2 })));

    // 翻頁不應動到作業分組（同一組按鍵在不同頁面有不同作用）。
    assert_eq!(app.homework_group, HomeworkGroup::Unfinished);

    // 舊的翻頁鍵不再作用：不翻頁、不改分組、不送任務。
    press(&mut app, &jobs, KeyCode::Char('n'));
    press(&mut app, &jobs, KeyCode::Char('p'));
    assert!(rx.try_recv().is_err(), "n/p 不应再送出翻页任务");
    assert_eq!(app.flow_pending_page, Some(2), "n/p 不应改变待回页码");
    assert_eq!(app.homework_group, HomeworkGroup::Unfinished);
}

/// 看過的流水頁：翻回去時直接顯示記憶體中的那一頁（不進載入中、不重查）；
/// 待回頁碼仍要記（遲到的舊頁結果必須據此丟棄）。
#[test]
fn switching_back_to_a_cached_flow_page_shows_it_without_reloading() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::model::FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 3,
        total: 20,
    });
    app.store_flow_page(&crate::model::FlowData {
        records: Vec::new(),
        page: 2,
        total_pages: 3,
        total: 60,
    });

    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.flow_pending_page, Some(2), "应记下待回的目标页码");
    assert!(!app.attendance.is_loading(), "命中快取不应进入加载中");
    assert_eq!(
        app.attendance.ready().map(|data| data.total),
        Some(60),
        "应直接显示该页的快取资料"
    );
    assert!(rx.try_recv().is_err(), "命中快取不应送出网络任务");
}

/// `r` 強制刷新：頁快取作廢，之後翻回任何一頁都會重新查詢。
#[test]
fn refresh_clears_the_flow_page_cache() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(crate::model::FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 3,
        total: 20,
    });
    app.store_flow_page(&crate::model::FlowData {
        records: Vec::new(),
        page: 2,
        total_pages: 3,
        total: 60,
    });

    // 重新查詢目前頁（第 1 頁）：頁快取一併作廢。
    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(rx.try_recv(), Ok(Job::LoadFlow { page: 1 })));

    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(app.attendance.is_loading(), "过期快取不应直接显示");
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

/// 學期選擇器上的 `^P` 同樣是「開啟帳戶設定」，不是只把彈窗關掉。
///
/// 底欄（彈窗開啟時只留登入狀態）不再列出 `^P`，因此按鍵本身的意義必須
/// 與其他畫面一致；選擇器被設定畫面取代後可用 `s` 重新開啟。
#[test]
fn control_p_on_the_term_picker_opens_settings() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    let options = vec![TermCode::parse("2026-2027-1").expect("学期")];
    app.term_options = options.clone();
    app.set_screen(Screen::TermPicker(TermPickerState::new(
        options,
        None,
        "考勤系统不可用".to_owned(),
    )));

    press_ctrl(&mut app, &jobs, 'p');

    assert!(
        matches!(app.screen, Screen::Settings(_)),
        "^P 应开启账户设置：{:?}",
        app.screen
    );
    assert_eq!(app.term_options.len(), 1, "选项应保留以供重新开启");
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
        panic!("应停留在设置弹窗");
    };
    assert_eq!(state.policy(app.access_policy), AccessPolicy::Direct);
    assert!(rx.try_recv().is_err(), "调整草稿不应送出任务");

    // 左鍵回到 Auto。
    press(&mut app, &jobs, KeyCode::Left);
    let Screen::Settings(state) = app.screen else {
        panic!("应停留在设置弹窗");
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
fn settings_sync_item_opens_the_sync_menu() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));
    for _ in 0..SettingsState::SYNC_INDEX {
        press(&mut app, &jobs, KeyCode::Down);
    }
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(
        matches!(app.screen, Screen::SyncMenu(_)),
        "{:?}",
        app.screen
    );
    assert!(rx.try_recv().is_err(), "開啟子選單不送任務");

    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Settings(_)));
}

#[test]
fn sync_menu_opens_the_config_form_and_submits_settings() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncMenu(crate::tui::app::SyncMenuState::default()));

    // 尚未設定：唯一項目是「配置并启用同步」，開啟同步設定表單。
    press(&mut app, &jobs, KeyCode::Enter);
    let Screen::SettingsForm(form) = &app.screen else {
        panic!("应开启同步设置表单：{:?}", app.screen);
    };
    assert_eq!(form.kind, FormKind::SyncConfig);
    assert!(
        form.value("服务器地址").starts_with("https://"),
        "伺服器位址应预填预设值"
    );

    // 伺服器位址已預填，填帳號與應用密碼後送出。
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");
    press(&mut app, &jobs, KeyCode::Enter);

    match rx.try_recv() {
        Ok(Job::SetSyncConfig { config }) => {
            assert_eq!(config.url, "https://dav.jianguoyun.com/dav/");
            assert_eq!(config.account, "u@example.com");
            assert_eq!(config.app_password, "app-pass");
        }
        other => panic!("应送出同步设置：{other:?}"),
    }
}

#[test]
fn sync_menu_submits_each_action() {
    let mut app = App::new(AccessPolicy::Auto);
    app.sync = crate::task::SyncStateView {
        configured: true,
        unavailable: false,
        url: "https://dav.example/dav/".to_owned(),
        account: "u@example.com".to_owned(),
        auto_sync: false,
    };

    // 已設定：項目依序為 [立即同步, 上傳, 下載, 自動同步, 修改設定, 清除]。
    let (jobs, rx) = channel();
    app.set_screen(Screen::SyncMenu(crate::tui::app::SyncMenuState::default()));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::SyncNow)));

    let (jobs, rx) = channel();
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::SyncPush)));

    let (jobs, rx) = channel();
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::SyncPull)));

    // 自動同步：目前為關，切換後送出「開」。
    let (jobs, rx) = channel();
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetSyncAuto { enabled: true })
    ));

    // 修改伺服器設定：開啟預填現有值的表單。
    let (jobs, _rx) = channel();
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    let Screen::SettingsForm(form) = &app.screen else {
        panic!("应开启同步设置表单：{:?}", app.screen);
    };
    assert_eq!(form.value("坚果云账号"), "u@example.com");

    // 清除同步配置。
    let (jobs, rx) = channel();
    app.set_screen(Screen::SyncMenu(crate::tui::app::SyncMenuState::default()));
    for _ in 0..5 {
        press(&mut app, &jobs, KeyCode::Down);
    }
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::ClearSyncConfig)));
}

/// 同步設定檔讀不開時，子選單只留「清除同步配置」。
///
/// 那個狀態下同步與重新設定都會被存儲拒絕（見 `SyncStore::save`）；若畫面仍
/// 只提供「配置并启用同步」，使用者永遠回不到可用狀態。
#[test]
fn unavailable_sync_settings_only_offer_clear() {
    let mut app = App::new(AccessPolicy::Auto);
    app.sync = crate::task::SyncStateView {
        unavailable: true,
        ..crate::task::SyncStateView::default()
    };

    // 唯一項目就是清除：上下鍵不會跑出別的動作，enter 送出清除。
    let (jobs, rx) = channel();
    app.set_screen(Screen::SyncMenu(crate::tui::app::SyncMenuState::default()));
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::ClearSyncConfig)));
}

#[test]
fn unlock_ctrl_y_opens_the_import_screen() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Unlock(FormState::unlock()));
    press_ctrl(&mut app, &jobs, 'y');
    assert!(
        matches!(app.screen, Screen::SyncImport(_)),
        "{:?}",
        app.screen
    );
    assert!(rx.try_recv().is_err(), "開啟導入畫面不送任務");

    // esc 回到解鎖畫面。
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Unlock(_)));
}

#[test]
fn sync_import_tests_and_imports_with_the_entered_settings() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(false)));
    // 伺服器位址已預填，填入帳號與應用密碼。
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");

    press_ctrl(&mut app, &jobs, 't');
    match rx.try_recv() {
        Ok(Job::SyncTestConnection { .. }) => {}
        other => panic!("^t 应送出连线测试：{other:?}"),
    }

    // 處理中：忽略其餘輸入，不重送。
    press_ctrl(&mut app, &jobs, 's');
    assert!(rx.try_recv().is_err(), "处理中不应重复送出");
}

/// 導入會覆寫本機檔案且不留備份：第一次 `enter` 只跳到確認，不送出任務。
#[test]
fn sync_import_asks_for_confirmation_before_overwriting() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(false)));
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");

    // 第一次 enter：只顯示確認，不送出。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "确认前不得送出导入任务");
    let Screen::SyncImport(state) = &app.screen else {
        panic!("应停留在导入画面：{:?}", app.screen);
    };
    assert!(state.confirming, "应进入确认步骤");
    let (message, tone) = state.message.clone().expect("应显示确认提示");
    assert!(message.contains("覆写"), "应说明会覆写本机文件：{message}");
    assert!(message.contains("不保留备份"), "{message}");
    assert_eq!(tone, Tone::Warning, "确认提示应为警告色");

    // 確認期間不得編輯欄位（設定已驗證過，內容不該再變）。
    let before = state.form.value("坚果云账号").to_owned();
    type_text(&mut app, &jobs, "x");
    let Screen::SyncImport(state) = &app.screen else {
        panic!("应停留在导入画面");
    };
    assert_eq!(state.form.value("坚果云账号"), before, "确认时不应接受输入");

    // 第二次 enter：真的送出導入。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::SyncImport { .. })));
}

/// 確認步驟的 `esc` 回到表單繼續編輯（不離開畫面，已填的設定保留）。
#[test]
fn sync_import_confirmation_can_be_cancelled() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(false)));
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");
    press(&mut app, &jobs, KeyCode::Enter);
    press(&mut app, &jobs, KeyCode::Esc);

    let Screen::SyncImport(state) = &app.screen else {
        panic!("esc 应回到表单而非离开画面：{:?}", app.screen);
    };
    assert!(!state.confirming, "应取消确认");
    assert!(state.message.is_none(), "确认提示应清除");
    assert_eq!(
        state.form.value("坚果云账号"),
        "u@example.com",
        "设定应保留"
    );
    assert!(rx.try_recv().is_err(), "取消确认不得送出任务");
}

/// 首次設定（本機尚無憑證檔）的確認文字不得說「覆寫」。
#[test]
fn sync_import_confirmation_wording_matches_the_source_screen() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(true)));
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");
    press(&mut app, &jobs, KeyCode::Enter);

    let Screen::SyncImport(state) = &app.screen else {
        panic!("应停留在导入画面");
    };
    let (message, _) = state.message.clone().expect("应显示确认提示");
    assert!(message.contains("写入本机"), "{message}");
    assert!(
        !message.contains("覆写"),
        "首次设定没有东西会被覆写：{message}"
    );
}

#[test]
fn sync_import_reports_validation_errors_in_place() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(false)));
    // 帳號與應用密碼留空：enter 就地報錯、不送任務。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "验证失败不应送出任务");
    let Screen::SyncImport(state) = &app.screen else {
        panic!("应停留在导入画面");
    };
    assert!(state.message.is_some(), "应就地显示验证错误");
}

/// 明文 HTTP 會讓 HTTP Basic 的帳號與應用密碼在網路上裸奔：表單就要擋下。
#[test]
fn sync_import_rejects_a_plain_http_server_url() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(false)));
    // 清掉預填的 https 位址，換成明文 http。
    press_ctrl(&mut app, &jobs, 'u');
    type_text(&mut app, &jobs, "http://dav.example/dav/");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "明文 HTTP 不应送出任务");
    let Screen::SyncImport(state) = &app.screen else {
        panic!("应停留在导入画面：{:?}", app.screen);
    };
    let (message, tone) = state.message.clone().expect("应就地显示验证错误");
    assert!(
        message.contains("https://"),
        "应指出必须使用 https：{message}"
    );
    assert_eq!(tone, Tone::Danger, "验证错误应以错误色显示");
}

/// 位址帶查詢參數會讓固定檔名被拼進查詢字串、請求打到錯的目標：表單就要擋下。
#[test]
fn sync_import_rejects_a_server_url_with_a_query() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::SyncImport(SyncImportState::new(false)));
    press_ctrl(&mut app, &jobs, 'u');
    type_text(&mut app, &jobs, "https://dav.example/dav/?x=1");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "u@example.com");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "app-pass");

    press(&mut app, &jobs, KeyCode::Enter);
    assert!(rx.try_recv().is_err(), "带查询参数的地址不应送出任务");
    let Screen::SyncImport(state) = &app.screen else {
        panic!("应停留在导入画面：{:?}", app.screen);
    };
    let (message, _) = state.message.clone().expect("应就地显示验证错误");
    assert!(
        message.contains("查询参数"),
        "应指出不得带查询参数：{message}"
    );
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
        site: crate::session::SiteKind::Attendance,
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
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::RetryLogin {
            site: crate::session::SiteKind::Attendance
        })
    ));
    assert!(
        matches!(app.login.as_deref(), Some(LoginScreen::Progress { .. })),
        "重试时应显示进度覆盖层"
    );

    press(&mut app, &jobs, KeyCode::Char('q'));
    assert!(app.quit);
}

/// 失敗畫面按 esc 關閉覆蓋層：使用者得以回到頁面按 r 重試（不必重啟程式）。
#[test]
fn failed_screen_esc_closes_overlay_so_refresh_works() {
    let (jobs, rx) = channel();
    let mut app = failed_app("登录失败：网络连接失败（域名解析失败）");

    press(&mut app, &jobs, KeyCode::Esc);
    assert!(app.login.is_none(), "esc 应关闭登录覆盖层");
    assert!(
        app.login_cancel_pending,
        "esc 后应进入等待取消状态，直到工作者回报取消完成"
    );
    assert!(
        matches!(rx.try_recv(), Ok(Job::CancelLogin)),
        "关闭覆盖层应一并取消工作者端的登录流程"
    );

    // 覆蓋層關閉後，主畫面的 r 才能刷新目前頁面。
    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(
        matches!(rx.try_recv(), Ok(Job::LoadSchedule { force: true })),
        "关闭覆盖层后 r 应能刷新目前页面"
    );
}

#[test]
fn failed_screen_opens_credentials_form() {
    let (jobs, _rx) = channel();
    let mut app = failed_app("登录失败：用户名或密码错误");

    press(&mut app, &jobs, KeyCode::Char('e'));

    let form = credentials_form(&app);
    assert!(matches!(form.kind, FormKind::LoginRetry(_)));
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
        Some(LoginScreen::Failed { message, .. }) => {
            assert_eq!(message, "登录失败：用户名或密码错误");
        }
        other => panic!("应回到失败画面，实际为 {other:?}"),
    }
}

/// 由思源學堂的失敗畫面重新輸入帳密時，任務必須帶回思源學堂。
#[test]
fn failed_lms_screen_retries_with_the_lms_site() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Failed {
        site: crate::session::SiteKind::Lms,
        message: "登录失败：用户名或密码错误".to_owned(),
    }));

    // 表單本身也帶著同一個站點。
    press(&mut app, &jobs, KeyCode::Char('e'));
    assert!(matches!(
        credentials_form(&app).kind,
        FormKind::LoginRetry(crate::session::SiteKind::Lms)
    ));

    type_text(&mut app, &jobs, "3120000001");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "pw-12345");
    press(&mut app, &jobs, KeyCode::Tab);
    type_text(&mut app, &jobs, "secret123");
    press(&mut app, &jobs, KeyCode::Enter);

    match rx.try_recv() {
        Ok(Job::RetryWithAccount { site, .. }) => {
            assert_eq!(site, crate::session::SiteKind::Lms)
        }
        other => panic!("应为凭证重输任务，实际为 {other:?}"),
    }
}

#[test]
fn login_progress_esc_dismisses_and_cancels_login() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录考勤系统…".to_owned(),
    }));

    press(&mut app, &jobs, KeyCode::Esc);

    assert!(app.login.is_none(), "esc 应关闭登录覆盖层");
    assert!(
        app.login_cancel_pending,
        "esc 后应进入等待取消状态，直到工作者回报取消完成"
    );
    assert!(
        matches!(rx.try_recv(), Ok(Job::CancelLogin)),
        "应送出取消登录任务"
    );
}

#[test]
fn paste_inserts_text_into_the_focused_field() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));

    handle_paste(&mut app, "secret123");
    let Screen::Setup(form) = &app.screen else {
        panic!("应停留在首次设置表单");
    };
    assert_eq!(form.fields[0].value.value(), "secret123");

    // 換行與回車不得被插入，也不得送出表單。
    handle_paste(&mut app, "\nmore\r");
    let Screen::Setup(form) = &app.screen else {
        panic!("应停留在首次设置表单");
    };
    assert_eq!(form.fields[0].value.value(), "secret123more");
}

#[test]
fn form_values_debug_never_leaks_secrets() {
    let mut form = FormState::change_account();
    form.fields[0].value.set("SUPER-SECRET-PASSPHRASE");
    form.fields[1].value.set("3120000009");
    form.fields[2].value.set("SUPER-SECRET-PASSWORD");
    form.fields[3].value.set("SUPER-SECRET-PASSWORD");

    let values = FormValues::from_form(&form);
    let debug = format!("{values:?}");

    assert!(
        !debug.contains("SUPER-SECRET-PASSPHRASE"),
        "口令不得进 Debug：{debug}"
    );
    assert!(
        !debug.contains("SUPER-SECRET-PASSWORD"),
        "密码不得进 Debug：{debug}"
    );
    assert!(!debug.contains("3120000009"), "账号不得进 Debug：{debug}");
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
    assert_eq!(app.nav, NavItem::Schedule, "覆盖层开启时不应切换页面");
    assert!(matches!(app.screen, Screen::Main), "底层画面维持不变");

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
    assert!(matches!(app.screen, Screen::Main), "覆盖层期间不应开启设置");
}

#[test]
fn bracket_keys_switch_homework_group_only_on_homework_page() {
    let (jobs, rx) = channel();
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

    // 課表頁的同一組按鍵改為切換週次（見
    // `bracket_keys_switch_schedule_week_on_schedule_page`）：不得連帶改動
    // 作業分組；尚未載入課表時也不會送出任何任務。
    app.nav = NavItem::Schedule;
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(
        app.homework_group,
        HomeworkGroup::Completed,
        "课表页不应改动作业分组"
    );
    assert!(rx.try_recv().is_err(), "未载入课表时不应送出任务");
}

/// 課表頁的 `[`／`]` 切換週次：標題立即顯示目標週、內容進入載入中，
/// 記下待回的目標週次（供 `event::apply_schedule` 丟棄遲到的舊週結果），
/// 並送出 `SetScheduleWeek`（`reload` 表示介面還需要該週資料）；到邊界不動作。
#[test]
fn bracket_keys_switch_schedule_week_on_schedule_page() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;

    // 尚未載入課表（沒有週次與總週數）：不應送出任務。
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(rx.try_recv().is_err(), "未载入课表时不应切周");

    app.schedule_week = Some(3);
    app.schedule_total = Some(5);
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.schedule_week, Some(4), "标题应立即显示目标周");
    assert_eq!(app.schedule_pending_week, Some(4), "应记下待回的目标周次");
    assert!(app.schedule.is_loading(), "内容应进入加载中");
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetScheduleWeek {
            week: 4,
            reload: true
        })
    ));

    press(&mut app, &jobs, KeyCode::Char('['));
    assert_eq!(app.schedule_week, Some(3));
    assert_eq!(app.schedule_pending_week, Some(3));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetScheduleWeek {
            week: 3,
            reload: true
        })
    ));

    // 邊界：第 1 週不再往前，最後一週不再往後。
    app.schedule_week = Some(1);
    press(&mut app, &jobs, KeyCode::Char('['));
    assert_eq!(app.schedule_week, Some(1), "第 1 周不应再往前");
    app.schedule_week = Some(5);
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.schedule_week, Some(5), "最后一周不应再往后");
    assert!(rx.try_recv().is_err(), "边界不应送出任务");
}

/// 看過的週次：翻回去時直接顯示記憶體中的課表（不進載入中、不重查考勤），
/// 但仍要通知工作者目前的週次（`reload: false`）——按 `r` 時送的是
/// `LoadSchedule`，工作者只依自己的週次狀態決定要載入哪一週，不同步就會載入
/// 上一次真正查詢過的那一週。
#[test]
fn switching_back_to_a_cached_week_shows_it_without_reloading() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule_week = Some(4);
    app.schedule_total = Some(5);
    app.schedule = Page::Ready(ScheduleData {
        semester: "2026-2027-1".to_owned(),
        week: 4,
        total_weeks: 5,
        ..ScheduleData::default()
    });
    app.store_schedule_week(&ScheduleData {
        semester: "2026-2027-1".to_owned(),
        week: 3,
        total_weeks: 5,
        notice: Some("第三周资料".to_owned()),
        ..ScheduleData::default()
    });

    press(&mut app, &jobs, KeyCode::Char('['));
    assert_eq!(app.schedule_week, Some(3));
    assert_eq!(app.schedule_pending_week, Some(3));
    assert!(!app.schedule.is_loading(), "命中快取不应进入加载中");
    assert_eq!(
        app.schedule.ready().and_then(|data| data.notice.as_deref()),
        Some("第三周资料"),
        "应直接显示该周的快取资料"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetScheduleWeek {
            week: 3,
            reload: false
        })
    ));
}

/// `r` 強制刷新：週快取作廢，之後翻回任何一週都會重新查詢。
#[test]
fn refresh_clears_the_schedule_week_cache() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule_week = Some(4);
    app.schedule_total = Some(5);
    app.store_schedule_week(&ScheduleData {
        week: 3,
        total_weeks: 5,
        ..ScheduleData::default()
    });

    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadSchedule { force: true })
    ));

    // 快取已作廢：翻到第 3 週必須重新查詢。
    press(&mut app, &jobs, KeyCode::Char('['));
    assert!(app.schedule.is_loading(), "过期快取不应直接显示");
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::SetScheduleWeek {
            week: 3,
            reload: true
        })
    ));
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
        data: None,
        top_level_description: None,
        uploads: Vec::new(),
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
    assert!(rx.try_recv().is_err(), "切换分组不应送出网络任务");

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
        description: None,
        end_time: None,
        submit_by_group: Some(false),
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
        description: None,
        submit_by_group: Some(false),
        submission_count: Some(submitted),
        note: None,
    };
    HomeworkData {
        term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
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
    assert!(rx.try_recv().is_err(), "未加载时不应送出任务");
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
    assert!(rx.try_recv().is_err(), "空清单不应送出任务");
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
        Ok(Job::LoadActivityDetail { activity_id, .. }) if activity_id == "2"
    ));
    assert_eq!(app.lms.level, LmsLevel::Detail);
    assert!(app.lms.detail.is_loading(), "应切换到详情加载中");
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

/// 切換到另一門課程時，不得沿用前一門課的活動清單。
#[test]
fn switching_courses_drops_the_previous_courses_activities() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    app.lms.courses = Page::Ready(vec![
        course_with_term("1", Some("2026-1")),
        course_with_term("2", Some("2026-1")),
    ]);

    // 課程 1 的活動已載入。
    app.course_state.select(Some(0));
    press(&mut app, &jobs, KeyCode::Enter);
    let _ = rx.try_recv();
    app.lms.activities = Page::Ready(vec![lms_activity("11", "homework")]);
    app.lms.activities_course = Some("1".to_owned());

    // 改看課程 2：活動必須清空，不能還顯示課程 1 的清單。
    press(&mut app, &jobs, KeyCode::Esc);
    app.course_state.select(Some(1));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(rx.try_recv(), Ok(Job::LoadActivities { course_id, .. }) if course_id == "2"));
    assert_eq!(app.lms.activities_course.as_deref(), Some("2"));
    assert!(app.lms.activities.is_loading());
    assert!(
        app.lms.activities.ready().is_none(),
        "不得沿用前一门课的活动清单"
    );

    // 重新進入同一門課則保留舊資料：刷新期間仍可閱讀。
    app.lms.activities = Page::Ready(vec![lms_activity("21", "homework")]);
    press(&mut app, &jobs, KeyCode::Esc);
    press(&mut app, &jobs, KeyCode::Enter);
    let _ = rx.try_recv();
    assert_eq!(
        app.lms.activities.ready().map(Vec::len),
        Some(1),
        "同一门课重新加载时应保留旧数据"
    );
}

/// 切換活動時同樣不得沿用上一個活動的詳情。
#[test]
fn switching_activities_drops_the_previous_detail() {
    let (jobs, rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activity_group = ActivityGroup::Homework;
    app.lms.activities = Page::Ready(vec![
        lms_activity("1", "homework"),
        lms_activity("2", "homework"),
    ]);
    app.lms.detail_activity = Some("1".to_owned());
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "活动 1".to_owned(),
        ..ActivityDetailView::default()
    });

    // 改看活動 2：詳情必須清空（不能還顯示活動 1 的內容）。
    app.activity_state.select(Some(1));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadActivityDetail { activity_id, .. }) if activity_id == "2"
    ));
    assert_eq!(app.lms.detail_activity.as_deref(), Some("2"));
    assert!(app.lms.detail.is_loading());
    assert!(app.lms.detail.ready().is_none(), "不得沿用上一个活动的详情");
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
            site,
            credentials,
            passphrase,
        }) => {
            assert_eq!(site, crate::session::SiteKind::Attendance);
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
    app.agreement.as_deref().expect("协议阅读门应开启")
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
        "协议开启时 ctrl+p 不作用"
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
            "ctrl+u 不得穿透协议画面"
        );
    }

    // 主畫面快捷鍵不作用：`s` 不開學期選擇器、`]` 不切換分組。
    app.set_screen(Screen::Main);
    let group = app.homework_group;
    press(&mut app, &jobs, KeyCode::Char('s'));
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert!(matches!(app.screen, Screen::Main), "协议开启时 s 不作用");
    assert_eq!(app.homework_group, group, "协议开启时 ] 不作用");
}

#[test]
fn agreement_scroll_keys_move_document() {
    let (jobs, _rx) = channel();
    let mut app = app_with_agreement();
    app.agreement.as_mut().expect("阅读门").sync_layout(10, 50);

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
    app.agreement.as_mut().expect("阅读门").sync_layout(10, 50);

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
    assert!(app.quit, "q 应直接退出");
    assert!(rx.try_recv().is_err(), "退出不得送出同意");

    let mut app = app_with_agreement();
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(app.quit, "esc 应直接退出");
    assert!(rx.try_recv().is_err(), "退出不得送出同意");

    let mut app = app_with_agreement();
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &jobs,
    );
    assert!(app.quit, "ctrl+c 一律可退出");
}

/// 帶 CONTROL／ALT 修飾的字元不得被當成普通字元寫進輸入框。
///
/// crossterm 對 `Ctrl+A` 同樣回報 `Char('a')`；若只看 `code`，控制鍵會污染
/// 表單內容（含口令與密碼欄位）。
#[test]
fn control_modified_characters_are_not_inserted() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Unlock(FormState::unlock()));

    for (code, modifiers) in [
        (KeyCode::Char('a'), KeyModifiers::CONTROL),
        (KeyCode::Char('s'), KeyModifiers::CONTROL),
        (
            KeyCode::Char('x'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
        (KeyCode::Char('a'), KeyModifiers::ALT),
    ] {
        handle_key(&mut app, KeyEvent::new(code, modifiers), &jobs);
    }

    let Screen::Unlock(form) = &app.screen else {
        panic!("应停留在解锁表单");
    };
    assert!(
        form.focused().expect("聚焦字段").value.is_empty(),
        "控制键不得插入内容"
    );

    // 一般字元仍可正常輸入，Ctrl+U 仍是清空（既有行為）。
    type_text(&mut app, &jobs, "abc");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &jobs,
    );
    let Screen::Unlock(form) = &app.screen else {
        panic!("应停留在解锁表单");
    };
    assert_eq!(form.value("加密口令"), "", "ctrl+u 应清空字段");
}

/// 捲動鍵（PgUp／PgDn／Home／End）：只在詳情可捲動的頁面生效。
#[test]
fn page_keys_scroll_detail_only_on_detail_pages() {
    let (jobs, _rx) = channel();
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;

    // 未展開詳情：捲動鍵不動作。
    app.homework_scroll.sync(5, 20);
    press(&mut app, &jobs, KeyCode::PageDown);
    assert_eq!(app.homework_scroll.offset(), 0, "未展开详情时不应滚动");

    // 展開詳情：PgDn／PgUp 以視窗為一步，End／Home 直達兩端。
    app.homework_detail = true;
    press(&mut app, &jobs, KeyCode::PageDown);
    assert_eq!(app.homework_scroll.offset(), 5, "PgDn 应往下滚一页");
    press(&mut app, &jobs, KeyCode::PageUp);
    assert_eq!(app.homework_scroll.offset(), 0, "PgUp 应往上滚一页");
    press(&mut app, &jobs, KeyCode::End);
    assert_eq!(app.homework_scroll.offset(), 15, "End 应滚到底端");
    press(&mut app, &jobs, KeyCode::Home);
    assert_eq!(app.homework_scroll.offset(), 0, "Home 应回到顶端");

    // 換一筆作業（↓）：新的說明從頂端開始讀。
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[
            HomeworkInput {
                course_id: "1".to_owned(),
                course_name: "编译原理".to_owned(),
                activity_id: "a-1".to_owned(),
                title: "作业一".to_owned(),
                end_time: None,
                description: None,
                submit_by_group: Some(false),
                submission_count: Some(0),
                note: None,
            },
            HomeworkInput {
                course_id: "1".to_owned(),
                course_name: "编译原理".to_owned(),
                activity_id: "a-2".to_owned(),
                title: "作业二".to_owned(),
                end_time: None,
                description: None,
                submit_by_group: Some(false),
                submission_count: Some(0),
                note: None,
            },
        ],
        now,
    );
    app.homework = Page::Ready(HomeworkData {
        items,
        ..HomeworkData::default()
    });
    app.homework_scroll.sync(5, 20);
    app.homework_scroll.to_bottom();
    assert_eq!(app.homework_scroll.offset(), 15, "前置条件：已滚到底端");
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(app.homework_scroll.offset(), 0, "换作业后应回到顶端");

    // 思源學堂詳情層：捲動作用於活動詳情。
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail_scroll.sync(5, 20);
    press(&mut app, &jobs, KeyCode::PageDown);
    assert_eq!(app.lms.detail_scroll.offset(), 5, "详情层应滚动活动详情");

    // 其他頁面：不影響任何捲動狀態。
    app.nav = NavItem::Schedule;
    press(&mut app, &jobs, KeyCode::PageDown);
    assert_eq!(app.lms.detail_scroll.offset(), 5, "其他页面不得滚动");
}

// ── 任務頁（自訂義任務） ─────────────────────────────────

/// 按下帶 control 修飾鍵的字元。
fn press_ctrl(app: &mut App, jobs: &Sender<Job>, character: char) {
    handle_key(
        app,
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
        jobs,
    );
}

/// 任務頁測試用資料：一筆未完成任務、一筆已完成任務與一筆未完成作業。
///
/// 未完成分組的順序為「任务段在前、作业段在后」：索引 0 是任務，索引 1 是作業。
fn task_page_app() -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.task_page.tasks = vec![
        Task {
            id: 1,
            content: "写实验报告".to_owned(),
            description: Some("第三章".to_owned()),
            tag: None,
            deadline: None,
            priority: Priority::High,
            completed: false,
        },
        Task {
            id: 2,
            content: "复习".to_owned(),
            description: None,
            tag: None,
            deadline: None,
            priority: Priority::Low,
            completed: true,
        },
    ];
    app.homework = Page::Ready(homework_page("1", "a-1", 0));
    app
}

#[test]
fn control_a_adds_a_task_from_the_form() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    press_ctrl(&mut app, &jobs, 'a');
    assert!(
        matches!(app.screen, Screen::TaskForm(_)),
        "^a 应打开添加任务表单"
    );

    type_text(&mut app, &jobs, "买教材");
    tab_to(&mut app, &jobs, TaskField::Deadline);
    type_text(&mut app, &jobs, "2026-12-31 12:30");
    press_ctrl(&mut app, &jobs, 's');

    match rx.try_recv() {
        Ok(Job::AddTask { task }) => {
            assert_eq!(task.content, "买教材");
            assert_eq!(task.description, None, "描述留空时不写入");
            assert_eq!(task.tag, None, "标签留空时不写入");
            assert_eq!(
                task.deadline
                    .map(|deadline| deadline.format("%Y-%m-%d %H:%M").to_string()),
                Some("2026-12-31 12:30".to_owned()),
                "截止时间应按校园时区解析"
            );
            assert_eq!(task.priority, Priority::Low, "默认优先级为低");
            assert!(!task.completed);
        }
        other => panic!("应为新增任务任务，实际为 {other:?}"),
    }
    let Screen::TaskForm(form) = &app.screen else {
        panic!("提交后应停留在任务表单");
    };
    assert!(form.busy, "提交后应显示正在保存");
}

#[test]
fn task_form_reports_validation_errors_in_place() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'a');

    // 內容為空：就地報錯、不送出、不進入保存中。
    press_ctrl(&mut app, &jobs, 's');
    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert_eq!(form.error.as_deref(), Some("任务内容不能为空"));
    assert!(!form.busy, "验证失败不得进入保存中");
    assert!(rx.try_recv().is_err(), "验证失败不得送出任务");

    // 截止時間格式錯誤：同樣就地報錯，且已輸入的內容保留。
    type_text(&mut app, &jobs, "买教材");
    tab_to(&mut app, &jobs, TaskField::Deadline);
    type_text(&mut app, &jobs, "不是日期");
    press_ctrl(&mut app, &jobs, 's');
    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert!(
        form.error
            .as_deref()
            .is_some_and(|error| error.contains("截止时间")),
        "应提示截止时间格式：{:?}",
        form.error
    );
    assert_eq!(form.content.value(), "买教材", "失败应保留已输入内容");
    assert!(rx.try_recv().is_err(), "验证失败不得送出任务");

    // `^u` 清空聚焦欄位後，空截止時間代表「沒有截止時間」。
    press_ctrl(&mut app, &jobs, 'u');
    press_ctrl(&mut app, &jobs, 's');
    match rx.try_recv() {
        Ok(Job::AddTask { task }) => {
            assert_eq!(task.deadline, None, "空截止时间应视为没有截止时间");
        }
        other => panic!("应为新增任务任务，实际为 {other:?}"),
    }
    // 送出中的面板仍可用 `esc` 關閉：保存已在背景進行，不該把使用者鎖住。
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Main), "esc 应关闭面板");
    assert_eq!(app.message_text(), Some("面板已关闭，保存仍在进行"));
}

#[test]
fn task_form_description_accepts_multiple_lines() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'a');
    type_text(&mut app, &jobs, "复习");
    tab_to(&mut app, &jobs, TaskField::Description);

    type_text(&mut app, &jobs, "第一行");
    press(&mut app, &jobs, KeyCode::Enter);
    type_text(&mut app, &jobs, "第二行");
    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert_eq!(form.description.value(), "第一行\n第二行", "描述应允许多行");
    assert_eq!(form.description.line_count(), 2);

    // 優先級與完成狀態以左右鍵切換。
    tab_to(&mut app, &jobs, TaskField::Priority);
    press(&mut app, &jobs, KeyCode::Right);
    press(&mut app, &jobs, KeyCode::Right);
    tab_to(&mut app, &jobs, TaskField::Completed);
    press(&mut app, &jobs, KeyCode::Char(' '));
    press_ctrl(&mut app, &jobs, 's');

    match rx.try_recv() {
        Ok(Job::AddTask { task }) => {
            assert_eq!(task.description.as_deref(), Some("第一行\n第二行"));
            assert_eq!(
                task.priority,
                Priority::High,
                "按两次右方向键应由默认的低依次切到中、高"
            );
            assert!(task.completed, "空格应切换完成状态");
        }
        other => panic!("应为新增任务任务，实际为 {other:?}"),
    }
}

#[test]
fn space_toggles_the_selected_task_and_refuses_homework() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    press(&mut app, &jobs, KeyCode::Char(' '));
    assert!(
        matches!(rx.try_recv(), Ok(Job::SetTaskDone { id: 1, done: true })),
        "空格应标记选中的任务为已完成"
    );

    // 移到作業列：作業只能檢視，不能標記。
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Char(' '));
    assert!(rx.try_recv().is_err(), "作业不应送出标记任务");
    assert_eq!(
        app.message_text(),
        Some("只能标记自定义任务"),
        "应提示作业不可标记"
    );
}

#[test]
fn m_toggles_multi_select_and_space_checks_tasks() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    press(&mut app, &jobs, KeyCode::Char('m'));
    let Some(selection) = app.task_page.multi.as_ref() else {
        panic!("m 应进入多选模式");
    };
    assert!(selection.is_empty(), "进入多选时不应预先勾选");
    assert!(rx.try_recv().is_err(), "进入多选不应送出任务");

    press(&mut app, &jobs, KeyCode::Char(' '));
    assert!(
        app.task_page
            .multi
            .as_ref()
            .is_some_and(|set| set.contains(&1)),
        "空格应勾选当前任务"
    );
    assert!(rx.try_recv().is_err(), "多选模式下空格只勾选，不标记完成");

    // 再按一次取消勾選。
    press(&mut app, &jobs, KeyCode::Char(' '));
    assert!(
        app.task_page.multi.as_ref().is_some_and(HashSet::is_empty),
        "再次按空格应取消勾选"
    );

    press(&mut app, &jobs, KeyCode::Char('m'));
    assert!(app.task_page.multi.is_none(), "再次按 m 应退出多选");
    assert_eq!(app.message_text(), Some("已退出多选"));

    // `esc` 同樣能離開（提示列寫的就是「esc 退出多选」）。
    press(&mut app, &jobs, KeyCode::Char('m'));
    assert!(app.task_page.multi.is_some());
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(app.task_page.multi.is_none(), "esc 应退出多选");
    assert_eq!(app.message_text(), Some("已退出多选"));
}

#[test]
fn enter_in_multi_select_opens_the_batch_menu() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    // 未勾選任何任務：只提示，不開選單。
    press(&mut app, &jobs, KeyCode::Char('m'));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::Main), "未勾选时不开选单");
    assert_eq!(app.message_text(), Some("尚未选择任务（space 勾选）"));

    // 勾選後開啟批量操作選單，enter 送出批量任務。
    press(&mut app, &jobs, KeyCode::Char(' '));
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(
        matches!(app.screen, Screen::TaskBatchMenu(_)),
        "enter 应打开批量操作选单"
    );
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(
        matches!(
            rx.try_recv(),
            Ok(Job::SetTasksDone { ids, done: true }) if ids == vec![1]
        ),
        "默认选项应批量标记完成"
    );
    assert!(app.task_page.multi.is_none(), "送出后应退出多选");
    assert!(matches!(app.screen, Screen::Main), "送出后应回到主画面");
}

/// 切換分組後，多選只作用於目前可見的任務：勾選仍跨分組保留，但批量操作
/// 不得刪改畫面上看不到的項目。
#[test]
fn multi_select_only_acts_on_visible_tasks_after_switching_group() {
    use crate::domain::homework::HomeworkGroup;

    let (jobs, rx) = channel();
    let mut app = task_page_app();

    // 在「未完成」勾選任務 1（未完成分組索引 0）。
    press(&mut app, &jobs, KeyCode::Char('m'));
    press(&mut app, &jobs, KeyCode::Char(' '));
    assert!(
        app.task_page
            .multi
            .as_ref()
            .is_some_and(|set| set.contains(&1)),
        "空格应勾选任务 1"
    );

    // 切到「已完成」：勾選仍在（跨分組保留），但任務 1 已不可見。
    press(&mut app, &jobs, KeyCode::Char(']'));
    assert_eq!(app.homework_group, HomeworkGroup::Completed);
    assert!(
        app.task_page
            .multi
            .as_ref()
            .is_some_and(|set| set.contains(&1)),
        "勾选跨分组保留"
    );
    assert!(
        crate::tui::controller::selected_task_ids(&app).is_empty(),
        "已完成分组看不到未完成的任务，批量操作不得作用于它"
    );

    // enter 不開選單、不送出任何任務。
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::Main), "不可见时不开选单");
    assert_eq!(app.message_text(), Some("尚未选择任务（space 勾选）"));
    assert!(rx.try_recv().is_err(), "不得送出任何批量任务");
}

#[test]
fn control_t_menu_confirms_deleting_completed_tasks() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    press_ctrl(&mut app, &jobs, 't');
    assert!(
        matches!(app.screen, Screen::TaskMenu(_)),
        "^t 应打开任务设置选单"
    );
    // 第二項是「删除所有已完成的任务」。
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(
        matches!(
            app.screen,
            Screen::TaskConfirm(TaskConfirmState { count: 1 })
        ),
        "应进入二次确认并带出已完成数量"
    );
    press(&mut app, &jobs, KeyCode::Char('y'));
    assert!(
        matches!(rx.try_recv(), Ok(Job::DeleteCompletedTasks)),
        "y 应送出删除已完成任务"
    );
    assert!(matches!(app.screen, Screen::Main), "确认后应回到主画面");

    // 沒有已完成任務時不進二次確認。
    app.task_page.tasks.retain(|task| !task.completed);
    press_ctrl(&mut app, &jobs, 't');
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Enter);
    assert!(matches!(app.screen, Screen::Main));
    assert_eq!(app.message_text(), Some("没有已完成的任务"));

    // esc 關閉選單。
    press_ctrl(&mut app, &jobs, 't');
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Main));
}

#[test]
fn control_f_filters_by_keyword_and_escape_clears_it() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    press_ctrl(&mut app, &jobs, 'f');
    assert!(app.task_page.search.is_some(), "^f 应打开搜索输入框");
    type_text(&mut app, &jobs, "报告");
    press(&mut app, &jobs, KeyCode::Enter);
    assert_eq!(app.task_page.filter.as_deref(), Some("报告"));
    assert!(app.task_page.search.is_none(), "套用后应关闭输入框");
    assert!(
        app.message_text()
            .is_some_and(|message| message.contains("报告")),
        "应提示筛选结果：{:?}",
        app.message_text()
    );
    assert_eq!(app.task_filter_matches(), 1, "只剩内容含“报告”的任务");
    assert!(
        !app.task_page.tasks.is_empty(),
        "筛选只影响显示，不删除数据"
    );
    assert!(rx.try_recv().is_err(), "筛选不触发任何任务");

    // 主畫面的 esc 清除篩選。
    press(&mut app, &jobs, KeyCode::Esc);
    assert_eq!(app.task_page.filter, None);
    assert_eq!(app.message_text(), Some("已清除筛选"));

    // 再開搜尋但直接 enter：視為清除篩選。
    press_ctrl(&mut app, &jobs, 'f');
    press(&mut app, &jobs, KeyCode::Enter);
    assert_eq!(app.task_page.filter, None);
    assert_eq!(app.message_text(), Some("已清除筛选"));

    // 搜尋中按 esc：取消並保留原篩選。
    app.task_page.filter = Some("报告".to_owned());
    press_ctrl(&mut app, &jobs, 'f');
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(app.task_page.search.is_none());
    assert_eq!(
        app.task_page.filter.as_deref(),
        Some("报告"),
        "取消不应清除筛选"
    );
    assert_eq!(app.message_text(), Some("已取消编辑（保留现有筛选）"));
}

#[test]
fn escape_in_the_search_box_says_nothing_was_filtered_when_none_was() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();

    press_ctrl(&mut app, &jobs, 'f');
    press(&mut app, &jobs, KeyCode::Esc);
    assert!(app.task_page.search.is_none());
    assert_eq!(app.task_page.filter, None);
    assert_eq!(app.message_text(), Some("已取消编辑"));
}

#[test]
fn control_e_and_control_d_apply_to_tasks_only() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();

    press_ctrl(&mut app, &jobs, 'e');
    let Screen::TaskForm(form) = &app.screen else {
        panic!("^e 应打开编辑表单");
    };
    assert_eq!(form.mode, TaskFormMode::Edit { id: 1 });
    assert_eq!(form.content.value(), "写实验报告", "应预填既有内容");
    assert_eq!(form.description.value(), "第三章");
    assert_eq!(form.priority, Priority::High);
    press(&mut app, &jobs, KeyCode::Esc);

    // 作業列不可編輯、不可刪除。
    press(&mut app, &jobs, KeyCode::Down);
    press_ctrl(&mut app, &jobs, 'e');
    assert!(matches!(app.screen, Screen::Main));
    assert_eq!(app.message_text(), Some("只能修改自定义任务"));
    press_ctrl(&mut app, &jobs, 'd');
    assert!(rx.try_recv().is_err(), "作业不得送出删除任务");
    assert_eq!(app.message_text(), Some("只能删除自定义任务"));

    // 任務需要按兩次 ^d；中間按下其他鍵會取消。
    press(&mut app, &jobs, KeyCode::Up);
    press_ctrl(&mut app, &jobs, 'd');
    assert!(rx.try_recv().is_err(), "第一次 ^d 只提示，不删除");
    assert_eq!(
        app.task_page.pending_delete.as_ref().map(|(id, _)| *id),
        Some(1)
    );
    assert!(
        app.message_text()
            .is_some_and(|message| message.contains("再按一次"))
    );
    press(&mut app, &jobs, KeyCode::Char('x'));
    assert!(
        app.task_page.pending_delete.is_none(),
        "其他按键应取消待确认"
    );

    press_ctrl(&mut app, &jobs, 'd');
    press_ctrl(&mut app, &jobs, 'd');
    assert!(
        matches!(rx.try_recv(), Ok(Job::DeleteTask { id: 1 })),
        "第二次 ^d 才删除"
    );
    assert!(app.task_page.pending_delete.is_none());
}

#[test]
fn task_keys_do_nothing_on_other_pages() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    app.nav = NavItem::Schedule;

    for character in ['a', 'e', 'd', 'f', 't'] {
        press_ctrl(&mut app, &jobs, character);
    }
    press(&mut app, &jobs, KeyCode::Char(' '));
    press(&mut app, &jobs, KeyCode::Char('m'));

    assert!(matches!(app.screen, Screen::Main), "其他页面不得开弹窗");
    assert!(app.task_page.multi.is_none(), "其他页面不得进入多选");
    assert!(app.task_page.search.is_none(), "其他页面不得开搜索");
    assert!(rx.try_recv().is_err(), "其他页面不得送出任务任务");
}

// ── 任務頁排序（^L） ─────────────────────────────────────

/// 目前選取項目的標籤（任務內容或作業標題）。
fn selected_label(app: &App) -> String {
    match app.selected_entry() {
        Some(TaskEntry::Task(task)) => task.content.clone(),
        Some(TaskEntry::Homework(item)) => item.title.clone(),
        None => "<none>".to_owned(),
    }
}

#[test]
fn sort_keys_switch_the_mode_and_return_to_the_main_screen() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();

    for (character, mode, label) in [
        ('p', SortMode::Priority, "优先级"),
        ('d', SortMode::Deadline, "截止时间"),
        ('n', SortMode::Default, "默认"),
    ] {
        press_ctrl(&mut app, &jobs, 'l');
        assert!(matches!(app.screen, Screen::Sort), "^L 应进入排序提示");
        press(&mut app, &jobs, KeyCode::Char(character));
        assert!(matches!(app.screen, Screen::Main), "选定后应回到主画面");
        assert_eq!(app.task_page.sort, mode, "{character} 应套用 {label}");
        assert!(
            app.message_text().is_some_and(|text| text.contains(label)),
            "提示应说明目前的排序方式：{:?}",
            app.message_text()
        );
    }

    // 大寫與其他按鍵的相容性：`P` 等同 `p`。
    press_ctrl(&mut app, &jobs, 'l');
    press(&mut app, &jobs, KeyCode::Char('P'));
    assert_eq!(app.task_page.sort, SortMode::Priority);
}

#[test]
fn sort_prompt_ignores_other_keys_and_esc_cancels() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    app.task_page.sort = SortMode::Priority;

    press_ctrl(&mut app, &jobs, 'l');
    for code in [KeyCode::Char('x'), KeyCode::Enter, KeyCode::Down] {
        press(&mut app, &jobs, code);
        assert!(
            matches!(app.screen, Screen::Sort),
            "{code:?} 不应离开排序提示"
        );
    }
    assert_eq!(
        app.task_page.sort,
        SortMode::Priority,
        "未选定前不得改变排序"
    );

    press(&mut app, &jobs, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Main), "esc 应取消并回到主画面");
    assert_eq!(
        app.task_page.sort,
        SortMode::Priority,
        "取消不得改变原本的排序方式"
    );
    assert!(rx.try_recv().is_err(), "排序是纯界面操作，不送任务");
}

#[test]
fn sort_key_only_works_on_the_task_page() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    app.nav = NavItem::Schedule;

    press_ctrl(&mut app, &jobs, 'l');
    assert!(
        matches!(app.screen, Screen::Main),
        "其他页面不得进入排序提示"
    );
    assert_eq!(app.task_page.sort, SortMode::Default);
    assert!(rx.try_recv().is_err(), "其他页面不得送出任务");
}

#[test]
fn switching_sort_keeps_the_selection_on_the_same_item() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    // 預設排序：未完成分組為「任务 → 作业」，選取作業（索引 1）。
    app.set_selection(1);
    assert_eq!(selected_label(&app), "第一次作业");

    // 優先級排序：作業（高、有截止時間）排到任務（高、無截止）之前。
    press_ctrl(&mut app, &jobs, 'l');
    press(&mut app, &jobs, KeyCode::Char('p'));
    assert_eq!(app.task_page.sort, SortMode::Priority);
    assert_eq!(app.page_selection(), 0, "作业应排到第一列");
    assert_eq!(
        selected_label(&app),
        "第一次作业",
        "切换排序后游标应留在同一个项目"
    );
}

// ── 按鍵立刻換掉提示列 ───────────────────────────────────

#[test]
fn key_press_replaces_the_previous_message_with_the_current_hints() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    // 模擬背景通知還在顯示（會停留數秒）時使用者按下 `^L`。
    app.set_message("作业已更新（用时 3.2s）");

    press_ctrl(&mut app, &jobs, 'l');
    assert!(matches!(app.screen, Screen::Sort), "^L 应进入排序提示");
    assert!(
        app.message_text().is_none(),
        "按键后应立刻清掉上一则通知，让底部改显示排序按键：{:?}",
        app.message_text()
    );

    // 動作本身設定的訊息仍要顯示（清除只發生在處理這次按鍵之前）。
    press(&mut app, &jobs, KeyCode::Char('p'));
    assert_eq!(app.task_page.sort, SortMode::Priority);
    assert!(
        app.message_text()
            .is_some_and(|text| text.contains("已按优先级排序")),
        "排序结果仍应提示：{:?}",
        app.message_text()
    );
}

#[test]
fn any_key_clears_the_message_even_when_the_action_does_nothing() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    app.set_message("已删除 2 个已完成任务");

    // 未綁定的按鍵沒有動作，但仍代表使用者操作過一次。
    press(&mut app, &jobs, KeyCode::Char('x'));
    assert!(app.message_text().is_none(), "未绑定的按键也应清掉旧通知");
}

// ── 任務表單的優先級方向 ─────────────────────────────────

#[test]
fn task_form_priority_cycles_low_to_high_with_right_arrow() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'a');
    tab_to(&mut app, &jobs, TaskField::Priority);
    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert_eq!(form.focus, TaskField::Priority, "焦点应落在优先级字段");
    assert_eq!(form.priority, Priority::Low, "默认优先级为低");

    // `→`：低 → 中 → 高 → 低。
    press(&mut app, &jobs, KeyCode::Right);
    assert_eq!(task_form_priority(&app), Priority::Medium);
    press(&mut app, &jobs, KeyCode::Right);
    assert_eq!(task_form_priority(&app), Priority::High);
    press(&mut app, &jobs, KeyCode::Right);
    assert_eq!(task_form_priority(&app), Priority::Low, "应在高之后回到低");

    // `←` 反向：低 → 高。
    press(&mut app, &jobs, KeyCode::Left);
    assert_eq!(task_form_priority(&app), Priority::High);
}

/// 目前任務表單的優先級。
fn task_form_priority(app: &App) -> Priority {
    match &app.screen {
        Screen::TaskForm(form) => form.priority,
        other => panic!("应在任务表单，实际为 {other:?}"),
    }
}

/// 以 `Tab` 移動焦點到指定欄位。
///
/// 不硬編 Tab 次數：欄位順序改動（例如新增欄位）時測試不必跟著改，同時仍
/// 驗證「該欄位可以由 Tab 抵達」——走完一輪（`ALL.len()` 次）仍未抵達即失敗。
fn tab_to(app: &mut App, jobs: &Sender<Job>, field: TaskField) {
    for _ in 0..TaskField::ALL.len() {
        if matches!(&app.screen, Screen::TaskForm(form) if form.focus == field) {
            return;
        }
        press(app, jobs, KeyCode::Tab);
    }
    panic!("Tab 无法聚焦到 {field:?} 字段");
}

#[test]
fn task_form_fields_cycle_in_screen_order() {
    // 畫面順序＝Tab 順序：`next()` 走一輪應回到起點，且順序與 `ALL` 一致。
    let mut field = TaskField::Content;
    let mut order = vec![field];
    for _ in 1..TaskField::ALL.len() {
        field = field.next();
        order.push(field);
    }
    assert_eq!(order, TaskField::ALL.to_vec(), "Tab 顺序即画面字段顺序");
    assert_eq!(field.next(), TaskField::Content, "Tab 应循环回第一个字段");
    assert_eq!(
        TaskField::Content.previous(),
        TaskField::Completed,
        "Shift+Tab 应反向循环"
    );
}

// ── 任務標籤 ───────────────────────────────────────────

/// 搜尋輸入框目前的內容。
fn search_value(app: &App) -> Option<String> {
    app.task_page
        .search
        .as_ref()
        .map(|input| input.value().to_owned())
}

#[test]
fn task_form_tag_field_blocks_typing_beyond_the_limit() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'a');
    type_text(&mut app, &jobs, "写报告");
    tab_to(&mut app, &jobs, TaskField::Tag);
    type_text(&mut app, &jobs, "六个汉字宽度啊");

    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert_eq!(form.tag.value(), "六个汉字宽度", "第七个汉字打不进去");

    press_ctrl(&mut app, &jobs, 's');
    match rx.try_recv() {
        Ok(Job::AddTask { task }) => {
            assert_eq!(task.content, "写报告");
            assert_eq!(task.tag.as_deref(), Some("六个汉字宽度"));
        }
        other => panic!("应为新增任务任务，实际为 {other:?}"),
    }
}

#[test]
fn task_form_tag_field_truncates_pasted_text() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'a');
    tab_to(&mut app, &jobs, TaskField::Tag);
    handle_paste(&mut app, "一二三四五六七八九十");

    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert_eq!(form.tag.value(), "一二三四五六", "贴上的文字应截断到上限");
}

#[test]
fn edit_prefills_the_tag_and_ctrl_u_clears_it() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    app.task_page.tasks[0].tag = Some("实验".to_owned());

    press_ctrl(&mut app, &jobs, 'e');
    let Screen::TaskForm(form) = &app.screen else {
        panic!("^e 应打开编辑表单");
    };
    assert_eq!(form.tag.value(), "实验", "编辑表单应预填标签");

    tab_to(&mut app, &jobs, TaskField::Tag);
    press_ctrl(&mut app, &jobs, 'u');
    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert!(form.tag.is_empty(), "^u 应清空聚焦的标签字段");
}

#[test]
fn task_form_tag_length_is_validated_on_save() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'a');
    type_text(&mut app, &jobs, "写报告");
    // 直接塞進超長標籤（繞過輸入欄的攔截）以驗證保存前的防禦檢查。
    if let Screen::TaskForm(form) = &mut app.screen {
        form.tag.set("七个汉字宽度啊");
    }
    press_ctrl(&mut app, &jobs, 's');

    let Screen::TaskForm(form) = &app.screen else {
        panic!("应停留在任务表单");
    };
    assert!(
        form.error
            .as_deref()
            .is_some_and(|error| error.contains("标签")),
        "应提示标签长度：{:?}",
        form.error
    );
    assert!(!form.busy, "验证失败不得进入保存中");
    assert!(rx.try_recv().is_err(), "验证失败不得送出任务");
}

#[test]
fn search_up_down_cycles_existing_tags() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    app.task_page.tasks[0].tag = Some("实验".to_owned());
    app.task_page.tasks[1].tag = Some("复习".to_owned());

    press_ctrl(&mut app, &jobs, 'f');
    // `↓` 第一次由第一個開始，之後首尾循環。
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(search_value(&app).as_deref(), Some("实验"));
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(search_value(&app).as_deref(), Some("复习"));
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(search_value(&app).as_deref(), Some("实验"), "到尾端应回卷");

    // 重新開啟後 `↑` 第一次由最後一個開始。
    press(&mut app, &jobs, KeyCode::Esc);
    assert_eq!(app.task_page.tag_cursor, None, "关闭搜索框应清除游标");
    press_ctrl(&mut app, &jobs, 'f');
    press(&mut app, &jobs, KeyCode::Up);
    assert_eq!(search_value(&app).as_deref(), Some("复习"));
    press(&mut app, &jobs, KeyCode::Up);
    assert_eq!(search_value(&app).as_deref(), Some("实验"));
    press(&mut app, &jobs, KeyCode::Up);
    assert_eq!(search_value(&app).as_deref(), Some("复习"), "到首端应回卷");

    // `enter` 以預填的標籤套用篩選。
    press(&mut app, &jobs, KeyCode::Enter);
    assert_eq!(app.task_page.filter.as_deref(), Some("复习"));
    assert_eq!(app.task_page.tag_cursor, None);
    assert!(app.task_page.search.is_none(), "套用后应关闭输入框");
}

#[test]
fn typing_in_the_search_box_restarts_tag_suggestions() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    app.task_page.tasks[0].tag = Some("实验".to_owned());
    app.task_page.tasks[1].tag = Some("复习".to_owned());

    press_ctrl(&mut app, &jobs, 'f');
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(search_value(&app).as_deref(), Some("复习"));

    // 手動編輯之後由第一個重新起算。
    type_text(&mut app, &jobs, "报");
    assert_eq!(app.task_page.tag_cursor, None);
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(search_value(&app).as_deref(), Some("实验"));
}

#[test]
fn tag_suggestions_do_nothing_without_any_tag() {
    let (jobs, _rx) = channel();
    let mut app = task_page_app();
    press_ctrl(&mut app, &jobs, 'f');
    type_text(&mut app, &jobs, "报告");

    press(&mut app, &jobs, KeyCode::Up);
    press(&mut app, &jobs, KeyCode::Down);
    assert_eq!(
        search_value(&app).as_deref(),
        Some("报告"),
        "一个标签也没有时上下键不应改变输入"
    );
    assert_eq!(app.task_page.tag_cursor, None);
    assert_eq!(app.message_text(), None, "不应出现任何提示");
}

/// 主畫面的單鍵操作不接受 Ctrl／Alt 修飾鍵。
///
/// 這些按鍵若一併吃組合鍵會造成意外副作用：`Ctrl+O` 會開啟瀏覽器、`Ctrl+R`
/// 會強制重新查詢、`Ctrl+S` 會開啟學期選擇器、`Ctrl+Q` 會直接結束程式。
/// 修復前（選取停在作業上）`Ctrl+O` 與 `Ctrl+R` 都會真的送出任務。
#[test]
fn control_modified_keys_do_not_trigger_main_screen_shortcuts() {
    let (jobs, rx) = channel();
    let mut app = task_page_app();
    app.term_options = vec![TermCode::parse("2026-2027-1").expect("学期")];
    // 選取移到作業（索引 1）：修復前 `Ctrl+O` 會為它送出 `OpenActivity`。
    app.select_next();

    press_ctrl(&mut app, &jobs, 'o');
    press_ctrl(&mut app, &jobs, 'r');
    press_ctrl(&mut app, &jobs, 's');
    press_ctrl(&mut app, &jobs, 'h');
    press_ctrl(&mut app, &jobs, 'q');

    assert!(rx.try_recv().is_err(), "组合键不应送出任何任务");
    assert!(!app.quit, "Ctrl+Q 不应退出");
    assert!(
        matches!(app.screen, Screen::Main),
        "Ctrl+S 不应打开学期选择器"
    );
    assert_eq!(app.nav, NavItem::Homework, "Ctrl+H 不应切换页面");
}

/// 設定選單與學期選擇器同樣不接受 Ctrl／Alt 修飾鍵。
///
/// 兩者的 `h`／`l`／`j`／`k` 都只綁定單鍵；帶修飾鍵時不該改動訪問模式草稿
/// （修復前 `Ctrl+L` 會真的把草稿切到下一個模式）或移動學期選取。
#[test]
fn control_modified_keys_do_not_adjust_settings_or_the_term_picker() {
    let (jobs, _rx) = channel();

    // 設定選單：移到「訪問模式」後 `Ctrl+L` 不應改動草稿。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Settings(SettingsState::open(AccessPolicy::Auto)));
    press(&mut app, &jobs, KeyCode::Down);
    press(&mut app, &jobs, KeyCode::Down);
    press_ctrl(&mut app, &jobs, 'l');
    let Screen::Settings(state) = app.screen else {
        panic!("应仍在设定选单");
    };
    assert!(
        !state.policy_dirty(app.access_policy),
        "Ctrl+L 不应改动访问模式草稿"
    );

    // 學期選擇器：`Ctrl+J` 不應移動選取。
    let first = TermCode::parse("2026-2027-1").expect("学期");
    let second = TermCode::parse("2025-2026-2").expect("学期");
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::TermPicker(TermPickerState::new(
        vec![first, second],
        None,
        "选择要查看的学期".to_owned(),
    )));
    press_ctrl(&mut app, &jobs, 'j');
    let Screen::TermPicker(state) = app.screen else {
        panic!("应仍在学期选择器");
    };
    assert_eq!(state.selected(), Some(first), "Ctrl+J 不应移动学期选择");
}

/// `r` 在思源學堂依目前層級刷新，不把人彈回課程清單。
///
/// 修復前 `request` 無條件 `level = Courses`：在活動層或詳情層按 `r` 會被丟回
/// 課程清單，`apply_courses` 也因為層級已變成 `Courses` 而走了「目前課程不存在」
/// 的分支，連選取的課程都重設為第一門。
#[test]
fn refresh_on_the_lms_page_keeps_the_current_level() {
    let (jobs, rx) = channel();

    // 課程層：重新載入課程清單。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadCourses { force: true })
    ));

    // 活動層：重載活動，層級與目前課程都不變。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities_course = Some("1".to_owned());
    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadActivities { course_id, force: true }) if course_id == "1"
    ));
    assert_eq!(
        app.lms.level,
        LmsLevel::Activities,
        "活动层刷新应留在活动层"
    );
    assert_eq!(app.lms.activities_course.as_deref(), Some("1"));

    // 詳情層：重載詳情，層級不變。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail_activity = Some("2".to_owned());
    press(&mut app, &jobs, KeyCode::Char('r'));
    assert!(matches!(
        rx.try_recv(),
        Ok(Job::LoadActivityDetail { activity_id, force: true }) if activity_id == "2"
    ));
    assert_eq!(app.lms.level, LmsLevel::Detail, "详情层刷新应留在详情层");
}
