//! 驗證碼圖片取得與暫存。
//!
//! 每次請求驗證碼端點都會產生新圖，因此呼叫方必須自行快取：
//! 取得後顯示檔案路徑供使用者查看，直到重新整理或本次登入結束。

use std::path::{Path, PathBuf};

use crate::error::AppResult;
use crate::http::{HttpClient, HttpRequest};
use crate::io;

/// 統一認證的驗證碼端點。
pub const CAPTCHA_URL: &str = "https://login.xjtu.edu.cn/cas/captcha.jpg";

/// 取得驗證碼圖片並寫入 0600 權限的暫存檔，回傳檔案路徑。
pub fn fetch(client: &dyn HttpClient) -> AppResult<PathBuf> {
    let response = client.send(HttpRequest::get(CAPTCHA_URL))?;
    response.error_for_status()?;

    let path = io::captcha_path()?;
    io::write_private_atomic(&path, &response.body)?;
    Ok(path)
}

/// 刪除暫存的驗證碼圖片（不存在時視為成功）。
pub fn remove(path: &Path) -> AppResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}
