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
use crate::error::{AppError, AppResult};
use crate::task::{self, Event, Job};

use app::{App, FormState, LoginScreen, Page, Screen};
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
            apply_event(app, event);
        }

        if app.quit {
            return Ok(());
        }
    }
}

/// 套用背景事件。
fn apply_event(app: &mut App, event: Event) {
    match event {
        Event::VaultReady => {
            app.set_screen(Screen::Login(Box::new(LoginScreen::Progress {
                note: "凭证已解锁，正在登录…".to_owned(),
            })));
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
            app.set_message("登录成功");
        }
        Event::Schedule(data) => {
            app.schedule = Page::Ready(*data);
            app.schedule_state.select(Some(0));
            app.ensure_main();
        }
        Event::Homework(items) => {
            let count = items.len();
            app.homework = Page::Ready(items);
            app.homework_state.select(Some(0));
            app.set_message(format!("共 {count} 项待处理作业"));
            app.ensure_main();
        }
        Event::Flow(data) => {
            app.attendance = Page::Ready(*data);
            app.flow_state.select(Some(0));
            app.ensure_main();
        }
        Event::Courses(courses) => {
            let count = courses.len();
            app.lms.courses = Page::Ready(courses);
            app.course_state.select(Some(0));
            app.lms.level = app::LmsLevel::Courses;
            app.set_message(format!("共 {count} 门课程"));
            app.ensure_main();
        }
        Event::Activities(activities) => {
            app.lms.activities = Page::Ready(activities);
            app.activity_state.select(Some(0));
            app.ensure_main();
        }
        Event::ActivityDetail(detail) => {
            app.lms.detail = Page::Ready(*detail);
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
            app.set_message(format!("访问模式已切换为 {}", policy.label()));
        }
        Event::Notice(message) => {
            app.set_message(message);
        }
        Event::Failed { what, message } => {
            app.fail_loading(&message);
            let text = format!("{what}失败：{message}");
            // 登入過程中失敗（例如網路不通）也要離開「正在登录…」畫面，
            // 否則使用者按 enter 即可重試；憑證表單則就地顯示錯誤。
            set_login_error(app, text.clone());
            app.set_message(text);
        }
    }
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
