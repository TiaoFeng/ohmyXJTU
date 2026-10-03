//! 併發傳輸測試：並行度上限、結果順序與單筆錯誤的隔離。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{MAX_CONCURRENT_REQUESTS, send_concurrently};
use crate::error::{AppError, AppResult};
use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// 記錄同時在途數與呼叫順序的假客戶端。
struct Probe {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    delay: Duration,
    calls: Mutex<Vec<String>>,
}

impl Probe {
    fn new(delay: Duration) -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            delay,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 觀察到的最大同時在途數。
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl HttpClient for Probe {
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(current, Ordering::SeqCst);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        let url = request.url;
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(url.clone());
        }
        if url.contains("boom") {
            return Err(AppError::protocol("测试注入：单笔失败"));
        }
        // 回應內容即請求網址：呼叫端可據此檢查順序。
        Ok(HttpResponse::new(200, url.clone(), url))
    }
}

#[test]
fn sends_batches_concurrently_and_keeps_order() {
    let client = Arc::new(Probe::new(Duration::from_millis(30)));
    let requests: Vec<HttpRequest> = (0..6)
        .map(|index| HttpRequest::get(format!("https://lms.xjtu.edu.cn/api/{index}")))
        .collect();

    let responses = send_concurrently(Arc::clone(&client) as Arc<dyn HttpClient>, requests);

    assert_eq!(responses.len(), 6, "每个请求都应有结果");
    for (index, response) in responses.iter().enumerate() {
        let response = response.as_ref().expect("每个请求都应成功");
        assert!(
            response.final_url.ends_with(&format!("/api/{index}")),
            "结果应与输入同序（第 {index} 笔）：{}",
            response.final_url
        );
    }
    assert!(
        client.peak() >= 2,
        "应确实并行送出，实际峰值 {}",
        client.peak()
    );
    assert!(
        client.peak() <= MAX_CONCURRENT_REQUESTS,
        "不得超出并发上限，实际峰值 {}",
        client.peak()
    );
}

#[test]
fn keeps_each_request_outcome_in_place() {
    let client = Arc::new(Probe::new(Duration::ZERO));
    let requests = vec![
        HttpRequest::get("https://lms.xjtu.edu.cn/api/ok-1"),
        HttpRequest::get("https://lms.xjtu.edu.cn/api/boom"),
        HttpRequest::get("https://lms.xjtu.edu.cn/api/ok-2"),
    ];

    let responses = send_concurrently(Arc::clone(&client) as Arc<dyn HttpClient>, requests);

    assert!(responses[0].is_ok(), "单笔失败不应影响其他请求");
    assert!(
        matches!(&responses[1], Err(AppError::Protocol(message)) if message.contains("单笔失败")),
        "失败应留在原位：{:?}",
        responses[1]
    );
    assert!(responses[2].is_ok(), "单笔失败不应影响其他请求");
}

#[test]
fn handles_empty_and_single_requests() {
    let client = Arc::new(Probe::new(Duration::ZERO)) as Arc<dyn HttpClient>;
    assert!(send_concurrently(Arc::clone(&client), Vec::new()).is_empty());

    let responses = send_concurrently(
        client,
        vec![HttpRequest::get("https://lms.xjtu.edu.cn/api/only")],
    );
    assert_eq!(responses.len(), 1, "单个请求也应回传结果");
    assert!(responses[0].is_ok());
}
