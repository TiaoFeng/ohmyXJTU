//! HTTP 抽象層。
//!
//! 業務程式碼只依賴 [`HttpClient`]：正式執行時注入 [`ReqwestClient`]，
//! 單元測試時注入回放固定回應的假客戶端，因此所有站點邏輯都能離線驗證。

pub mod reqwest_client;

#[cfg(test)]
pub mod fake;

pub use reqwest_client::ReqwestClient;

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
}

/// 請求主體。
#[derive(Debug, Clone)]
pub enum Body {
    /// `application/x-www-form-urlencoded` 表單。
    Form(Vec<(String, String)>),
    /// `application/json`。
    Json(serde_json::Value),
    /// 原始位元組。
    Bytes(Vec<u8>),
}

/// 一次 HTTP 請求。
#[derive(Debug, Clone)]
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

/// 一次 HTTP 回應。
#[derive(Debug, Clone)]
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
    /// 建立回應（主要供測試使用）。
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

/// HTTP 客戶端抽象。
pub trait HttpClient: Send + Sync {
    /// 送出請求並讀取完整回應。
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse>;
}
