//! 私有檔案的讀寫：限制權限為 0600，並以「同目錄暫存檔 + rename」保證原子性。

use std::fs;
use std::io::Write as _;
use std::path::Path;

use tempfile::NamedTempFile;

use crate::error::{AppError, AppResult};

/// 讀取檔案內容。
pub fn read_private(path: &Path) -> AppResult<Vec<u8>> {
    Ok(fs::read(path)?)
}

/// 原子地覆寫檔案，並將權限限制為僅擁有者可讀寫。
///
/// 內容先寫入同目錄的暫存檔，`fsync` 後再 rename 覆蓋目標，
/// 因此寫入過程中失敗時，原有檔案內容不會被破壞。
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let dir = parent_dir(path)?;
    fs::create_dir_all(dir)?;

    let mut tmp = NamedTempFile::new_in(dir)?;
    restrict_permissions(tmp.path())?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|err| AppError::Io(err.error))?;
    restrict_permissions(path)?;
    // 盡力同步目錄項：`rename` 的結果要在中繼資料落盤後才保證可見。
    // 部分平台不支援對目錄開啟檔案（例如 Windows），失敗一律忽略。
    if let Ok(dir) = fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// 確保檔案權限僅擁有者可讀寫。
///
/// 回傳 `true` 代表原本權限過寬、已收緊為 0600（呼叫端可據此提示使用者）；
/// `false` 代表本來就合乎要求，或平台不支援（非 Unix 一律為 `false`）。
pub fn ensure_private(path: &Path) -> AppResult<bool> {
    #[cfg(unix)]
    {
        if is_private(path)? {
            return Ok(false);
        }
        restrict_permissions(path)?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

/// 將檔案權限收緊為 0600（僅 Unix 有效）。
pub fn restrict_permissions(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 判斷檔案是否僅擁有者可讀寫。
#[cfg(unix)]
pub fn is_private(path: &Path) -> AppResult<bool> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 == 0)
}

fn parent_dir(path: &Path) -> AppResult<&Path> {
    path.parent()
        .ok_or_else(|| AppError::config(format!("路径缺少父目录：{}", path.display())))
}
