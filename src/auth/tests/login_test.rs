//! 登入狀態機測試：以假 HTTP 客戶端回放脫敏的固定回應，離線驗證五種狀態轉換。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use ::rsa::pkcs8::EncodePublicKey as _;
use ::rsa::traits::PublicKeyParts as _;
use ::rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

use crate::credentials::Credentials;
use crate::http::fake::FakeClient;
use crate::http::{HttpClient, HttpRequest, HttpResponse};

use super::*;

const LOGIN_URL: &str = "https://lms.xjtu.edu.cn";
const POST_URL: &str = "https://login.xjtu.edu.cn/cas/login?service=lms";
const TARGET_URL: &str = "https://lms.xjtu.edu.cn/user/index";
const VISITOR_ID: &str = "00112233445566778899aabbccddeeff";

const TARGET_PAGE: &str =
    "<html><head><title>思源学堂</title></head><body>globalData</body></html>";
const FAILED_PAGE: &str =
    "<html><body><el-alert title=\"用户名或密码错误\" type=\"error\"></el-alert></body></html>";

struct TestKey {
    private: RsaPrivateKey,
    pem: String,
}

/// 2048 位元金鑰產生較慢，整個測試二進位檔共用一份。
fn test_key() -> &'static TestKey {
    static KEY: OnceLock<TestKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut rng = chacha20poly1305::aead::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("生成测试密钥");
        let pem = RsaPublicKey::from(&private)
            .to_public_key_pem(::rsa::pkcs8::LineEnding::LF)
            .expect("导出公钥 PEM");
        TestKey { private, pem }
    })
}

fn credentials() -> Credentials {
    Credentials::new("3120000001", "secret-password")
}

fn login_page(mfa_enabled: bool, execution: &str) -> String {
    format!(
        r#"<html><head><script>
        var globalConfig = eval('(' + "{{\"mfaEnabled\":{mfa_enabled}}}" + ')');
        </script></head><body>
        <input type="hidden" name="execution" value="{execution}" />
        </body></html>"#
    )
}

fn page(final_url: &str, body: &str) -> HttpResponse {
    HttpResponse::new(200, final_url, body.as_bytes())
}

fn ok_json(final_url: &str, value: serde_json::Value) -> HttpResponse {
    HttpResponse::new(
        200,
        final_url,
        serde_json::to_vec(&value).expect("序列化 JSON"),
    )
}

fn driver(client: &Arc<FakeClient>) -> LoginDriver {
    let http: Arc<dyn HttpClient> = client.clone();
    LoginDriver::new(http, LOGIN_URL, VISITOR_ID).expect("建立登录驱动器")
}

fn login_posts(client: &FakeClient) -> Vec<HttpRequest> {
    client
        .requests()
        .into_iter()
        .filter(|request| request.url == POST_URL && request.form_field("username").is_some())
        .collect()
}

/// 讀取 JSON 請求主體中的字串欄位。
fn json_field(request: &HttpRequest, key: &str) -> Option<String> {
    match &request.body {
        Some(crate::http::Body::Json(value)) => value
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// 伺服器公鑰端點的回應。
fn public_key_response() -> HttpResponse {
    page(rsa::PUBLIC_KEY_URL, &test_key().pem)
}

#[test]
fn logs_in_and_submits_expected_form_fields() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        if request.url == LOGIN_URL {
            Ok(page(POST_URL, &login_page(false, "e1s1")))
        } else if request.url == rsa::PUBLIC_KEY_URL {
            Ok(public_key_response())
        } else {
            Ok(page(TARGET_URL, TARGET_PAGE))
        }
    }));

    let mut driver = driver(&client);
    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("执行登录");
    assert_eq!(reply, LoginReply::Success);
    assert_eq!(
        driver.final_response().map(|r| r.final_url.clone()),
        Some(TARGET_URL.to_owned())
    );

    let posts = login_posts(&client);
    assert_eq!(posts.len(), 1);
    let post = &posts[0];
    assert_eq!(post.form_field("username"), Some("3120000001"));
    assert_eq!(post.form_field("execution"), Some("e1s1"));
    assert_eq!(post.form_field("_eventId"), Some("submit"));
    assert_eq!(post.form_field("currentMenu"), Some("1"));
    assert_eq!(post.form_field("failN"), Some("0"));
    assert_eq!(post.form_field("captcha"), Some(""));
    assert_eq!(post.form_field("mfaState"), Some(""));
    assert_eq!(post.form_field("trustAgent"), Some(""));
    assert_eq!(post.form_field("fpVisitorId"), Some(VISITOR_ID));

    // 密碼必須是 `__RSA__` 形式的密文，且可被伺服器端私鑰還原。
    let password = post.form_field("password").expect("密码字段");
    assert!(password.starts_with("__RSA__"), "密码必须加密后提交");
    let raw = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &password["__RSA__".len()..],
    )
    .expect("base64 解码");
    let decrypted = test_key().private.decrypt(Pkcs1v15Encrypt, &raw).unwrap();
    assert_eq!(String::from_utf8(decrypted).unwrap(), "secret-password");
    assert_eq!(raw.len(), test_key().private.size());
}

#[test]
fn short_circuits_when_login_state_already_exists() {
    let client = Arc::new(FakeClient::with_responder(|_| {
        Ok(page(TARGET_URL, TARGET_PAGE))
    }));

    let mut driver = driver(&client);
    assert!(driver.is_already_authenticated());
    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("执行登录");
    assert_eq!(reply, LoginReply::Success);
    assert!(
        login_posts(&client).is_empty(),
        "已有登录态时不应提交账号密码"
    );
}

#[test]
fn requires_captcha_after_repeated_failures_then_succeeds() {
    let attempts = AtomicUsize::new(0);
    let client = Arc::new(FakeClient::with_responder(move |request| {
        if request.url == LOGIN_URL {
            return Ok(page(POST_URL, &login_page(false, "e9s1")));
        }
        if request.url == rsa::PUBLIC_KEY_URL {
            return Ok(public_key_response());
        }
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        if attempt < 3 {
            Ok(HttpResponse::new(401, POST_URL, FAILED_PAGE.as_bytes()))
        } else {
            Ok(page(TARGET_URL, TARGET_PAGE))
        }
    }));

    let mut driver = driver(&client);
    assert!(matches!(
        driver
            .start(&credentials(), AccountType::Undergraduate)
            .unwrap(),
        LoginReply::Fail { .. }
    ));
    assert!(matches!(driver.resume().unwrap(), LoginReply::Fail { .. }));
    assert!(matches!(driver.resume().unwrap(), LoginReply::Fail { .. }));

    // 三次失敗後（且未提供驗證碼）不再提交，而是要求驗證碼。
    assert_eq!(driver.resume().unwrap(), LoginReply::NeedCaptcha);
    assert_eq!(login_posts(&client).len(), 3, "需要验证码时不应继续提交");

    assert_eq!(driver.submit_captcha("a1b2").unwrap(), LoginReply::Success);
    let posts = login_posts(&client);
    assert_eq!(posts.last().unwrap().form_field("captcha"), Some("a1b2"));
    assert_eq!(posts.last().unwrap().form_field("failN"), Some("3"));
}

#[test]
fn clears_captcha_after_failure() {
    let attempts = AtomicUsize::new(0);
    let client = Arc::new(FakeClient::with_responder(move |request| {
        if request.url == LOGIN_URL {
            return Ok(page(POST_URL, &login_page(false, "e9s1")));
        }
        if request.url == rsa::PUBLIC_KEY_URL {
            return Ok(public_key_response());
        }
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        match attempt {
            0..=3 => Ok(HttpResponse::new(401, POST_URL, FAILED_PAGE.as_bytes())),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let mut driver = driver(&client);
    let _ = driver.start(&credentials(), AccountType::Undergraduate);
    let _ = driver.resume();
    let _ = driver.resume();
    assert_eq!(driver.resume().unwrap(), LoginReply::NeedCaptcha);

    // 驗證碼錯誤 → 下一次仍必須重新輸入，不能沿用舊碼。
    assert!(matches!(
        driver.submit_captcha("wrong").unwrap(),
        LoginReply::Fail { .. }
    ));
    assert_eq!(driver.resume().unwrap(), LoginReply::NeedCaptcha);
    assert_eq!(driver.submit_captcha("right").unwrap(), LoginReply::Success);
}

#[test]
fn completes_sms_mfa_flow() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(true, "e5s1"))),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
            MFA_DETECT_URL => Ok(ok_json(
                MFA_DETECT_URL,
                serde_json::json!({ "code": 0, "data": { "state": "mfa-state-1", "need": true } }),
            )),
            MFA_SEND_URL => Ok(ok_json(MFA_SEND_URL, serde_json::json!({ "code": 0 }))),
            MFA_VALID_URL => Ok(ok_json(
                MFA_VALID_URL,
                serde_json::json!({ "code": 0, "data": { "status": 2 } }),
            )),
            _ if request.url.contains("/cas/mfa/initByType/securephone") => Ok(ok_json(
                MFA_DETECT_URL,
                serde_json::json!({
                    "code": 0,
                    "data": { "gid": "gid-1", "securePhone": "138****8888" }
                }),
            )),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let mut driver = driver(&client);
    assert_eq!(
        driver
            .start(&credentials(), AccountType::Undergraduate)
            .unwrap(),
        LoginReply::NeedMfa
    );

    // 需要 MFA 時尚不應提交帳密。
    assert!(login_posts(&client).is_empty());

    assert_eq!(driver.mfa_phone().unwrap(), "138****8888");
    assert_eq!(driver.send_mfa_code().unwrap(), "138****8888");
    driver.verify_mfa_code("123456").expect("核验短信验证码");
    assert_eq!(driver.resume().unwrap(), LoginReply::Success);

    let post = login_posts(&client).pop().expect("登录提交");
    assert_eq!(post.form_field("mfaState"), Some("mfa-state-1"));
    assert_eq!(post.form_field("trustAgent"), Some("true"));

    let valid = client
        .requests()
        .into_iter()
        .find(|request| request.url == MFA_VALID_URL)
        .expect("核验请求");
    assert_eq!(json_field(&valid, "gid").as_deref(), Some("gid-1"));
    assert_eq!(json_field(&valid, "code").as_deref(), Some("123456"));
}

#[test]
fn selects_undergraduate_account() {
    let choice_page = std::fs::read_to_string(format!(
        "{}/tests/fixtures/account_choice_page.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("读取身份选择 fixture");

    let client = Arc::new(FakeClient::with_responder(move |request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(false, "e6s1"))),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
            // 身份選擇提交帶有 useDefault 欄位，據此與帳密提交區分。
            url if url.starts_with(ACCOUNT_CHOICE_URL)
                && request.form_field("useDefault").is_some() =>
            {
                Ok(page(TARGET_URL, TARGET_PAGE))
            }
            _ => Ok(page(POST_URL, &choice_page)),
        }
    }));

    let mut driver = driver(&client);
    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("执行登录");
    assert!(matches!(reply, LoginReply::NeedAccountChoice(_)));
    assert_eq!(driver.resume().unwrap(), LoginReply::Success);

    let choice = client
        .requests()
        .into_iter()
        .find(|request| request.form_field("useDefault").is_some())
        .expect("身份选择请求");
    assert_eq!(choice.form_field("username"), Some("3120000001-1"));
    assert_eq!(choice.form_field("useDefault"), Some("false"));
}

#[test]
fn completes_safety_verify_flow() {
    let safety_page = std::fs::read_to_string(format!(
        "{}/tests/fixtures/safety_verify_page.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("读取二次认证 fixture");
    let safety_url = "https://login.xjtu.edu.cn/cas/sec/verify";

    let client = Arc::new(FakeClient::with_responder(move |request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(false, "e7s1"))),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
            url if url == safety_url => Ok(page(TARGET_URL, TARGET_PAGE)),
            _ if request.url.contains("/cas/sec/initByType/securephone") => Ok(ok_json(
                MFA_VALID_URL,
                serde_json::json!({
                    "code": 0,
                    "data": { "gid": "gid-2", "securePhone": "139****9999" }
                }),
            )),
            MFA_VALID_URL => Ok(ok_json(
                MFA_VALID_URL,
                serde_json::json!({ "code": 0, "data": { "status": 2 } }),
            )),
            MFA_SEND_URL => Ok(ok_json(MFA_SEND_URL, serde_json::json!({ "code": 0 }))),
            _ => Ok(page(safety_url, &safety_page)),
        }
    }));

    let mut driver = driver(&client);
    assert_eq!(
        driver
            .start(&credentials(), AccountType::Undergraduate)
            .unwrap(),
        LoginReply::NeedMfa
    );
    assert_eq!(driver.send_mfa_code().unwrap(), "139****9999");
    driver.verify_mfa_code("654321").expect("核验验证码");
    assert_eq!(driver.resume().unwrap(), LoginReply::Success);

    let verify = client
        .requests()
        .into_iter()
        .find(|request| request.url == safety_url && request.form_field("secState").is_some())
        .expect("二次认证提交");
    assert_eq!(verify.form_field("secState"), Some("sec-state-fixture"));
    assert_eq!(
        verify.form_field("execution"),
        Some("e4s1-fixture-execution")
    );
}

#[test]
fn surfaces_server_alert_message_on_failure() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(false, "e8s1"))),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
            _ => Ok(page(POST_URL, FAILED_PAGE)),
        }
    }));

    let mut driver = driver(&client);
    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("执行登录");
    match reply {
        LoginReply::Fail { message } => assert_eq!(message, "登录失败：用户名或密码错误"),
        other => panic!("应当是失败状态，实际为 {other:?}"),
    }
}

#[test]
fn reports_protocol_error_when_login_page_is_unexpected() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        match request.url.as_str() {
            // 既沒有 execution，final_url 也不含 /cas/login → 會被視為已登入；
            // 這裡刻意讓它是 `/cas/login` 但不含 execution，以觸發協議錯誤。
            LOGIN_URL => Ok(page(POST_URL, TARGET_PAGE)),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let mut driver = driver(&client);
    let err = driver
        .start(&credentials(), AccountType::Undergraduate)
        .unwrap_err();
    assert!(matches!(err, AppError::Protocol(_)), "实际错误：{err}");
}
