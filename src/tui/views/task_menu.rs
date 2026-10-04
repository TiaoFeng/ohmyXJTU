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

/// 繪製任務設置彈窗（`^T`）。
pub fn draw_menu(frame: &mut Frame, state: TaskMenuState) {
    let area = centered_rect(frame.area(), 44, 8);
    let inner = popup_surface(frame, area, "任务设置");
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(inner);

    let lines: Vec<Line> = TaskMenuKind::ALL
        .iter()
        .enumerate()
        .map(|(index, kind)| menu_item(index == state.index, kind.label()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(hint_line("↑/↓ 选择 · enter 确定 · esc 关闭")).style(THEME.surface_style()),
        chunks[1],
    );
}

/// 繪製多選後的批量操作選單。
pub fn draw_batch_menu(frame: &mut Frame, state: TaskBatchMenuState) {
    let area = centered_rect(frame.area(), 44, 9);
    let inner = popup_surface(frame, area, "批量操作");
    let chunks = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(inner);

    let lines: Vec<Line> = TaskBatchOp::ALL
        .iter()
        .enumerate()
        .map(|(index, op)| menu_item(index == state.index, op.label()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(hint_line("↑/↓ 选择 · enter 确定 · esc 返回")).style(THEME.surface_style()),
        chunks[1],
    );
}

/// 繪製刪除已完成任務的二次確認（`y` 確認、`n`／`esc` 取消）。
pub fn draw_confirm(frame: &mut Frame, state: TaskConfirmState) {
    let area = centered_rect(frame.area(), 52, 7);
    let inner = popup_surface(frame, area, "确认删除");
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);

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
