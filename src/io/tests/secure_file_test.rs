//! 私有檔案與目錄權限測試（`0700`／`0600` 僅 Unix 有實際語意）。

use super::*;

/// 寫入目標的父目錄不存在時，應由原子寫入自行建立為 `0700`。
///
/// 生產路徑都會先經 `paths::data_dir()`，但「任何呼叫端忘了先建目錄」不該
/// 留下權限過寬的目錄——那會讓同機其他使用者能列出檔名（例如觀察驗證碼圖片
/// 的出現與消失）。
#[cfg(unix)]
#[test]
fn write_private_atomic_creates_a_private_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("临时目录");
    let target = root.path().join("nested").join("config.json");

    write_private_atomic(&target, b"{}").expect("写入应当成功");

    let dir = std::fs::metadata(target.parent().expect("父目录")).expect("目录元数据");
    assert_eq!(dir.permissions().mode() & 0o777, 0o700, "新建目录应为 0700");
    let file = std::fs::metadata(&target).expect("文件元数据");
    assert_eq!(file.permissions().mode() & 0o777, 0o600, "文件仍为 0600");
}

/// 已存在的目錄不改權限：那可能是使用者的安排，程式只保證自己建立的部分。
#[cfg(unix)]
#[test]
fn create_private_dir_keeps_an_existing_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("shared");
    std::fs::create_dir(&dir).expect("建立目录");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("设定权限");

    create_private_dir(&dir).expect("已存在的目录应当直接通过");

    let mode = std::fs::metadata(&dir)
        .expect("目录元数据")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755, "不得擅自修改已存在目录的权限");
}

/// 多層路徑一次建立時，最終目錄仍是 `0700`。
#[cfg(unix)]
#[test]
fn create_private_dir_builds_nested_paths() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("临时目录");
    let dir = root.path().join("a").join("b");

    create_private_dir(&dir).expect("建立应当成功");

    assert!(dir.is_dir(), "目录应当被建立");
    let mode = std::fs::metadata(&dir)
        .expect("目录元数据")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}
