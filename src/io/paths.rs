//! 應用程式資料目錄與檔案路徑解析。

use std::path::PathBuf;

use crate::error::{AppError, AppResult};

use super::secure_file::create_private_dir;

/// 應用程式資料目錄名稱（位於使用者的標準資料目錄之下）。
pub const APP_DIR_NAME: &str = "ohmyXJTU";

/// 憑證檔檔名。
pub const VAULT_FILE_NAME: &str = "credentials.vault";

/// 設定檔檔名。
pub const CONFIG_FILE_NAME: &str = "config.json";

/// 驗證碼圖片檔名（暫存，每次取得時覆寫）。
pub const CAPTCHA_FILE_NAME: &str = "captcha.png";

/// 自訂義任務檔檔名（以使用者的加密口令加密，非明文）。
pub const TASKS_FILE_NAME: &str = "tasks.vault";

/// 堅果雲同步設定檔檔名（WebDAV 設定與同步記錄，以加密口令加密）。
pub const SYNC_FILE_NAME: &str = "sync.vault";

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

/// 自訂義任務檔路徑。
pub fn tasks_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join(TASKS_FILE_NAME))
}

/// 堅果雲同步設定檔路徑。
pub fn sync_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join(SYNC_FILE_NAME))
}

/// 驗證碼圖片路徑（不建立資料目錄；供程式結束時清理使用）。
pub fn captcha_path_no_create() -> Option<PathBuf> {
    dirs::data_dir().map(|base| base.join(APP_DIR_NAME).join(CAPTCHA_FILE_NAME))
}
