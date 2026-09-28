//! 應用程式資料目錄與檔案路徑解析。

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// 應用程式資料目錄名稱（位於使用者的標準資料目錄之下）。
pub const APP_DIR_NAME: &str = "ohmyXJTU";

/// 憑證檔檔名。
pub const VAULT_FILE_NAME: &str = "credentials.vault";

/// 設定檔檔名。
pub const CONFIG_FILE_NAME: &str = "config.json";

/// 驗證碼圖片檔名（暫存，每次取得時覆寫）。
pub const CAPTCHA_FILE_NAME: &str = "captcha.png";

/// 回傳應用程式資料目錄，不存在時建立（權限 0700）。
pub fn data_dir() -> AppResult<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| AppError::config("无法定位用户数据目录"))?;
    let dir = base.join(APP_DIR_NAME);
    create_private_dir(&dir)?;
    Ok(dir)
}

/// 憑證檔路徑。
pub fn vault_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join(VAULT_FILE_NAME))
}

/// 設定檔路徑。
pub fn config_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join(CONFIG_FILE_NAME))
}

/// 驗證碼圖片路徑。
pub fn captcha_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join(CAPTCHA_FILE_NAME))
}

fn create_private_dir(dir: &Path) -> AppResult<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
