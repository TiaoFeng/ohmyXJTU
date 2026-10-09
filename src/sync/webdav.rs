//! WebDAV 客戶端（堅果雲同步用）。
//!
//! 只使用 `PUT`／`GET`／`HEAD`／`DELETE`，**刻意不用 `PROPFIND`**：列出目錄需要
//! 解析 `207 Multi-Status` 的 XML，而本專案不引入 XML 解析依賴。遠端檔案名固定，
//! 「檔案是否存在」用 `HEAD`／`GET` 的 `404` 判斷即可。
//!
//! 認證採 HTTP Basic（帳號＋應用密碼）。`Authorization` 標頭只在記憶體中短暫
//! 存在，且 [`crate::http::HttpRequest`] 的 `Debug` 只輸出標頭名稱，憑證不會寫進
//! 任何使用者可見的訊息或紀錄。

use std::sync::Arc;

use base64::Engine as _;
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};
use crate::http::{HttpClient, HttpRequest, HttpResponse};

/// 遠端檔案的中介資料。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteMeta {
    /// 檔案是否存在（`404` 視為不存在）。
    pub exists: bool,
    /// `ETag`（伺服器有回應才為 `Some`）。
    pub etag: Option<String>,
    /// `Last-Modified`（`ETag` 不可用時的備援）。
    pub last_modified: Option<String>,
}

/// 一個 WebDAV 端點（伺服器位址＋Basic 認證）。
pub struct WebDav {
    client: Arc<dyn HttpClient>,
    base: String,
    auth: Zeroizing<String>,
}

impl WebDav {
    /// 建立端點；`base` 會正規化為以 `/` 結尾。
    pub fn new(
        client: Arc<dyn HttpClient>,
        base: impl Into<String>,
        account: &str,
        app_password: &str,
    ) -> Self {
        Self {
            client,
            base: normalize_base(&base.into()),
            auth: Zeroizing::new(basic_auth(account, app_password)),
        }
    }

    /// 遠端檔案網址。
    pub fn url_for(&self, file: &str) -> String {
        format!("{}{}", self.base, file)
    }

    /// 測試連線：對目標檔案做一次 `HEAD`。
    ///
    /// `401/403` 代表認證失敗；`404` 代表認證通過但檔案尚未建立（正常）；
    /// `405/501`（不支援 `HEAD`）改以 `GET` 探測；其他非 2xx 視為錯誤。
    pub fn probe(&self, file: &str) -> AppResult<()> {
        let response = self.send(HttpRequest::head(self.url_for(file)))?;
        match response.status {
            200..=299 | 404 | 410 => Ok(()),
            401 | 403 => Err(AppError::WebDavAuth),
            405 | 501 => {
                let response = self.send(HttpRequest::get(self.url_for(file)))?;
                match response.status {
                    200..=299 | 404 | 410 => Ok(()),
                    401 | 403 => Err(AppError::WebDavAuth),
                    other => Err(unexpected_status(other)),
                }
            }
            other => Err(unexpected_status(other)),
        }
    }

    /// 讀取遠端檔案中介資料（不下載主體）。
    pub fn head(&self, file: &str) -> AppResult<RemoteMeta> {
        let response = self.send(HttpRequest::head(self.url_for(file)))?;
        match response.status {
            200..=299 => Ok(meta_from(&response, true)),
            404 | 410 => Ok(RemoteMeta::default()),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(unexpected_status(other)),
        }
    }

    /// 下載遠端檔案；不存在時回 `None`。
    pub fn get(&self, file: &str) -> AppResult<Option<(Vec<u8>, RemoteMeta)>> {
        let response = self.send(HttpRequest::get(self.url_for(file)))?;
        match response.status {
            200..=299 => Ok(Some((response.body.clone(), meta_from(&response, true)))),
            404 | 410 => Ok(None),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(unexpected_status(other)),
        }
    }

    /// 上傳（覆寫）遠端檔案。
    ///
    /// `if_match` 為 `Some` 時附上 `If-Match`，讓「檢查→上傳」成為條件請求：
    /// 遠端已被其他裝置改動時伺服器會回 `412`，映射為 [`AppError::WebDavConflict`]。
    pub fn put(&self, file: &str, bytes: &[u8], if_match: Option<&str>) -> AppResult<RemoteMeta> {
        let mut request = HttpRequest::put(self.url_for(file), bytes.to_vec())
            .header("Content-Type", "application/octet-stream");
        if let Some(etag) = if_match {
            request = request.header("If-Match", etag);
        }
        let response = self.send(request)?;
        match response.status {
            200..=299 => Ok(meta_from(&response, true)),
            401 | 403 => Err(AppError::WebDavAuth),
            412 => Err(AppError::WebDavConflict),
            other => Err(unexpected_status(other)),
        }
    }

    /// 刪除遠端檔案（不存在視為刪除成功）。
    pub fn delete(&self, file: &str) -> AppResult<()> {
        let response = self.send(HttpRequest::delete(self.url_for(file)))?;
        match response.status {
            200..=299 | 404 | 410 => Ok(()),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(unexpected_status(other)),
        }
    }

    /// 送出請求：一律附上 `Authorization`，且不跟隨重定向（避免被帶到其他主機）。
    fn send(&self, request: HttpRequest) -> AppResult<HttpResponse> {
        self.client.send(
            request
                .header("Authorization", self.auth.as_str())
                .no_redirect(),
        )
    }
}

impl std::fmt::Debug for WebDav {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebDav")
            .field("base", &self.base)
            .field("auth", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// 非預期的 HTTP 狀態碼。
fn unexpected_status(status: u16) -> AppError {
    AppError::webdav(format!("服务器返回异常状态码：{status}"))
}

/// 確保伺服器位址以 `/` 結尾。
fn normalize_base(base: &str) -> String {
    let trimmed = base.trim();
    if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    }
}

/// 組出 HTTP Basic 認證標頭值。
fn basic_auth(account: &str, app_password: &str) -> String {
    let raw = Zeroizing::new(format!("{account}:{app_password}"));
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
    format!("Basic {encoded}")
}

/// 由回應標頭取出中介資料。
fn meta_from(response: &HttpResponse, exists: bool) -> RemoteMeta {
    RemoteMeta {
        exists,
        etag: response.header("etag").map(normalize_etag),
        last_modified: response.header("last-modified").map(str::to_owned),
    }
}

/// 去除 `ETag` 的引號與弱驗證前綴（`W/`）。
fn normalize_etag(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_weak = trimmed.strip_prefix("W/").unwrap_or(trimmed);
    without_weak.trim_matches('"').to_owned()
}

#[cfg(test)]
#[path = "tests/webdav_test.rs"]
mod webdav_test;
