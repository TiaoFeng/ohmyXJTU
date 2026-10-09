//! 「坚果云同步」子選單（由帳戶設定中的「坚果云同步」開啟）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::tui::app::{App, SyncMenuAction, SyncMenuState};
use crate::tui::theme::THEME;
use crate::tui::ui::{centered_rect, hint_line, menu_item, popup_surface};

/// 彈窗寬度。
const WIDTH: u16 = 52;
/// 彈窗最大高度（含邊框）。
const MAX_HEIGHT: u16 = 14;
/// 選項區的最小高度。
const BODY_MIN_HEIGHT: u16 = 1;
/// 提示列高度。
const HINT_HEIGHT: u16 = 1;

/// 繪製同步子選單（疊在主畫面上）。
pub fn draw(frame: &mut Frame, app: &App, state: SyncMenuState) {
    let actions = SyncMenuAction::items(app.sync.configured);
    // 高度＝標題列與上下邊框（3）＋選項數＋提示列＋下邊框——以選項數為準。
    let height = u16::try_from(actions.len())
        .unwrap_or(MAX_HEIGHT)
        .saturating_add(5)
        .min(MAX_HEIGHT);
    let area = centered_rect(frame.area(), WIDTH, height);
    let inner = popup_surface(frame, area, "坚果云同步");
    let chunks = Layout::vertical([
        Constraint::Min(BODY_MIN_HEIGHT),
        Constraint::Length(HINT_HEIGHT),
    ])
    .split(inner);

    let lines: Vec<Line> = actions
        .iter()
        .enumerate()
        .map(|(position, action)| {
            menu_item(position == state.index, action.label(app.sync.auto_sync))
        })
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
