//! 主畫面：左側導航＋右側內容＋底部提示列。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::tui::app::{App, LmsLevel, NavItem, Screen};
use crate::tui::text::display_width;
use crate::tui::theme::THEME;
use crate::tui::views::{content, settings, term_picker};

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
    if let Screen::TermPicker(state) = &app.screen {
        term_picker::draw(frame, state);
    }
}

/// 側邊欄載入指示燈的固定寬度（三個點＋一個空白）。
const LOADING_DOTS_WIDTH: usize = 4;

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    let inner_width = usize::from(area.width.saturating_sub(2));
    let items: Vec<ListItem<'static>> = NavItem::ALL
        .iter()
        .map(|nav| nav_item(*nav, app, inner_width))
        .collect();

    app.nav_state.select(Some(app.nav.index()));
    let list = List::new(items)
        .block(THEME.block("ohmyXJTU"))
        .highlight_style(THEME.highlight_style());
    frame.render_stateful_widget(list, area, &mut app.nav_state);
}

/// 側邊欄單列：載入指示燈（三點循環動畫）＋置中標籤。
///
/// 指示燈佔用固定寬度的前置欄位（未載入時為空白），因此載入狀態與動畫
/// 都不會讓標籤左右跳動；指示燈與標籤一起在側邊欄內框置中。
fn nav_item(nav: NavItem, app: &App, width: usize) -> ListItem<'static> {
    let label = nav.label();
    let label_width = display_width(label);
    let pad = width.saturating_sub(LOADING_DOTS_WIDTH + label_width) / 2;
    let dots = if app.is_loading(nav) {
        loading_dots(app.tick)
    } else {
        ""
    };
    // 選取列以 accent 為底，指示燈需改用深色才看得見。
    let dots_style = if app.nav == nav {
        Style::default().fg(THEME.base)
    } else {
        THEME.accent_style()
    };
    ListItem::new(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            format!("{dots:<width$}", width = LOADING_DOTS_WIDTH),
            dots_style,
        ),
        Span::styled(label.to_owned(), Style::default().fg(THEME.text)),
    ]))
}

/// 載入指示的三點動畫：每幀前進一格並循環，空相位形成閃爍。
fn loading_dots(tick: u64) -> &'static str {
    match tick % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
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
    let mut text = format!(
        "[{} · {}]  q 退出  ←/→ 切换页面  ↑/↓ 选择  r 刷新  ^P 账户设置",
        app.session_label(),
        app.access_policy.label()
    );

    match app.nav {
        NavItem::Attendance => text.push_str("  n/p 翻页"),
        NavItem::Homework => {
            text.push_str("  [ ] 分组  s 学期");
            if app.homework_detail {
                text.push_str("  enter 收起详情");
            } else {
                text.push_str("  enter 查看详情");
            }
        }
        NavItem::Lms => match app.lms.level {
            LmsLevel::Courses => text.push_str("  enter 进入课程"),
            LmsLevel::Activities => {
                text.push_str("  [ ] 分组  enter 查看详情  o 打开网页  esc 返回课程");
            }
            LmsLevel::Detail => text.push_str("  o 打开网页  esc 返回活动"),
        },
        _ => {
            if app.schedule_detail {
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
