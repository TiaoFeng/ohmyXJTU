//! 登入態失效判定測試（脫敏固定樣本）。
//!
//! 覆蓋三類容易被漏判的「被導向登入入口」情況：統一身份主機、站點自身的 CAS
//! 入口，以及 WebVPN 閘道（含被代理的登入頁）；同時確認正常資料回應與外部
//! 站點的 `/login` 不會被誤判。

use super::*;

/// 登入收尾結果的 `Debug` 只輸出標頭名稱，不輸出業務憑證。
#[test]
fn site_login_debug_hides_header_values() {
    let login = SiteLogin {
        headers: vec![("X-Business-Token".to_owned(), "secret-token".to_owned())],
        user_id: Some("7788".to_owned()),
    };
    let text = format!("{login:?}");
    assert!(
        text.contains("X-Business-Token"),
        "标头名称仍应可见：{text}"
    );
    assert!(!text.contains("secret-token"), "不得输出凭据：{text}");
    assert!(!text.contains("7788"), "识别码只应显示有无：{text}");
    assert!(text.contains("user_id: true"), "{text}");
}

/// 改址規則單一入口：只有 WebVPN 模式且校內網址才改寫。
#[test]
fn rewrite_for_mode_only_touches_school_urls_in_webvpn_mode() {
    let school = "https://lms.xjtu.edu.cn/api/my-courses";
    let external = "https://example.com/page";

    assert_eq!(
        rewrite_for_mode(AccessMode::Direct, school).unwrap(),
        school,
        "直连模式不应改写"
    );
    assert_eq!(
        rewrite_for_mode(AccessMode::Direct, external).unwrap(),
        external
    );
    assert_eq!(
        rewrite_for_mode(AccessMode::WebVpn, external).unwrap(),
        external,
        "非校内网址不应改写"
    );
    let rewritten = rewrite_for_mode(AccessMode::WebVpn, school).unwrap();
    assert!(
        rewritten.starts_with("https://webvpn.xjtu.edu.cn/"),
        "WebVPN 模式应改写校内网址：{rewritten}"
    );
    assert_eq!(
        rewritten,
        crate::webvpn::to_webvpn_url(school).unwrap(),
        "应与底层转换一致"
    );
}

/// 以指定狀態、最終網址與內容組出回應。
fn response(status: u16, final_url: &str, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        final_url: final_url.to_owned(),
        headers: vec![(
            "content-type".to_owned(),
            "text/html;charset=UTF-8".to_owned(),
        )],
        body: body.as_bytes().to_vec(),
    }
}

/// JSON 回應（無 HTML 標頭，不會進入二次認證頁判定）。
fn json_response(final_url: &str) -> HttpResponse {
    HttpResponse::new(200, final_url, br#"{"courses":[]}"#.to_vec())
}

/// 登入入口都應被判為登入態失效。
#[test]
fn login_endpoints_are_auth_failures() {
    let cases = [
        // 統一認證（任何路徑都代表仍在登入流程中）與二次認證頁。
        "https://login.xjtu.edu.cn/cas/login?service=https%3A%2F%2Flms.xjtu.edu.cn",
        "https://login.xjtu.edu.cn/oauth2/authorize?client_id=x",
        // 統一身份（Keycloak）主機。
        "https://identity1.xjtu.edu.cn/realms/xjtu/protocol/openid-connect/auth",
        // 站點自身的 CAS 入口（考勤）與站點登入頁（思源學堂）。
        "https://bk-kq.xjtu.edu.cn/sa/auth/cas/login/student-pc",
        "https://lms.xjtu.edu.cn/login",
        // WebVPN 閘道自身的登入頁。
        "https://webvpn.xjtu.edu.cn/login?cas_login=true",
    ];

    for url in cases {
        assert!(
            is_auth_failure(&response(200, url, "<html></html>")),
            "应判定为登录态失效：{url}"
        );
    }
}

/// 401 為明確的登入態失效。
#[test]
fn unauthorized_status_is_auth_failure() {
    assert!(is_auth_failure(&response(
        401,
        "https://lms.xjtu.edu.cn/api/my-courses",
        ""
    )));
}

/// 二次認證頁（HTML）仍需被認出。
#[test]
fn safety_verify_page_is_auth_failure() {
    let path = format!(
        "{}/tests/fixtures/safety_verify_page.html",
        env!("CARGO_MANIFEST_DIR")
    );
    let html = std::fs::read_to_string(path).expect("读取 fixture");
    assert!(is_auth_failure(&response(
        200,
        "https://bk-kq.xjtu.edu.cn/sa/student/pc/home",
        &html
    )));
}

/// 正常資料回應（含 WebVPN 代理路徑）不得被判為失效。
#[test]
fn ordinary_responses_are_not_auth_failures() {
    // 直連的 JSON 資料。
    assert!(!is_auth_failure(&json_response(
        "https://lms.xjtu.edu.cn/api/courses/1/activities"
    )));
    // WebVPN 代理的考勤資料（代理目標為 bk-kq，非登入入口）。
    let proxied =
        webvpn::to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa/student/pc/attendance-streams/page")
            .expect("WebVPN 网址");
    assert!(!is_auth_failure(&json_response(&proxied)));
}

/// WebVPN 代理的登入入口（CAS 路徑或統一認證主機）應判為失效。
#[test]
fn proxied_login_targets_are_auth_failures() {
    let proxied_cas =
        webvpn::to_webvpn_url("https://bk-kq.xjtu.edu.cn/sa/auth/cas/login/student-pc")
            .expect("WebVPN 网址");
    assert!(
        is_auth_failure(&json_response(&proxied_cas)),
        "被代理的站点 CAS 入口应判定为失效：{proxied_cas}"
    );

    let proxied_login = webvpn::to_webvpn_url("https://login.xjtu.edu.cn/cas/login?service=x")
        .expect("WebVPN 网址");
    assert!(
        is_auth_failure(&json_response(&proxied_login)),
        "被代理的统一认证应判定为失效：{proxied_login}"
    );
}

/// 非學校網域的 `/login` 與我們無關，不得誤判（否則會白白重新登入）。
#[test]
fn external_login_paths_are_not_auth_failures() {
    assert!(!is_auth_failure(&json_response(
        "https://cdn.example.com/login"
    )));
    assert!(!is_auth_failure(&response(
        200,
        "not a url",
        "<html></html>"
    )));
}
