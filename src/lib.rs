//! ohmyXJTU：西安交通大学工具箱（TUI）。

pub mod auth;
pub mod config;
pub mod credentials;
pub mod domain;
pub mod error;
pub mod http;
pub mod io;
pub mod random;
pub mod session;
pub mod sites;
pub mod task;
pub mod tui;

pub use error::{AppError, AppResult};

/// 啟動 TUI 應用程式。
pub fn run() -> AppResult<()> {
    tui::run()
}
