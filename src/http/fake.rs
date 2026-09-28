//! 測試用的假 HTTP 客戶端。
//!
//! 讓所有站點與認證邏輯都能以「脫敏固定回應」離線驗證，不需要真實網路。

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{HttpClient, HttpRequest, HttpResponse};
use crate::error::{AppError, AppResult};

type Responder = Box<dyn Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync>;

/// 記錄所有送出的請求，並依序回放或動態產生回應。
pub struct FakeClient {
    responder: Responder,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FakeClient {
    /// 依序回放預置回應；回應用盡後回報錯誤。
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        let queue = Mutex::new(VecDeque::from(responses));
        Self::with_responder(move |_| {
            let mut queue = queue.lock().expect("fake client lock poisoned");
            queue
                .pop_front()
                .ok_or_else(|| AppError::network("FakeClient：没有更多预置响应"))
        })
    }

    /// 依請求動態產生回應。
    pub fn with_responder<F>(responder: F) -> Self
    where
        F: Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
    {
        Self {
            responder: Box::new(responder),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// 已送出的請求（依序）。
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests
            .lock()
            .expect("fake client lock poisoned")
            .clone()
    }

    /// 最近一次送出的請求。
    pub fn last_request(&self) -> Option<HttpRequest> {
        self.requests
            .lock()
            .expect("fake client lock poisoned")
            .last()
            .cloned()
    }
}

impl HttpClient for FakeClient {
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse> {
        self.requests
            .lock()
            .expect("fake client lock poisoned")
            .push(request.clone());
        (self.responder)(&request)
    }
}

/// HTML 回應（狀態 200）。
pub fn html(body: &str) -> HttpResponse {
    HttpResponse::new(200, "https://example.invalid/page", body.as_bytes())
}

/// JSON 回應（狀態 200）。
pub fn json(value: serde_json::Value) -> HttpResponse {
    HttpResponse::new(
        200,
        "https://example.invalid/api",
        serde_json::to_vec(&value).expect("序列化测试 JSON"),
    )
}

/// 指定狀態碼與本文的回應。
pub fn status(status: u16, body: &str) -> HttpResponse {
    HttpResponse::new(status, "https://example.invalid/page", body.as_bytes())
}

/// 重定向回應：`final_url` 代表跟隨重定向後的最終網址。
pub fn redirect(final_url: &str) -> HttpResponse {
    HttpResponse::new(200, final_url, b"".as_slice())
}
