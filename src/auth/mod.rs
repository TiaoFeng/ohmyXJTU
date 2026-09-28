//! 統一身份認證：登入頁解析、WebVPN 網址轉換、密碼加密與登入狀態機。

pub mod captcha;
pub mod html;
pub mod login;
pub mod rsa;
pub mod state;
pub mod webvpn;

pub use login::LoginDriver;
pub use state::{AccountChoice, AccountType, LoginReply, MfaFlow};
