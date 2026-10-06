//! 帳戶設定彈窗（`Ctrl+P`）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, SettingsState};
use crate::tui::theme::THEME;
use crate::tui::ui::{hint_line, menu_item};

/// 彈窗寬度。
const SETTINGS_WIDTH: u16 = 60;
/// 彈窗高度。
const SETTINGS_HEIGHT: u16 = 12;

/// 繪製設定選單。
pub fn draw(frame: &mut Frame, app: &App, state: SettingsState) {
    let area = crate::tui::ui::centered_rect(frame.area(), SETTINGS_WIDTH, SETTINGS_HEIGHT);
    let inner = crate::tui::ui::popup_surface(frame, area, "账户设置");

    let chunks = Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).split(inner);

    let mut lines: Vec<Line> = Vec::new();
    for index in 0..SettingsState::COUNT {
        let selected = index == state.index;
        let mut line = menu_item(selected, SettingsState::label(index));
        if index == SettingsState::POLICY_INDEX {
            let policy = state.policy(app.access_policy);
            let value = if state.saving {
                format!("  < {} > 保存中…", policy.label())
            } else {
                format!("  < {} >", policy.label())
            };
            line.spans.push(Span::styled(
                value,
                if selected {
                    THEME.highlight_style()
                } else {
                    THEME.accent_style()
                },
            ));
        }
        lines.push(line);
    }

    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(hint_line(
            "↑/↓ 选择 · ←/→ 调整访问模式 · enter 确认 · esc 关闭",
        ))
        .style(THEME.surface_style()),
        chunks[1],
    );
}
