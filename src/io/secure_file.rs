//! 私有檔案的讀寫：限制權限為 0600，並以「同目錄暫存檔 + rename」保證原子性。

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::error::{AppError, AppResult};

/// 讀取檔案內容。
pub fn read_private(path: &Path) -> AppResult<Vec<u8>> {
    Ok(fs::read(path)?)
}

/// 建立私有目錄（權限 0700）；目錄已存在時不變更其權限。
///
/// 目錄權限與檔案權限分開處理：檔案寫入會收緊為 0600（見
/// [`ensure_private`]），目錄則只在「由本程式建立」時設定 0700——已存在的
/// 目錄可能是使用者自己的安排，不由程式擅自修改。
///
/// 只設定**最終**目錄：上層目錄（例如 `~/.local/share`）不屬於本程式，不該
/// 被一併收緊。
pub fn create_private_dir(dir: &Path) -> AppResult<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 已寫好但尚未覆蓋目標的暫存檔（見 [`stage_private`]）。
pub struct StagedWrite {
    tmp: NamedTempFile,
    target: PathBuf,
}

/// 原子地覆寫檔案，並將權限限制為僅擁有者可讀寫。
///
/// 內容先寫入同目錄的暫存檔，`fsync` 後再 rename 覆蓋目標，
/// 因此寫入過程中失敗時，原有檔案內容不會被破壞。
///
/// 目標目錄不存在時一併建立為 0700（見 [`create_private_dir`]）：任何呼叫端
/// 都不會因為忘了先建目錄而留下權限過寬的目錄。
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    stage_private(path, bytes)?.commit()
}

/// 階段一：把內容寫入目標同目錄的暫存檔並 `fsync`，但**不**覆蓋目標。
///
/// 供「多個檔案要一起更新」的呼叫端使用：先把所有目標 stage 成功，再逐一
/// [`StagedWrite::commit`]。磁碟已滿、權限等寫入失敗都集中在此階段，此時所有
/// 目標檔案都還維持原狀；落盤只剩同目錄 `rename` 這一段。
pub fn stage_private(path: &Path, bytes: &[u8]) -> AppResult<StagedWrite> {
    let dir = parent_dir(path)?;
    create_private_dir(dir)?;

    let mut tmp = NamedTempFile::new_in(dir)?;
    restrict_permissions(tmp.path())?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    Ok(StagedWrite {
        tmp,
        target: path.to_owned(),
    })
}

impl StagedWrite {
    /// 階段二：`rename` 覆蓋目標、收緊權限，並盡力同步目錄項。
    ///
    /// `rename` 的結果要在中繼資料落盤後才保證可見；部分平台不支援對目錄開啟
    /// 檔案（例如 Windows），同步目錄失敗一律忽略。
    pub fn commit(self) -> AppResult<()> {
        let dir = parent_dir(&self.target)?.to_owned();
        let target = self.target;
        self.tmp
            .persist(&target)
            .map_err(|err| AppError::Io(err.error))?;
        restrict_permissions(&target)?;
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
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

#[cfg(test)]
#[path = "tests/secure_file_test.rs"]
mod secure_file_test;
