//! 以系統預設瀏覽器開啟網址。
//!
//! 只允許校內網域的 `https` 網址；命令與參數逐一傳遞（`Command`），不經過
//! shell 字串拼接，避免注入。實際的 URL 來源是伺服器回應或既定常數
//! （例如思源學堂首頁），不拼接未經驗證的路徑。

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use url::Url;

use crate::error::{AppError, AppResult};
use crate::webvpn::is_school_host;

/// 啟動後等待啟動器結束的寬限時間。
///
/// `xdg-open` 之類的啟動器失敗時會很快以非零碼結束；成功時通常立刻返回，
/// 少數實作會一直等到瀏覽器關閉。等待上限即為此值，逾時後視為啟動成功並
/// 交由背景執行緒回收。
const LAUNCH_GRACE: Duration = Duration::from_millis(200);

/// 以系統預設瀏覽器開啟網址（不等待程序結束）。
///
/// 開啟前先經 [`ensure_openable`] 檢驗目標（所有平台路徑都經 [`command_for`]）。
/// 三個標準串流都接到 null：`xdg-open` 之類的啟動器會把訊息寫到 stdout／stderr，
/// 直接繼承會打亂 TUI 畫面。子行程交由獨立執行緒回收，避免每開一次就累積殭屍。
pub fn open_url(url: &str) -> AppResult<()> {
    let (program, args) = command_for(std::env::consts::OS, url)?;
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| AppError::protocol(format!("无法启动浏览器：{err}")))?;

    reap_launcher(child)
}

/// 確認啟動器是否立即失敗；仍在執行時交由背景執行緒回收。
///
/// `spawn` 成功只代表找到執行檔：沒有瀏覽器的環境下，啟動器會以非零碼結束
/// （例如 `xdg-open: no method available`）。短暫等待以攔截這種立即失敗，
/// 避免介面誤報「已開啟」。
fn reap_launcher(mut child: Child) -> AppResult<()> {
    let deadline = Instant::now() + LAUNCH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(AppError::protocol(format!(
                        "浏览器启动失败（退出码 {}）",
                        status
                            .code()
                            .map_or_else(|| "未知".to_owned(), |code| code.to_string())
                    )))
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(AppError::protocol(format!("无法确认浏览器是否启动：{err}"))),
        }
    }

    // 仍在執行：由獨立執行緒回收，避免子行程結束後成為殭屍。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// 依平台組出開啟網址的系統命令（供測試檢驗程式與參數）。
///
/// 這是所有開啟路徑的必經點，因此 [`ensure_openable`] 的檢驗也放在這裡。
pub fn command_for(target_os: &str, url: &str) -> AppResult<(&'static str, Vec<String>)> {
    ensure_openable(url)?;

    let (program, args) = match target_os {
        "macos" => ("open", vec![url.to_owned()]),
        // `rundll32 url.dll,FileProtocolHandler` 直接交系統處理（ShellExecute），
        // **不經命令殼層**：`cmd /C start` 會解析 `%`（URL 的百分號編碼幾乎必然
        // 出現，會被當成環境變數展開）與 `&` 等符號，等於把網址當命令列處理。
        "windows" => (
            "rundll32.exe",
            vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
        ),
        // Linux 與其他 Unix 使用 xdg-open。
        _ => ("xdg-open", vec![url.to_owned()]),
    };
    Ok((program, args))
}

/// 確認網址可安全交給系統瀏覽器：必須是 `https`，且主機為校內網域。
///
/// 要開啟的網址可能來自伺服器（例如思源學堂的播放地址，附帶存取 token）：
/// 只允許校內主機，才不會在回應被篡改、後端出現開放重導或代理被污染時，
/// 把 token 與查詢參數送去第三方。`webvpn.xjtu.edu.cn` 本身也是校內主機，
/// 因此 WebVPN 模式沿用同一條規則（改寫後的網址仍會通過檢驗）。
///
/// 錯誤訊息只含主機名，不含完整網址（見 [`AppError::UntrustedUrl`]）。
pub fn ensure_openable(url: &str) -> AppResult<()> {
    let parsed = Url::parse(url).map_err(|_| AppError::protocol("网址无法解析，已阻止打开"))?;
    let host = parsed.host_str().unwrap_or_default();
    if parsed.scheme() != "https" {
        return Err(AppError::untrusted_url(host, "必须使用 https"));
    }
    if !is_school_host(host) {
        return Err(AppError::untrusted_url(host, "非校内网域"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/browser_test.rs"]
mod browser_test;
