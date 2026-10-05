//! 統一身份認證：登入頁解析、密碼加密與登入狀態機。
//!
//! WebVPN 網址轉換與校內主機判定見 [`crate::webvpn`]：它是傳輸層（重導信任
//! 判定）與認證層共用的網址格式工具，因此放在 crate 根，不屬於本模組。

pub mod captcha;
pub mod html;
pub mod login;
pub mod rsa;
pub mod state;

pub use login::LoginDriver;
pub use state::{AccountChoice, AccountType, LoginReply, MfaFlow};
