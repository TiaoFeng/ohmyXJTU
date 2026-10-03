//! 併發傳輸：以固定的並行度送出多個請求。
//!
//! 只負責傳輸。站點標頭注入、WebVPN 改址、登入態判定與請求計數仍由
//! [`crate::session::SessionManager`] 處理，這裡只把「已經可以送出的請求」
//! 同時送出去並依原順序收回結果。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::{HttpClient, HttpRequest, HttpResponse};
use crate::error::{AppError, AppResult};

/// 同時在途的請求數上限。
///
/// 保守取值：一次只平行送出少數請求，對校內服務維持低負載。呼叫端以「一批」
/// 為單位控制節奏——批次之間仍會處理控制任務（設定、重新整理），因此這個值
/// 同時也是「一次批次最長會佔用工作執行緒多久」的上界。
pub const MAX_CONCURRENT_REQUESTS: usize = 3;

/// 併發送出多個請求，回傳與輸入**同序**的結果。
///
/// 每個請求各自獨立成敗：單一失敗不會影響其他請求，呼叫端可自行決定要略過
/// 還是中止。並行度取 [`MAX_CONCURRENT_REQUESTS`] 與請求數的較小值。
pub fn send_concurrently(
    client: Arc<dyn HttpClient>,
    requests: Vec<HttpRequest>,
) -> Vec<AppResult<HttpResponse>> {
    let total = requests.len();
    if total == 0 {
        return Vec::new();
    }
    if total == 1 {
        return requests
            .into_iter()
            .map(|request| client.send(request))
            .collect();
    }

    // 各執行緒以原子游標領取下一個索引，結果寫回自己的欄位：不需要把請求
    // 搬進佇列，也不必為輸出排序。
    let cursor = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<AppResult<HttpResponse>>>> =
        (0..total).map(|_| Mutex::new(None)).collect();

    thread::scope(|scope| {
        for _ in 0..MAX_CONCURRENT_REQUESTS.min(total) {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    if index >= total {
                        break;
                    }
                    let result = client.send(requests[index].clone());
                    // 鎖被毒化（其他執行緒在持鎖時 panic）時仍取用內容；
                    // 結果的完整性由下方的 `None` 檢查負責。
                    let mut slot = slots[index]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *slot = Some(result);
                }
            });
        }
    });

    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or_else(|| Err(AppError::protocol("并发请求的结果不完整")))
        })
        .collect()
}

#[cfg(test)]
#[path = "tests/batch_test.rs"]
mod batch_test;
