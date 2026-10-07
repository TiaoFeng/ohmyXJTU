//! 加密信封原語的單元測試：往返、AAD 隔離、重用金鑰重封裝與損毀偵測。

use super::*;
use crate::error::AppError;

/// 測試用口令。
const PASSPHRASE: &str = "correct horse battery staple";
/// 憑證檔的 AAD 前綴（與 `vault.rs` 相同）。
const VAULT_PREFIX: &str = "ohmyXJTU-vault";
/// 任務檔的 AAD 前綴（與 `task::worker::tasks` 相同）。
const TASKS_PREFIX: &str = "ohmyXJTU-tasks";

fn seal_vault(plaintext: &[u8]) -> Vec<u8> {
    seal(PASSPHRASE, VAULT_PREFIX, plaintext).expect("密封").0
}

#[test]
fn seal_then_unseal_round_trips() {
    let bytes = seal_vault(b"hello envelope");
    let (plaintext, _sealed) = unseal(PASSPHRASE, VAULT_PREFIX, &bytes).expect("解封");
    assert_eq!(&plaintext[..], b"hello envelope");
}

#[test]
fn each_seal_uses_a_fresh_salt_and_nonce() {
    let first = seal_vault(b"same plaintext");
    let second = seal_vault(b"same plaintext");
    assert_ne!(first, second, "同一口令两次密封不应产生相同位元组");
    assert_eq!(
        &unseal(PASSPHRASE, VAULT_PREFIX, &first).unwrap().0[..],
        b"same plaintext"
    );
    assert_eq!(
        &unseal(PASSPHRASE, VAULT_PREFIX, &second).unwrap().0[..],
        b"same plaintext"
    );
}

#[test]
fn wrong_passphrase_is_reported() {
    let bytes = seal_vault(b"secret");
    let err = unseal("not the passphrase", VAULT_PREFIX, &bytes).unwrap_err();
    assert!(matches!(err, AppError::WrongPassphrase), "实际错误：{err}");
}

#[test]
fn prefixes_are_not_interchangeable() {
    // 以憑證前綴密封的檔案，用任務前綴解不開（AAD 不符）。
    let vault_bytes = seal_vault(b"credentials payload");
    let err = unseal(PASSPHRASE, TASKS_PREFIX, &vault_bytes).unwrap_err();
    assert!(matches!(err, AppError::WrongPassphrase), "实际错误：{err}");

    // 反向亦然。
    let tasks_bytes = seal(PASSPHRASE, TASKS_PREFIX, b"tasks payload").unwrap().0;
    let err = unseal(PASSPHRASE, VAULT_PREFIX, &tasks_bytes).unwrap_err();
    assert!(matches!(err, AppError::WrongPassphrase), "实际错误：{err}");
    assert!(unseal(PASSPHRASE, TASKS_PREFIX, &tasks_bytes).is_ok());
}

#[test]
fn aad_derives_from_prefix_and_version() {
    assert_eq!(aad(VAULT_PREFIX, VERSION), b"ohmyXJTU-vault-v1");
    assert_eq!(aad(TASKS_PREFIX, VERSION), b"ohmyXJTU-tasks-v1");
}

#[test]
fn tampered_ciphertext_is_rejected() {
    let mut bytes = seal_vault(b"secret");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    let err = unseal(PASSPHRASE, VAULT_PREFIX, &bytes).unwrap_err();
    assert!(matches!(err, AppError::WrongPassphrase), "实际错误：{err}");
}

#[test]
fn malformed_envelopes_are_rejected() {
    for bytes in [
        b"short".to_vec(),
        b"WRONGMAGIC-and-some-padding-bytes".to_vec(),
        {
            // 合法標識但版本不支援。
            let mut bytes = seal_vault(b"secret");
            bytes[MAGIC.len()] = 99;
            bytes
        },
        {
            // KDF 參數超出上限（防資源耗盡）。
            let mut bytes = seal_vault(b"secret");
            let offset = MAGIC.len() + 1;
            bytes[offset..offset + 4].copy_from_slice(&(MAX_M_COST + 1).to_le_bytes());
            bytes
        },
    ] {
        let err = unseal(PASSPHRASE, VAULT_PREFIX, &bytes).unwrap_err();
        assert!(matches!(err, AppError::VaultFile(_)), "实际错误：{err}");
    }
}

#[test]
fn reseal_reuses_the_salt_and_stays_readable() {
    let (first, sealed) = seal(PASSPHRASE, VAULT_PREFIX, b"one").expect("密封");
    let second = reseal(&sealed, VAULT_PREFIX, b"two").expect("重封装");

    // 鹽值與參數必須沿用（只換 nonce）：否則下次以口令解鎖派生不出同一把金鑰。
    let salt_start = MAGIC.len() + 1 + 4 + 4 + 4;
    assert_eq!(
        first[salt_start..salt_start + SALT_LEN],
        second[salt_start..salt_start + SALT_LEN],
        "重封装必须沿用同一盐值"
    );
    assert_ne!(first, second, "重封装应使用新的 nonce 与内容");

    // 舊檔案以口令仍可解開，新檔案亦然。
    assert_eq!(
        &unseal(PASSPHRASE, VAULT_PREFIX, &first).unwrap().0[..],
        b"one"
    );
    let (plaintext, _) = unseal(PASSPHRASE, VAULT_PREFIX, &second).expect("以口令解开重封装结果");
    assert_eq!(&plaintext[..], b"two");
}

#[test]
fn resealed_bytes_open_under_the_cached_key_semantics() {
    // 連續兩次重封裝都不得改變鹽值（模擬同一會話中的多次保存）。
    let (_, sealed) = seal(PASSPHRASE, TASKS_PREFIX, b"[]").expect("密封");
    let first = reseal(&sealed, TASKS_PREFIX, b"[1]").expect("重封装");
    let second = reseal(&sealed, TASKS_PREFIX, b"[1,2]").expect("再重封装");
    let salt_start = MAGIC.len() + 1 + 4 + 4 + 4;
    assert_eq!(
        first[salt_start..salt_start + SALT_LEN],
        second[salt_start..salt_start + SALT_LEN]
    );
    assert_eq!(
        &unseal(PASSPHRASE, TASKS_PREFIX, &second).unwrap().0[..],
        b"[1,2]"
    );
}
