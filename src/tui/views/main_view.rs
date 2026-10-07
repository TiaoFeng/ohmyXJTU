//! 主畫面：左側導航＋右側內容＋底部提示列。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use crate::text::display_width;
use crate::tone::Tone;
use crate::tui::app::{App, LmsLevel, NavItem, Screen};
use crate::tui::theme::THEME;
use crate::tui::views::{content, settings, task_form, task_menu, term_picker};

/// 側邊欄寬度。
const SIDEBAR_WIDTH: u16 = 22;
/// 主體區域的最小高度（其餘列數留給底部提示列）。
const BODY_MIN_HEIGHT: u16 = 8;
/// 內容區的最小寬度（左側是固定寬度的導航欄）。
const CONTENT_MIN_WIDTH: u16 = 30;

/// 繪製主畫面。
///
/// 尺寸守衛由 `views::draw` 在最上層統一處理（所有畫面共用）。
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let [body, footer] =
        Layout::vertical([Constraint::Min(BODY_MIN_HEIGHT), Constraint::Length(1)]).areas(area);
    let [sidebar, content_area] = Layout::horizontal([
        Constraint::Length(SIDEBAR_WIDTH),
        Constraint::Min(CONTENT_MIN_WIDTH),
    ])
    .areas(body);

    draw_sidebar(frame, sidebar, app);
    content::draw(frame, content_area, app);
    draw_footer(frame, footer, app);

    if let Screen::Settings(state) = &app.screen {
        settings::draw(frame, app, *state);
    }
    if let Screen::TermPicker(state) = &app.screen {
        term_picker::draw(frame, state);
    }
    if let Screen::TaskMenu(state) = &app.screen {
        task_menu::draw_menu(frame, *state);
    }
    if let Screen::TaskBatchMenu(state) = &app.screen {
        task_menu::draw_batch_menu(frame, *state);
    }
    if let Screen::TaskConfirm(state) = &app.screen {
        task_menu::draw_confirm(frame, *state);
    }
    if let Screen::TaskForm(form) = &mut app.screen {
        task_form::draw(frame, form.as_mut());
    }
}

/// 側邊欄載入指示燈的固定寬度（三個點＋一個空白）。
const LOADING_DOTS_WIDTH: usize = 4;

/// 載入指示的三點動畫相位：每幀前進一格並循環，空相位形成閃爍。
const LOADING_DOTS_FRAMES: &[&str] = &["", ".", "..", "..."];

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    let inner_width = usize::from(area.width.saturating_sub(2));
    // 文字欄起點：以最寬標籤為基準讓整欄置中，標籤之間保持左對齊。
    let widest = NavItem::ALL
        .iter()
        .map(|nav| display_width(nav.label()))
        .max()
        .unwrap_or(0);
    let text_start = inner_width.saturating_sub(widest) / 2;
    let items: Vec<ListItem<'static>> = NavItem::ALL
        .iter()
        .map(|nav| nav_item(*nav, app, text_start))
        .collect();

    app.nav_state.select(Some(app.nav.index()));
    let list = List::new(items)
        .block(THEME.block("ohmyXJTU"))
        .highlight_style(THEME.highlight_style());
    frame.render_stateful_widget(list, area, &mut app.nav_state);
}

/// 側邊欄單列：文字欄（標籤左對齊、整欄置中）＋文字正前方的載入指示燈。
///
/// 指示燈固定佔用文字前方的四欄（未載入時為空白），不參與置中計算；
/// 因此載入狀態與動畫相位都不會讓標籤左右跳動。
fn nav_item(nav: NavItem, app: &App, text_start: usize) -> ListItem<'static> {
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
        Span::raw(" ".repeat(text_start.saturating_sub(LOADING_DOTS_WIDTH))),
        Span::styled(
            format!("{dots:<width$}", width = LOADING_DOTS_WIDTH),
            dots_style,
        ),
        Span::styled(nav.label().to_owned(), Style::default().fg(THEME.text)),
    ]))
}

/// 載入指示的三點動畫：每幀前進一格並循環，空相位形成閃爍。
fn loading_dots(tick: u64) -> &'static str {
    let phase = usize::try_from(tick % LOADING_DOTS_FRAMES.len() as u64).unwrap_or(0);
    LOADING_DOTS_FRAMES[phase]
}

/// 底部提示列的前置空白（訊息與提示都以此縮排）。
const FOOTER_INDENT: u16 = 1;

/// 提示片段之間的間隔。
const HINT_SEPARATOR: &str = "  ";

/// 提示放不下時補的省略號。
const HINT_ELLIPSIS: &str = " …";

/// 詳情面板的捲動提示（終端不夠寬時最先保留的片段之一）。
const SCROLL_HINT: &str = "PgUp/PgDn 滚动";

/// 排序提示（`^L`）開啟時，底部只顯示這行按鍵說明。
const SORT_HINT: &str = "排序：[p] 优先级  [d] 截止时间  [n] 默认  esc 取消";

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    // 訊息依語意上色：失敗與異常狀態用紅色粗體，操作結果與一般提示用一般色——
    // 「登录成功」「已添加任务」不該長得像錯誤。
    let line = match app.message_text() {
        Some(message) => Line::from(Span::styled(
            format!(" {message}"),
            THEME
                .status_style(app.message_tone().unwrap_or(Tone::Info))
                .add_modifier(Modifier::BOLD),
        )),
        None => Line::from(Span::styled(
            format!(" {}", hints(app, area.width.saturating_sub(FOOTER_INDENT))),
            THEME.muted_style(),
        )),
    };
    frame.render_widget(Paragraph::new(line).style(THEME.base_style()), area);
}

/// 底部提示：依重要度排列——登入狀態、當下可捲動的詳情、當前頁面操作、通用按鍵。
///
/// 終端不夠寬時由後往前整段捨棄並補上省略號（見 [`fit_hints`]）。順序即重要度：
/// 通用按鍵在所有頁面都一樣、也最快記住，因此排在最後；畫面專屬的操作（尤其
/// 「這份說明還能往下讀」）才是使用者當下需要的，不會被固定的長前綴擠掉。
fn hints(app: &App, width: u16) -> String {
    let mut segments = vec![format!(
        "[{} {}]",
        app.session_label(),
        app.access_policy.label()
    )];
    if matches!(app.screen, Screen::Sort) {
        segments.push(SORT_HINT.to_owned());
        return fit_hints(&segments, width);
    }
    // 彈窗接管按鍵時只留登入狀態：各彈窗自己畫按鍵提示（見 `settings`／
    // `term_picker`／`task_form`／`task_menu` 的 `hint_line`），底欄再列一次
    // 主畫面的操作只會顯示當下按不到的鍵（例如表單開啟時的「^P 账户设置」）。
    if popup_owns_keys(&app.screen) {
        return fit_hints(&segments, width);
    }
    if scrollable_panel(app) {
        segments.push(SCROLL_HINT.to_owned());
    }
    segments.extend(page_hints(app));
    segments.extend(
        [
            "q 退出",
            "r 刷新",
            "←/→ 切换页面",
            "↑/↓ 选择",
            "^P 账户设置",
        ]
        .map(str::to_owned),
    );
    fit_hints(&segments, width)
}

/// 彈窗是否接管按鍵（此時底欄不再列出主畫面的操作）。
///
/// 這些畫面的按鍵提示由彈窗自己繪製；設定表單、登入表單等「非主畫面」也
/// 同樣不屬於主畫面。
fn popup_owns_keys(screen: &Screen) -> bool {
    matches!(
        screen,
        Screen::Settings(_)
            | Screen::SettingsForm(_)
            | Screen::TermPicker(_)
            | Screen::TaskMenu(_)
            | Screen::TaskBatchMenu(_)
            | Screen::TaskConfirm(_)
            | Screen::TaskForm(_)
    )
}

/// 目前畫面上的詳情面板是否可捲動（決定是否提示 `PgUp/PgDn`）。
///
/// 視窗資訊由繪製端回寫（見 `content` 的 `scrolled_panel`），而提示列在內容之後
/// 繪製，因此讀到的是本幀的值。
fn scrollable_panel(app: &App) -> bool {
    match app.nav {
        NavItem::Homework => app.homework_detail && app.homework_scroll.scrollable(),
        NavItem::Lms => app.lms.level == LmsLevel::Detail && app.lms.detail_scroll.scrollable(),
        NavItem::Schedule | NavItem::Attendance => false,
    }
}

/// 當前頁面的操作提示（依重要度排序，排在前面者優先保留）。
fn page_hints(app: &App) -> Vec<String> {
    let mut hints = Vec::new();
    match app.nav {
        NavItem::Attendance => hints.push("n/p 翻页".to_owned()),
        NavItem::Homework => {
            // 任務頁的互動模式（搜尋／多選）會接管大部分按鍵，提示以當下可用者為主。
            if app.task_page.search.is_some() {
                hints.push("enter 应用筛选".to_owned());
                hints.push("esc 取消".to_owned());
                // 沒有任何標籤可選時不提示（上下鍵也等同沒反應）。
                if app
                    .task_page
                    .tasks
                    .iter()
                    .any(|task| task.display_tag().is_some())
                {
                    hints.push("↑/↓ 选标签".to_owned());
                }
                return hints;
            }
            if app.task_page.multi.is_some() {
                hints.push("space 勾选".to_owned());
                hints.push("enter 批量操作".to_owned());
                hints.push("esc 退出多选".to_owned());
                return hints;
            }
            hints.push("[ ] 分组".to_owned());
            hints.push(
                if app.homework_detail {
                    "enter 收起详情"
                } else {
                    "enter 查看详情"
                }
                .to_owned(),
            );
            hints.push("^a 添加".to_owned());
            hints.push("space 完成".to_owned());
            hints.push("^e 编辑".to_owned());
            hints.push("^d 删除".to_owned());
            hints.push("m 多选".to_owned());
            hints.push("^f 搜索".to_owned());
            hints.push("^t 设置".to_owned());
            hints.push("^L 排序".to_owned());
            if app.task_page.filter.is_some() {
                hints.push("esc 清除筛选".to_owned());
            }
            hints.push("s 学期".to_owned());
            hints.push("o 打开网页".to_owned());
        }
        NavItem::Lms => match app.lms.level {
            LmsLevel::Courses => hints.push("enter 进入课程".to_owned()),
            LmsLevel::Activities => {
                hints.push("[ ] 分组".to_owned());
                hints.push("enter 查看详情".to_owned());
                hints.push("o 打开网页".to_owned());
                hints.push("esc 返回课程".to_owned());
            }
            LmsLevel::Detail => {
                hints.push("o 打开网页".to_owned());
                hints.push("esc 返回活动".to_owned());
            }
        },
        NavItem::Schedule => {
            hints.push(
                if app.schedule_detail {
                    "enter 收起详情"
                } else {
                    "enter 查看详情"
                }
                .to_owned(),
            );
            hints.push("[ ] 切换周次".to_owned());
        }
    }
    hints
}

/// 依可用寬度挑選提示片段：由前往後加入，放不下的整段捨棄並以省略號收尾。
///
/// 逐段判斷（而非事後截斷字串）才不會把片段切成半句；加入非最後一段時會預留
/// 省略號的寬度，避免「剩下的放不下、省略號又被裁掉」。連第一段都放不下時
/// （正常情況下 `ui::too_small` 已先擋住）原樣輸出，交由繪製端裁切。
fn fit_hints(segments: &[String], width: u16) -> String {
    let width = usize::from(width);
    let mut text = String::new();
    let mut used = 0_usize;
    let mut shown = 0_usize;
    for (index, segment) in segments.iter().enumerate() {
        let separator = if shown == 0 {
            0
        } else {
            display_width(HINT_SEPARATOR)
        };
        let reserve = if index + 1 < segments.len() {
            display_width(HINT_ELLIPSIS)
        } else {
            0
        };
        let segment_width = display_width(segment);
        if used + separator + segment_width + reserve > width {
            break;
        }
        if shown > 0 {
            text.push_str(HINT_SEPARATOR);
        }
        text.push_str(segment);
        used += separator + segment_width;
        shown += 1;
    }
    if shown == 0 {
        return segments.first().cloned().unwrap_or_default();
    }
    if shown < segments.len() {
        text.push_str(HINT_ELLIPSIS);
    }
    text
}

#[cfg(test)]
#[path = "tests/main_view_test.rs"]
mod main_view_test;
