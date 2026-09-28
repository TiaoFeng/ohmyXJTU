//! 帳戶設定彈窗（`Ctrl+P`）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::tui::app::{App, SettingsState};
use crate::tui::theme::THEME;

/// 繪製設定選單。
pub fn draw(frame: &mut Frame, app: &App, state: SettingsState) {
    let area = crate::tui::ui::centered_rect(frame.area(), 60, 12);
    frame.render_widget(Clear, area);

    let block = THEME.popup_block("账户设置");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).split(inner);

    let mut lines: Vec<Line> = Vec::new();
    for index in 0..SettingsState::COUNT {
        let selected = index == state.index;
        let cursor = if selected { "▍" } else { " " };
        let style = if selected {
            THEME.highlight_style()
        } else {
            THEME.muted_style()
        };
        let mut line = vec![Span::styled(
            format!(" {cursor} {}", SettingsState::label(index)),
            style,
        )];
        if index == 2 {
            let policy = state.policy(app.access_policy);
            let value = if state.saving {
                format!("  < {} > 保存中…", policy.label())
            } else {
                format!("  < {} >", policy.label())
            };
            line.push(Span::styled(
                value,
                if selected {
                    THEME.highlight_style()
                } else {
                    THEME.accent_style()
                },
            ));
        }
        lines.push(Line::from(line));
    }

    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑/↓ 选择 · ←/→ 调整访问模式 · enter 确认 · esc 关闭",
            THEME.muted_style().add_modifier(Modifier::DIM),
        )))
        .style(THEME.surface_style()),
        chunks[1],
    );
}
