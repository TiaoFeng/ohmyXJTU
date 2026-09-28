//! 統一認證的密碼加密。
//!
//! 伺服器要求以 RSA（PKCS#1 v1.5）加密密碼，再以 base64 編碼並加上 `__RSA__` 前綴。
//!
//! 嚴格 PEM 解碼器（`pem-rfc7468`）要求 base64 內文「除最後一行外每行恰為 64 字元」，
//! 真實端點只要改成單行或 76 字元換行就會回報 `PEM Base64 error`。因此
//! [`parse_public_key`] 自行擷取 PEM 內文、去除所有空白後才解碼，對換行方式不敏感。

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use rsa::pkcs1::DecodeRsaPublicKey as _;
use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};

use crate::error::{AppError, AppResult};

/// 統一認證的 RSA 公鑰端點。
pub const PUBLIC_KEY_URL: &str = "https://login.xjtu.edu.cn/cas/jwt/publicKey";

/// 加密後密碼的前綴。
pub const PREFIX: &str = "__RSA__";

/// PKCS#8 `SubjectPublicKeyInfo` 的 PEM 標籤。
const SPKI_LABEL: &str = "PUBLIC KEY";

/// PKCS#1 的 PEM 標籤。
const PKCS1_LABEL: &str = "RSA PUBLIC KEY";

/// JSON 包裝時可能放置公鑰的欄位名稱。
const JSON_FIELDS: [&str; 4] = ["publicKey", "public_key", "key", "data"];

/// 解析公鑰端點的回應正文。
///
/// 可處理裸 PEM（任意行寬、CRLF、BOM、前後空白、以 `\n` 表示的跳脫換行）與 JSON 包裝
/// （字串或物件欄位）。錯誤訊息只描述格式，絕不包含正文內容。
pub fn parse_public_key(raw: &str) -> AppResult<RsaPublicKey> {
    let text = normalize(raw);
    let (label, body) = split_pem(&text)?;
    let der = decode_base64(&body)?;

    // 兩個 DER 解碼器各自回傳不同的錯誤型別，因此各自轉成應用層錯誤。
    match label {
        SPKI_LABEL => RsaPublicKey::from_public_key_der(&der)
            .map_err(|detail| AppError::protocol(format!("公钥 DER 结构无法解析：{detail}"))),
        _ => RsaPublicKey::from_pkcs1_der(&der)
            .map_err(|detail| AppError::protocol(format!("公钥 DER 结构无法解析：{detail}"))),
    }
}

/// 以公鑰加密密碼，回傳可直接提交的表單值。
pub fn encrypt_password(password: &str, public_key: &RsaPublicKey) -> AppResult<String> {
    let mut rng = chacha20poly1305::aead::OsRng;
    let encrypted = public_key
        .encrypt(&mut rng, Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|err| AppError::Crypto(format!("密码加密失败：{err}")))?;

    Ok(format!("{PREFIX}{}", STANDARD.encode(encrypted)))
}

/// 去除 BOM 與前後空白，必要時解開 JSON 包裝與跳脫換行。
fn normalize(raw: &str) -> String {
    let text = raw.trim_start_matches('\u{feff}').trim();
    let inner = match text.chars().next() {
        Some('{' | '[' | '"') => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| find_text(&value))
            .unwrap_or_else(|| text.to_owned()),
        _ => text.to_owned(),
    };

    unescape(&inner)
}

/// 在 JSON 值中尋找公鑰文本：字串本身，或常見欄位內的字串。
fn find_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(fields) => JSON_FIELDS
            .iter()
            .find_map(|name| fields.get(*name).and_then(find_text)),
        serde_json::Value::Array(items) => items.iter().find_map(find_text),
        _ => None,
    }
}

/// 還原以 `\n` 等轉義序列表示的換行（部分伺服器會如此回傳）。
fn unescape(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_owned();
    }

    text.replace("\\r\\n", "\n")
        .replace("\\n", "\n")
        .replace("\\r", "\n")
        .replace("\\t", "\t")
}

/// 擷取 PEM 邊界內的標籤與 base64 內文（已去除所有空白）。
fn split_pem(text: &str) -> AppResult<(&'static str, String)> {
    if text.is_empty() {
        return Err(AppError::protocol("公钥响应内容为空"));
    }

    for label in [SPKI_LABEL, PKCS1_LABEL] {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let Some(start) = text.find(&begin) else {
            continue;
        };

        let rest = &text[start + begin.len()..];
        let stop = rest
            .find(&end)
            .ok_or_else(|| AppError::protocol(format!("公钥响应缺少 {end}")))?;
        let body: String = rest[..stop]
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        if body.is_empty() {
            return Err(AppError::protocol("公钥响应中 PEM 内容为空"));
        }

        return Ok((label, body));
    }

    Err(AppError::protocol(format!(
        "公钥响应{}，共 {} 字节",
        describe(text),
        text.len()
    )))
}

/// 描述非 PEM 正文的形態，僅供診斷、不含正文內容。
fn describe(text: &str) -> &'static str {
    let head = text.trim_start();
    if head.starts_with('<') {
        "不是 PEM 文本（看起来是 HTML）"
    } else if head.starts_with('{') || head.starts_with('[') {
        "不是 PEM 文本（看起来是 JSON）"
    } else {
        "不是 PEM 文本"
    }
}

/// 解碼 PEM 的 base64 內文（允許缺少補齊字元）。
fn decode_base64(body: &str) -> AppResult<Vec<u8>> {
    STANDARD
        .decode(body)
        .or_else(|_| STANDARD_NO_PAD.decode(body))
        .map_err(|err| AppError::protocol(format!("公钥 base64 内容无法解码：{err}")))
}

#[cfg(test)]
#[path = "tests/rsa_test.rs"]
mod rsa_test;
