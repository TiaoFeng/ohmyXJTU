//! TUI 啟動、主迴圈與事件套用。

pub mod app;
pub mod handler;
pub mod text;
pub mod theme;
pub mod ui;
pub mod views;

use std::io::Stdout;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::credentials::Vault;
use crate::domain::homework::HomeworkGroup;
use crate::error::{AppError, AppResult};
use crate::task::{self, Event, FailedTarget, Job};

use app::{
    AgreementState, App, FormKind, FormState, HomeworkData, LoginScreen, Page, Screen,
    SettingsState, TermPickerState,
};
use text::InputLine;

/// 事件輪詢間隔。
const TICK: Duration = Duration::from_millis(200);

/// 啟動 TUI。
pub fn run() -> AppResult<()> {
    let config = Config::load_or_create()?;
    let vault = Vault::at_default_path()?;
    let vault_exists = vault.exists();

    let (jobs, events) = task::spawn(config.clone(), vault)?;

    let mut app = App::new(config.access_policy);
    if !vault_exists {
        app.set_screen(Screen::Setup(FormState::setup()));
    }
    // 尚未同意本版用户协议：先顯示閱讀門。底層畫面（首次設定或解鎖）已就緒，
    // 同意後直接揭露。
    if !config.privacy_accepted(crate::privacy::VERSION) {
        app.agreement = Some(Box::new(AgreementState::new()));
    }

    // 刻意不用 `?` 提前返回：任何情況下都要還原終端狀態。
    // `try_init` 本身失敗時（例如無法查詢終端尺寸）可能已開啟原始模式，
    // 也要先盡力還原再回報錯誤。
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = ratatui::try_restore();
            return Err(AppError::Tui(err.to_string()));
        }
    };
    let loop_result = main_loop(&mut terminal, &mut app, &events, &jobs);
    let restore_result = ratatui::try_restore().map_err(|err| AppError::Tui(err.to_string()));

    let _ = jobs.send(Job::Shutdown);

    loop_result.and(restore_result)
}

/// 安裝 panic hook：還原終端、輸出單行錯誤訊息後結束行程。
///
/// 訊息不含 panic 內容（避免任何潛在敏感資料外洩），只保留發生位置；
/// 任何執行緒 panic 都會終止行程，避免背景執行緒死亡後介面卡死。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // 盡力還原終端：panic 可能發生在進入原始模式或替代畫面之後。
        let _ = ratatui::try_restore();
        let location = info
            .location()
            .map(|location| (location.file(), location.line()));
        eprintln!("{}", panic_hook_message(location));
        std::process::exit(101);
    }));
}

/// panic 時的單行錯誤訊息（不含 panic payload）。
fn panic_hook_message(location: Option<(&str, u32)>) -> String {
    match location {
        Some((file, line)) => format!("错误：程序发生内部错误（{file}:{line}），已退出。"),
        None => "错误：程序发生内部错误，已退出。".to_owned(),
    }
}

fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    events: &Receiver<Event>,
    jobs: &Sender<Job>,
) -> AppResult<()> {
    loop {
        app.expire_message();
        app.advance_tick();
        terminal
            .draw(|frame| views::draw(frame, app))
            .map_err(|err| AppError::Tui(err.to_string()))?;

        if event::poll(TICK).map_err(|err| AppError::Tui(err.to_string()))?
            && let CrosstermEvent::Key(key) =
                event::read().map_err(|err| AppError::Tui(err.to_string()))?
            && key.kind == KeyEventKind::Press
        {
            handler::handle_key(app, key, jobs);
        }

        while let Ok(event) = events.try_recv() {
            apply_event(app, event, jobs);
        }

        // 事件要求的瀏覽器開啟：在這裡執行，錯誤以狀態訊息回報。
        if let Some(url) = app.pending_open.take() {
            match crate::system::browser::open_url(&url) {
                Ok(()) => app.set_message("已在浏览器打开：如需登录请在浏览器完成登录"),
                Err(err) => app.set_message(format!("打开网页失败：{err}")),
            }
        }

        if app.quit {
            return Ok(());
        }
    }
}

/// 套用背景事件。
fn apply_event(app: &mut App, event: Event, jobs: &Sender<Job>) {
    match event {
        Event::VaultReady => {
            // 憑證已就緒：清掉殘留的登入覆蓋層，直接進入主畫面
            //（不預先登入任何站點），由目前頁面按需觸發惰性登入。
            app.login = None;
            if app.is_main() {
                // 修改帳號後回到主畫面：舊資料屬於舊帳號，強制刷新目前頁面。
                let nav = app.nav;
                handler::request(app, jobs, nav, true);
            } else {
                app.ensure_main();
                handler::ensure_page(app, jobs);
            }
            app.set_message("凭证已就绪");
        }
        Event::LoginProgress(note) => {
            // 登入互動是覆蓋層：底層畫面保持不變，事件只更新彈窗內容。
            app.login = Some(Box::new(LoginScreen::Progress { note }));
        }
        Event::LoginNeedsCaptcha(path) => {
            app.captcha_path = Some(path.clone());
            let previous_error = match app.login.as_deref() {
                Some(LoginScreen::Captcha { error, .. }) => error.clone(),
                _ => None,
            };
            app.login = Some(Box::new(LoginScreen::Captcha {
                path,
                input: InputLine::new(),
                error: previous_error,
            }));
        }
        Event::LoginNeedsMfa { phone, sent } => {
            let (input, error) = match app.login.as_deref() {
                Some(LoginScreen::Mfa { input, error, .. }) => (input.clone(), error.clone()),
                _ => (InputLine::new(), None),
            };
            app.login = Some(Box::new(LoginScreen::Mfa {
                phone,
                sent,
                input,
                error,
            }));
        }
        Event::LoginFailed(message) => {
            set_login_error(app, message);
        }
        Event::LoginSucceeded { site, mode } => {
            app.login = None;
            match mode {
                Some(mode) => app.set_site_mode(site, mode),
                None => app.clear_site_mode(site),
            }
            if !app.is_main() {
                app.set_screen(Screen::Main);
            }
            // 若目前頁面尚未載入（例如從失敗畫面重試成功），補一次載入。
            handler::ensure_page(app, jobs);
            app.set_message("登录成功");
        }
        Event::SessionsCleared { account_changed } => {
            app.clear_site_modes();
            app.invalidate_data(account_changed);
        }
        Event::AgreementAccepted => {
            // 協議已同意：關閉閱讀門，揭露底層畫面（首次設定或解鎖）。
            app.agreement = None;
        }
        Event::SessionExpired { site } => {
            app.clear_site_mode(site);
        }
        Event::LoadingCancelled { target } => {
            // 進行中的載入被取消：解除載入中狀態，保留已取得的部分資料。
            app.cancel_loading(target);
        }
        Event::Schedule(data) => {
            app.schedule = Page::Ready(*data);
            app.updated_at.schedule = Some(now_clock());
            app.schedule_state.select(Some(0));
            app.ensure_main();
        }
        Event::Homework(update) => {
            let progress = update.progress;
            let finished = progress.is_none();
            let elapsed = update.elapsed;
            let unfinished = update
                .items
                .iter()
                .filter(|item| item.state.group() == HomeworkGroup::Unfinished)
                .count();
            let data = HomeworkData {
                term_label: update.term_label,
                term_source: update.term_source.map(|source| source.label()),
                courses_included: update.courses_included,
                courses_skipped: update.courses_skipped,
                term_options: update.term_options,
                items: update.items,
                issues: update.issues,
                courses_failed: update.courses_failed,
                progress,
            };
            app.term_options = data.term_options.clone();
            app.updated_at.homework = Some(now_clock());
            let len = data.group_count(app.homework_group);
            app.homework = match progress {
                // 部分結果：頁面維持載入中（資料持續可顯示），終態才轉為就緒。
                Some((done, total)) => Page::Loading {
                    note: format!(
                        "正在汇总作业（已完成 {done}/{total} 门课程，累计 {} 项）…",
                        data.items.len()
                    ),
                    stale: Some(data),
                },
                None => Page::Ready(data),
            };
            // 夾取選取索引，避免分組內容變動後越界。
            let selected = app
                .homework_state
                .selected()
                .unwrap_or(0)
                .min(len.saturating_sub(1));
            app.homework_state.select(Some(selected));
            if finished {
                app.set_message(format!(
                    "作业已更新：未完成 {unfinished} 项（用时 {:.1}s）",
                    elapsed.as_secs_f32()
                ));
            }
            app.ensure_main();
        }
        Event::HomeworkNeedsTerm {
            options,
            suggestion,
            reason,
        } => {
            app.term_options = options.clone();
            app.homework.fail("未确定本学期：按 s 选择要查看的学期");
            app.set_screen(Screen::TermPicker(TermPickerState::new(
                options, suggestion, reason,
            )));
        }
        Event::Flow(data) => {
            app.attendance = Page::Ready(*data);
            app.updated_at.attendance = Some(now_clock());
            app.flow_state.select(Some(0));
            app.ensure_main();
        }
        Event::Courses(data) => {
            let count = data.courses.len();
            app.lms.courses_term = data.current_term;
            app.lms.courses = Page::Ready(data.courses);
            app.updated_at.lms = Some(now_clock());
            app.course_state.select(Some(0));
            app.lms.level = app::LmsLevel::Courses;
            app.set_message(format!("共 {count} 门课程"));
            app.ensure_main();
        }
        Event::Activities(activities) => {
            app.lms.activities = Page::Ready(activities);
            app.updated_at.lms = Some(now_clock());
            // 目前分組為空時，改顯示第一個有內容的分組（依顯示順序）。
            let current = app.lms.activity_group;
            let counts = app.activity_group_counts();
            let current_empty = counts
                .iter()
                .find(|(group, _)| *group == current)
                .is_none_or(|(_, count)| *count == 0);
            if current_empty && let Some((group, _)) = counts.iter().find(|(_, count)| *count > 0) {
                app.lms.activity_group = *group;
            }
            app.activity_state.select(Some(0));
            app.ensure_main();
        }
        Event::ActivityDetail(detail) => {
            app.lms.detail = Page::Ready(*detail);
            app.updated_at.lms = Some(now_clock());
            app.ensure_main();
        }
        Event::OpenUrl(url) => {
            // 實際啟動瀏覽器交由主迴圈執行（測試不觸發外部程序）。
            app.pending_open = Some(url);
            app.set_message("正在打开浏览器…");
        }
        Event::AccountUpdated => {
            // 修改帳號成功：離開表單；資料由隨後的事件重新載入。
            if matches!(app.screen, Screen::SettingsForm(_)) {
                app.set_screen(Screen::Main);
            }
            app.set_message("账号已更新");
        }
        Event::PassphraseUpdated => {
            // 修改口令成功：離開處理中狀態並回到設定選單。
            if matches!(
                &app.screen,
                Screen::SettingsForm(form) if form.kind == FormKind::ChangePassphrase
            ) {
                app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));
            }
            app.set_message("加密口令已更新");
        }
        Event::AccessPolicyUpdated(policy) => {
            app.access_policy = policy;
            // 設定彈窗若開著：更新草稿並解除「保存中」，彈窗不關閉。
            if let Screen::Settings(state) = &mut app.screen {
                state.draft = Some(policy);
                state.saving = false;
            }
            app.set_message(format!(
                "访问模式已切换为 {}；按 r 刷新当前页面",
                policy.label()
            ));
        }
        Event::Notice(message) => {
            app.set_message(message);
        }
        Event::Failed {
            what,
            message,
            target,
        } => {
            // 錯誤只標記受影響的頁面；其他頁面保持原狀。
            app.fail_target(target, &message);
            let text = format!("{what}失败：{message}");
            match target {
                FailedTarget::Login => set_login_error(app, text.clone()),
                FailedTarget::Settings => {
                    // 設定保存失敗：保留彈窗與草稿，僅解除「保存中」；
                    // 設定表單失敗則就地顯示錯誤並恢復輸入。
                    match &mut app.screen {
                        Screen::Settings(state) => state.saving = false,
                        Screen::SettingsForm(form) => {
                            form.busy = false;
                            form.error = Some(text.clone());
                        }
                        _ => {}
                    }
                }
                FailedTarget::Credentials => {
                    // 憑證操作失敗：留在可編輯表單就地顯示錯誤；敏感欄位清空、帳號保留。
                    match &mut app.screen {
                        Screen::Setup(form) | Screen::Unlock(form) | Screen::SettingsForm(form) => {
                            form.busy = false;
                            form.error = Some(text.clone());
                            form.clear_secrets();
                        }
                        _ => {}
                    }
                }
                FailedTarget::Agreement => {
                    // 協議同意保存失敗：留在閱讀畫面就地顯示錯誤，可重試。
                    if let Some(state) = app.agreement.as_mut() {
                        state.fail(message.clone());
                    }
                }
                _ => {
                    // 資料任務失敗：只標記對應頁面，不影響登入覆蓋層或根畫面。
                }
            }
            app.set_message(text);
        }
    }
}

/// 目前時鐘（顯示更新時間用）。
fn now_clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 顯示登入錯誤：憑證表單就地顯示，其餘登入畫面回到失敗畫面。
fn set_login_error(app: &mut App, message: String) {
    match app.login.as_deref_mut() {
        Some(LoginScreen::Credentials { form, .. }) => {
            form.busy = false;
            form.error = Some(message);
        }
        _ => app.login = Some(Box::new(LoginScreen::Failed { message })),
    }
}

#[cfg(test)]
#[path = "tests/event_test.rs"]
mod event_test;

#[cfg(test)]
#[path = "tests/panic_hook_test.rs"]
mod panic_hook_test;
