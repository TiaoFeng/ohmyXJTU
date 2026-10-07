//! TUI 啟動與主迴圈。
//!
//! 背景事件的套用（事件 → 介面狀態）集中在 [`event`]；本檔負責啟動、
//! 終端進出、panic hook 與主迴圈（繪製、按鍵與事件排空）。

pub mod app;
mod controller;
mod event;
pub mod handler;
pub mod text;
pub mod theme;
pub mod ui;
pub mod views;

use std::io::Stdout;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, KeyEventKind, poll, read};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::credentials::Vault;
use crate::error::{AppError, AppResult};
use crate::task::{self, Event, Job};

use app::{AgreementState, App, FormState, Screen};

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
    // 啟用 bracketed paste：貼上內容會以單一 Paste 事件送達，不會被拆成
    // 一連串按鍵（貼上含換行的內容時也不會意外送出表單）。
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::EnableBracketedPaste
    );
    let loop_result = main_loop(&mut terminal, &mut app, &events, &jobs);
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste
    );
    let restore_result = ratatui::try_restore().map_err(|err| AppError::Tui(err.to_string()));

    let _ = jobs.send(Job::Shutdown);

    loop_result.and(restore_result)
}

/// 安裝 panic hook：還原終端、清掉暫存檔、輸出單行錯誤訊息後結束行程。
///
/// 訊息不含 panic 內容（避免任何潛在敏感資料外洩），只保留發生位置；
/// 任何執行緒 panic 都會終止行程，避免背景執行緒死亡後介面卡死。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // 盡力還原終端：panic 可能發生在進入原始模式或替代畫面之後。
        let _ = ratatui::try_restore();
        // `try_restore` 不會重現游標，而 `exit` 又會跳過 `Terminal::Drop` 的補救；
        // 這裡一併關閉 bracketed paste 並顯示游標。
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        );
        // `exit` 會跳過 `main` 收尾的清理，因此在這裡補做一次：登入流程若正停在
        // 驗證碼，磁碟上會留著 captcha.png，而 `PRIVACY.md` 承諾登入終態與結束時
        // 一律刪除它。清理是 best-effort（不建立目錄、失敗忽略）。
        crate::auth::captcha::cleanup_on_exit();
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

        if poll(TICK).map_err(|err| AppError::Tui(err.to_string()))? {
            match read().map_err(|err| AppError::Tui(err.to_string()))? {
                // 長按（Repeat）與單次按下（Press）都應作用；Release 忽略。
                CrosstermEvent::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    handler::handle_key(app, key, jobs);
                }
                // bracketed paste：整段貼上不應被拆成一連串按鍵。
                CrosstermEvent::Paste(text) => handler::handle_paste(app, &text),
                _ => {}
            }
        }

        loop {
            match events.try_recv() {
                Ok(event) => event::apply_event(app, event, jobs),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // 背景工作執行緒已結束：之後不會再有任何事件，頁面會永遠停在
                    // 「載入中」且按鍵無效。明確提示使用者退出重啟，而不是靜默停滯。
                    app.set_error_message("后台任务已停止，请按 q 退出后重新启动");
                    break;
                }
            }
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

/// 套用背景事件（測試用：供跨模組的「工作者 → 介面」串接測試呼叫）。
#[cfg(test)]
pub(crate) use event::apply_event_for_test;

#[cfg(test)]
#[path = "tests/panic_hook_test.rs"]
mod panic_hook_test;
