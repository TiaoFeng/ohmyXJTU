//! `WebDav` 客戶端的請求組裝與狀態碼分流測試。

use std::sync::Arc;

use base64::Engine as _;

use super::{RemoteMeta, WebDav, normalize_etag};
use crate::error::AppError;
use crate::http::fake::FakeClient;
use crate::http::{HttpResponse, Method};

/// 建立帶標頭的回應（`HttpResponse::new` 不支援標頭）。
fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        final_url: "https://dav.example/dav/file".to_owned(),
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.to_vec(),
    }
}

fn endpoint(responses: Vec<HttpResponse>) -> (Arc<FakeClient>, WebDav) {
    let client = Arc::new(FakeClient::new(responses));
    let dav = WebDav::new(
        client.clone(),
        "https://dav.jianguoyun.com/dav",
        "user@example.com",
        "app-pass",
    );
    (client, dav)
}

/// 伺服器位址會正規化為以 `/` 結尾，再接上子目錄與檔名。
#[test]
fn url_for_normalizes_base_and_appends_the_subfolder() {
    let (_client, dav) = endpoint(vec![]);
    assert_eq!(
        dav.url_for("ohmyXJTU-tasks.vault"),
        "https://dav.jianguoyun.com/dav/ohmyXJTU/ohmyXJTU-tasks.vault"
    );
}

/// 每個請求都帶 Basic 認證，且不跟隨重定向。
#[test]
fn requests_carry_basic_auth_without_following_redirects() {
    let (client, dav) = endpoint(vec![response(200, &[], b"")]);
    dav.head("f.vault").expect("HEAD 应成功");

    let request = client.last_request().expect("应有请求");
    assert_eq!(request.method, Method::Head);
    assert!(!request.follow_redirects, "不得跟随重定向");
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(b"user@example.com:app-pass")
    );
    assert_eq!(
        request.header_value("Authorization"),
        Some(expected.as_str())
    );
}

/// `HEAD` 200 帶 `ETag`：存在且正規化後的中介資料正確。
#[test]
fn head_reports_existence_and_normalized_etag() {
    let (_client, dav) = endpoint(vec![response(200, &[("ETag", "\"abc123\"")], b"")]);
    let meta = dav.head("f.vault").expect("HEAD 应成功");
    assert_eq!(
        meta,
        RemoteMeta {
            exists: true,
            etag: Some("abc123".to_owned()),
            last_modified: None,
        }
    );
}

/// `HEAD` 404：檔案不存在。
#[test]
fn head_missing_file_is_reported_as_absent() {
    let (_client, dav) = endpoint(vec![response(404, &[], b"")]);
    let meta = dav.head("f.vault").expect("404 不该是错误");
    assert!(!meta.exists);
}

/// `HEAD` 401：認證失敗。
#[test]
fn auth_failure_is_surfaced() {
    let (_client, dav) = endpoint(vec![response(401, &[], b"")]);
    assert!(matches!(dav.head("f.vault"), Err(AppError::WebDavAuth)));
}

/// `GET` 成功回傳主體與中介資料；404 回 `None`。
#[test]
fn get_returns_body_or_none() {
    let (_client, dav) = endpoint(vec![
        response(200, &[("ETag", "v1")], b"ciphertext"),
        response(404, &[], b""),
    ]);
    let (body, meta) = dav.get("f.vault").expect("GET 应成功").expect("应有内容");
    assert_eq!(body, b"ciphertext");
    assert_eq!(meta.etag.as_deref(), Some("v1"));
    assert!(dav.get("f.vault").expect("GET 应成功").is_none());
}

/// `PUT` 送出原始位元組與 `If-Match`，且標上 content-type。
#[test]
fn put_sends_bytes_and_conditional_header() {
    let (client, dav) = endpoint(vec![response(201, &[("ETag", "v2")], b"")]);
    let meta = dav
        .put("f.vault", b"encrypted-bytes", Some("v1"))
        .expect("PUT 应成功");
    assert_eq!(meta.etag.as_deref(), Some("v2"));

    let request = client.last_request().expect("应有请求");
    assert_eq!(request.method, Method::Put);
    assert_eq!(request.header_value("If-Match"), Some("v1"));
    assert_eq!(
        request.header_value("Content-Type"),
        Some("application/octet-stream")
    );
    assert!(matches!(
        request.body,
        Some(crate::http::Body::Bytes(ref bytes)) if bytes == b"encrypted-bytes"
    ));
}

/// `PUT` 收到 412：映射為衝突錯誤。
#[test]
fn put_precondition_failure_is_a_conflict() {
    let (_client, dav) = endpoint(vec![response(412, &[], b"")]);
    assert!(matches!(
        dav.put("f.vault", b"x", Some("stale")),
        Err(AppError::WebDavConflict)
    ));
}

/// `ETag` 正規化去掉引號與弱驗證前綴。
#[test]
fn normalize_etag_strips_quotes_and_weak_prefix() {
    assert_eq!(normalize_etag("\"abc123\""), "abc123");
    assert_eq!(normalize_etag("W/\"abc123\""), "abc123");
    assert_eq!(normalize_etag("abc123"), "abc123");
}

/// 連線測試（`check`）：先 `PUT` 探測檔再 `DELETE`。
#[test]
fn check_uploads_and_removes_a_probe_file() {
    let (client, dav) = endpoint(vec![response(201, &[], b""), response(204, &[], b"")]);
    dav.check().expect("可写入时应通过");
    let methods: Vec<Method> = client.requests().iter().map(|r| r.method).collect();
    assert_eq!(methods, vec![Method::Put, Method::Delete]);
}

/// 連線測試遇到持續 404（路徑不可寫）時应报错，并指出方法。
#[test]
fn check_reports_an_unwritable_path() {
    // PUT 404 → MKCOL 也 404：回报建立目錄失敗。
    let (_client, dav) = endpoint(vec![response(404, &[], b""), response(404, &[], b"")]);
    let err = dav.check().expect_err("不可写入时应报错");
    let text = err.to_string();
    assert!(text.contains("MKCOL"), "应指出方法：{text}");
    assert!(text.contains("404"), "{text}");
}

/// `PUT` 遇到 `404` 時先以 `MKCOL` 建立子目錄，再重試一次。
#[test]
fn put_creates_the_folder_then_retries() {
    let (client, dav) = endpoint(vec![
        response(404, &[], b""),
        response(201, &[], b""),
        response(201, &[("ETag", "v1")], b""),
    ]);
    let meta = dav.put("f.vault", b"x", None).expect("建立目录后应成功");
    assert_eq!(meta.etag.as_deref(), Some("v1"));
    let methods: Vec<Method> = client.requests().iter().map(|r| r.method).collect();
    assert_eq!(methods, vec![Method::Put, Method::Mkcol, Method::Put]);
}

/// `PUT` 遇到 `409`（上層集合不存在）同樣先 `MKCOL` 再重試。
#[test]
fn put_creates_the_folder_when_the_parent_is_missing() {
    let (client, dav) = endpoint(vec![
        response(409, &[], b""),
        response(201, &[], b""),
        response(201, &[], b""),
    ]);
    dav.put("f.vault", b"x", None).expect("建立目录后应成功");
    let methods: Vec<Method> = client.requests().iter().map(|r| r.method).collect();
    assert_eq!(methods, vec![Method::Put, Method::Mkcol, Method::Put]);
}

/// `MKCOL` 回 `405`（目錄已存在）视为成功。
#[test]
fn ensure_folder_treats_already_exists_as_success() {
    let (_client, dav) = endpoint(vec![response(405, &[], b"")]);
    dav.ensure_folder().expect("405 应视为已存在");
}

/// `Debug` 不得洩漏應用密碼或其 base64。
#[test]
fn debug_does_not_leak_credentials() {
    let (_client, dav) = endpoint(vec![]);
    let text = format!("{dav:?}");
    assert!(text.contains("dav.jianguoyun.com"), "{text}");
    assert!(!text.contains("app-pass"), "{text}");
    let encoded = base64::engine::general_purpose::STANDARD.encode(b"user@example.com:app-pass");
    assert!(!text.contains(&encoded), "{text}");
}
