//! 登入畫面的「從堅果雲導入」。

use ratatui::Frame;

use crate::tui::app::SyncImportState;

/// 繪製導入畫面（沿用表單版面；測試結果／確認提示／錯誤顯示在欄位上方）。
///
/// 確認步驟中的提示列只列出當下可用的按鍵：那時無法編輯欄位或測試連線。
pub fn draw(frame: &mut Frame, state: &SyncImportState) {
    let hint = if state.confirming {
        "enter 确认导入（将覆写本机文件）· esc 返回修改"
    } else {
        "tab 切换字段 · ^t 测试连接 · enter/^s 导入（需确认）· esc 返回（仅支持 https）"
    };
    crate::tui::ui::draw_form(
        frame,
        &state.form,
        "从坚果云导入",
        hint,
        state
            .message
            .as_ref()
            .map(|(text, tone)| (text.as_str(), *tone)),
    );
}
