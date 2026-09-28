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

use app::{App, FormState, HomeworkData, LoginScreen, Page, Screen, TermPickerState};
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

    // 刻意不用 `?` 提前返回：任何情況下都要還原終端狀態。
    let mut terminal = ratatui::try_init().map_err(|err| AppError::Tui(err.to_string()))?;
    let loop_result = main_loop(&mut terminal, &mut app, &events, &jobs);
    let restore_result = ratatui::try_restore().map_err(|err| AppError::Tui(err.to_string()));

    let _ = jobs.send(Job::Shutdown);

    loop_result.and(restore_result)
}

fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    events: &Receiver<Event>,
    jobs: &Sender<Job>,
) -> AppResult<()> {
    loop {
        app.expire_message();
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

        if app.quit {
            return Ok(());
        }
    }
}

/// 套用背景事件。
fn apply_event(app: &mut App, event: Event, jobs: &Sender<Job>) {
    match event {
        Event::VaultReady => {
            // 憑證已就緒：直接進入主畫面（不預先登入任何站點），
            // 由目前頁面按需觸發惰性登入。
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
            app.set_screen(Screen::Login(Box::new(LoginScreen::Progress { note })));
        }
        Event::LoginNeedsCaptcha(path) => {
            app.captcha_path = Some(path.clone());
            let previous_error = match &app.screen {
                Screen::Login(screen) => match screen.as_ref() {
                    LoginScreen::Captcha { error, .. } => error.clone(),
                    _ => None,
                },
                _ => None,
            };
            app.set_screen(Screen::Login(Box::new(LoginScreen::Captcha {
                path,
                input: InputLine::new(),
                error: previous_error,
            })));
        }
        Event::LoginNeedsMfa { phone, sent } => {
            let (input, error) = match &app.screen {
                Screen::Login(screen) => match screen.as_ref() {
                    LoginScreen::Mfa { input, error, .. } => (input.clone(), error.clone()),
                    _ => (InputLine::new(), None),
                },
                _ => (InputLine::new(), None),
            };
            app.set_screen(Screen::Login(Box::new(LoginScreen::Mfa {
                phone,
                sent,
                input,
                error,
            })));
        }
        Event::LoginFailed(message) => {
            set_login_error(app, message);
        }
        Event::LoginSucceeded => {
            if !app.is_main() {
                app.set_screen(Screen::Main);
            }
            // 若目前頁面尚未載入（例如從失敗畫面重試成功），補一次載入。
            handler::ensure_page(app, jobs);
            app.set_message("登录成功");
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
                progress,
            };
            app.term_options = data.term_options.clone();
            app.updated_at.homework = Some(now_clock());
            let len = data.group_count(app.homework_group);
            app.homework = Page::Ready(data);
            // 夾取選取索引，避免分組內容變動後越界。
            let selected = app
                .homework_state
                .selected()
                .unwrap_or(0)
                .min(len.saturating_sub(1));
            app.homework_state.select(Some(selected));
            if finished {
                app.set_message(format!("作业已更新：未完成 {unfinished} 项"));
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
        Event::Courses(courses) => {
            let count = courses.len();
            app.lms.courses = Page::Ready(courses);
            app.updated_at.lms = Some(now_clock());
            app.course_state.select(Some(0));
            app.lms.level = app::LmsLevel::Courses;
            app.set_message(format!("共 {count} 门课程"));
            app.ensure_main();
        }
        Event::Activities(activities) => {
            app.lms.activities = Page::Ready(activities);
            app.updated_at.lms = Some(now_clock());
            app.activity_state.select(Some(0));
            app.ensure_main();
        }
        Event::ActivityDetail(detail) => {
            app.lms.detail = Page::Ready(*detail);
            app.updated_at.lms = Some(now_clock());
            app.ensure_main();
        }
        Event::AccountUpdated => {
            app.set_message("账号已更新");
        }
        Event::PassphraseUpdated => {
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
                _ => {
                    // 資料任務失敗：若因重登而停在登入進度畫面，回到主畫面。
                    if matches!(
                        &app.screen,
                        Screen::Login(screen)
                            if matches!(screen.as_ref(), LoginScreen::Progress { .. })
                    ) {
                        app.set_screen(Screen::Main);
                    }
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
    let mut next = None;
    if let Screen::Login(screen) = &mut app.screen {
        match screen.as_mut() {
            LoginScreen::Credentials { form, .. } => {
                form.busy = false;
                form.error = Some(message);
            }
            _ => next = Some(Screen::Login(Box::new(LoginScreen::Failed { message }))),
        }
    }
    if let Some(screen) = next {
        app.set_screen(screen);
    }
}

#[cfg(test)]
#[path = "tests/event_test.rs"]
mod event_test;
