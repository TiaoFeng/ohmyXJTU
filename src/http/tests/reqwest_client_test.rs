//! `ReqwestClient` 的真實位元組層回歸測試。
//!
//! 假客戶端回放的是「已解析」的回應，無法覆蓋 Hyper 的 HTTP/1 位元組解析路徑。
//! 這裡以標準庫 TCP 模擬伺服器回放原始位元組，重現考勤入口的舊式多行回應標頭
//! （`Content-Security-Policy` 以裸 LF 折行）等情境。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use super::ReqwestClient;
use crate::error::{AppError, NetworkKind};
use crate::http::{HttpClient, HttpRequest};

/// 啟動最小 HTTP 伺服器。
///
/// `responder` 依連線序號（從 0 起）與伺服器 base URL 產生原始回應位元組；
/// 處理滿 `connections` 個連線後結束。回應應帶 `Connection: close`，
/// 避免 keep-alive 讓後續請求重用同一條連線。
fn serve(
    connections: usize,
    responder: impl Fn(usize, &str) -> Vec<u8> + Send + 'static,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let addr = listener.local_addr().expect("取得本地地址");
    let base = format!("http://{addr}");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);

    let server_base = base.clone();
    thread::spawn(move || {
        for index in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer);
            let response = responder(index, &server_base);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });

    (base, hits)
}

/// 考勤入口 302 回應的原始形態：`Content-Security-Policy` 以裸 LF 折行。
fn multiline_csp_redirect(location: &str) -> Vec<u8> {
    let mut response = String::new();
    response.push_str("HTTP/1.1 302 Found\r\n");
    response.push_str("Server: nginx\r\n");
    response.push_str("Content-Length: 0\r\n");
    response.push_str(&format!("Location: {location}\r\n"));
    response.push_str("Content-Security-Policy: \n");
    response.push_str("        default-src 'self';\n");
    response.push_str("        script-src 'self' 'unsafe-inline';\n");
    response.push_str("        img-src 'self' data:;\n");
    response.push_str("    \r\n");
    response.push_str("Connection: close\r\n");
    response.push_str("\r\n");
    response.into_bytes()
}

/// 一般 JSON 200 回應。
fn ok_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn client() -> ReqwestClient {
    ReqwestClient::new("ohmyXJTU-test").expect("建立 HTTP 客户端")
}

/// 多行標頭不應再讓請求失敗；重定向仍被正確跟隨。
#[test]
fn follows_redirect_with_obsolete_multiline_headers() {
    let (base, hits) = serve(2, |index, base| {
        if index == 0 {
            multiline_csp_redirect(&format!("{base}/ok"))
        } else {
            ok_response("{}")
        }
    });

    let response = client()
        .send(HttpRequest::get(format!("{base}/portal")))
        .expect("多行標頭的 302 應可解析並跟隨");

    assert_eq!(response.status, 200);
    assert!(response.final_url.ends_with("/ok"));
    assert_eq!(hits.load(Ordering::SeqCst), 2, "應跟隨一次重定向");
}

/// `no_redirect` 分支：不跟隨，但仍能讀到 302、Location 與多行 CSP。
#[test]
fn reads_multiline_headers_without_following_redirect() {
    let (base, _hits) = serve(1, |_index, _base| {
        multiline_csp_redirect("https://login.xjtu.edu.cn/cas/login?service=x")
    });

    let response = client()
        .send(HttpRequest::get(format!("{base}/portal")).no_redirect())
        .expect("多行標頭的 302 應可解析");

    assert_eq!(response.status, 302);
    assert_eq!(
        response.header("Location"),
        Some("https://login.xjtu.edu.cn/cas/login?service=x")
    );
    let csp = response
        .header("Content-Security-Policy")
        .expect("應保留 CSP 標頭");
    assert!(csp.contains("default-src"), "CSP 內容：{csp}");
    assert!(csp.contains("img-src"), "CSP 內容：{csp}");
}

/// 一般（無多行標頭）的回應不受影響。
#[test]
fn normal_responses_are_unaffected() {
    let (base, _hits) = serve(1, |_index, _base| ok_response(r#"{"ok":true}"#));

    let response = client()
        .send(HttpRequest::get(format!("{base}/api")))
        .expect("一般回應應可解析");

    assert_eq!(response.status, 200);
    let value: serde_json::Value = response.json().expect("解析 JSON");
    assert_eq!(value["ok"], serde_json::Value::Bool(true));
}

/// 真正不合法的標頭仍應報「響應頭解析失敗」，且訊息不得包含網址。
#[test]
fn classifies_malformed_header_response() {
    let (base, _hits) = serve(1, |_index, _base| {
        // 標頭名含控制位元組，httparse 無法接受。
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-Bad\x01Name: v\r\nConnection: close\r\n\r\n{}"
            .to_vec()
    });

    let err = client()
        .send(HttpRequest::get(format!("{base}/api")))
        .expect_err("無效標頭應回報錯誤");

    match &err {
        AppError::Network { kind, detail } => {
            assert_eq!(*kind, NetworkKind::HttpParse, "錯誤鏈：{detail}");
            assert!(!detail.contains("://"), "摘要不應含網址：{detail}");
        }
        other => panic!("應為網路錯誤，實際：{other:?}"),
    }
    assert!(err.to_string().contains("响应头解析失败"), "訊息：{err}");
    assert!(!err.to_string().contains("://"), "訊息不應含網址：{err}");
}

/// 逾時仍歸類為 `Timeout`。
#[test]
fn classifies_timeout() {
    let (base, _hits) = serve(1, |_index, _base| {
        thread::sleep(Duration::from_millis(800));
        ok_response("{}")
    });

    let err = client()
        .send(HttpRequest::get(format!("{base}/slow")).timeout(Duration::from_millis(150)))
        .expect_err("應回報逾時");

    match &err {
        AppError::Network { kind, .. } => assert_eq!(*kind, NetworkKind::Timeout),
        other => panic!("應為網路錯誤，實際：{other:?}"),
    }
    assert!(err.to_string().contains("请求超时"));
}

/// 分類矩陣：以真實世界的錯誤鏈文字驗證純函式（代理型環境不會產生可重現的 DNS 錯誤）。
#[test]
fn classifies_real_world_error_chains() {
    use super::classify_chain;

    // 域名解析失敗。
    assert_eq!(
        classify_chain(
            "error sending request for url <url>\nclient error (connect)\ndns error: failed to lookup address information: name or service not known\n",
            false,
            false,
            true,
        ),
        NetworkKind::Dns
    );
    // 連線在回應完成前被關閉（沙箱代理等環境的常見形態）。
    assert_eq!(
        classify_chain(
            "error sending request for url <url>\nclient error (sendrequest)\nconnection closed before message completed\n",
            false,
            false,
            false,
        ),
        NetworkKind::Connect
    );
    // 憑證錯誤。
    assert_eq!(
        classify_chain(
            "invalid peer certificate: unknownissuer",
            false,
            false,
            true
        ),
        NetworkKind::Tls
    );
    // 標頭解析失敗。
    assert_eq!(
        classify_chain("invalid http header parsed", false, false, false),
        NetworkKind::HttpParse
    );
    // 逾時與重定向旗標優先於文字比對。
    assert_eq!(
        classify_chain("connection refused", true, false, true),
        NetworkKind::Timeout
    );
    assert_eq!(
        classify_chain("redirect policy", false, true, false),
        NetworkKind::Redirect
    );
    // 無特徵文字且無標記。
    assert_eq!(
        classify_chain("error sending request for url <url>", false, false, false),
        NetworkKind::Other
    );
}

/// 無法解析的域名在任何環境都應是網路錯誤，且訊息不含網址。
#[test]
fn reports_unresolvable_host_as_network_error() {
    let err = client()
        .send(HttpRequest::get("http://ohmyxjtu-no-such-host.invalid/"))
        .expect_err("無法解析的域名應回報錯誤");

    match &err {
        AppError::Network { kind, detail } => {
            assert!(
                !matches!(
                    kind,
                    NetworkKind::Timeout | NetworkKind::HttpParse | NetworkKind::Redirect
                ),
                "類別：{kind:?}｜錯誤鏈：{detail}"
            );
        }
        other => panic!("應為網路錯誤，實際：{other:?}"),
    }
    assert!(!err.to_string().contains("://"), "訊息不應含網址：{err}");
}

/// 連線被拒歸類為 `Connect`。
#[test]
fn classifies_connection_refused() {
    // 取得一個已釋放的本地端口（連線必被拒絕）。
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let addr = listener.local_addr().expect("取得本地地址");
    drop(listener);

    let err = client()
        .send(HttpRequest::get(format!("http://{addr}/")))
        .expect_err("連線被拒應回報錯誤");

    match &err {
        AppError::Network { kind, .. } => assert_eq!(*kind, NetworkKind::Connect),
        other => panic!("應為網路錯誤，實際：{other:?}"),
    }
}
