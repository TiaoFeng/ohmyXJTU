//! 以系統預設瀏覽器開啟網址。
//!
//! 只允許 `http`/`https`；命令與參數逐一傳遞（`Command`），不經過 shell
//! 字串拼接，避免注入。實際的 URL 來源一律是伺服器回應或既定常數
//! （例如思源學堂首頁），不拼接未經驗證的路徑。

use std::process::Command;

use url::Url;

use crate::error::{AppError, AppResult};

/// 以系統預設瀏覽器開啟網址（不等待程序結束）。
pub fn open_url(url: &str) -> AppResult<()> {
    let (program, args) = command_for(std::env::consts::OS, url)?;
    Command::new(program)
        .args(args)
        .spawn()
        .map(|_child| ())
        .map_err(|err| AppError::protocol(format!("无法启动浏览器：{err}")))
}

/// 依平台組出開啟網址的系統命令（供測試檢驗程式與參數）。
pub fn command_for(target_os: &str, url: &str) -> AppResult<(&'static str, Vec<String>)> {
    if !is_http_url(url) {
        return Err(AppError::protocol("只能打开 http 或 https 网址"));
    }

    let (program, args) = match target_os {
        "macos" => ("open", vec![url.to_owned()]),
        // `start` 是 cmd 的內建指令：第一個參數是視窗標題（空字串），
        // 網址必須是另一個獨立參數。
        "windows" => (
            "cmd",
            vec![
                "/C".to_owned(),
                "start".to_owned(),
                String::new(),
                url.to_owned(),
            ],
        ),
        // Linux 與其他 Unix 使用 xdg-open。
        _ => ("xdg-open", vec![url.to_owned()]),
    };
    Ok((program, args))
}

/// 是否為可開啟的 http/https 網址。
fn is_http_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

#[cfg(test)]
#[path = "tests/browser_test.rs"]
mod browser_test;
