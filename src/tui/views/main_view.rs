//! 主畫面：左側導航＋右側內容＋底部提示列。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::tui::app::{App, LmsLevel, NavItem, Screen};
use crate::tui::theme::THEME;
use crate::tui::views::{content, settings};

/// 側邊欄寬度。
const SIDEBAR_WIDTH: u16 = 22;

/// 繪製主畫面。
pub fn draw(frame: &mut Frame, app: &mut App) {
    if crate::tui::ui::too_small(frame) {
        return;
    }

    let area = frame.area();
    let [body, footer] = Layout::vertical([Constraint::Min(8), Constraint::Length(1)]).areas(area);
    let [sidebar, content_area] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(30)]).areas(body);

    draw_sidebar(frame, sidebar, app);
    content::draw(frame, content_area, app);
    draw_footer(frame, footer, app);

    if let Screen::Settings(state) = &app.screen {
        settings::draw(frame, app, *state);
    }
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem<'static>> = NavItem::ALL
        .iter()
        .map(|nav| {
            let marker = if app.is_loading(*nav) { "…" } else { "" };
            let note = match app.nav == *nav {
                true => app.current_note().unwrap_or("").to_owned(),
                false => String::new(),
            };
            let note = if note.is_empty() {
                String::new()
            } else {
                format!(" {note}")
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<10}", nav.label()),
                    Style::default().fg(THEME.text),
                ),
                Span::styled(marker, THEME.muted_style()),
                Span::styled(note, THEME.muted_style()),
            ]))
        })
        .collect();

    app.nav_state.select(Some(app.nav.index()));
    let list = List::new(items)
        .block(THEME.block("ohmyXJTU"))
        .highlight_style(THEME.highlight_style());
    frame.render_stateful_widget(list, area, &mut app.nav_state);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let line = match app.message_text() {
        Some(message) => Line::from(Span::styled(
            format!(" {message}"),
            THEME.error_style().add_modifier(Modifier::BOLD),
        )),
        None => Line::from(Span::styled(
            format!(" {}", hints(app)),
            THEME.muted_style(),
        )),
    };
    frame.render_widget(Paragraph::new(line).style(THEME.base_style()), area);
}

fn hints(app: &App) -> String {
    let mode = app
        .access_mode
        .map_or("未登录".to_owned(), |mode| mode.label().to_owned());
    let mut text = format!(
        "[{mode} · {}]  q 退出  ←/→ 切换页面  ↑/↓ 选择  r 刷新  ^P 账户设置",
        app.access_policy.label()
    );

    match app.nav {
        NavItem::Attendance => text.push_str("  n/p 翻页"),
        NavItem::Lms => match app.lms.level {
            LmsLevel::Courses => text.push_str("  enter 进入课程"),
            LmsLevel::Activities => text.push_str("  enter 查看详情  esc 返回课程"),
            LmsLevel::Detail => text.push_str("  esc 返回活动"),
        },
        _ => {
            if app.schedule_detail || app.homework_detail {
                text.push_str("  enter 收起详情");
            } else {
                text.push_str("  enter 查看详情");
            }
        }
    }
    text
}

/// 供其他繪製函式使用的清單狀態包裝。
pub fn render_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'static>>,
    state: &mut ListState,
) {
    let list = List::new(items)
        .block(THEME.block(title))
        .highlight_style(THEME.highlight_style())
        .highlight_symbol("▍");
    frame.render_stateful_widget(list, area, state);
}
