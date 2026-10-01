//! 密碼學安全隨機數工具。
//!
//! 統一從作業系統隨機源取得亂數，供憑證鹽值、AEAD nonce 與裝置標識使用。

use std::fmt::Write as _;

use crate::error::{AppError, AppResult};

/// 以作業系統隨機源填滿緩衝區。
pub fn fill(buf: &mut [u8]) -> AppResult<()> {
    getrandom::fill(buf).map_err(|err| AppError::Crypto(format!("随机数生成失败：{err}")))
}

/// 產生指定長度的隨機位元組。
fn bytes(len: usize) -> AppResult<Vec<u8>> {
    let mut buf = vec![0_u8; len];
    fill(&mut buf)?;
    Ok(buf)
}

/// 產生指定長度的小寫十六進位字串，字串長度為 `byte_len * 2`。
pub fn hex(byte_len: usize) -> AppResult<String> {
    let mut out = String::with_capacity(byte_len * 2);
    for byte in bytes(byte_len)? {
        // 寫入 `String` 不可能失敗。
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}
