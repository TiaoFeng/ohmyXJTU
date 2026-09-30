//! 二進位入口：僅負責啟動應用程式並回報錯誤。

fn main() {
    // 任何執行緒 panic 時都還原終端，避免畫面卡在原始模式。
    ohmy_xjtu::tui::install_panic_hook();

    let result = ohmy_xjtu::run();
    // 結束時清掉暫存的驗證碼圖片（不建立資料目錄，失敗忽略）。
    ohmy_xjtu::auth::captcha::cleanup_on_exit();

    if let Err(err) = result {
        eprintln!("错误：{err}");
        std::process::exit(1);
    }
}
