//! 公鑰解析與密碼加密測試。
//!
//! 公鑰端點只保證回傳 PEM 文本，不保證換行方式；[`parse_public_key`] 必須能吃下
//! 裸 PEM（單行、64 欄、76 欄、CRLF、BOM、跳脫換行）與 JSON 包裝，
//! 並對非公鑰的正文給出不含正文的錯誤。

use rsa::pkcs1::EncodeRsaPublicKey as _;
use rsa::pkcs8::EncodePublicKey as _;
use rsa::traits::PublicKeyParts as _;
use rsa::{RsaPrivateKey, RsaPublicKey};

use super::*;

/// 產生測試金鑰對與其 SPKI PEM。
fn key_pair() -> (RsaPrivateKey, String) {
    let mut rng = chacha20poly1305::aead::OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("生成测试密钥对");
    let pem = RsaPublicKey::from(&private)
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .expect("导出公钥 PEM");
    (private, pem)
}

/// PEM 的 base64 內文（去除邊界行與換行）。
fn pem_body(pem: &str) -> String {
    pem.lines()
        .filter(|line| !line.starts_with("-----"))
        .collect()
}

/// 依指定欄寬與換行符號組出 PEM；`width` 為 0 表示不折行。
fn pem_with(label: &str, body: &str, width: usize, eol: &str) -> String {
    let wrapped = if width == 0 {
        body.to_owned()
    } else {
        body.as_bytes()
            .chunks(width)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "-----BEGIN {label}-----{eol}{}{eol}-----END {label}-----{eol}",
        wrapped.replace('\n', eol)
    )
}

#[test]
fn parses_every_line_layout() {
    let (_private, pem) = key_pair();
    let body = pem_body(&pem);
    let escaped = pem_with(SPKI_LABEL, &body, 64, "\n").replace('\n', "\\n");
    let variants = [
        ("裸 PEM（64 栏）", pem.clone()),
        ("单行", pem_with(SPKI_LABEL, &body, 0, "\n")),
        ("76 栏", pem_with(SPKI_LABEL, &body, 76, "\n")),
        ("CRLF", pem_with(SPKI_LABEL, &body, 64, "\r\n")),
        (
            "BOM 与前后空白",
            format!("\u{feff}  \n{}\n  ", pem_with(SPKI_LABEL, &body, 64, "\n")),
        ),
        ("跳脱换行", escaped),
    ];

    for (name, text) in variants {
        let key = parse_public_key(&text).unwrap_or_else(|err| panic!("{name} 应可解析：{err}"));
        assert_eq!(key.size(), 256, "{name} 的模长应为 2048 位");
    }
}

#[test]
fn parses_json_wrapped_pem() {
    let (_private, pem) = key_pair();
    let as_string = serde_json::Value::String(pem.clone()).to_string();
    let as_object = serde_json::json!({ "code": 0, "data": { "publicKey": pem } });

    assert!(
        parse_public_key(&as_string).is_ok(),
        "JSON 字符串包装应可解析"
    );
    assert!(
        parse_public_key(&as_object.to_string()).is_ok(),
        "JSON 对象字段包装应可解析"
    );
}

#[test]
fn parses_pkcs1_pem() {
    let (private, _pem) = key_pair();
    let pkcs1 = RsaPublicKey::from(&private)
        .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
        .expect("导出 PKCS#1 PEM");

    let key = parse_public_key(&pkcs1).expect("PKCS#1 公钥应可解析");
    assert_eq!(key.size(), private.size());
}

#[test]
fn strict_pem_decoder_rejects_non_canonical_wrapping() {
    // 固化修復前的故障形態：嚴格解碼器要求 base64 內文每行恰為 64 字元。
    let (_private, pem) = key_pair();
    let single_line = pem_with(SPKI_LABEL, &pem_body(&pem), 0, "\n");

    let error =
        RsaPublicKey::from_public_key_pem(&single_line).expect_err("严格解码器应拒绝单行正文");
    assert!(
        error.to_string().contains("Base64"),
        "应为 PEM Base64 error，实际：{error}"
    );
    assert!(
        parse_public_key(&single_line).is_ok(),
        "规范化解析应接受单行正文"
    );
}

#[test]
fn rejects_bodies_that_are_not_public_keys() {
    let variants = [
        ("HTML", "<html><body>login required</body></html>"),
        ("空白", "   \n\t"),
        ("缺少结尾边界", "-----BEGIN PUBLIC KEY-----\nMIIBIjA\n"),
        (
            "空内容",
            "-----BEGIN PUBLIC KEY-----\n-----END PUBLIC KEY-----",
        ),
        (
            "畸形 base64",
            "-----BEGIN PUBLIC KEY-----\nx\n-----END PUBLIC KEY-----",
        ),
        (
            "截断 DER",
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkq\n-----END PUBLIC KEY-----",
        ),
        ("纯文本", "not a pem"),
    ];

    for (name, text) in variants {
        let error = parse_public_key(text).expect_err(&format!("{name} 不应被接受"));
        assert!(matches!(error, AppError::Protocol(_)), "{name}：{error}");
    }
}

#[test]
fn error_messages_never_echo_the_body() {
    let marker = "SECRET-MARKER-9f3a2b";
    let body = format!("<html><body>{marker}</body></html>");

    let error = parse_public_key(&body).expect_err("HTML 不应被接受");
    assert!(
        !error.to_string().contains(marker),
        "错误信息不得包含响应正文：{error}"
    );
}

#[test]
fn encrypts_password_that_can_be_decrypted_again() {
    let (private, pem) = key_pair();
    let public_key = parse_public_key(&pem).expect("解析公钥");
    let password = "pa55w0rd-测试";

    let encrypted = encrypt_password(password, &public_key).expect("加密密码");
    assert!(encrypted.starts_with(PREFIX), "必须带 __RSA__ 前缀");

    let raw = STANDARD
        .decode(&encrypted[PREFIX.len()..])
        .expect("base64 解码");
    assert_eq!(raw.len(), private.size(), "密文长度应等于模长");

    let decrypted = private
        .decrypt(Pkcs1v15Encrypt, &raw)
        .expect("解密应当成功");
    assert_eq!(String::from_utf8(decrypted).unwrap(), password);
}

#[test]
fn produces_different_ciphertexts_for_same_password() {
    let (_private, pem) = key_pair();
    let public_key = parse_public_key(&pem).expect("解析公钥");

    let first = encrypt_password("same-password", &public_key).unwrap();
    let second = encrypt_password("same-password", &public_key).unwrap();
    assert_ne!(first, second, "PKCS#1 v1.5 填充必须随机");
}
