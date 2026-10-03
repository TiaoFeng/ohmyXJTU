//! 內容面板：課表、作業、考勤流水與思源學堂四個頁面。
//!
//! 各頁的繪製在子模組（`schedule`／`homework`／`flow`／`lms`），欄寬計算
//! 集中在 `columns`；本檔只做頁面分派，並保留跨頁共用的列繪製、空畫面與
//! 格式化工具。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::domain::homework::parse_time;
use crate::text::{fit_display, wrap_display};
use crate::tui::app::{App, NavItem};
use crate::tui::theme::THEME;

mod columns;
mod flow;
mod homework;
mod lms;
mod schedule;

use columns::{DEADLINE_PREFIX, RowColumns};

/// 依目前頁面繪製內容。
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.nav {
        NavItem::Schedule => schedule::draw(frame, area, app),
        NavItem::Homework => homework::draw(frame, area, app),
        NavItem::Attendance => flow::draw(frame, area, app),
        NavItem::Lms => lms::draw(frame, area, app),
    }
}

// ── 共用 ─────────────────────────────────────────────

/// 清單列以外的固定佔用欄寬：外框左右欄線與選取列的高亮符號。
const ROW_CHROME_WIDTH: u16 = 3;

/// 詳情面板的預設高度（課表與流水頁；作業頁依內容區高度自行計算）。
const DETAIL_HEIGHT: u16 = 7;

/// 說明含圖片等無法以文字呈現的內容時的提示。
///
/// 純圖片說明沒有任何文字可顯示，若不標註，使用者會以為這項作業沒有說明。
/// 用詞刻意只寫「打开思源学堂」：`o` 對資料等活動只會開啟思源學堂首頁
/// （見 `Worker::open_activity_url`），並非該活動的頁面。
const MEDIA_HINT: &str = "（说明含图片，按 o 打开思源学堂查看）";

/// 清單列的可用顯示寬度：扣除外框左右欄線與選取列的高亮符號。
///
/// [`crate::tui::ui::render_list`] 以 `▍` 作為高亮符號，ratatui 會為每一列
/// 保留該欄寬；欄寬計算若漏扣，最寬的內容會在最後一欄被裁掉。
fn row_width(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(ROW_CHROME_WIDTH))
}

/// 終端過窄、清單無法完整顯示時顯示的提示。
///
/// `required` 與 `available` 都是列寬（顯示欄）；訊息換算成使用者看到的終端
/// 欄數（含側邊欄與外框），方便對照要放大到多寬。
fn too_narrow(frame: &mut Frame, area: Rect, title: &str, required: usize, available: usize) {
    // 側邊欄寬度＝畫面總寬 − 內容區寬度。
    let sidebar = usize::from(frame.area().width.saturating_sub(area.width));
    let chrome = sidebar + usize::from(ROW_CHROME_WIDTH);
    let message = format!(
        "终端过窄，无法完整显示列表：请放大窗口（当前 {} 栏，本页至少需要 {} 栏）",
        available + chrome,
        required + chrome
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(message, THEME.muted_style())))
            .block(THEME.block(title))
            .style(THEME.base_style())
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn split_detail(area: Rect, open: bool, detail_height: u16) -> (Rect, Option<Rect>) {
    if !open {
        return (area, None);
    }
    let [list, detail] =
        Layout::vertical([Constraint::Min(6), Constraint::Length(detail_height)]).areas(area);
    (list, Some(detail))
}

/// 面板的可用文字寬度（扣除外框左右欄線）。
fn panel_width(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(2))
}

/// 依顯示寬度把一段文字換行後加入內容列。
///
/// 需要精確控制捲動的內容一律預先換行：繪製端的 `Wrap` 無法回報實際列數，
/// 也就無法正確夾取捲動位移。
fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    text: impl Into<String>,
    style: Style,
    width: usize,
) {
    for line in wrap_display(&text.into(), width) {
        lines.push(Line::from(Span::styled(line, style)));
    }
}

/// 繪製可捲動的內容面板：內容已預先換行，`offset` 為起始列。
fn scrolled_panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    offset: u16,
) {
    let viewport = usize::from(area.height.saturating_sub(2));
    let visible = lines
        .into_iter()
        .skip(usize::from(offset))
        .take(viewport)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible)
            .block(THEME.block(title))
            .style(THEME.base_style()),
        area,
    );
}

fn detail_panel(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(THEME.block(title))
            .style(THEME.base_style())
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// 頁面狀態的空畫面：尚未載入（提示按 r）、載入中（muted）或載入失敗（error）。
///
/// 「載入成功但沒有資料」請用 [`empty_note`]：同一個 `Some` 說明搭配
/// `loading = false` 會被視為失敗訊息。
fn empty(frame: &mut Frame, area: Rect, title: &str, note: Option<&str>, loading: bool) {
    let line = match note {
        Some(note) if loading => Line::from(Span::styled(note.to_owned(), THEME.muted_style())),
        Some(note) => Line::from(Span::styled(
            format!("加载失败：{note}"),
            THEME.error_style(),
        )),
        None => Line::from(Span::styled(
            "按 r 重新加载".to_owned(),
            THEME.muted_style(),
        )),
    };
    frame.render_widget(
        Paragraph::new(line)
            .block(THEME.block(title))
            .style(THEME.base_style())
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// 空結果提示：載入成功但沒有資料時使用（中性色）。
///
/// 「沒有資料」是正常結果；誤用 [`empty`] 會讓它看起來像載入失敗。
fn empty_note(frame: &mut Frame, area: Rect, title: &str, message: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            message.to_owned(),
            THEME.muted_style(),
        )))
        .block(THEME.block(title))
        .style(THEME.base_style())
        .wrap(Wrap { trim: true }),
        area,
    );
}

/// 截止時間欄內容：可含「截止」前綴，後面還有欄位時補上間隔。
fn deadline_cell(deadline: &str, columns: RowColumns) -> String {
    let mut cell = String::new();
    if columns.deadline_prefix {
        cell.push_str(DEADLINE_PREFIX);
    }
    cell.push_str(&fit_display(deadline, columns.deadline));
    if columns.group {
        cell.push(' ');
    }
    cell
}

/// 提交單位欄文字（個人作業與未知皆留白，維持欄位寬度）。
fn group_label(submit_by_group: Option<bool>) -> &'static str {
    if submit_by_group == Some(true) {
        "小组"
    } else {
        ""
    }
}

/// 列表中顯示的截止時間（`compact` 為真時省略年份）。
fn deadline_list_label(value: Option<&str>, compact: bool) -> String {
    parse_time(value).map_or_else(
        || value.map_or_else(|| "无截止时间".to_owned(), |raw| raw.trim().to_owned()),
        |time| {
            time.format(if compact {
                "%m-%d %H:%M"
            } else {
                "%Y-%m-%d %H:%M"
            })
            .to_string()
        },
    )
}

/// 提交記錄時間：解析後統一以校園時區（+08:00）顯示，解析失敗時回退原始字串。
fn submission_time_label(value: Option<&str>) -> String {
    parse_time(value).map_or_else(
        || value.map_or_else(|| "未知时间".to_owned(), |raw| raw.trim().to_owned()),
        |time| time.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// 完整截止時間文字（詳情面板用，列表請用 [`deadline_list_label`]）。
fn deadline_label(value: Option<&str>) -> String {
    deadline_list_label(value, false)
}

/// 標題後綴：更新時間與載入狀態。
fn title_suffix(updated: Option<&String>, loading: bool) -> String {
    let mut suffix = String::new();
    if let Some(at) = updated {
        suffix.push_str(" · 更新于 ");
        suffix.push_str(at);
    }
    if loading {
        suffix.push_str(" · 更新中…");
    }
    suffix
}

#[cfg(test)]
#[path = "tests/content_test.rs"]
mod content_test;
