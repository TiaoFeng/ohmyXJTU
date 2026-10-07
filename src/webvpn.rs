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

/// WebVPN 代理路徑的目標資訊。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxiedTarget {
    /// 目標訪問方式（`http`／`https`）。
    pub scheme: String,
    /// 目標主機（主機名解密失敗時為 `None`）。
    pub host: Option<String>,
    /// 目標埠（網址未指定時為 `None`）。
    pub port: Option<u16>,
    /// 目標路徑（含開頭的 `/`）。
    pub path: String,
}

/// 解析 WebVPN 代理路徑（形如 `/https[-port]/<前綴+密文>/<目標路徑>`）。
///
/// 不是代理路徑時回 `None`。主機名解密失敗仍會回傳其餘欄位，由呼叫端決定
/// 要多保守。外層主機都是 `webvpn.xjtu.edu.cn`，不代表內層目的站點相同，
/// 因此需要比較來源時一律以此結果為準。
pub fn proxied_target(path: &str) -> Option<ProxiedTarget> {
    let rest = path.trim_start_matches('/');
    let (scheme, rest) = rest.split_once('/')?;
    let (scheme, port) = match scheme.split_once('-') {
        Some((scheme, port)) => (scheme, port.parse::<u16>().ok()),
        None => (scheme, None),
    };
    if !matches!(scheme, "https" | "http") {
        return None;
    }
    let (cipher, rest) = rest.split_once('/')?;
    let host = cipher
        .strip_prefix(PREFIX)
        .and_then(|hex| decrypt_host(hex).ok());
    Some(ProxiedTarget {
        scheme: scheme.to_owned(),
        host,
        port,
        path: format!("/{rest}"),
    })
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
///
/// 僅供測試驗證 [`to_webvpn_url`] 的往返（生產碼用的是 [`proxied_target`]）：
/// 標記為 `cfg(test)` 以免讓人以為有生產呼叫端。
#[cfg(test)]
pub fn from_webvpn_url(url: &str) -> AppResult<String> {
    let parsed =
        Url::parse(url).map_err(|err| AppError::protocol(format!("无法解析 URL：{err}")))?;
    if parsed.host_str() != Some(WEBVPN_HOST) {
        return Err(AppError::protocol("不是 WebVPN 网址"));
    }

    let segments: Vec<&str> = parsed.path().trim_start_matches('/').split('/').collect();
    if segments.len() < 3 {
        return Err(AppError::protocol("WebVPN 网址格式不正确"));
    }
    let (scheme, port) = match segments[0].split_once('-') {
        Some((scheme, port)) => (scheme, Some(port)),
        None => (segments[0], None),
    };
    let cipher = segments[1];
    // 前綴長度檢查必須是 32 個字元（參考實作此處比對 16 個字元，永遠不成立）。
    let cipher = cipher
        .strip_prefix(PREFIX)
        .ok_or_else(|| AppError::protocol("WebVPN 网址前缀不匹配"))?;
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
