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

/// 把 PEM 內文壓成單行（真實端點不保證 64 欄換行）。
fn single_line_pem(pem: &str) -> String {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    format!("-----BEGIN PUBLIC KEY-----\n{body}\n-----END PUBLIC KEY-----\n")
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
fn injected_failure_count_continues_across_drivers() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        if request.url == LOGIN_URL {
            return Ok(page(POST_URL, &login_page(false, "e9s1")));
        }
        if request.url == rsa::PUBLIC_KEY_URL {
            return Ok(public_key_response());
        }
        Ok(HttpResponse::new(401, POST_URL, FAILED_PAGE.as_bytes()))
    }));

    // 模擬前兩次失敗後重建驅動器：計數由呼叫端保存並注入。
    let mut first = driver(&client);
    first.set_fail_count(2);
    assert_eq!(first.fail_count(), 2);
    assert!(matches!(
        first
            .start(&credentials(), AccountType::Undergraduate)
            .unwrap(),
        LoginReply::Fail { .. }
    ));
    assert_eq!(first.fail_count(), 3, "失敗後應遞增");
    assert_eq!(
        login_posts(&client)[0].form_field("failN"),
        Some("2"),
        "重建後應沿用保存的失敗次數"
    );

    // 已達門檻：新驅動器注入後不再提交，直接要求驗證碼。
    let mut next = driver(&client);
    next.set_fail_count(first.fail_count());
    let posts_before = login_posts(&client).len();
    assert_eq!(
        next.start(&credentials(), AccountType::Undergraduate)
            .unwrap(),
        LoginReply::NeedCaptcha
    );
    assert_eq!(
        login_posts(&client).len(),
        posts_before,
        "已達門檻的驅動器不得再提交帳密"
    );
    assert!(
        !next.used_existing_session(),
        "有提交帳密的流程不應被視為沿用既有登入態"
    );
}

/// 沿用既有登入態的「登入」必須可辨識：換帳號時不得據此寫回憑證。
#[test]
fn reports_login_that_reused_an_existing_session() {
    let client = Arc::new(FakeClient::with_responder(|_| {
        Ok(page(TARGET_URL, TARGET_PAGE))
    }));

    let mut driver = driver(&client);
    assert!(driver.is_already_authenticated());
    assert!(!driver.used_existing_session(), "執行前不應判定");

    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("执行登录");

    assert_eq!(reply, LoginReply::Success);
    assert!(
        driver.used_existing_session(),
        "未提交帳密即成功必須被標記，否則換帳號會寫回未驗證的憑證"
    );
    assert!(login_posts(&client).is_empty());
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
    // 純帳密失敗：不帶驗證碼，因此後續失敗不屬於「可重試的驗證碼錯誤」。
    assert!(!driver.last_attempt_submitted_captcha());

    // 驗證碼錯誤 → 下一次仍必須重新輸入，不能沿用舊碼。
    assert!(matches!(
        driver.submit_captcha("wrong").unwrap(),
        LoginReply::Fail { .. }
    ));
    // 這次確實送出了驗證碼：呼叫端可據此保留登入流程讓使用者重輸。
    assert!(driver.last_attempt_submitted_captcha());
    assert_eq!(driver.resume().unwrap(), LoginReply::NeedCaptcha);
    assert_eq!(driver.submit_captcha("right").unwrap(), LoginReply::Success);
    // 換帳號／重新登入後不得沿用上一次的判定。
    let _ = driver.start(&credentials(), AccountType::Undergraduate);
    assert!(!driver.last_attempt_submitted_captcha());
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

/// 以指定的 `securephone/valid` 回應內容驅動登入至可核對驗證碼的狀態。
fn mfa_driver_with_valid_data(data: serde_json::Value) -> LoginDriver {
    let client = Arc::new(FakeClient::with_responder(move |request| {
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
                serde_json::json!({ "code": 0, "data": data.clone() }),
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
    driver.mfa_phone().expect("读取绑定手机号");
    driver
}

#[test]
fn verify_mfa_code_accepts_success_codes_and_rejects_the_rest() {
    // 成功：整數 2、字串 "2"、null 與缺少欄位（與參考實作的 status not in (2, "2") 一致）。
    for data in [
        serde_json::json!({ "status": 2 }),
        serde_json::json!({ "status": "2" }),
        serde_json::json!({ "status": null }),
        serde_json::json!({}),
    ] {
        let mut driver = mfa_driver_with_valid_data(data.clone());
        driver
            .verify_mfa_code("123456")
            .unwrap_or_else(|err| panic!("{data} 應視為通過：{err}"));
    }

    // 失敗：其他整數與字串（非整數的失敗狀態先前會被誤判為成功）。
    for data in [
        serde_json::json!({ "status": 3 }),
        serde_json::json!({ "status": "3" }),
        serde_json::json!({ "status": "error" }),
    ] {
        let mut driver = mfa_driver_with_valid_data(data.clone());
        let Err(err) = driver.verify_mfa_code("123456") else {
            panic!("{data} 應視為失敗");
        };
        assert!(
            matches!(err, AppError::VerificationRetry(_)),
            "{data} 應為可重試的驗證碼錯誤：{err}"
        );
    }
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

#[test]
fn logs_in_with_non_canonical_public_key_layout() {
    let public_key = single_line_pem(&test_key().pem);
    let client = Arc::new(FakeClient::with_responder(move |request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(false, "e1s1"))),
            rsa::PUBLIC_KEY_URL => Ok(page(rsa::PUBLIC_KEY_URL, &public_key)),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let mut driver = driver(&client);
    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("单行公钥也应能登录");
    assert_eq!(reply, LoginReply::Success);

    // 密碼仍必須以伺服器公鑰加密後提交。
    let post = login_posts(&client).pop().expect("登录提交");
    let password = post.form_field("password").expect("密码字段");
    let raw = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &password["__RSA__".len()..],
    )
    .expect("base64 解码");
    let decrypted = test_key().private.decrypt(Pkcs1v15Encrypt, &raw).unwrap();
    assert_eq!(String::from_utf8(decrypted).unwrap(), "secret-password");
}

#[test]
fn refetches_public_key_instead_of_caching_a_bad_body() {
    let fetches = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&fetches);
    let client = Arc::new(FakeClient::with_responder(move |request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(false, "e1s1"))),
            rsa::PUBLIC_KEY_URL => {
                // 第一次回傳 HTML（例如被登入頁面欄截），第二次才是真正的公鑰。
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(page(rsa::PUBLIC_KEY_URL, "<html>登录门户</html>"))
                } else {
                    Ok(public_key_response())
                }
            }
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let mut driver = driver(&client);
    let err = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect_err("公钥响应不是 PEM 时应报错");
    assert!(matches!(err, AppError::Protocol(_)), "实际错误：{err}");

    let reply = driver
        .start(&credentials(), AccountType::Undergraduate)
        .expect("重新取公钥后应能登录");
    assert_eq!(reply, LoginReply::Success);
    assert_eq!(fetches.load(Ordering::SeqCst), 2, "错误正文不得被缓存");
}

#[test]
fn rejects_login_redirect_outside_school_domain() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(
                "https://evil.example.com/cas/login",
                &login_page(false, "e9s1"),
            )),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let http: Arc<dyn HttpClient> = client;
    let err = LoginDriver::new(http, LOGIN_URL, VISITOR_ID)
        .err()
        .expect("不得接受校外的提交目标");
    match err {
        AppError::UntrustedHost { host } => assert_eq!(host, "evil.example.com"),
        other => panic!("应为不受信任主机错误，实际：{other}"),
    }
}

#[test]
fn rejects_domain_suffix_confusion_in_redirect() {
    let client = Arc::new(FakeClient::with_responder(|request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(
                "https://lms.xjtu.edu.cn.evil.com/cas/login",
                &login_page(false, "e9s1"),
            )),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let http: Arc<dyn HttpClient> = client;
    let err = LoginDriver::new(http, LOGIN_URL, VISITOR_ID)
        .err()
        .expect("后缀混淆不得通过");
    assert!(
        matches!(err, AppError::UntrustedHost { .. }),
        "实际错误：{err}"
    );
}

#[test]
fn rejects_plain_http_submission_target() {
    // 學校網域但為 http：不得提交（憑證雖以 RSA 加密，仍要求 TLS）。
    let client = Arc::new(FakeClient::with_responder(|request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(
                "http://bk-kq.xjtu.edu.cn/cas/login",
                &login_page(false, "e9s1"),
            )),
            _ => Ok(page(TARGET_URL, TARGET_PAGE)),
        }
    }));

    let http: Arc<dyn HttpClient> = client;
    let err = LoginDriver::new(http, LOGIN_URL, VISITOR_ID)
        .err()
        .expect("http 目标不得通过");
    match err {
        AppError::Protocol(message) => {
            assert!(message.contains("https"), "错误应说明需要 https：{message}");
        }
        other => panic!("应为协议错误，实际：{other}"),
    }
}

#[test]
fn rejects_safety_verify_submission_outside_school_domain() {
    let safety_page = std::fs::read_to_string(format!(
        "{}/tests/fixtures/safety_verify_page.html",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("读取二次认证 fixture");
    let evil_safety_url = "https://evil.example.com/cas/sec/verify";

    let client = Arc::new(FakeClient::with_responder(move |request| {
        match request.url.as_str() {
            LOGIN_URL => Ok(page(POST_URL, &login_page(false, "e7s1"))),
            rsa::PUBLIC_KEY_URL => Ok(public_key_response()),
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
            // 二次認證頁面落在學校網域之外：提交前必須被拒絕。
            _ => Ok(page(evil_safety_url, &safety_page)),
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
    let err = driver.resume().expect_err("校外二次认证提交应被中止");
    match err {
        AppError::UntrustedHost { host } => assert_eq!(host, "evil.example.com"),
        other => panic!("应为不受信任主机错误，实际：{other}"),
    }
}
