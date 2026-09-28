//! 密碼加密測試：以即時產生的金鑰對驗證 PKCS#1 v1.5 與 `__RSA__` 格式。

use rsa::pkcs8::EncodePublicKey as _;
use rsa::traits::PublicKeyParts as _;
use rsa::{RsaPrivateKey, RsaPublicKey};

use super::*;

fn key_pair() -> (RsaPrivateKey, String) {
    let mut rng = chacha20poly1305::aead::OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("生成测试密钥对");
    let pem = RsaPublicKey::from(&private)
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .expect("导出公钥 PEM");
    (private, pem)
}

#[test]
fn encrypts_password_that_can_be_decrypted_again() {
    let (private, pem) = key_pair();
    let password = "pa55w0rd-测试";

    let encrypted = encrypt_password(password, &pem).expect("加密密码");
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
    let first = encrypt_password("same-password", &pem).unwrap();
    let second = encrypt_password("same-password", &pem).unwrap();
    assert_ne!(first, second, "PKCS#1 v1.5 填充必须随机");
}

#[test]
fn rejects_invalid_public_key() {
    let err = encrypt_password("password", "not a pem").unwrap_err();
    assert!(matches!(err, AppError::Protocol(_)), "实际错误：{err}");
}
