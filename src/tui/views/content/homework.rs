//! 任務頁：自訂義任務與思源學堂作業的合併清單（未完成／已完成／待核实三分組）。
//!
//! 預設排序（`SortMode::Default`）下，分組内先顯示「任务」段（使用者自己新增
//! 的），再顯示「作业」段（思源學堂），兩段之間留一列空白；分段標題以強調色
//! （粉）呈現，不分深淺灰。切到優先級或截止時間排序時沒有分段標題，任務與
//! 作業混合排列（見 `App::task_page_entries`）。
//!
//! 不論哪種排序，任務與作業都共用同一組欄位骸架、同一組選取索引與同一個
//! 詳情面板。
//!
//! 本檔只負責頁面本身（清單、分段標籤、提示列、空狀態與繪製流程）：列的建構
//! 見 [`rows`]，詳情面板的內容見 [`detail`]。

mod detail;
mod rows;

use chrono::Local;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph, Wrap};

use crate::domain::homework::HomeworkGroup;
use crate::domain::todo::{PageRow, visual_index};
use crate::text::display_width;
use crate::tui::app::{App, Page, TaskPageCounts};
use crate::tui::text::InputLine;
use crate::tui::theme::THEME;
use crate::tui::ui::{input_window, render_list};

use super::columns::{RowNeeds, page_columns, page_min_row_width};
use super::{empty, panel_width, row_width, scrolled_panel, split_detail, too_narrow};
use detail::{homework_lines, task_lines};
use rows::{homework_item, section_header_item, task_item};

/// 展開詳情時的框高範圍：內容區一半，並限制在可讀區間。
const DETAIL_MIN_HEIGHT: u16 = 7;
const DETAIL_MAX_HEIGHT: u16 = 14;
/// 多選模式勾選框欄寬（`[x] `）。
const MULTI_CHECKBOX_WIDTH: usize = 4;

/// 依目前資料繪製任務頁（任务段在前、作业段在後）。
pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    // 本幀若沒畫到詳情面板（載入中、空分組、終端過窄…），不得沿用上一幀的視窗
    // 資訊，否則底欄會一直提示「PgUp/PgDn 滚动」卻沒有東西可捲。
    app.homework_scroll.clear();

    // 任務頁的列模型與分組計數每幀只建一次（主迴圈固定 200ms 重繪一次）：
    // 標題、分組標籤列與提示列都共用同一份結果。
    let counts = app.task_page_group_counts();
    let title = homework_title(app, counts);
    let rows = app.task_page_rows();
    // 作業尚未載入且沒有任何任務：整頁顯示載入中／失敗／空結果。
    if rows.is_empty() && app.task_page.tasks.is_empty() && app.homework.ready().is_none() {
        empty(
            frame,
            area,
            &title,
            app.homework.note(),
            app.homework.is_loading(),
        );
        return;
    }

    // 分組標籤列＋（搜尋中）搜尋輸入框＋（更新失敗或有待核实）提示列。
    let header = homework_tabs(app, counts);
    let warning = homework_warning(app, counts);
    let search_row = u16::from(app.task_page.search.is_some());
    let header_height = 1 + search_row + u16::from(warning.is_some());
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(3)]).areas(area);
    let header_rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(search_row),
        Constraint::Length(u16::from(warning.is_some())),
    ])
    .split(header_area);
    frame.render_widget(
        Paragraph::new(header).style(THEME.base_style()),
        header_rows[0],
    );
    if let Some(input) = app.task_page.search.as_ref() {
        draw_search(frame, input, header_rows[1]);
    }
    if let Some(line) = warning {
        frame.render_widget(
            Paragraph::new(line).style(THEME.base_style()),
            header_rows[2],
        );
    }

    if rows.is_empty() {
        empty_homework(frame, body_area, &title, app);
        return;
    }

    // 展開詳情時加大面板：描述可能有好幾行，原本的 5 列內容區讀不了多少。
    let detail_height = (body_area.height / 2).clamp(DETAIL_MIN_HEIGHT, DETAIL_MAX_HEIGHT);
    let (list_area, detail_area) = split_detail(body_area, app.homework_detail, detail_height);

    // 任務與作業共用同一組欄寬：需求直接由**本幀要畫的列**推導（與畫面同一份
    // 資料，不必再過濾一次），標籤欄下限依「是否只看任務」決定。
    let tasks_only = !rows.iter().any(|row| matches!(row, PageRow::Homework(_)));
    let needs = rows
        .iter()
        .fold(RowNeeds::default(), |needs, row| match row {
            PageRow::Task(task) => needs.merge(RowNeeds::of_task(task)),
            PageRow::Homework(item) => needs.merge(RowNeeds::of_homework_item(item)),
            PageRow::Header(_) | PageRow::Spacer => needs,
        });
    // 多選模式會在列首加一個勾選框欄，欄寬計算必須同步扣除。
    let checkbox_width = if app.task_page.multi.is_some() {
        MULTI_CHECKBOX_WIDTH
    } else {
        0
    };
    let width = row_width(list_area).saturating_sub(checkbox_width);
    let Some(columns) = page_columns(width, needs, tasks_only) else {
        too_narrow(
            frame,
            list_area,
            &title,
            page_min_row_width(tasks_only),
            width,
        );
        return;
    };

    let now = Local::now().fixed_offset();
    // 借用都在這個區塊內結束（`rows` 與選取索引都借用 `app`），之後才能以可變
    // 借用更新清單狀態。
    let (items, detail, visual) = {
        let visual = visual_index(&rows, app.page_selection());
        let multi = app.task_page.multi.as_ref();
        let items: Vec<ListItem<'static>> = rows
            .iter()
            .map(|row| match row {
                PageRow::Header(text) => section_header_item(text),
                PageRow::Spacer => ListItem::new(Line::default()),
                PageRow::Task(task) => task_item(
                    task,
                    now,
                    columns,
                    multi.map(|selection| selection.contains(&task.id)),
                ),
                PageRow::Homework(item) => homework_item(item, columns, multi.map(|_| "    ")),
            })
            .collect();
        // 詳情內容預先換行：列數必須已知，捲動位移才夾得住。取的是同一份列模型
        // 的選取項，不再重算一次任務頁清單。
        let detail = detail_area.map(|area| {
            let width = panel_width(area);
            match visual.and_then(|index| rows.get(index)) {
                Some(PageRow::Task(task)) => (task_lines(task, now, width), "任务详情"),
                Some(PageRow::Homework(item)) => (homework_lines(item, width), "作业详情"),
                _ => (Vec::new(), "详情"),
            }
        });
        (items, detail, visual)
    };

    render_entries(
        frame,
        list_area,
        &title,
        items,
        &mut app.homework_state,
        visual,
    );
    if let (Some(area), Some((lines, detail_title))) = (detail_area, detail) {
        scrolled_panel(frame, area, detail_title, lines, &mut app.homework_scroll);
    }
}

/// 搜尋輸入框（`^f`）：把目前輸入的內容與游標畫出來。
///
/// 少了這一列，使用者只能盲打（看不到自己打了什麼，也没有游標），看起來就
/// 像畫面卡住了；按鍵說明放在底欄（`enter 应用筛选 · esc 取消`）。
fn draw_search(frame: &mut Frame, input: &InputLine, area: Rect) {
    /// 輸入框前綴。
    const PREFIX: &str = " 搜索：";

    let prefix = u16::try_from(display_width(PREFIX)).unwrap_or(0);
    let value_width = usize::from(area.width.saturating_sub(prefix));
    let (visible, cursor) = input_window(input, value_width);
    let line = Line::from(vec![
        Span::styled(PREFIX, THEME.accent_style()),
        Span::styled(visible, Style::default().fg(THEME.text)),
    ]);
    frame.render_widget(Paragraph::new(line).style(THEME.base_style()), area);
    if value_width > 0 {
        frame.set_cursor_position((area.x + prefix + cursor, area.y));
    }
}

/// 頁面標題：學期與各組計數（計數含自訂義任務）。
fn homework_title(app: &App, counts: TaskPageCounts) -> String {
    match app.homework.ready() {
        Some(data) => {
            let term = data.term_label.as_deref().unwrap_or("未确定学期");
            format!(
                "任务 · {term} · 未完成 {} / 已完成 {} / 待核实 {}",
                counts.unfinished, counts.completed, counts.unknown,
            )
        }
        None => "任务".to_owned(),
    }
}

/// 繪製任務頁清單：`state` 以「可選取項目的序號」為準，繪製時映射到含分段
/// 標題與空白列的視覺位置；捲動位移沿用原 state 並在繪製後寫回。
fn render_entries(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'static>>,
    state: &mut ListState,
    visual: Option<usize>,
) {
    let mut scrolled = ListState::default();
    scrolled.select(visual);
    *scrolled.offset_mut() = state.offset();
    render_list(frame, area, title, items, &mut scrolled);
    *state.offset_mut() = scrolled.offset();
}

/// 清單為空時的提示：載入中顯示進度，篩選中說明無匹配，終態才是「沒有…」。
///
/// 不經 [`super::empty`]：該函式將非載入中的文字視為失敗訊息，會讓空結果看起來像載入失敗。
fn empty_homework(frame: &mut Frame, area: Rect, title: &str, app: &App) {
    let message = if let Some(keyword) = &app.task_page.filter {
        format!("没有匹配的条目（筛选“{keyword}”）")
    } else if app.homework.is_loading() {
        app.homework.note().unwrap_or("正在汇总作业…").to_owned()
    } else if app.task_page.tasks.is_empty()
        && app
            .homework
            .ready()
            .is_some_and(|data| data.items.is_empty())
    {
        "本学期暂无作业；按 ^a 可添加自定义任务".to_owned()
    } else {
        match app.homework_group {
            HomeworkGroup::Unfinished => "没有未完成的作业或任务",
            HomeworkGroup::Completed => "没有已完成的作业或任务",
            HomeworkGroup::Unknown => "没有待核实的作业",
        }
        .to_owned()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(message, THEME.muted_style())))
            .block(THEME.block(title))
            .style(THEME.base_style())
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// 分組標籤列（計數含自訂義任務，另附載入狀態與篩選提示）。
fn homework_tabs(app: &App, counts: TaskPageCounts) -> Line<'static> {
    let mut spans = Vec::new();
    for candidate in HomeworkGroup::ALL {
        let text = format!(" {} {} ", candidate.label(), counts.get(candidate));
        let style = if candidate == app.homework_group {
            THEME.highlight_style()
        } else {
            THEME.muted_style()
        };
        spans.push(Span::styled(text, style));
    }
    match app.homework.ready().and_then(|data| data.progress) {
        Some((done, total)) => spans.push(Span::styled(
            format!("  加载中 {done}/{total} 门课程"),
            THEME.accent_style(),
        )),
        None if app.homework.is_loading() => {
            spans.push(Span::styled("  正在更新…", THEME.accent_style()));
        }
        None => {}
    }
    if let Some(skipped) = app
        .homework
        .ready()
        .map(|data| data.courses_skipped)
        .filter(|skipped| *skipped > 0)
    {
        spans.push(Span::styled(
            format!("  （{skipped} 门课程缺少学期信息，未纳入）"),
            THEME.muted_style(),
        ));
    }
    if let Some(keyword) = &app.task_page.filter {
        spans.push(Span::styled(
            format!(
                "  筛选“{keyword}”：{} 项（esc 清除）",
                app.task_filter_matches()
            ),
            THEME.accent_style(),
        ));
    }
    // 混合排序時分段標題不再出現，這裡說明目前的排序方式（任務與作業混在一起）。
    if app.task_page.sort.is_sorted() {
        spans.push(Span::styled(
            format!("  排序：{}（任务与作业混合）", app.task_page.sort.label()),
            THEME.accent_style(),
        ));
    }
    Line::from(spans)
}

/// 提示列：更新失敗（保留舊資料時）或「已确认 / 待核实」。
fn homework_warning(app: &App, counts: TaskPageCounts) -> Option<Line<'static>> {
    // 重新整理失敗但仍有舊資料：標題仍顯示舊清單，這裡明確告知並提示重試。
    if let Page::Failed {
        message,
        stale: Some(_),
    } = &app.homework
    {
        return Some(Line::from(Span::styled(
            format!(" 更新失败：{message}（按 r 重试）"),
            THEME.error_style(),
        )));
    }

    let unknown = counts.unknown;
    let courses_failed = app.homework.ready().map_or(0, |data| data.courses_failed);
    if unknown == 0 && courses_failed == 0 {
        return None;
    }
    let mut spans = Vec::new();
    if unknown > 0 {
        let total = app.homework.ready().map_or(0, |data| data.items.len());
        let confirmed = total.saturating_sub(unknown);
        let reason = app
            .homework
            .ready()
            .and_then(|data| data.issues.first())
            .map_or("具体原因见条目详情", |issue| issue.reason.as_str());
        spans.push(Span::styled(
            format!(" 已确认 {confirmed} / 待核实 {unknown}："),
            THEME.accent_style(),
        ));
        spans.push(Span::styled(
            format!("{reason}（按 r 重试）"),
            THEME.muted_style(),
        ));
    }
    if courses_failed > 0 {
        if !spans.is_empty() {
            spans.push(Span::styled("；", THEME.muted_style()));
        }
        spans.push(Span::styled(
            format!("{courses_failed} 门课程查询失败（按 r 重试）"),
            THEME.error_style(),
        ));
    }
    Some(Line::from(spans))
}
