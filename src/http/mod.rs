//! HTTP 抽象層。
//!
//! 業務程式碼只依賴 [`HttpClient`]：正式執行時注入 [`ReqwestClient`]，
//! 單元測試時注入回放固定回應的假客戶端，因此所有站點邏輯都能離線驗證。

pub mod batch;
pub mod reqwest_client;

#[cfg(test)]
pub mod fake;

pub use reqwest_client::ReqwestClient;

use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::error::{AppError, AppResult};

/// HTTP 方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `HEAD`
    Head,
    /// `MKCOL`（WebDAV：建立集合）
    Mkcol,
}

/// 請求主體。
///
/// 手寫 [`fmt::Debug`]：表單欄位帶著帳號、圖形驗證碼與簡訊驗證碼，任何 `{:?}`
/// 都只輸出**欄位名稱**，不輸出內容。
#[derive(Clone)]
pub enum Body {
    /// `application/x-www-form-urlencoded` 表單。
    Form(Vec<(String, String)>),
    /// `application/json`。
    Json(serde_json::Value),
    /// 原始位元組（例如 WebDAV 同步的加密文檔）。
    Bytes(Vec<u8>),
}

impl fmt::Debug for Body {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Form(fields) => formatter
                .debug_tuple("Form")
                .field(&field_names(fields))
                .finish(),
            // JSON 主體含業務資料與識別碼，只描述型別。
            Self::Json(_) => formatter.write_str("Json(<redacted>)"),
            // 原始位元組（例如同步的加密文檔）不得印出內容，只輸出長度。
            Self::Bytes(bytes) => formatter.debug_tuple("Bytes").field(&bytes.len()).finish(),
        }
    }
}

/// 取出成對欄位的名稱（供遮罩後的 `Debug` 使用）。
pub(crate) fn field_names(fields: &[(String, String)]) -> Vec<&str> {
    fields.iter().map(|(name, _)| name.as_str()).collect()
}

/// 去掉查詢字串與 userinfo 的網址（供遮罩後的 `Debug` 使用）。
///
/// 查詢字串可能帶著一次性 ticket 或業務憑證（登入回跳位址就是這樣傳遞的），
/// userinfo（`https://token@host/`）同樣是憑證：兩者都只保留協定、主機與路徑，
/// 與網路錯誤訊息一貫的處理方式相同。
pub(crate) fn redacted_url(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return "<url>".to_owned();
    };
    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
    let has_query = parsed.query().is_some() || parsed.fragment().is_some();
    if !has_userinfo && !has_query {
        return url.to_owned();
    }
    parsed.set_username("").ok();
    parsed.set_password(None).ok();
    parsed.set_query(None);
    parsed.set_fragment(None);
    if has_query {
        format!("{}?<redacted>", parsed.as_str())
    } else {
        parsed.to_string()
    }
}

/// 一次 HTTP 請求。
///
/// 手寫 [`fmt::Debug`]：標頭值（例如考勤的 `X-Business-Token`）與主體內容都是
/// 憑證，任何 `{:?}` 都不得把它們印出來（見 `crate::credentials::secret` 的同一條規則）。
#[derive(Clone)]
pub struct HttpRequest {
    /// 請求方法。
    pub method: Method,
    /// 目標網址。
    pub url: String,
    /// 附加標頭，同名標頭覆蓋用戶端預設值。
    pub headers: Vec<(String, String)>,
    /// 請求主體。
    pub body: Option<Body>,
    /// 單次請求逾時，未設定時使用客戶端預設值。
    pub timeout: Option<Duration>,
    /// 是否跟隨重定向。
    pub follow_redirects: bool,
}

impl HttpRequest {
    /// 建立 GET 請求。
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立無主體的 POST 請求。
    pub fn post(url: impl Into<String>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立表單 POST 請求。
    pub fn post_form<S, T>(url: impl Into<String>, fields: impl IntoIterator<Item = (S, T)>) -> Self
    where
        S: Into<String>,
        T: Into<String>,
    {
        let fields = fields
            .into_iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body: Some(Body::Form(fields)),
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立 JSON POST 請求。
    pub fn post_json(url: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body: Some(Body::Json(value)),
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立 PUT 請求（原始位元組主體）。
    pub fn put(url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            method: Method::Put,
            url: url.into(),
            headers: Vec::new(),
            body: Some(Body::Bytes(body.into())),
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立 DELETE 請求。
    pub fn delete(url: impl Into<String>) -> Self {
        Self {
            method: Method::Delete,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立 HEAD 請求。
    pub fn head(url: impl Into<String>) -> Self {
        Self {
            method: Method::Head,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 建立 `MKCOL` 請求（WebDAV：建立集合；無主體）。
    pub fn mkcol(url: impl Into<String>) -> Self {
        Self {
            method: Method::Mkcol,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: None,
            follow_redirects: true,
        }
    }

    /// 附加標頭。
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 不跟隨重定向（用於需要觀察狀態碼的探測請求）。
    pub fn no_redirect(mut self) -> Self {
        self.follow_redirects = false;
        self
    }

    /// 設定單次請求逾時。
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// 讀取表單欄位（僅 `Body::Form`）。
    ///
    /// 僅供測試斷言送出的欄位。
    #[cfg(test)]
    pub fn form_field(&self, name: &str) -> Option<&str> {
        match &self.body {
            Some(Body::Form(fields)) => fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str()),
            _ => None,
        }
    }

    /// 讀取標頭值（不分大小寫）。
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &redacted_url(&self.url))
            .field("headers", &field_names(&self.headers))
            .field("body", &self.body)
            .field("timeout", &self.timeout)
            .field("follow_redirects", &self.follow_redirects)
            .finish()
    }
}

/// 一次 HTTP 回應。
///
/// 手寫 [`fmt::Debug`]：`final_url` 可能帶著一次性 ticket，`body` 是未經處理的
/// 回應內容（登入頁、成績、提交記錄…），因此只輸出狀態碼、去查詢的網址、標頭
/// 名稱與本文長度。
#[derive(Clone)]
pub struct HttpResponse {
    /// HTTP 狀態碼。
    pub status: u16,
    /// 跟隨重定向後的最終網址。
    pub final_url: String,
    /// 回應標頭。
    pub headers: Vec<(String, String)>,
    /// 回應本文。
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 建立回應（僅供測試：生產回應一律來自客戶端）。
    #[cfg(test)]
    pub fn new(status: u16, final_url: impl Into<String>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            final_url: final_url.into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// 以有損 UTF-8 解碼回應本文。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// 將回應本文解析為 JSON。
    pub fn json<T: DeserializeOwned>(&self) -> AppResult<T> {
        serde_json::from_slice(&self.body).map_err(|err| {
            AppError::protocol(format!(
                "响应 JSON 解析失败（{}）",
                crate::error::describe_json_failure(err.classify())
            ))
        })
    }

    /// 讀取標頭值（不分大小寫）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 是否為 2xx 回應。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 若狀態碼非 2xx，回報 [`AppError::Http`]。
    pub fn error_for_status(&self) -> AppResult<()> {
        if self.is_success() {
            Ok(())
        } else {
            Err(AppError::Http {
                status: self.status,
            })
        }
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("final_url", &redacted_url(&self.final_url))
            .field("headers", &field_names(&self.headers))
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// HTTP 客戶端抽象。
pub trait HttpClient: Send + Sync {
    /// 送出請求並讀取完整回應。
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse>;
}

#[cfg(test)]
#[path = "tests/mod_test.rs"]
mod mod_test;
