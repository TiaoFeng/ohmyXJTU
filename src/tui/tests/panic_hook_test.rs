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
    assert!(!message.contains('\n'), "訊息必須是單行");
}
