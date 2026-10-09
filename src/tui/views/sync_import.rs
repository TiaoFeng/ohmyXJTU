//! 登入畫面的「從堅果雲導入」。

use ratatui::Frame;

use crate::tui::app::SyncImportState;

/// 繪製導入畫面（沿用表單版面；測試結果／錯誤顯示在欄位上方）。
pub fn draw(frame: &mut Frame, state: &SyncImportState) {
    crate::tui::ui::draw_form(
        frame,
        &state.form,
        "从坚果云导入",
        "tab 切换字段 · ^t 测试连接 · enter/^s 导入 · esc 返回（仅支持 https）",
        state.message.as_deref(),
    );
}
