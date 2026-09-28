//! 會話管理：訪問方式解析、登入流程編排與帶自動改寫的請求轉送。

pub mod manager;
pub mod site;

pub use manager::{LoginStage, SessionManager};
pub use site::{AccessMode, PostLogin, SiteAdapter, SiteKind, SiteLogin, SitePolicy};
