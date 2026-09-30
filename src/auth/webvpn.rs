//! WebVPN 網址轉換。
//!
//! 校內站點在校外需要經由 `webvpn.xjtu.edu.cn` 轉發，轉發網址由主機名加密後組成：
//!
//! ```text
//! https://webvpn.xjtu.edu.cn/{proto}[-{port}]/{固定前綴}{hex(AES-128-CFB(host))}/{path?query}
//! ```
//!
//! 其中 AES 的金鑰與 IV 都是公開常數 `wrdvpnisthebest!`（僅用於網址混淆，非機密），
//! CFB 的分段長度為 128 位元。

use cfb_mode::cipher::{AsyncStreamCipher as _, KeyIvInit as _};
use url::Url;

use crate::error::{AppError, AppResult};

/// WebVPN 站點主機名。
pub const WEBVPN_HOST: &str = "webvpn.xjtu.edu.cn";

/// 加密用的金鑰與初始向量。
const KEY: &[u8; 16] = b"wrdvpnisthebest!";
const IV: &[u8; 16] = b"wrdvpnisthebest!";

/// 六角編碼後的初始向量，固定出現在密文之前。
pub const PREFIX: &str = "77726476706e69737468656265737421";

/// 以 AES-128-CFB 加密主機名，回傳十六進位字串。
pub fn encrypt_host(host: &str) -> String {
    let mut buffer = host.as_bytes().to_vec();
    cfb_mode::Encryptor::<aes::Aes128>::new(KEY.into(), IV.into()).encrypt(&mut buffer);
    to_hex(&buffer)
}

/// 將十六進位密文還原為主機名。
pub fn decrypt_host(cipher_hex: &str) -> AppResult<String> {
    let mut buffer = from_hex(cipher_hex)?;
    cfb_mode::Decryptor::<aes::Aes128>::new(KEY.into(), IV.into()).decrypt(&mut buffer);
    String::from_utf8(buffer)
        .map_err(|err| AppError::protocol(format!("WebVPN 主机名解码失败：{err}")))
}

/// 主機名是否屬於學校網域（`xjtu.edu.cn` 或其子網域）。
pub fn is_school_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "xjtu.edu.cn" || host.ends_with(".xjtu.edu.cn")
}

/// 判斷網址是否需要改寫為 WebVPN 網址。
///
/// 只改寫 `xjtu.edu.cn` 及其子網域，且 WebVPN 本身不再改寫，避免無限遞迴。
pub fn should_rewrite(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    if host == WEBVPN_HOST {
        return false;
    }
    is_school_host(host)
}

/// 將一般網址轉換為 WebVPN 網址。
pub fn to_webvpn_url(url: &str) -> AppResult<String> {
    let parsed =
        Url::parse(url).map_err(|err| AppError::protocol(format!("无法解析 URL：{err}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::protocol("URL 缺少主机名"))?;
    let scheme = parsed.scheme().to_owned();
    let port = parsed
        .port()
        .map_or(String::new(), |port| format!("-{port}"));
    let path = parsed.path().trim_start_matches('/');
    let query = parsed
        .query()
        .map_or(String::new(), |query| format!("?{query}"));

    Ok(format!(
        "https://{WEBVPN_HOST}/{scheme}{port}/{PREFIX}{}/{path}{query}",
        encrypt_host(host)
    ))
}

/// 將 WebVPN 網址還原為一般網址。
pub fn from_webvpn_url(url: &str) -> AppResult<String> {
    let parsed =
        Url::parse(url).map_err(|err| AppError::protocol(format!("无法解析 URL：{err}")))?;
    if parsed.host_str() != Some(WEBVPN_HOST) {
        return Err(AppError::protocol("不是 WebVPN 網址"));
    }

    let segments: Vec<&str> = parsed.path().trim_start_matches('/').split('/').collect();
    if segments.len() < 3 {
        return Err(AppError::protocol("WebVPN 網址格式不正确"));
    }
    let (scheme, port) = match segments[0].split_once('-') {
        Some((scheme, port)) => (scheme, Some(port)),
        None => (segments[0], None),
    };
    let cipher = segments[1];
    // 前綴長度檢查必須是 32 個字元（參考實作此處比對 16 個字元，永遠不成立）。
    let cipher = cipher
        .strip_prefix(PREFIX)
        .ok_or_else(|| AppError::protocol("WebVPN 網址前缀不匹配"))?;
    let mut host = decrypt_host(cipher)?;
    if let Some(port) = port {
        host.push(':');
        host.push_str(port);
    }

    let path = segments[2..].join("/");
    let query = parsed
        .query()
        .map_or(String::new(), |query| format!("?{query}"));
    Ok(format!("{scheme}://{host}/{path}{query}"))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn from_hex(text: &str) -> AppResult<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(AppError::protocol("十六进制字符串长度必须为偶数"));
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair)
                .map_err(|_| AppError::protocol("十六进制字符串包含非法字符"))?;
            u8::from_str_radix(pair, 16)
                .map_err(|_| AppError::protocol("十六进制字符串包含非法字符"))
        })
        .collect()
}

#[cfg(test)]
#[path = "tests/webvpn_test.rs"]
mod webvpn_test;
