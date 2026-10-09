//! 同步設定存儲的單元測試：加密往返、口令錯誤保護、清除與換口令。

use tempfile::{TempDir, tempdir};

use super::*;
use crate::error::AppError;

const PASSPHRASE: &str = "correct horse battery staple";

fn new_store(dir: &TempDir) -> SyncStore {
    SyncStore::at(dir.path().join("sync.vault"))
}

fn config() -> SyncConfig {
    SyncConfig::new("https://dav.jianguoyun.com/dav/", "a@b.com", "app-pass")
}

#[test]
fn lazy_init_does_not_create_a_file() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    assert!(store.init(PASSPHRASE).unwrap().is_none());
    assert!(store.config().is_none());
    assert!(
        !dir.path().join("sync.vault").exists(),
        "从未保存时不应产生文件"
    );
}

#[test]
fn round_trips_config_and_records() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    store
        .set_record(
            SyncFile::Tasks,
            FileRecord {
                version: Some("v1".to_owned()),
                etag: Some("\"v1\"".to_owned()),
                hash: Some("h1".to_owned()),
            },
        )
        .unwrap();

    let mut reloaded = new_store(&dir);
    reloaded.init(PASSPHRASE).unwrap();
    let config = reloaded.config().expect("应有设定");
    assert_eq!(config.url, "https://dav.jianguoyun.com/dav/");
    assert_eq!(config.account, "a@b.com");
    assert_eq!(config.app_password, "app-pass");
    assert!(!config.auto_sync);
    let record = reloaded.record(SyncFile::Tasks);
    assert_eq!(record.version.as_deref(), Some("v1"));
    assert_eq!(record.etag.as_deref(), Some("\"v1\""));
    assert_eq!(record.hash.as_deref(), Some("h1"));
    assert_eq!(reloaded.record(SyncFile::Credentials).version, None);
}

#[test]
fn wrong_passphrase_marks_unavailable_without_overwriting() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    let path = dir.path().join("sync.vault");
    let before = std::fs::read(&path).unwrap();

    let mut other = new_store(&dir);
    let notice = other
        .init("a different passphrase")
        .unwrap()
        .expect("应有提示");
    assert!(notice.contains("无法读取"), "{notice}");
    assert!(other.config().is_none());
    assert_eq!(std::fs::read(&path).unwrap(), before, "原文件不得被修改");
    assert!(matches!(
        other.set_config(SyncConfig::new("x", "y", "z")),
        Err(AppError::Crypto(_))
    ));
}

#[test]
fn set_auto_sync_requires_a_config() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    assert!(store.set_auto_sync(true).is_err());

    store.set_config(config()).unwrap();
    store.set_auto_sync(true).unwrap();
    assert!(store.config().unwrap().auto_sync);
}

#[test]
fn clear_removes_the_file_and_config_records() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    store.clear().unwrap();
    assert!(store.config().is_none());
    assert!(!dir.path().join("sync.vault").exists(), "清除后文件应删除");
}

#[test]
fn clear_is_idempotent_and_allows_reconfiguring() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.clear().unwrap();
    store.set_config(config()).unwrap();
    assert!(store.config().is_some());
}

#[test]
fn rekey_changes_the_passphrase() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    store.rekey("new passphrase").unwrap();

    let mut old = new_store(&dir);
    assert!(old.init(PASSPHRASE).unwrap().is_some(), "旧口令应无法读取");
    let mut new = new_store(&dir);
    new.init("new passphrase").unwrap();
    assert!(new.config().is_some(), "新口令应可读取");
}

#[test]
fn rekey_is_a_noop_when_never_configured() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.rekey("new passphrase").unwrap();
    assert!(
        !dir.path().join("sync.vault").exists(),
        "从未设定时换钥不应建立文件"
    );
}

#[test]
fn rekey_is_skipped_when_the_store_is_unavailable() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    let path = dir.path().join("sync.vault");
    let before = std::fs::read(&path).unwrap();

    // 以錯誤口令載入 → 標記不可用（原檔不被修改）。
    let mut other = new_store(&dir);
    other.init("wrong passphrase").unwrap();

    // 換口令不得因同步檔不可用而失敗，也不得改動原檔。
    other.rekey("new passphrase").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before, "原檔不得被修改");
}

#[test]
fn debug_does_not_leak_credentials() {
    let config = SyncConfig::new("https://dav.example/dav/", "secret-account", "secret-pass");
    let text = format!("{config:?}");
    assert!(text.contains("dav.example"), "{text}");
    assert!(!text.contains("secret-account"), "{text}");
    assert!(!text.contains("secret-pass"), "{text}");
}

#[test]
fn validate_accepts_a_well_formed_https_config() {
    assert!(validate(&config()).is_ok());
    // 大小寫不影響判定，前後空白由設定本身修剪。
    assert!(validate(&SyncConfig::new("HTTPS://dav.example/dav/", "u", "p")).is_ok());
}

/// 明文 HTTP 會讓 HTTP Basic 的帳號與應用密碼在網路上裸奔：一律拒絕。
#[test]
fn validate_rejects_plain_http_and_other_schemes() {
    for url in [
        "http://dav.example/dav/",
        "HTTP://dav.example/dav/",
        "ftp://dav.example/dav/",
        "dav.example/dav/",
    ] {
        let bad = SyncConfig::new(url, "u@example.com", "app-pass");
        let err = validate(&bad).expect_err("非 https 应被拒绝");
        assert!(err.to_string().contains("https://"), "{err}");
    }
}

#[test]
fn validate_rejects_missing_account_or_password() {
    let no_account = SyncConfig::new("https://dav.example/dav/", "   ", "app-pass");
    assert!(validate(&no_account).is_err());

    let no_password = SyncConfig::new("https://dav.example/dav/", "u@example.com", "");
    assert!(validate(&no_password).is_err());
}
