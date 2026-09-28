//! 統一認證的密碼加密。
//!
//! 伺服器要求以 RSA（PKCS#1 v1.5）加密密碼，再以 base64 編碼並加上 `__RSA__` 前綴。

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};

use crate::error::{AppError, AppResult};

/// 統一認證的 RSA 公鑰端點。
pub const PUBLIC_KEY_URL: &str = "https://login.xjtu.edu.cn/cas/jwt/publicKey";

/// 加密後密碼的前綴。
pub const PREFIX: &str = "__RSA__";

/// 以 PEM 公鑰加密密碼，回傳可直接提交的表單值。
pub fn encrypt_password(password: &str, public_key_pem: &str) -> AppResult<String> {
    let public_key = RsaPublicKey::from_public_key_pem(public_key_pem)
        .map_err(|err| AppError::protocol(format!("RSA 公钥解析失败：{err}")))?;

    let mut rng = chacha20poly1305::aead::OsRng;
    let encrypted = public_key
        .encrypt(&mut rng, Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|err| AppError::Crypto(format!("密码加密失败：{err}")))?;

    Ok(format!("{PREFIX}{}", STANDARD.encode(encrypted)))
}

#[cfg(test)]
#[path = "tests/rsa_test.rs"]
mod rsa_test;
