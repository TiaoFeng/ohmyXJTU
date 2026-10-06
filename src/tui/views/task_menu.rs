//! 任務相關的彈窗：任務設置（`^T`）、多選批量操作與刪除確認。
//!
//! 三者都沿用 [`crate::tui::ui::popup_surface`] 的不透明彈窗契約（先鋪底再
//! 畫粉色邊框），與帳戶設定、學期選擇器的外觀一致。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::app::{
    TaskBatchMenuState, TaskBatchOp, TaskConfirmState, TaskMenuKind, TaskMenuState,
};
use crate::tui::theme::THEME;
use crate::tui::ui::{centered_rect, hint_line, menu_item, popup_surface};

/// 任務設置彈窗（`^T`）尺寸（含邊框）。
const MENU_WIDTH: u16 = 44;
const MENU_HEIGHT: u16 = 8;
/// 批量操作彈窗尺寸（含邊框）。
const BATCH_WIDTH: u16 = 44;
const BATCH_HEIGHT: u16 = 9;
/// 刪除已完成任務的確認彈窗尺寸（含邊框）。
const CONFIRM_WIDTH: u16 = 52;
const CONFIRM_HEIGHT: u16 = 7;
/// 選單選項區的最小高度（其餘列留給提示列）。
const MENU_BODY_MIN_HEIGHT: u16 = 3;
/// 確認彈窗提問區的最小高度。
const CONFIRM_BODY_MIN_HEIGHT: u16 = 1;
/// 彈窗提示列的高度。
const HINT_HEIGHT: u16 = 1;
/// 確認彈窗提示區的高度（多留一列與提問隔開）。
const CONFIRM_HINT_HEIGHT: u16 = 2;

/// 繪製任務設置彈窗（`^T`）。
pub fn draw_menu(frame: &mut Frame, state: TaskMenuState) {
    draw_choice_menu(
        frame,
        MENU_WIDTH,
        MENU_HEIGHT,
        "任务设置",
        TaskMenuKind::ALL.iter().map(|kind| kind.label()),
        state.index,
        "↑/↓ 选择 · enter 确定 · esc 关闭",
    );
}

/// 繪製多選後的批量操作選單。
pub fn draw_batch_menu(frame: &mut Frame, state: TaskBatchMenuState) {
    draw_choice_menu(
        frame,
        BATCH_WIDTH,
        BATCH_HEIGHT,
        "批量操作",
        TaskBatchOp::ALL.iter().map(|op| op.label()),
        state.index,
        "↑/↓ 选择 · enter 确定 · esc 返回",
    );
}

/// 繪製「單欄選項」彈窗：一列一個選項，底部一列按鍵提示。
///
/// 任務設置與批量操作只有選項來源與文案不同，版面完全一樣（二次確認多了提問列，
/// 因此不共用）。標籤由呼叫端以迭代器提供，不必先配置一份切片。
fn draw_choice_menu<'a>(
    frame: &mut Frame,
    width: u16,
    height: u16,
    title: &str,
    labels: impl Iterator<Item = &'a str>,
    index: usize,
    hint: &str,
) {
    let area = centered_rect(frame.area(), width, height);
    let inner = popup_surface(frame, area, title);
    let chunks = Layout::vertical([
        Constraint::Min(MENU_BODY_MIN_HEIGHT),
        Constraint::Length(HINT_HEIGHT),
    ])
    .split(inner);

    let lines: Vec<Line> = labels
        .enumerate()
        .map(|(position, label)| menu_item(position == index, label))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(hint_line(hint)).style(THEME.surface_style()),
        chunks[1],
    );
}

/// 繪製刪除已完成任務的二次確認（`y` 確認、`n`／`esc` 取消）。
pub fn draw_confirm(frame: &mut Frame, state: TaskConfirmState) {
    let area = centered_rect(frame.area(), CONFIRM_WIDTH, CONFIRM_HEIGHT);
    let inner = popup_surface(frame, area, "确认删除");
    let chunks = Layout::vertical([
        Constraint::Min(CONFIRM_BODY_MIN_HEIGHT),
        Constraint::Length(CONFIRM_HINT_HEIGHT),
    ])
    .split(inner);

    let question = Line::from(vec![
        Span::styled(" 删除全部 ", THEME.surface_style()),
        Span::styled(state.count.to_string(), THEME.accent_style()),
        Span::styled(" 个已完成任务？", THEME.surface_style()),
    ]);
    frame.render_widget(Paragraph::new(question), chunks[0]);
    frame.render_widget(
        Paragraph::new(hint_line("y 确认 · n/esc 取消")).style(THEME.surface_style()),
        chunks[1],
    );
}
