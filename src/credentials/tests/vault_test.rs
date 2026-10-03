//! 憑證保險庫的單元測試：加解密往返、口令錯誤、竄改偵測與原子寫入。

use std::fs;

use tempfile::{TempDir, tempdir};

use super::*;
use crate::error::AppError;

/// 測試用口令。
const PASSPHRASE: &str = "correct horse battery staple";

fn vault_in_temp() -> (TempDir, Vault) {
    let dir = tempdir().expect("创建临时目录");
    let vault = Vault::at(dir.path().join("credentials.vault"));
    (dir, vault)
}

fn credentials() -> Credentials {
    Credentials::new("3120000001", "secret-password")
}

#[test]
fn store_then_load_round_trips() {
    let (_dir, vault) = vault_in_temp();
    vault.store(PASSPHRASE, &credentials()).expect("写入凭证");
    assert!(vault.exists(), "写入后凭证文件应当存在");

    let loaded = vault.load(PASSPHRASE).expect("读取凭证");
    assert_eq!(loaded, credentials());
}

#[test]
fn wrong_passphrase_is_reported() {
    let (_dir, vault) = vault_in_temp();
    vault.store(PASSPHRASE, &credentials()).unwrap();

    let err = vault.load("not the passphrase").unwrap_err();
    assert!(matches!(err, AppError::WrongPassphrase), "实际错误：{err}");
}

#[test]
fn ciphertext_differs_between_writes() {
    let (_dir, vault) = vault_in_temp();
    vault.store(PASSPHRASE, &credentials()).unwrap();
    let first = fs::read(vault.path()).unwrap();

    vault.store(PASSPHRASE, &credentials()).unwrap();
    let second = fs::read(vault.path()).unwrap();

    assert_ne!(first, second, "同一口令两次写入应使用不同的盐值与 nonce");
    assert_eq!(vault.load(PASSPHRASE).unwrap(), credentials());
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let (_dir, vault) = vault_in_temp();
    vault.store(PASSPHRASE, &credentials()).unwrap();

    let mut bytes = fs::read(vault.path()).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(vault.path(), &bytes).unwrap();

    let err = vault.load(PASSPHRASE).unwrap_err();
    assert!(matches!(err, AppError::WrongPassphrase), "实际错误：{err}");
}

#[test]
fn malformed_file_is_rejected() {
    let (_dir, vault) = vault_in_temp();

    fs::write(vault.path(), b"not a vault").unwrap();
    let err = vault.load(PASSPHRASE).unwrap_err();
    assert!(matches!(err, AppError::VaultFile(_)), "实际错误：{err}");

    fs::write(vault.path(), b"WRONGMAGIC-and-some-padding-bytes").unwrap();
    let err = vault.load(PASSPHRASE).unwrap_err();
    assert!(matches!(err, AppError::VaultFile(_)), "实际错误：{err}");
}

#[test]
fn malformed_file_error_includes_path_and_rebuild_hint() {
    let (_dir, vault) = vault_in_temp();
    fs::write(vault.path(), b"not a vault").unwrap();

    let message = vault.load(PASSPHRASE).unwrap_err().to_string();
    assert!(message.contains("可删除"), "应附重建指引：{message}");
    assert!(
        message.contains(&vault.path().display().to_string()),
        "应包含凭证文件路径：{message}"
    );
    assert!(
        !message.contains("not a vault"),
        "不得包含文件内容：{message}"
    );

    // 口令錯誤属正常情况，不應附帶重建指引。
    vault.store(PASSPHRASE, &credentials()).unwrap();
    let message = vault.load("wrong passphrase").unwrap_err().to_string();
    assert!(
        !message.contains("可删除"),
        "口令错误不应附带重建指引：{message}"
    );
}

#[test]
fn change_passphrase_rotates_secret() {
    let (_dir, vault) = vault_in_temp();
    vault.store(PASSPHRASE, &credentials()).unwrap();

    vault
        .change_passphrase(PASSPHRASE, "a brand new passphrase")
        .expect("更换口令");

    assert_eq!(vault.load("a brand new passphrase").unwrap(), credentials());
    assert!(
        matches!(
            vault.load(PASSPHRASE).unwrap_err(),
            AppError::WrongPassphrase
        ),
        "旧口令应当失效"
    );
}

#[cfg(unix)]
#[test]
fn vault_file_is_private() {
    use crate::io::secure_file::is_private;

    let (_dir, vault) = vault_in_temp();
    vault.store(PASSPHRASE, &credentials()).unwrap();

    assert!(
        is_private(vault.path()).unwrap(),
        "凭证文件权限必须仅拥有者可读写"
    );
}

#[cfg(unix)]
#[test]
fn failed_write_keeps_previous_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempdir().unwrap();
    let data = dir.path().join("data");
    fs::create_dir(&data).unwrap();
    let vault = Vault::at(data.join("credentials.vault"));
    vault.store(PASSPHRASE, &credentials()).unwrap();

    fs::set_permissions(&data, fs::Permissions::from_mode(0o500)).unwrap();
    // 若當前使用者不受權限限制（例如 root），此測試沒有意義。
    if fs::write(data.join("probe"), b"x").is_ok() {
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }

    let result = vault.store(PASSPHRASE, &Credentials::new("another", "another"));
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(result.is_err(), "目录不可写时写入应当失败");
    assert_eq!(
        vault.load(PASSPHRASE).unwrap(),
        credentials(),
        "写入失败时原有凭证必须保留"
    );
}

/// 解密的內容不是預期的結構時，錯誤訊息不得夾帶明文片段。
///
/// `serde_json` 對型別不符的訊息會引用出問題的值（例如
/// `invalid type: string "…", expected struct Credentials`）；修復前這裡會把
/// 解密後的明文寫進使用者可見的訊息。
#[test]
fn parse_failure_message_does_not_leak_plaintext() {
    const PLAINTEXT: &str = "TOP-SECRET-PLAINTEXT";

    let dir = tempdir().expect("创建临时目录");
    let vault = Vault::at(dir.path().join("credentials.vault"));
    fs::write(vault.path(), seal(PLAINTEXT.as_bytes())).expect("写入凭证文件");

    let err = vault.load(PASSPHRASE).unwrap_err();
    let message = err.to_string();
    assert!(
        !message.contains(PLAINTEXT),
        "信息不得夹带解密内容：{message}"
    );
    assert!(matches!(err, AppError::VaultFile(_)), "实际错误：{err}");
}

/// AAD 由格式版本派生；現行 v1 必須與舊版常數位元組相同，否則既有憑證庫無法解密。
#[test]
fn aad_stays_compatible_with_previous_release() {
    assert_eq!(aad(VERSION), b"ohmyXJTU-vault-v1");
}

/// 以 `PASSPHRASE` 加密任意位元組，組出格式合法的憑證檔。
fn seal(plaintext: &[u8]) -> Vec<u8> {
    use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};

    let mut salt = [0_u8; SALT_LEN];
    let mut nonce = [0_u8; NONCE_LEN];
    crate::random::fill(&mut salt).expect("盐值");
    crate::random::fill(&mut nonce).expect("nonce");

    let key = derive_key(PASSPHRASE, DEFAULT_KDF, &salt).expect("派生密钥");
    let cipher = ChaCha20Poly1305::new((&*key).into());
    let aad = aad(VERSION);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .expect("加密");

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&DEFAULT_KDF.m_cost.to_le_bytes());
    out.extend_from_slice(&DEFAULT_KDF.t_cost.to_le_bytes());
    out.extend_from_slice(&DEFAULT_KDF.p_cost.to_le_bytes());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}
