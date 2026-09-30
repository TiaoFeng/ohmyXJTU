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
    assert_eq!(built.load(Ordering::SeqCst), 2, "建立時兩個後端各建一次");

    manager.register(Box::new(TestLmsSite));
    manager.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());
    assert!(manager.is_logged_in(SiteKind::Lms));

    manager.reset_session().expect("重建会话");

    assert_eq!(
        built.load(Ordering::SeqCst),
        4,
        "重建時兩個後端都必須換成新實例（新的 cookie jar）"
    );
    assert!(
        !manager.is_logged_in(SiteKind::Lms),
        "重建後不得殘留舊的站點登入態"
    );
    assert!(manager.access_mode(SiteKind::Lms).is_none());
    assert!(manager.resolved_access_mode(SiteKind::Lms).is_none());

    // 下一次登入必須重新走完整流程（不會被判定為已登入）。
    let stage = manager
        .next_login_step(SiteKind::Lms)
        .expect("取得登录步骤");
    assert!(
        matches!(stage, LoginStage::Drive(_)),
        "重建後應重新驅動登入流程"
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
        panic!("尚未登入时应有下一步");
    };
    let requests = webvpn.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.url.starts_with("https://webvpn.xjtu.edu.cn/login")),
        "第一步應先登入 WebVPN 後端，实际请求：{:?}",
        requests
            .iter()
            .map(|request| &request.url)
            .collect::<Vec<_>>()
    );

    // 完成 WebVPN 後端登入後，下一步是站點登入，且網址已改寫。
    let next = manager
        .complete_login_step(SiteKind::Attendance, &driver)
        .expect("完成后端登入");
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
    assert!(!manager.is_logged_in(SiteKind::Lms), "失效後应重置登入态");
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
}

#[test]
fn fallback_to_webvpn_switches_once_under_auto() {
    let (mut manager, _, _) = manager_with(AccessPolicy::Auto, |_| ok_response());
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct,
        "校內探測成功時先走直連"
    );
    manager.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());

    assert!(
        manager.fallback_to_webvpn(SiteKind::Attendance),
        "Auto 下直连失败应允许回退"
    );
    assert!(
        manager.access_mode(SiteKind::Attendance).is_none(),
        "回退後旧的直连登入态必须失效，重新登入"
    );
    assert_eq!(
        manager.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::WebVpn,
        "回退後该站应解析为 WebVPN"
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
    assert!(text.contains("考勤系统"), "訊息：{text}");
    assert!(text.contains("直连"), "訊息：{text}");
    assert!(text.contains("connection refused"), "訊息：{text}");
}
