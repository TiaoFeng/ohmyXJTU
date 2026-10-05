//! `ReqwestClient` 的真實位元組層回歸測試。
//!
//! 假客戶端回放的是「已解析」的回應，無法覆蓋 Hyper 的 HTTP/1 位元組解析路徑。
//! 這裡以標準庫 TCP 模擬伺服器回放原始位元組，重現考勤入口的舊式多行回應標頭
//! （`Content-Security-Policy` 以裸 LF 折行）等情境。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use url::Url;

use super::ReqwestClient;
use crate::error::{AppError, NetworkKind};
use crate::http::{HttpClient, HttpRequest};
use crate::webvpn;

/// 啟動最小 HTTP 伺服器。
///
/// `responder` 依連線序號（從 0 起）與伺服器 base URL 產生原始回應位元組；
/// 處理滿 `connections` 個連線後結束。回應應帶 `Connection: close`，
/// 避免 keep-alive 讓後續請求重用同一條連線。
fn serve(
    connections: usize,
    responder: impl Fn(usize, &str) -> Vec<u8> + Send + 'static,
) -> (String, Arc<AtomicUsize>) {
    let (base, hits, _requests) = serve_recording(connections, responder);
    (base, hits)
}

/// 與 [`serve`] 相同，但另外記錄每個連線收到的原始請求文字。
fn serve_recording(
    connections: usize,
    responder: impl Fn(usize, &str) -> Vec<u8> + Send + 'static,
) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let addr = listener.local_addr().expect("取得本地地址");
    let base = format!("http://{addr}");
    let hits = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::clone(&hits);
    let requests = Arc::clone(&recorded);

    let server_base = base.clone();
    thread::spawn(move || {
        for index in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or_default();
            requests
                .lock()
                .expect("请求记录锁")
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            let response = responder(index, &server_base);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });

    (base, hits, recorded)
}

/// 指定狀態碼與 `Location` 的重定向回應。
fn redirect_response(status: u16, location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} Moved\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
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

/// 不檢查重定向目的地的客戶端（本機假伺服器用；信任規則由純函式測試涵蓋）。
fn insecure_client() -> ReqwestClient {
    ReqwestClient::new_insecure_for_tests("ohmyXJTU-test").expect("建立 HTTP 客户端")
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

    let response = insecure_client()
        .send(HttpRequest::get(format!("{base}/portal")))
        .expect("多行标头的 302 应可解析并跟随");

    assert_eq!(response.status, 200);
    assert!(response.final_url.ends_with("/ok"));
    assert_eq!(hits.load(Ordering::SeqCst), 2, "应跟随一次重定向");
}

/// `no_redirect` 分支：不跟隨，但仍能讀到 302、Location 與多行 CSP。
#[test]
fn reads_multiline_headers_without_following_redirect() {
    let (base, _hits) = serve(1, |_index, _base| {
        multiline_csp_redirect("https://login.xjtu.edu.cn/cas/login?service=x")
    });

    let response = client()
        .send(HttpRequest::get(format!("{base}/portal")).no_redirect())
        .expect("多行标头的 302 应可解析");

    assert_eq!(response.status, 302);
    assert_eq!(
        response.header("Location"),
        Some("https://login.xjtu.edu.cn/cas/login?service=x")
    );
    let csp = response
        .header("Content-Security-Policy")
        .expect("应保留 CSP 标头");
    assert!(csp.contains("default-src"), "CSP 内容：{csp}");
    assert!(csp.contains("img-src"), "CSP 内容：{csp}");
}

/// 一般（無多行標頭）的回應不受影響。
#[test]
fn normal_responses_are_unaffected() {
    let (base, _hits) = serve(1, |_index, _base| ok_response(r#"{"ok":true}"#));

    let response = client()
        .send(HttpRequest::get(format!("{base}/api")))
        .expect("一般响应应可解析");

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
        .expect_err("无效标头应回报错误");

    match &err {
        AppError::Network { kind, detail } => {
            assert_eq!(*kind, NetworkKind::HttpParse, "错误链：{detail}");
            assert!(!detail.contains("://"), "摘要不应含网址：{detail}");
        }
        other => panic!("应为网络错误，实际：{other:?}"),
    }
    assert!(err.to_string().contains("响应头解析失败"), "信息：{err}");
    assert!(!err.to_string().contains("://"), "信息不应含网址：{err}");
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
        .expect_err("应回报超时");

    match &err {
        AppError::Network { kind, .. } => assert_eq!(*kind, NetworkKind::Timeout),
        other => panic!("应为网络错误，实际：{other:?}"),
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
        .expect_err("无法解析的域名应回报错误");

    match &err {
        AppError::Network { kind, detail } => {
            assert!(
                !matches!(
                    kind,
                    NetworkKind::Timeout | NetworkKind::HttpParse | NetworkKind::Redirect
                ),
                "类别：{kind:?}｜错误链：{detail}"
            );
        }
        other => panic!("应为网络错误，实际：{other:?}"),
    }
    assert!(!err.to_string().contains("://"), "信息不应含网址：{err}");
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
        .expect_err("连接被拒应回报错误");

    match &err {
        AppError::Network { kind, .. } => assert_eq!(*kind, NetworkKind::Connect),
        other => panic!("应为网络错误，实际：{other:?}"),
    }
}

/// 同源重定向：自訂標頭保留。
#[test]
fn same_origin_redirect_keeps_custom_headers() {
    let (base, hits, requests) = serve_recording(2, |index, base| {
        if index == 0 {
            redirect_response(302, &format!("{base}/ok"))
        } else {
            ok_response("{}")
        }
    });

    insecure_client()
        .send(HttpRequest::get(format!("{base}/portal")).header("X-Business-Token", "token-1"))
        .expect("同源重定向应可跟随");

    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let requests = requests.lock().expect("请求记录锁");
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("x-business-token"),
        "同源应保留凭证：{}",
        requests[1]
    );
}

/// 跨主機重定向：跟隨，但不得重送自訂標頭（業務憑證）。
///
/// 同一台伺服器以不同主機名（`127.0.0.1` → `localhost`）代表跨來源，
/// 既不影響可達性，又能驗證逐跳的標頭規則。
#[test]
fn cross_origin_redirect_drops_custom_headers() {
    let (base, hits, requests) = serve_recording(2, |index, base| {
        if index == 0 {
            let port = Url::parse(base)
                .expect("解析服务器地址")
                .port()
                .expect("端口");
            redirect_response(302, &format!("http://localhost:{port}/ok"))
        } else {
            ok_response("{}")
        }
    });

    let response = insecure_client()
        .send(
            HttpRequest::get(format!("{base}/portal"))
                .header("X-Business-Token", "token-1")
                .header("Referer", "https://lms.xjtu.edu.cn/"),
        )
        .expect("跨主机重定向应可跟随");

    assert_eq!(response.status, 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let requests = requests.lock().expect("请求记录锁");
    let first = requests[0].to_ascii_lowercase();
    let second = requests[1].to_ascii_lowercase();
    assert!(
        first.contains("x-business-token"),
        "首跳应带凭证（测试前提）：{first}"
    );
    assert!(
        !second.contains("x-business-token"),
        "跨主机不得重送凭证：{second}"
    );
    assert!(
        !second.contains("referer"),
        "跨主机不得重送来源标头：{second}"
    );
}

/// 跨來源 307：拒絕重送請求主體，且不得再送出第二個請求。
#[test]
fn cross_origin_307_is_rejected_without_resending_the_body() {
    let (base, hits) = serve(1, |_index, base| {
        let port = Url::parse(base)
            .expect("解析服务器地址")
            .port()
            .expect("端口");
        redirect_response(307, &format!("http://localhost:{port}/submit"))
    });

    let err = insecure_client()
        .send(HttpRequest::post_form(
            format!("{base}/submit"),
            [("password", "__RSA__secret")],
        ))
        .expect_err("跨来源 307 应被拒绝");

    match &err {
        AppError::Network { kind, .. } => assert_eq!(*kind, NetworkKind::Redirect),
        other => panic!("应为重定向错误，实际：{other:?}"),
    }
    assert!(
        err.to_string().contains("拒绝把请求主体重送到其他来源"),
        "{err}"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "被拒绝时不得再送出第二个请求"
    );
}

/// 同源 307：保留方法與請求主體。
#[test]
fn same_origin_307_resends_the_body() {
    let (base, hits, requests) = serve_recording(2, |index, base| {
        if index == 0 {
            redirect_response(307, &format!("{base}/next"))
        } else {
            ok_response("{}")
        }
    });

    let response = insecure_client()
        .send(HttpRequest::post_form(
            format!("{base}/submit"),
            [("password", "__RSA__secret")],
        ))
        .expect("同源 307 应重送主体");

    assert_eq!(response.status, 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let requests = requests.lock().expect("请求记录锁");
    assert!(
        requests[1].contains("__RSA__secret"),
        "同源 307 应保留请求主体：{}",
        requests[1]
    );
}

/// 重定向決策矩陣（純函式）：同源保留標頭、跨源剝奪、降級與不可信主機拒絕。
#[test]
fn plan_redirect_keeps_headers_only_for_same_origin() {
    use super::{MAX_REDIRECTS, is_trusted_redirect_host, plan_redirect};

    let school = Url::parse("https://lms.xjtu.edu.cn/api/x").expect("解析");

    // 同源（相對路徑）：自訂標頭可保留。
    let plan = plan_redirect(
        &school,
        "/api/y",
        302,
        crate::http::Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect("同源应可跟随");
    assert_eq!(plan.url, "https://lms.xjtu.edu.cn/api/y");
    assert!(plan.keep_headers, "同源应保留自订标头");

    // 校內跨主機：跟隨但丟棄自訂標頭。
    let plan = plan_redirect(
        &school,
        "https://bk-kq.xjtu.edu.cn/sa",
        302,
        crate::http::Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect("校内跨主机应可跟随");
    assert!(!plan.keep_headers, "跨主机不得保留自订标头");

    // 非學校網域：拒絕。
    let err = plan_redirect(
        &school,
        "https://evil.example/steal",
        302,
        crate::http::Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect_err("校外主机应被拒绝");
    assert!(
        matches!(
            err,
            AppError::Network {
                kind: NetworkKind::Redirect,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("学校网域之外"), "{err}");

    // HTTPS → HTTP 降級：拒絕。
    let err = plan_redirect(
        &school,
        "http://lms.xjtu.edu.cn/api/y",
        302,
        crate::http::Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect_err("降级应被拒绝");
    assert!(err.to_string().contains("降级"), "{err}");

    // 跳數超限：拒絕。
    let err = plan_redirect(
        &school,
        "/api/y",
        302,
        crate::http::Method::Get,
        false,
        MAX_REDIRECTS + 1,
        is_trusted_redirect_host,
    )
    .expect_err("跳数超限应被拒绝");
    assert!(err.to_string().contains("次数过多"), "{err}");
}

/// 重定向的方法與主體規則（純函式）。
#[test]
fn plan_redirect_rules_for_request_bodies() {
    use super::{is_trusted_redirect_host, plan_redirect};

    let login = Url::parse("https://login.xjtu.edu.cn/cas/login").expect("解析");

    // 301/302/303 的 POST：轉為 GET 且不重送主體。
    let plan = plan_redirect(
        &login,
        "/cas/next",
        302,
        crate::http::Method::Post,
        true,
        1,
        is_trusted_redirect_host,
    )
    .expect("302 应可跟随");
    assert_eq!(plan.method, crate::http::Method::Get);
    assert!(!plan.keep_body);

    // 307 同源：保留方法與主體。
    let plan = plan_redirect(
        &login,
        "/cas/next",
        307,
        crate::http::Method::Post,
        true,
        1,
        is_trusted_redirect_host,
    )
    .expect("同源 307 应可跟随");
    assert_eq!(plan.method, crate::http::Method::Post);
    assert!(plan.keep_body);

    // 307 跨來源：拒絕（等同把帳密重送到別的主機）。
    let err = plan_redirect(
        &login,
        "https://bk-kq.xjtu.edu.cn/sa",
        307,
        crate::http::Method::Post,
        true,
        1,
        is_trusted_redirect_host,
    )
    .expect_err("跨来源 307 应被拒绝");
    assert!(
        err.to_string().contains("拒绝把请求主体重送到其他来源"),
        "{err}"
    );
}

/// WebVPN 重導必須同時驗證**代理目標**，不能只看外層閘道網址。
///
/// 外層主機永遠是 `webvpn.xjtu.edu.cn`；只檢查外層會讓「代理到 http:// 或
/// 校外主機」的重導通過，查詢參數（可能含 ticket）也會被一起轉送。
#[test]
fn plan_redirect_validates_the_webvpn_proxy_target() {
    use super::{is_trusted_redirect_host, plan_redirect};
    use crate::http::Method;

    let previous =
        Url::parse(&webvpn::to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa/current").expect("改写"))
            .expect("解析");
    assert_eq!(
        previous.host_str(),
        Some(webvpn::WEBVPN_HOST),
        "测试前提：外层主机是闸道"
    );

    // 代理到校內主機（HTTPS）：允許，且同一代理目標算同源。
    let allowed = webvpn::to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa/next").expect("改写");
    let plan = plan_redirect(
        &previous,
        &allowed,
        302,
        Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect("代理校内主机应可跟随");
    assert!(plan.keep_headers, "同一代理目标应保留自订标头");

    // 代理到校外主機：拒絕。
    let foreign = webvpn::to_webvpn_url("https://evil.example/steal").expect("改写");
    let err = plan_redirect(
        &previous,
        &foreign,
        302,
        Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect_err("代理目标在校外时应拒绝");
    assert!(
        err.to_string().contains("代理目标位于学校网域之外"),
        "{err}"
    );

    // 代理到 http（內層降級）：拒絕。
    let insecure = webvpn::to_webvpn_url("http://bk-kq.xjtu.edu.cn/sa").expect("改写");
    let err = plan_redirect(
        &previous,
        &insecure,
        302,
        Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect_err("代理目标非 HTTPS 时应拒绝");
    assert!(err.to_string().contains("代理目标不是 HTTPS"), "{err}");

    // 代理目標解不開：身分無法確認，拒絕。
    let broken = "https://webvpn.xjtu.edu.cn/https/77726476706e69737468656265737421zzzz/x";
    let err = plan_redirect(
        &previous,
        broken,
        302,
        Method::Get,
        false,
        1,
        is_trusted_redirect_host,
    )
    .expect_err("无法解析的代理目标应拒绝");
    assert!(err.to_string().contains("代理目标无法解析"), "{err}");
}

/// 重定向目的主機的信任判斷。
#[test]
fn trusted_redirect_hosts() {
    use super::is_trusted_redirect_host;

    assert!(is_trusted_redirect_host("lms.xjtu.edu.cn"));
    assert!(is_trusted_redirect_host("xjtu.edu.cn"));
    assert!(is_trusted_redirect_host("webvpn.xjtu.edu.cn"));
    assert!(!is_trusted_redirect_host("xjtu.edu.cn.evil.com"));
    assert!(!is_trusted_redirect_host("evil.com"));
    assert!(!is_trusted_redirect_host("127.0.0.1"));
}

/// WebVPN 代理網址以**代理目標**判定同源，而非外層主機。
#[test]
fn webvpn_proxy_targets_define_the_origin() {
    use super::same_origin;

    let first = webvpn::to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa").expect("改写");
    let same_target = webvpn::to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa/next").expect("改写");
    let other_target = webvpn::to_webvpn_url("https://lms.xjtu.edu.cn/sa").expect("改写");

    let first = Url::parse(&first).expect("解析");
    let same_target = Url::parse(&same_target).expect("解析");
    let other_target = Url::parse(&other_target).expect("解析");

    assert_eq!(
        first.host_str(),
        other_target.host_str(),
        "外层主机相同（测试前提）"
    );
    assert!(same_origin(&first, &same_target), "同一代理目标应视为同源");
    assert!(!same_origin(&first, &other_target), "代理目标不同即为跨源");

    // 代理目標解不開：保守視為跨源。
    let broken = Url::parse("https://webvpn.xjtu.edu.cn/https/zzzz/broken").expect("解析");
    assert!(!same_origin(&broken, &broken), "无法判定时应视为跨源");
}
