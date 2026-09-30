//! 設定檔測試：旧版相容（缺少新欄位）與协议同意版本語意。

use crate::config::Config;

#[test]
fn legacy_config_without_privacy_version_parses() {
    let json = br#"{"visitor_id":"aabbccddeeff00112233445566778899","access_policy":"auto"}"#;
    let config: Config = serde_json::from_slice(json).expect("旧版设置应可解析");
    assert_eq!(config.privacy_version, None);
    assert!(!config.privacy_accepted("1.1"));
}

#[test]
fn privacy_version_round_trips_through_json() {
    let config = Config {
        privacy_version: Some("1.1".to_owned()),
        ..Config::default()
    };
    let bytes = serde_json::to_vec(&config).expect("序列化设置");
    let back: Config = serde_json::from_slice(&bytes).expect("反序列化设置");
    assert_eq!(back.privacy_version.as_deref(), Some("1.1"));
    assert!(back.privacy_accepted("1.1"));
}

#[test]
fn privacy_accepted_requires_exact_version() {
    let mut config = Config::default();
    assert!(!config.privacy_accepted("1.1"), "未记录时不应视为已同意");
    config.privacy_version = Some("1.0".to_owned());
    assert!(!config.privacy_accepted("1.1"), "旧版本记录应要求重新同意");
    assert!(config.privacy_accepted("1.0"));
}
