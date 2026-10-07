//! panic hook 訊息測試：單行、含位置、不含 panic 內容。

use super::*;

#[test]
fn panic_message_includes_location_without_payload() {
    let message = panic_hook_message(Some(("src/tui/views/content.rs", 64)));
    assert_eq!(
        message,
        "错误：程序发生内部错误（src/tui/views/content.rs:64），已退出。"
    );
}

#[test]
fn panic_message_without_location_is_single_line() {
    let message = panic_hook_message(None);
    assert_eq!(message, "错误：程序发生内部错误，已退出。");
    assert!(!message.contains('\n'), "信息必须是单行");
}

/// panic hook 必須在結束行程前清掉驗證碼暫存檔。
///
/// hook 會直接結束行程，無法在同一個行程內驗證；因此讓同一個測試執行檔以子
/// 行程再跑一次這個測試（帶上 `CHILD_ENV`），由子行程觸發 panic 後檢查暫存檔
/// 是否已被刪除。`exit(101)` 同時也是 panic 的預設結束碼，因此另外斷言 stderr
/// 是 hook 的訊息（而不是 panic payload），確保真的是 hook 跑過。
#[test]
fn panic_hook_removes_the_captcha_file() {
    use std::process::Command;

    /// 子行程專用：設定後這個測試會安裝 hook 並刻意 panic。
    const CHILD_ENV: &str = "OHMYXJTU_PANIC_HOOK_TEST_CHILD";
    /// 這個測試在子行程中的完整名稱（`--exact` 需要完全相符）。
    const TEST_NAME: &str = "tui::panic_hook_test::panic_hook_removes_the_captcha_file";

    if std::env::var_os(CHILD_ENV).is_some() {
        install_panic_hook();
        panic!("子行程刻意触发的 panic");
    }

    let dir = tempfile::TempDir::new().expect("建立暂存目录");
    let captcha = dir.path().join("ohmyXJTU").join("captcha.png");
    std::fs::create_dir_all(captcha.parent().expect("父目录")).expect("建立数据目录");
    std::fs::write(&captcha, b"png-bytes").expect("写入测试文件");

    let output = Command::new(std::env::current_exe().expect("测试执行档路径"))
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env("XDG_DATA_HOME", dir.path())
        .env(CHILD_ENV, "1")
        .output()
        .expect("启动子行程");

    assert_eq!(output.status.code(), Some(101), "hook 应以 101 结束行程");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("已退出"),
        "应输出 hook 的单行信息：{stderr}"
    );
    assert!(
        !stderr.contains("子行程刻意触发的 panic"),
        "hook 不得输出 panic 内容：{stderr}"
    );
    assert!(!captcha.exists(), "panic 后验证码暂存档应被删除");
}
