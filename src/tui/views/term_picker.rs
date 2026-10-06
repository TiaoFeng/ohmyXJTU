//! 學期選擇器彈窗：考勤不可用且沒有歷史選擇時，由使用者指定要查看的學期。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::app::TermPickerState;
use crate::tui::theme::THEME;
use crate::tui::ui::{hint_line, menu_item};

/// 彈窗寬度。
const PICKER_WIDTH: u16 = 62;
/// 彈窗高度。
const PICKER_HEIGHT: u16 = 16;

/// 繪製學期選擇器。
pub fn draw(frame: &mut Frame, state: &TermPickerState) {
    let area = crate::tui::ui::centered_rect(frame.area(), PICKER_WIDTH, PICKER_HEIGHT);
    let inner = crate::tui::ui::popup_surface(frame, area, "选择学期");

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
        let mut label = option.label();
        if state.suggestion == Some(*option) {
            label.push_str("（建议）");
        }
        lines.push(menu_item(selected, label));
    }
    frame.render_widget(
        Paragraph::new(lines).style(THEME.surface_style()),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(hint_line("↑/↓ 选择 · enter 确定 · esc 取消")).style(THEME.surface_style()),
        chunks[2],
    );
}
