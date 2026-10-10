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
/// 彈窗高度上限（選項再多也不超過）。
const MAX_HEIGHT: u16 = 14;
/// 彈窗上下框線佔用的列數。
const CHROME_HEIGHT: u16 = 2;
/// 提示列高度。
const HINT_HEIGHT: u16 = 1;
/// 選項再多也要留一列。
const MIN_BODY_HEIGHT: u16 = 1;

/// 繪製同步子選單（疊在主畫面上）。
pub fn draw(frame: &mut Frame, app: &App, state: SyncMenuState) {
    let actions = SyncMenuAction::items(app.sync.configured, app.sync.unavailable);
    // 高度＝框線＋選項數＋提示列（選項數依設定狀態而變：只有「清除」時是 1）。
    let body_height = u16::try_from(actions.len()).unwrap_or(MAX_HEIGHT);
    let height = body_height
        .saturating_add(CHROME_HEIGHT + HINT_HEIGHT)
        .min(MAX_HEIGHT);
    let area = centered_rect(frame.area(), WIDTH, height);
    let inner = popup_surface(frame, area, "坚果云同步");
    let chunks = Layout::vertical([
        Constraint::Min(MIN_BODY_HEIGHT),
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
