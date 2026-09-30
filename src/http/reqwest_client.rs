//! 以 `reqwest` 實作的 HTTP 客戶端。

use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::redirect::Policy;

use super::{Body, HttpClient, HttpRequest, HttpResponse, Method};
use crate::error::{AppError, AppResult, NetworkKind};

/// 單次請求的預設逾時。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 最多跟隨的重定向次數。
const MAX_REDIRECTS: usize = 10;

/// 錯誤鏈摘要的最大長度（字元數）。
const MAX_DETAIL_CHARS: usize = 320;

/// 內建 cookie jar 的阻塞式 HTTP 客戶端。
///
/// 同一個實例共享連線池與 cookie，對應一個「會話後端」。
/// 需要觀察 302 等狀態碼時，可透過 [`HttpRequest::no_redirect`] 改用不跟隨重定向的內部客戶端。
#[derive(Debug, Clone)]
pub struct ReqwestClient {
    redirecting: Client,
    fixed: Client,
    user_agent: String,
}

impl ReqwestClient {
    /// 建立客戶端。
    pub fn new(user_agent: impl Into<String>) -> AppResult<Self> {
        let user_agent = user_agent.into();
        Ok(Self {
            redirecting: build_client(&user_agent, Policy::limited(MAX_REDIRECTS))?,
            fixed: build_client(&user_agent, Policy::none())?,
            user_agent,
        })
    }

    /// 客戶端使用的 User-Agent。
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }
}

impl HttpClient for ReqwestClient {
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse> {
        let client = if request.follow_redirects {
            &self.redirecting
        } else {
            &self.fixed
        };

        let mut builder = match request.method {
            Method::Get => client.get(&request.url),
            Method::Post => client.post(&request.url),
        };

        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        builder = match request.body {
            Some(Body::Form(fields)) => builder.form(&fields),
            Some(Body::Json(value)) => builder.json(&value),
            Some(Body::Bytes(bytes)) => builder.body(bytes),
            None => builder,
        };

        let response = builder.send().map_err(map_error)?;
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    // 以 lossy 轉換保留非 UTF-8 標頭的存在與近似值：`unwrap_or_default`
                    // 會把任何非 UTF-8 標頭靜默變成空字串（例如 `content-type`），
                    // 使登入態失效判定失去依據。
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response.bytes().map_err(map_error)?.to_vec();

        Ok(HttpResponse {
            status,
            final_url,
            headers,
            body,
        })
    }
}

fn build_client(user_agent: &str, policy: Policy) -> AppResult<Client> {
    Client::builder()
        .user_agent(user_agent)
        .cookie_store(true)
        .redirect(policy)
        .timeout(DEFAULT_TIMEOUT)
        // 考勤入口（bk-kq.xjtu.edu.cn）的第一個回應以舊式多行標頭承載
        // Content-Security-Policy（續行使用裸 LF）。Hyper 預設拒收這類標頭，
        // 會讓請求在還沒開始重定向前就失敗；Python requests 對此寬容，參考實作
        // 因此不受影響。這裡只放寬這一種舊式折行格式，不選擇「忽略所有無效標頭」，
        // TLS 驗證等安全性設定維持不變。
        .http1_allow_obsolete_multiline_headers_in_responses(true)
        .build()
        .map_err(|err| AppError::network(format!("初始化 HTTP 客户端失败：{err}")))
}

/// 將 `reqwest` 錯誤映射為帶類別的網路錯誤。
///
/// 錯誤鏈會保留底層原因（例如 `invalid HTTP header parsed`），但一律去除
/// URL 與查詢參數，避免把敏感資訊帶進使用者可見訊息。
fn map_error(err: reqwest::Error) -> AppError {
    AppError::network_kind(classify(&err), describe(&err))
}

/// 依錯誤鏈判斷網路錯誤類別。
fn classify(err: &reqwest::Error) -> NetworkKind {
    classify_chain(
        &chain_text(err),
        err.is_timeout(),
        err.is_redirect(),
        err.is_connect(),
    )
}

/// 分類邏輯本體（純函式，便於以真實錯誤鏈文字測試）。
///
/// `chain` 需為小寫的錯誤鏈全文；旗標對應 `reqwest::Error` 的
/// `is_timeout`／`is_redirect`／`is_connect`。
fn classify_chain(
    chain: &str,
    is_timeout: bool,
    is_redirect: bool,
    is_connect: bool,
) -> NetworkKind {
    if is_timeout {
        return NetworkKind::Timeout;
    }
    if is_redirect {
        return NetworkKind::Redirect;
    }
    if chain.contains("invalid http header") || chain.contains("invalid header") {
        return NetworkKind::HttpParse;
    }
    if chain.contains("dns")
        || chain.contains("failed to lookup")
        || chain.contains("name or service not known")
        || chain.contains("nodename nor servname")
    {
        return NetworkKind::Dns;
    }
    if chain.contains("certificate") || chain.contains("tls") || chain.contains("handshake") {
        return NetworkKind::Tls;
    }
    if is_connect
        || chain.contains("connection closed")
        || chain.contains("connection reset")
        || chain.contains("broken pipe")
        || chain.contains("unexpected eof")
    {
        return NetworkKind::Connect;
    }
    NetworkKind::Other
}

/// 串接錯誤鏈全文（小寫）供關鍵字判類。
fn chain_text(err: &reqwest::Error) -> String {
    let mut text = String::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(item) = current {
        text.push_str(&item.to_string().to_lowercase());
        text.push('\n');
        current = item.source();
    }
    text
}

/// 將錯誤鏈整理為去識別化的單行摘要。
fn describe(err: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(item) = current {
        let text = sanitize(&item.to_string());
        if !text.is_empty() && parts.last().map(String::as_str) != Some(text.as_str()) {
            parts.push(text);
        }
        current = item.source();
    }

    let mut detail = parts.join("：");
    if detail.chars().count() > MAX_DETAIL_CHARS {
        let cut = detail
            .char_indices()
            .nth(MAX_DETAIL_CHARS)
            .map_or(detail.len(), |(index, _)| index);
        detail.truncate(cut);
        detail.push('…');
    }
    detail
}

/// 以 `<url>` 取代任何含查詢參數的網址。
fn sanitize(text: &str) -> String {
    text.split_whitespace()
        .map(|token| {
            if token.contains("://") {
                "<url>"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[path = "tests/reqwest_client_test.rs"]
mod reqwest_client_test;
