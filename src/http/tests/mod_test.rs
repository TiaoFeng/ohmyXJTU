//! HTTP 型別的 `Debug` 遮罩測試。
//!
//! 專案的規則是一致：任何 `{:?}` 都不得輸出憑證（見 `credentials::secret`、
//! `credentials::vault`、`tui::text`）。請求與回應同樣帶著帳號、驗證碼、簡訊
//! 驗證碼、業務 token 與一次性 ticket，因此也必須遮罩。

use super::*;

/// 表單主體只輸出欄位名稱。
#[test]
fn form_body_debug_only_shows_field_names() {
    let body = Body::Form(vec![
        ("username".to_owned(), "20230001".to_owned()),
        ("password".to_owned(), "__RSA__abc".to_owned()),
        ("smsCode".to_owned(), "654321".to_owned()),
    ]);
    let text = format!("{body:?}");
    assert!(text.contains("username"), "{text}");
    assert!(text.contains("smsCode"), "{text}");
    for secret in ["20230001", "__RSA__abc", "654321"] {
        assert!(!text.contains(secret), "不得输出字段值：{text}");
    }
}

/// JSON 主體只描述型別。
#[test]
fn json_body_debug_hides_the_value() {
    let body = Body::Json(serde_json::json!({"loginRequestId": "abc", "ticket": "ST-1"}));
    let text = format!("{body:?}");
    assert_eq!(text, "Json(<redacted>)");
}

/// 請求的標頭值與查詢字串都不得出現。
#[test]
fn request_debug_hides_header_values_and_query() {
    let request = HttpRequest::post_form(
        "https://login.xjtu.edu.cn/cas/login?service=lms&ticket=ST-1234",
        vec![("username", "20230001"), ("password", "__RSA__abc")],
    )
    .header("X-Business-Token", "secret-token")
    .header("Referer", "https://lms.xjtu.edu.cn/");

    let text = format!("{request:?}");
    assert!(
        text.contains("X-Business-Token"),
        "标头名称仍应可见：{text}"
    );
    assert!(text.contains("cas/login"), "路径仍应可见：{text}");
    assert!(text.contains("?<redacted>"), "查询串应被遮蔽：{text}");
    for secret in ["secret-token", "ST-1234", "__RSA__abc", "20230001"] {
        assert!(!text.contains(secret), "不得输出敏感内容：{text}");
    }
}

/// 回應的本文與最終網址（可能帶 ticket）都不得出現。
#[test]
fn response_debug_hides_body_and_final_url_query() {
    let response = HttpResponse::new(
        200,
        "https://login.xjtu.edu.cn/cas/login?ticket=ST-9999",
        "<html>登录成功</html>",
    );
    let text = format!("{response:?}");
    assert!(text.contains("200"), "{text}");
    assert!(text.contains("body_len"), "只应输出长度：{text}");
    assert!(!text.contains("ST-9999"), "{text}");
    assert!(!text.contains("登录成功"), "不得输出回应本文：{text}");
}

/// 沒有查詢字串時網址原樣呈現（方便除錯）；無法解析時退回佔位字串。
#[test]
fn url_without_query_is_shown_as_is() {
    assert_eq!(
        redacted_url("https://lms.xjtu.edu.cn/user/index"),
        "https://lms.xjtu.edu.cn/user/index"
    );
    assert_eq!(redacted_url("not a url"), "<url>");
}

/// userinfo（`https://token@host/`）也是憑證，同樣不得出現在 `Debug` 輸出裡。
#[test]
fn url_userinfo_is_redacted() {
    let userinfo = redacted_url("https://token@lms.xjtu.edu.cn/user/index");
    assert!(!userinfo.contains("token"), "不得输出 userinfo：{userinfo}");
    assert!(
        userinfo.contains("lms.xjtu.edu.cn/user/index"),
        "{userinfo}"
    );

    let both = redacted_url("https://user:pass@host/path?q=1");
    assert!(!both.contains("user"), "不得输出账号：{both}");
    assert!(!both.contains("pass"), "不得输出密码：{both}");
    assert!(both.contains("?<redacted>"), "查询串仍应遮蔽：{both}");
}

/// 新增的 PUT／DELETE／HEAD 建構子設定正確的方法與主體。
#[test]
fn put_delete_head_builders_set_method_and_body() {
    let put = HttpRequest::put("https://dav.example/x.vault", vec![1, 2, 3]);
    assert_eq!(put.method, Method::Put);
    assert!(matches!(put.body, Some(Body::Bytes(ref bytes)) if bytes == &[1, 2, 3]));

    let delete = HttpRequest::delete("https://dav.example/x.vault");
    assert_eq!(delete.method, Method::Delete);
    assert!(delete.body.is_none());

    let head = HttpRequest::head("https://dav.example/x.vault");
    assert_eq!(head.method, Method::Head);
    assert!(head.body.is_none());
}

/// 原始位元組主體（同步的加密文檔）不得出現在 `Debug` 輸出裡。
#[test]
fn bytes_body_debug_hides_the_content() {
    let payload = b"OMXJTU1\nsecret-ciphertext".to_vec();
    let text = format!("{:?}", Body::Bytes(payload.clone()));
    assert!(!text.contains("secret-ciphertext"), "{text}");
    assert!(text.contains("Bytes"), "{text}");
    assert!(
        text.contains(&payload.len().to_string()),
        "应输出长度：{text}"
    );
}
