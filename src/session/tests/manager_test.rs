//! 會話管理測試：訪問策略解析、WebVPN 改寫與登入態失效偵測。

use std::sync::Arc;

use crate::config::{AccessPolicy, Config};
use crate::error::{AppError, AppResult};
use crate::http::fake::FakeClient;
use crate::http::{HttpClient, HttpRequest, HttpResponse};

use super::super::site::{PostLogin, SiteAdapter, SiteKind, SiteLogin, SitePolicy};
use super::{AccessMode, CAMPUS_PROBE_URL, LoginStage, SessionManager};

/// 考勤站點策略：校外需經 WebVPN。
struct TestAttendanceSite;

impl SiteAdapter for TestAttendanceSite {
    fn kind(&self) -> SiteKind {
        SiteKind::Attendance
    }

    fn policy(&self) -> SitePolicy {
        SitePolicy {
            login_url: "https://bk-kq.xjtu.edu.cn/sa/auth/cas/login/student-pc",
            supports_webvpn: true,
            use_webvpn_when_off_campus: true,
        }
    }

    fn post_login(&self, _context: &PostLogin<'_>) -> AppResult<SiteLogin> {
        Ok(SiteLogin {
            headers: vec![("X-Business-Token".to_owned(), "token-1".to_owned())],
            user_id: None,
        })
    }
}

/// 思源學堂策略：校外仍直連。
struct TestLmsSite;

impl SiteAdapter for TestLmsSite {
    fn kind(&self) -> SiteKind {
        SiteKind::Lms
    }

    fn policy(&self) -> SitePolicy {
        SitePolicy {
            login_url: "https://lms.xjtu.edu.cn",
            supports_webvpn: true,
            use_webvpn_when_off_campus: false,
        }
    }

    fn post_login(&self, _context: &PostLogin<'_>) -> AppResult<SiteLogin> {
        Ok(SiteLogin::default())
    }
}

const LOGIN_PAGE: &str = r#"<html><body>
    <input type="hidden" name="execution" value="e1s1" />
</body></html>"#;

fn manager_with<F>(
    policy: AccessPolicy,
    direct: F,
) -> (SessionManager, Arc<FakeClient>, Arc<FakeClient>)
where
    F: Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
{
    let direct_client = Arc::new(FakeClient::with_responder(direct));
    let webvpn_client = Arc::new(FakeClient::with_responder(|request| {
        if request.url.starts_with("https://webvpn.xjtu.edu.cn/login") {
            Ok(HttpResponse::new(
                200,
                "https://login.xjtu.edu.cn/cas/login?service=webvpn",
                LOGIN_PAGE.as_bytes(),
            ))
        } else {
            // 已改寫的站點請求：正常回應，且不落在統一認證網址上。
            Ok(HttpResponse::new(
                200,
                "https://webvpn.xjtu.edu.cn/https/abc/sa/student/home",
                b"{}".as_slice(),
            ))
        }
    }));

    let config = Config {
        visitor_id: "0".repeat(32),
        access_policy: policy,
        ..Config::default()
    };
    let mut manager = SessionManager::with_clients(
        &config,
        Arc::clone(&direct_client) as Arc<dyn HttpClient>,
        Arc::clone(&webvpn_client) as Arc<dyn HttpClient>,
    );
    manager.register(Box::new(TestAttendanceSite));
    manager.register(Box::new(TestLmsSite));
    (manager, direct_client, webvpn_client)
}

fn ok_response() -> AppResult<HttpResponse> {
    Ok(HttpResponse::new(
        200,
        "https://example.invalid/ok",
        b"{}".as_slice(),
    ))
}

/// 換帳號必須重建後端：舊的 cookie jar 不得被沿用。
///
/// 只清狀態表不足以丟棄舊帳號在服務端留下的 SSO cookie；殘留的登入態會讓
/// 新的登入流程被判定為「已登入」而略過帳密提交。
#[test]
fn reset_session_rebuilds_both_backends_and_clears_state() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let config = Config {
        visitor_id: "0".repeat(32),
        access_policy: AccessPolicy::Direct,
        ..Config::default()
    };

    // 兩個後端共用一個計數器：重建時必須各自換成全新的實例。
    let built = Arc::new(AtomicUsize::new(0));
    let direct_counter = Arc::clone(&built);
    let webvpn_counter = Arc::clone(&built);
    let direct_factory: super::ClientFactory = Arc::new(move || {
        direct_counter.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeClient::with_responder(|_| {
            Ok(HttpResponse::new(
                200,
                "https://lms.xjtu.edu.cn/user/index",
                LOGIN_PAGE.as_bytes(),
            ))
        })) as Arc<dyn HttpClient>)
    });
    let webvpn_factory: super::ClientFactory = Arc::new(move || {
        webvpn_counter.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeClient::with_responder(|_| {
            Ok(HttpResponse::new(
                200,
                "https://lms.xjtu.edu.cn/user/index",
                LOGIN_PAGE.as_bytes(),
            ))
        })) as Arc<dyn HttpClient>)
    });

    let mut manager =
        SessionManager::with_client_factories(&config, direct_factory, webvpn_factory)
            .expect("建立会话管理器");
    assert_eq!(built.load(Ordering::SeqCst), 2, "建立时两个后端各建一次");

    manager.register(Box::new(TestLmsSite));
    manager.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());
    assert!(manager.is_logged_in(SiteKind::Lms));

    manager.reset_session().expect("重建会话");

    assert_eq!(
        built.load(Ordering::SeqCst),
        4,
        "重建时两个后端都必须换成新实例（新的 cookie jar）"
    );
    assert!(
        !manager.is_logged_in(SiteKind::Lms),
        "重建后不得残留旧的站点登录态"
    );
    assert!(manager.access_mode(SiteKind::Lms).is_none());
    assert!(manager.resolved_access_mode(SiteKind::Lms).is_none());

    // 下一次登入必須重新走完整流程（不會被判定為已登入）。
    let stage = manager
        .next_login_step(SiteKind::Lms)
        .expect("取得登录步骤");
    assert!(
        matches!(stage, LoginStage::Drive(_)),
        "重建后应重新驱动登录流程"
    );
}

/// 切換訪問模式必須作廢進行中的登入步驟。
///
/// 登入步驟是配着當時解析出來的訪問方式與後端建立的；切換策略後路線與後端
/// 都可能不同，續用舊驅動器完成登入會把「舊後端的 cookie」與「新的訪問方式」
/// 湊在一起（比對 `reset_state` 有清 `pending`、而這裡原本沒清）。
#[test]
fn changing_access_policy_drops_the_in_flight_login_step() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |_| {
        Ok(HttpResponse::new(
            200,
            "https://lms.xjtu.edu.cn/user/index",
            LOGIN_PAGE.as_bytes(),
        ))
    });

    let LoginStage::Drive(driver) = manager
        .next_login_step(SiteKind::Lms)
        .expect("取得登录步骤")
    else {
        panic!("尚未登录时应有下一步");
    };

    manager.set_access_policy(AccessPolicy::WebVpn);

    assert!(
        manager.complete_login_step(SiteKind::Lms, &driver).is_err(),
        "切换访问模式后不得沿用旧的登录步骤"
    );
    assert!(!manager.is_logged_in(SiteKind::Lms));
    assert!(
        matches!(
            manager
                .next_login_step(SiteKind::Lms)
                .expect("重新取得登录步骤"),
            LoginStage::Drive(_)
        ),
        "重新登入必须重新驱动流程"
    );
}

#[test]
fn direct_policy_never_probes_campus_network() {
    let (mut manager, direct, _) = manager_with(AccessPolicy::Direct, |_| {
        panic!("直连策略不应发起探测请求");
    });

    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct
    );
    assert!(direct.requests().is_empty());
}

#[test]
fn auto_policy_uses_webvpn_off_campus_for_attendance_only() {
    let (mut manager, direct, _) = manager_with(AccessPolicy::Auto, |request| {
        if request.url == CAMPUS_PROBE_URL {
            Err(AppError::network("无法连接校园网"))
        } else {
            ok_response()
        }
    });

    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::WebVpn,
        "考勤系统校外需经 WebVPN"
    );
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Lms).unwrap(),
        AccessMode::Direct,
        "思源学堂为云端服务，校外仍直连"
    );
    // 探測結果會被快取，只發一次。
    let probes = direct
        .requests()
        .into_iter()
        .filter(|request| request.url == CAMPUS_PROBE_URL)
        .count();
    assert_eq!(probes, 1);
}

#[test]
fn auto_policy_uses_direct_on_campus() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct
    );
}

#[test]
fn webvpn_policy_logs_into_backend_before_site() {
    let (mut manager, _, webvpn) = manager_with(AccessPolicy::WebVpn, |_| ok_response());

    let LoginStage::Drive(driver) = manager.next_login_step(SiteKind::Attendance).unwrap() else {
        panic!("尚未登录时应有下一步");
    };
    let requests = webvpn.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.url.starts_with("https://webvpn.xjtu.edu.cn/login")),
        "第一步应先登录 WebVPN 后端，实际请求：{:?}",
        requests
            .iter()
            .map(|request| &request.url)
            .collect::<Vec<_>>()
    );

    // 完成 WebVPN 後端登入後，下一步是站點登入，且網址已改寫。
    let next = manager
        .complete_login_step(SiteKind::Attendance, &driver)
        .expect("完成后端登录");
    assert!(matches!(next, LoginStage::Drive(_)));
    assert!(
        webvpn
            .requests()
            .iter()
            .any(|request| request.url.starts_with("https://webvpn.xjtu.edu.cn/https/")),
        "站点登录网址应已改写为 WebVPN 网址"
    );
    assert!(!manager.is_logged_in(SiteKind::Attendance));
}

#[test]
fn send_rewrites_url_and_adds_site_headers_in_webvpn_mode() {
    let (mut manager, _, webvpn) = manager_with(AccessPolicy::WebVpn, |_| ok_response());
    manager.mark_logged_in(
        SiteKind::Attendance,
        AccessMode::WebVpn,
        vec![("X-Business-Token".to_owned(), "token-1".to_owned())],
    );

    let response = manager
        .send(
            SiteKind::Attendance,
            HttpRequest::get("https://bk-kq.xjtu.edu.cn/sa/student/home"),
        )
        .expect("转送请求");
    assert_eq!(response.status, 200);

    let request = webvpn.last_request().expect("已送出的请求");
    assert!(request.url.starts_with("https://webvpn.xjtu.edu.cn/https/"));
    assert!(request.url.ends_with("/sa/student/home"));
    assert_eq!(request.header_value("X-Business-Token"), Some("token-1"));
}

#[test]
fn send_keeps_url_in_direct_mode() {
    let (mut manager, direct, _) = manager_with(AccessPolicy::Direct, |_| ok_response());
    manager.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());

    manager
        .send(
            SiteKind::Lms,
            HttpRequest::get("https://lms.xjtu.edu.cn/api/my-courses"),
        )
        .expect("转送请求");

    let request = direct.last_request().expect("已送出的请求");
    assert_eq!(request.url, "https://lms.xjtu.edu.cn/api/my-courses");
}

#[test]
fn send_reports_expired_session_when_login_page_is_returned() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |_| {
        Ok(HttpResponse::new(
            200,
            "https://login.xjtu.edu.cn/cas/login?service=lms",
            LOGIN_PAGE.as_bytes(),
        ))
    });
    manager.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());

    let err = manager
        .send(
            SiteKind::Lms,
            HttpRequest::get("https://lms.xjtu.edu.cn/api/my-courses"),
        )
        .unwrap_err();
    assert!(matches!(err, AppError::SessionExpired), "实际错误：{err}");
    assert!(!manager.is_logged_in(SiteKind::Lms), "失效后应重置登录态");
}

#[test]
fn send_batch_keeps_order_and_injects_site_headers() {
    // 回應內容即請求網址：據此驗證「結果與輸入同序」。**送出的先後**在並行下
    // 不保證（也不是契約），只檢查三筆都送出且都帶了站點頭標。
    let (mut manager, direct, _) = manager_with(AccessPolicy::Direct, |request| {
        Ok(HttpResponse::new(
            200,
            request.url.clone(),
            request.url.clone(),
        ))
    });
    manager.mark_logged_in(
        SiteKind::Lms,
        AccessMode::Direct,
        vec![("X-Business-Token".to_owned(), "token-1".to_owned())],
    );

    let requests = ["/api/a", "/api/b", "/api/c"]
        .iter()
        .map(|path| HttpRequest::get(format!("https://lms.xjtu.edu.cn{path}")))
        .collect();
    let responses = manager
        .send_batch(SiteKind::Lms, requests)
        .expect("批次应成功");

    assert_eq!(responses.len(), 3, "每笔请求都应有结果");
    for (index, response) in responses.iter().enumerate() {
        let response = response.as_ref().expect("每笔请求都应成功");
        assert_eq!(
            response.text(),
            format!("https://lms.xjtu.edu.cn/api/{}", ["a", "b", "c"][index]),
            "结果应与输入同序"
        );
    }

    let sent = direct.requests();
    assert_eq!(sent.len(), 3);
    let mut urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
    urls.sort_unstable();
    assert_eq!(
        urls,
        [
            "https://lms.xjtu.edu.cn/api/a",
            "https://lms.xjtu.edu.cn/api/b",
            "https://lms.xjtu.edu.cn/api/c",
        ]
    );
    for request in &sent {
        assert_eq!(
            request.header_value("X-Business-Token"),
            Some("token-1"),
            "站点头标应逐笔注入"
        );
    }
}

#[test]
fn send_batch_rewrites_urls_in_webvpn_mode() {
    let (mut manager, _, webvpn) = manager_with(AccessPolicy::WebVpn, |_| ok_response());
    manager.mark_logged_in(
        SiteKind::Attendance,
        AccessMode::WebVpn,
        vec![("X-Business-Token".to_owned(), "token-1".to_owned())],
    );

    let requests = ["/sa/student/a", "/sa/student/b"]
        .iter()
        .map(|path| HttpRequest::get(format!("https://bk-kq.xjtu.edu.cn{path}")))
        .collect();
    manager
        .send_batch(SiteKind::Attendance, requests)
        .expect("批次应成功");

    let sent = webvpn.requests();
    assert_eq!(sent.len(), 2);
    let mut urls: Vec<&str> = sent.iter().map(|request| request.url.as_str()).collect();
    urls.sort_unstable();
    for (index, url) in urls.iter().enumerate() {
        assert!(
            url.starts_with("https://webvpn.xjtu.edu.cn/https/"),
            "应改写为 WebVPN 网址：{url}"
        );
        assert!(
            url.ends_with(&format!("/sa/student/{}", ["a", "b"][index])),
            "改写后仍应指向原路径：{url}"
        );
    }
    for request in &sent {
        assert_eq!(request.header_value("X-Business-Token"), Some("token-1"));
    }
}

#[test]
fn send_batch_reports_expired_session_when_any_response_is_a_login_page() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |request| {
        // 第二筆回傳統一認證登入頁：整批都應視為登入態失效。
        if request.url.ends_with("/api/b") {
            return Ok(HttpResponse::new(
                200,
                "https://login.xjtu.edu.cn/cas/login?service=lms",
                LOGIN_PAGE.as_bytes(),
            ));
        }
        ok_response()
    });
    manager.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());

    let requests = ["/api/a", "/api/b", "/api/c"]
        .iter()
        .map(|path| HttpRequest::get(format!("https://lms.xjtu.edu.cn{path}")))
        .collect();
    let err = manager
        .send_batch(SiteKind::Lms, requests)
        .expect_err("任一请求失效时整批应作废");
    assert!(matches!(err, AppError::SessionExpired), "实际错误：{err}");
    assert!(!manager.is_logged_in(SiteKind::Lms), "失效后应重置登录态");
}

#[test]
fn send_batch_without_login_reports_expired_session() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |_| ok_response());
    let err = manager
        .send_batch(
            SiteKind::Lms,
            vec![HttpRequest::get("https://lms.xjtu.edu.cn/api/a")],
        )
        .expect_err("未登录时不应送出请求");
    assert!(matches!(err, AppError::SessionExpired), "实际错误：{err}");
}

#[test]
fn send_without_login_reports_expired_session() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |_| ok_response());
    let err = manager
        .send(
            SiteKind::Lms,
            HttpRequest::get("https://lms.xjtu.edu.cn/api/my-courses"),
        )
        .unwrap_err();
    assert!(matches!(err, AppError::SessionExpired), "实际错误：{err}");
}

#[test]
fn auto_policy_skips_probe_for_site_without_webvpn_fallback() {
    let (mut manager, direct, _) = manager_with(AccessPolicy::Auto, |request| {
        assert_ne!(
            request.url, CAMPUS_PROBE_URL,
            "思源学堂校外直连，探测校园网没有意义"
        );
        ok_response()
    });

    assert_eq!(
        manager.resolve_access_mode(SiteKind::Lms).unwrap(),
        AccessMode::Direct
    );
    assert!(direct.requests().is_empty(), "不应发出任何探测请求");
}

#[test]
fn probe_treats_under_500_status_as_reachable() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |request| {
        if request.url == CAMPUS_PROBE_URL {
            // 網路層可達即視為校內；403/404 也一樣。
            Ok(HttpResponse::new(403, request.url.clone(), b"".as_slice()))
        } else {
            ok_response()
        }
    });

    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct
    );
    assert_eq!(
        manager.probe_rejection(),
        Some(403),
        "被拒絕的狀態碼要留下來，供直連失敗時判斷是否同源"
    );
}

/// 探測本身成功（2xx）時沒有「被拒絕」的訊號。
#[test]
fn successful_probe_records_no_rejection() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct
    );
    assert_eq!(manager.probe_rejection(), None);
}

/// 連不上校內主機時沒有狀態碼，只有「不可直連」。
#[test]
fn unreachable_probe_records_no_rejection() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |request| {
        if request.url == CAMPUS_PROBE_URL {
            return Err(AppError::network("无法连接校园网"));
        }
        ok_response()
    });
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::WebVpn
    );
    assert_eq!(manager.probe_rejection(), None);
    assert!(
        !manager.fallback_to_webvpn(SiteKind::Attendance),
        "已在 WebVPN 时无需回退"
    );
}

/// 5xx 代表校外攔截層而非校內服務：維持「不可直連」的既有語意。
#[test]
fn probe_treats_server_error_as_unreachable() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |request| {
        if request.url == CAMPUS_PROBE_URL {
            return Ok(HttpResponse::new(500, request.url.clone(), b"".as_slice()));
        }
        ok_response()
    });
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::WebVpn
    );
    assert_eq!(manager.probe_rejection(), None);
}

/// 回退後探測結果被改寫為「不可直連」，先前的拒絕訊號一併清除。
#[test]
fn fallback_clears_the_recorded_rejection() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |request| {
        if request.url == CAMPUS_PROBE_URL {
            return Ok(HttpResponse::new(403, request.url.clone(), b"".as_slice()));
        }
        ok_response()
    });
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct
    );
    manager.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    assert_eq!(manager.probe_rejection(), Some(403));

    assert!(manager.fallback_to_webvpn(SiteKind::Attendance));
    assert_eq!(manager.probe_rejection(), None);
}

#[test]
fn fallback_to_webvpn_switches_once_under_auto() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct,
        "校内探测成功时先走直连"
    );
    manager.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());

    assert!(
        manager.fallback_to_webvpn(SiteKind::Attendance),
        "Auto 下直连失败应允许回退"
    );
    assert!(
        manager.access_mode(SiteKind::Attendance).is_none(),
        "回退后旧的直连登录态必须失效，重新登录"
    );
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::WebVpn,
        "回退后该站应解析为 WebVPN"
    );
    assert!(
        !manager.fallback_to_webvpn(SiteKind::Attendance),
        "已在 WebVPN 时不得再次回退"
    );
}

#[test]
fn fallback_to_webvpn_respects_forced_policies_and_site_capability() {
    // 強制直連：即使失敗也不得擅自改道。
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct
    );
    assert!(!manager.fallback_to_webvpn(SiteKind::Attendance));

    // 強制 WebVPN：本來就走 WebVPN，無需回退。
    let (mut manager, _, _) = manager_with(AccessPolicy::WebVpn, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::WebVpn
    );
    assert!(!manager.fallback_to_webvpn(SiteKind::Attendance));

    // Auto 但站點設定為校外直連（思源学堂）時沒有回退目標。
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Lms).unwrap(),
        AccessMode::Direct
    );
    assert!(!manager.fallback_to_webvpn(SiteKind::Lms));
}

#[test]
fn send_wraps_network_errors_with_site_and_mode() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Direct, |_| {
        Err(AppError::network("connection refused"))
    });
    manager.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());

    let err = manager
        .send(
            SiteKind::Attendance,
            HttpRequest::get("https://bk-kq.xjtu.edu.cn/sa/student/home"),
        )
        .unwrap_err();

    let text = err.to_string();
    assert!(text.contains("考勤系统"), "信息：{text}");
    assert!(text.contains("直连"), "信息：{text}");
    assert!(text.contains("connection refused"), "信息：{text}");
}
