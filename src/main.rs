//! 二進位入口：僅負責啟動應用程式並回報錯誤。

fn main() {
    if let Err(err) = ohmy_xjtu::run() {
        eprintln!("错误：{err}");
        std::process::exit(1);
    }
}
