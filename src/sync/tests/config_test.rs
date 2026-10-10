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

/// 同步層以字串拼接把固定檔名接在位址之後：查詢字串、片段或 userinfo 都會讓
/// 請求打到錯誤的目標（或讓憑證混進 URL），一律拒絕。
#[test]
fn validate_rejects_urls_with_query_fragment_or_userinfo() {
    for url in [
        "https://dav.example/dav/?x=1",
        "https://dav.example/dav/#frag",
        "https://user:pass@dav.example/dav/",
        "https://user@dav.example/dav/",
        "https://",
    ] {
        let bad = SyncConfig::new(url, "u@example.com", "app-pass");
        assert!(validate(&bad).is_err(), "不应接受非基础路径的地址：{url}");
    }
}

/// 單純的基礎路徑照常通過（尾斜線有無皆可）。
#[test]
fn validate_accepts_a_bare_base_path() {
    for url in [
        "https://dav.jianguoyun.com/dav/",
        "https://dav.jianguoyun.com/dav",
    ] {
        assert!(
            validate(&SyncConfig::new(url, "u", "p")).is_ok(),
            "基础路径应通过：{url}"
        );
    }
}

#[test]
fn validate_rejects_missing_account_or_password() {
    let no_account = SyncConfig::new("https://dav.example/dav/", "   ", "app-pass");
    assert!(validate(&no_account).is_err());

    let no_password = SyncConfig::new("https://dav.example/dav/", "u@example.com", "");
    assert!(validate(&no_password).is_err());
}

/// 換伺服器或帳號要作廢同步記錄：它們指的是另一個雲端上的版本。
///
/// 留著舊記錄會讓新目標的比對失真——新伺服器上「沒有這個檔案」會被誤判成
/// 「遠端沒變」，本機指紋又與記錄相符，於是「立即同步」什麼都不做。
#[test]
fn changing_the_target_drops_the_records() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    let record = |version: &str| FileRecord {
        version: Some(version.to_owned()),
        etag: Some(format!("\"{version}\"")),
        hash: Some(format!("h-{version}")),
    };
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    store.set_record(SyncFile::Tasks, record("v1")).unwrap();

    // 只換應用密碼：同一個目標，記錄保留（否則每次輪替密碼都要重新上傳）。
    store
        .set_config(SyncConfig::new(
            "https://dav.jianguoyun.com/dav/",
            "a@b.com",
            "rotated-pass",
        ))
        .unwrap();
    assert_eq!(
        store.record(SyncFile::Tasks).version.as_deref(),
        Some("v1"),
        "只换应用密码不该清空记录"
    );

    // 換位址：兩個檔案的記錄都作廢。
    store
        .set_config(SyncConfig::new(
            "https://dav.example/dav/",
            "a@b.com",
            "rotated-pass",
        ))
        .unwrap();
    assert_eq!(
        store.record(SyncFile::Tasks),
        &FileRecord::default(),
        "换服务器应清空记录"
    );

    // 換帳號：同樣作廢。
    store.set_record(SyncFile::Tasks, record("v2")).unwrap();
    store
        .set_config(SyncConfig::new(
            "https://dav.example/dav/",
            "other@example.com",
            "rotated-pass",
        ))
        .unwrap();
    assert_eq!(
        store.record(SyncFile::Tasks),
        &FileRecord::default(),
        "换账号应清空记录"
    );
    assert_eq!(store.record(SyncFile::Credentials), &FileRecord::default());

    // 清空後仍可正常保存並讀回。
    let mut reloaded = new_store(&dir);
    reloaded.init(PASSPHRASE).unwrap();
    assert_eq!(reloaded.config().unwrap().account, "other@example.com");
    assert_eq!(reloaded.record(SyncFile::Tasks).version, None);
}

/// 鎖定（下載後重新鎖定／會話停用）要一併丟棄記憶體中的設定與記錄。
///
/// `SyncConfig` 帶著堅果雲應用密碼，金鑰都丟了就沒有留著它的理由；設定檔仍在
/// 磁碟上，重新解鎖時會再讀回來。
#[test]
fn lock_drops_the_config_and_the_key() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    let path = dir.path().join("sync.vault");
    let before = std::fs::read(&path).unwrap();

    store.lock();
    assert!(store.config().is_none(), "锁定时应丢弃内存中的设定");
    assert!(
        matches!(store.set_config(config()), Err(AppError::Crypto(_))),
        "锁定后不得再写入"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "锁定时不得改动原档");

    // 重新解鎖（新工作階段）後讀回設定，寫入恢復。
    store.init(PASSPHRASE).unwrap();
    assert!(store.config().is_some(), "重新解锁后应读回设定");
    store.set_auto_sync(true).unwrap();
}

/// 金鑰已丟棄時拒絕換鑰：記憶體中的內容已被清空，重新加密落盤會清掉原檔。
#[test]
fn rekey_is_refused_after_the_key_was_dropped() {
    let dir = tempdir().unwrap();
    let mut store = new_store(&dir);
    store.init(PASSPHRASE).unwrap();
    store.set_config(config()).unwrap();
    let path = dir.path().join("sync.vault");
    let before = std::fs::read(&path).unwrap();

    store.lock();
    assert!(
        matches!(store.rekey("new passphrase"), Err(AppError::Crypto(_))),
        "锁定后不得换钥"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "被拒绝的换钥不得改动原档"
    );

    // 原檔仍以舊口令可讀。
    let mut reloaded = new_store(&dir);
    reloaded.init(PASSPHRASE).unwrap();
    assert!(reloaded.config().is_some(), "原档应仍可读取");
}
