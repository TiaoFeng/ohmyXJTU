//! 驗證碼暫存檔清理測試。

use std::fs;

use tempfile::TempDir;

use super::*;

#[test]
fn cleanup_at_removes_existing_file() {
    let dir = TempDir::new().expect("建立暂存目录");
    let path = dir.path().join("captcha.png");
    fs::write(&path, b"png-bytes").expect("写入测试文件");

    cleanup_at(Some(path.as_path()));

    assert!(!path.exists(), "文件应被删除");
}

#[test]
fn cleanup_at_tolerates_missing_file_and_none() {
    let dir = TempDir::new().expect("建立暂存目录");
    let missing = dir.path().join("not-there.png");

    // 檔案不存在或未提供路徑時都不得 panic。
    cleanup_at(Some(missing.as_path()));
    cleanup_at(None);
}

#[test]
fn captcha_path_without_create_ends_with_captcha_file_name() {
    if let Some(path) = crate::io::paths::captcha_path_no_create() {
        assert!(
            path.ends_with("ohmyXJTU/captcha.png"),
            "路径应以 ohmyXJTU/captcha.png 结尾：{path:?}"
        );
    }
}
