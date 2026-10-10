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
use crate::http::{HttpClient, HttpRequest, HttpResponse, redacted_url};

/// 堅果雲 WebDAV 的預設伺服器位址。
pub const DEFAULT_BASE: &str = "https://dav.jianguoyun.com/dav/";

/// 連線測試用的探測檔名（與同步檔案不衝突）。
const PROBE_FILE: &str = "ohmyXJTU-probe.tmp";

/// 單一 WebDAV 回應的大小上限（8 MiB）。
///
/// 同步的容器（加密後的憑證／任務檔）只有數 KB；設上限是為了避免來歷不明或被
/// 入侵的 WebDAV 伺服器回傳超大內容，耗盡記憶體或塞爆磁碟。**每個**請求都套用
/// 這個上限（見 [`WebDav::send`]）：`PUT`／`DELETE`／`HEAD`／`MKCOL` 的回應本文
/// 雖然用不到，仍然會被客戶端讀進記憶體。
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// 遠端子目錄名：所有同步檔案都放在 `<伺服器位址>/ohmyXJTU/` 之下。
///
/// 堅果雲（及多數 WebDAV 服務）不接受直接在帳號根目錄建立檔案（實測 `PUT`
/// 回 `404`）；必須寫入已存在的集合。因此固定使用這個子目錄，並在需要時以
/// `MKCOL` 建立。
const FOLDER: &str = "ohmyXJTU";

/// 遠端檔案的中介資料。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteMeta {
    /// 檔案是否存在（`404` 視為不存在）。
    pub exists: bool,
    /// `ETag`（正規化：去引號與弱驗證前綴；伺服器有回應才為 `Some`）。
    pub etag: Option<String>,
    /// `Last-Modified`（`ETag` 不可用時的備援）。
    pub last_modified: Option<String>,
    /// 可用於 `If-Match` 的原始 `ETag`（保留引號）；遠端未提供 ETag 或僅有弱驗證
    /// 標籤（`W/`）時為 `None`——弱驗證標籤不可用作 `If-Match` 條件。
    pub if_match: Option<String>,
}

/// `PUT` 的前置條件。
///
/// 讓「檢查→上傳」成為條件請求，避免在檢查與上傳之間被其他裝置改動而覆蓋：
///
/// - [`Precondition::Any`]：無條件覆寫（遠端無 `ETag` 可用，或使用者主動強制上傳）。
/// - [`Precondition::MustNotExist`]：要求遠端**不存在**（`If-None-Match: *`）。
///   用於首次建立：即使伺服器支援 `ETag`，兩台裝置同時首次同步仍可能都看到
///   `404`；帶上這個條件後，後到者會收到 `412`（衝突），而非默默覆蓋先到者。
/// - [`Precondition::Match`]：要求遠端版本等於上次記錄的 `ETag`（`If-Match`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition<'a> {
    /// 無條件覆寫。
    Any,
    /// 要求遠端不存在（僅在建立時使用）。
    MustNotExist,
    /// 要求遠端版本等於指定的 `ETag`。
    Match(&'a str),
}

impl<'a> Precondition<'a> {
    /// 由上次記錄的 `ETag` 決定：有值用 [`Precondition::Match`]，否則無條件。
    pub fn from_etag(etag: Option<&'a str>) -> Self {
        match etag {
            Some(tag) => Self::Match(tag),
            None => Self::Any,
        }
    }
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

    /// 遠端檔案網址（位於 `<伺服器位址>/ohmyXJTU/` 之下）。
    pub fn url_for(&self, file: &str) -> String {
        format!("{}{}/{}", self.base, FOLDER, file)
    }

    /// 測試連線與寫入權限：上傳一個小探測檔後立即刪除。
    ///
    /// `HEAD` 只能證明「路徑可達」，無法保證可寫入；同步需要寫入權限，因此以
    /// 一次真實的 `PUT`＋`DELETE` 驗證，並在失敗時回報方法與目標網址。刪除失敗
    /// 不影響「可寫」的結論。
    pub fn check(&self) -> AppResult<()> {
        self.put(PROBE_FILE, b"ohmyXJTU probe", Precondition::Any)?;
        let _ = self.delete(PROBE_FILE);
        Ok(())
    }

    /// 確保遠端子目錄存在：`MKCOL`；已存在時伺服器回 `405`，視為成功。
    pub fn ensure_folder(&self) -> AppResult<()> {
        let mut url = self.base.clone();
        url.push_str(FOLDER);
        url.push('/');
        let response = self.send(HttpRequest::mkcol(url.clone()))?;
        match response.status {
            200..=299 | 405 => Ok(()),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(AppError::webdav(format!(
                "无法建立远端目录（MKCOL {} → {other}）：请确认服务器地址与账户权限（坚果云默认为 https://dav.jianguoyun.com/dav/）",
                redacted_url(&url)
            ))),
        }
    }

    /// 讀取遠端檔案中介資料（不下載主體）。
    pub fn head(&self, file: &str) -> AppResult<RemoteMeta> {
        let url = self.url_for(file);
        let response = self.send(HttpRequest::head(url.clone()))?;
        match response.status {
            200..=299 => Ok(meta_from(&response, true)),
            404 | 409 | 410 => Ok(RemoteMeta::default()),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(unexpected_status("HEAD", &url, other)),
        }
    }

    /// 下載遠端檔案；不存在時回 `None`。
    pub fn get(&self, file: &str) -> AppResult<Option<(Vec<u8>, RemoteMeta)>> {
        let url = self.url_for(file);
        let response = self.send(HttpRequest::get(url.clone()))?;
        match response.status {
            200..=299 => Ok(Some((response.body.clone(), meta_from(&response, true)))),
            404 | 409 | 410 => Ok(None),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(unexpected_status("GET", &url, other)),
        }
    }

    /// 上傳（覆寫）遠端檔案。
    ///
    /// `precondition` 決定是否附上條件標頭，讓「檢查→上傳」成為條件請求：
    /// 遠端已被其他裝置改動（或已存在，而要求的是「必須不存在」）時，伺服器
    /// 會回 `412`，映射為 [`AppError::WebDavConflict`]。
    ///
    /// 首次遇到 `404` 或 `409`（上層集合不存在）時先以 `MKCOL` 建立子目錄再重試
    /// 一次；這是堅果雲等服務的常見要求（不接受直接寫入根目錄，且 `PUT` 到不存
    /// 在集合下會回 `409`）。
    pub fn put(
        &self,
        file: &str,
        bytes: &[u8],
        precondition: Precondition<'_>,
    ) -> AppResult<RemoteMeta> {
        let url = self.url_for(file);
        let response = self.send_put(&url, bytes, precondition)?;
        if matches!(response.status, 404 | 409) {
            self.ensure_folder()?;
            let retried = self.send_put(&url, bytes, precondition)?;
            return self.finish_put(&url, retried);
        }
        self.finish_put(&url, response)
    }

    /// 送出一則 `PUT` 並取回回應。
    fn send_put(
        &self,
        url: &str,
        bytes: &[u8],
        precondition: Precondition<'_>,
    ) -> AppResult<HttpResponse> {
        let mut request = HttpRequest::put(url.to_owned(), bytes.to_vec())
            .header("Content-Type", "application/octet-stream");
        request = match precondition {
            Precondition::Any => request,
            Precondition::MustNotExist => request.header("If-None-Match", "*"),
            Precondition::Match(etag) => request.header("If-Match", etag),
        };
        self.send(request)
    }

    /// 解讀 `PUT` 回應。
    fn finish_put(&self, url: &str, response: HttpResponse) -> AppResult<RemoteMeta> {
        let status = response.status;
        match status {
            200..=299 => Ok(meta_from(&response, true)),
            401 | 403 => Err(AppError::WebDavAuth),
            412 => Err(AppError::WebDavConflict),
            404 | 409 => Err(AppError::webdav(format!(
                "服务器拒绝写入（PUT {} → {status}）：请确认服务器地址正确、目录可写，且应用密码未限制目录（坚果云默认为 https://dav.jianguoyun.com/dav/）",
                redacted_url(url)
            ))),
            other => Err(unexpected_status("PUT", url, other)),
        }
    }

    /// 刪除遠端檔案（不存在视为刪除成功）。
    pub fn delete(&self, file: &str) -> AppResult<()> {
        let url = self.url_for(file);
        let response = self.send(HttpRequest::delete(url.clone()))?;
        match response.status {
            200..=299 | 404 | 410 => Ok(()),
            401 | 403 => Err(AppError::WebDavAuth),
            other => Err(unexpected_status("DELETE", &url, other)),
        }
    }

    /// 送出請求：一律附上 `Authorization`、限制回應大小，且不跟隨重定向
    /// （避免被帶到其他主機）。
    ///
    /// 回應本文只有 `GET` 用得到，其餘方法只讀狀態碼與標頭；但**所有**請求都必須
    /// 有大小上限，否則惡意／被入侵的伺服器只要在 `PUT`／`DELETE`／`HEAD` 的回應
    /// 塞入超大本文，就能耗盡本程序記憶體。呼叫端已指定上限時不覆寫。
    fn send(&self, mut request: HttpRequest) -> AppResult<HttpResponse> {
        if request.max_body.is_none() {
            request = request.limit_body(MAX_RESPONSE_BYTES);
        }
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

/// 非預期的 HTTP 狀態碼（附方法與網址，方便診斷；網址去 userinfo 與查詢串）。
///
/// 網址由使用者填寫，可能夾帶 userinfo 或查詢參數憑證，因此一律經
/// [`redacted_url`] 去識別化，與其他網路錯誤訊息一致。
fn unexpected_status(method: &str, url: &str, status: u16) -> AppError {
    AppError::webdav(format!(
        "服务器返回异常状态码 {status}（{method} {}）",
        redacted_url(url)
    ))
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
    // base64 中介值同樣含憑證，一併零化，不留下可讀的堆積副本。
    let encoded = Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()));
    format!("Basic {}", encoded.as_str())
}

/// 由回應標頭取出中介資料。
fn meta_from(response: &HttpResponse, exists: bool) -> RemoteMeta {
    RemoteMeta {
        exists,
        etag: response.header("etag").map(normalize_etag),
        last_modified: response.header("last-modified").map(str::to_owned),
        if_match: if_match_tag(response),
    }
}

/// 取出可用於 `If-Match` 的原始 `ETag`（保留引號；弱驗證標籤不可用）。
fn if_match_tag(response: &HttpResponse) -> Option<String> {
    let raw = response.header("etag")?.trim();
    (!raw.starts_with("W/")).then(|| raw.to_owned())
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
