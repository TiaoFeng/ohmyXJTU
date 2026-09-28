//! 學期選擇器彈窗：考勤不可用且沒有歷史選擇時，由使用者指定要查看的學期。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use crate::tui::app::TermPickerState;
use crate::tui::theme::THEME;

/// 繪製學期選擇器。
pub fn draw(frame: &mut Frame, state: &TermPickerState) {
    let area = crate::tui::ui::centered_rect(frame.area(), 62, 16);
    frame.render_widget(Clear, area);

    let block = THEME.popup_block("选择学期");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", state.reason),
            THEME.muted_style(),
        )))
        .style(THEME.surface_style())
        .wrap(Wrap { trim: true }),
        chunks[0],
    );

    let mut lines: Vec<Line> = Vec::new();
    for (index, option) in state.options.iter().enumerate() {
        let selected = index == state.index;
        let cursor = if selected { "▍" } else { " " };
        let mut label = option.label();
        if state.suggestion == Some(*option) {
            label.push_str("（建议）");
        }
        let style = if selected {
            THEME.highlight_style()
        } else {
            THEME.muted_style()
        };
        lines.push(Line::from(Span::styled(
            format!(" {cursor} {label}"),
            style,
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ 选择 · enter 确定 · esc 取消",
            THEME.muted_style().add_modifier(Modifier::DIM),
        )))
        .style(THEME.surface_style()),
        chunks[2],
    );
}
