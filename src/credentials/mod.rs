//! 憑證儲存：以口令加解密帳號密碼，磁碟上不留明文。

pub mod secret;
pub mod vault;

pub use secret::Secret;
pub use vault::{Credentials, Vault};
