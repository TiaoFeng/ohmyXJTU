//! 以 `reqwest` 實作的 HTTP 客戶端。

use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::redirect::Policy;

use super::{Body, HttpClient, HttpRequest, HttpResponse, Method};
use crate::error::{AppError, AppResult};

/// 單次請求的預設逾時。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 最多跟隨的重定向次數。
const MAX_REDIRECTS: usize = 10;

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
                    value.to_str().unwrap_or_default().to_owned(),
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
        .build()
        .map_err(|err| AppError::network(format!("初始化 HTTP 客户端失败：{err}")))
}

fn map_error(err: reqwest::Error) -> AppError {
    if err.is_timeout() {
        AppError::network(format!("请求超时：{err}"))
    } else if err.is_connect() {
        AppError::network(format!("无法连接服务器：{err}"))
    } else {
        AppError::network(err.to_string())
    }
}
