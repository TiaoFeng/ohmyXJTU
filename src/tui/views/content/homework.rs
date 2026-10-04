//! 任務頁：自訂義任務與思源學堂作業的合併清單（未完成／已完成／待核实三分組）。
//!
//! 分組內先顯示「任务」段（使用者自己新增的），再顯示「作业」段（思源學堂），
//! 兩段之間留一列空白；分段標題以強調色（粉）呈現，不分深淺灰。任務與作業
//! 共用同一組欄位骨架、同一組選取索引（任務在前）與同一個詳情面板。

use chrono::{DateTime, FixedOffset, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph, Wrap};

use crate::domain::homework::{HomeworkGroup, HomeworkItem};
use crate::domain::todo::{PageRow, Task, visual_index};
use crate::sites::lms::ActivityKind;
use crate::text::fit_display;
use crate::tui::app::{App, Page, TaskEntry};
use crate::tui::theme::THEME;
use crate::tui::ui::render_list;

use super::columns::{GROUP_WIDTH, RowColumns, RowNeeds, page_columns, page_min_row_width};
use super::{
    deadline_cell, deadline_label, deadline_list_label, empty, group_label, panel_width,
    push_description, push_multiline, push_wrapped, row_width, scrolled_panel, split_detail,
    too_narrow,
};

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

    let title = homework_title(app);
    // 作業尚未載入且沒有任何任務：整頁顯示載入中／失敗／空結果。
    if app.task_page_rows().is_empty() && app.tasks.is_empty() && app.homework.ready().is_none() {
        empty(
            frame,
            area,
            &title,
            app.homework.note(),
            app.homework.is_loading(),
        );
        return;
    }

    // 分組標籤列＋載入狀態（更新失敗或有待核实項目時再加一行提示）。
    let header = homework_tabs(app);
    let warning = homework_warning(app);
    let header_height = 1 + u16::from(warning.is_some());
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(3)]).areas(area);
    let [tabs_area, warning_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(header_height - 1)])
            .areas(header_area);
    frame.render_widget(Paragraph::new(header).style(THEME.base_style()), tabs_area);
    if let Some(line) = warning {
        frame.render_widget(Paragraph::new(line).style(THEME.base_style()), warning_area);
    }

    if app.task_page_rows().is_empty() {
        empty_homework(frame, body_area, &title, app);
        return;
    }

    // 展開詳情時加大面板：描述可能有好幾行，原本的 5 列內容區讀不了多少。
    let detail_height = (body_area.height / 2).clamp(DETAIL_MIN_HEIGHT, DETAIL_MAX_HEIGHT);
    let (list_area, detail_area) = split_detail(body_area, app.homework_detail, detail_height);

    // 任務與作業共用同一組欄寬：需求取兩者的最大值，標籤欄下限依「是否只看任務」決定。
    let tasks_only = app.homework_group_items(app.homework_group).is_empty();
    let needs = RowNeeds::of_homework(&app.homework_group_items(app.homework_group)).merge(
        RowNeeds::of_tasks(&app.task_group_items(app.homework_group)),
    );
    // 多選模式會在列首加一個勾選框欄，欄寬計算必須同步扣除。
    let checkbox_width = if app.task_multi.is_some() {
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
    // 所有借用都在這個區塊內結束（選取索引與列模型借自 `app`），
    // 之後才能以可變借用更新清單狀態。
    let (items, detail, visual) = {
        let rows = app.task_page_rows();
        let visual = visual_index(&rows, app.page_selection());
        let multi = app.task_multi.as_ref();
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
        // 詳情內容預先換行：列數必須已知，捲動位移才夾得住。
        let detail = detail_area.map(|area| {
            let width = panel_width(area);
            match app.selected_entry() {
                Some(TaskEntry::Task(task)) => (task_lines(task, now, width), "任务详情"),
                Some(TaskEntry::Homework(item)) => (homework_lines(item, width), "作业详情"),
                None => (Vec::new(), "详情"),
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

/// 頁面標題：學期與各組計數（計數含自訂義任務）。
fn homework_title(app: &App) -> String {
    match app.homework.ready() {
        Some(data) => {
            let term = data.term_label.as_deref().unwrap_or("未确定学期");
            format!(
                "任务 · {term} · 未完成 {} / 已完成 {} / 待核实 {}",
                app.page_group_count(HomeworkGroup::Unfinished),
                app.page_group_count(HomeworkGroup::Completed),
                app.page_group_count(HomeworkGroup::Unknown),
            )
        }
        None => "任务".to_owned(),
    }
}

/// 分段標題列（強調色；不可選取）。
fn section_header_item(text: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("  {text}"),
        THEME.accent_style().add_modifier(Modifier::BOLD),
    )))
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
    let message = if let Some(keyword) = &app.task_filter {
        format!("没有匹配的条目（筛选“{keyword}”）")
    } else if app.homework.is_loading() {
        app.homework.note().unwrap_or("正在汇总作业…").to_owned()
    } else if app.tasks.is_empty()
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
fn homework_tabs(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    for candidate in HomeworkGroup::ALL {
        let text = format!(
            " {} {} ",
            candidate.label(),
            app.page_group_count(candidate)
        );
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
    if let Some(keyword) = &app.task_filter {
        spans.push(Span::styled(
            format!(
                "  筛选“{keyword}”：{} 项（esc 清除）",
                app.task_filter_matches()
            ),
            THEME.accent_style(),
        ));
    }
    Line::from(spans)
}

/// 提示列：更新失敗（保留舊資料時）或「已确认 / 待核实」。
fn homework_warning(app: &App) -> Option<Line<'static>> {
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

    let unknown = app.page_group_count(HomeworkGroup::Unknown);
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

/// 作業列：課程／標題／狀態／截止時間／（小组）。
///
/// `checkbox` 為 `Some` 時代表目前在多選模式；作業列不可勾選，以空白佔位
/// 維持與任務列相同的欄位對齊。
fn homework_item(
    item: &HomeworkItem,
    columns: RowColumns,
    checkbox: Option<&'static str>,
) -> ListItem<'static> {
    let deadline = deadline_list_label(item.end_time.as_deref(), columns.compact);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(checkbox) = checkbox {
        spans.push(Span::raw(checkbox));
    }
    spans.push(Span::styled(
        format!("{} ", fit_display(&item.course_name, columns.label)),
        THEME.accent_style(),
    ));
    spans.push(Span::styled(
        format!("{} ", fit_display(&item.title, columns.title)),
        Style::default().fg(THEME.text),
    ));
    spans.push(Span::styled(
        format!("{} ", fit_display(item.state.label(), columns.state)),
        THEME.status_style(item.state.tone()),
    ));
    spans.push(Span::styled(
        deadline_cell(&deadline, columns),
        THEME.muted_style(),
    ));
    if columns.group {
        spans.push(Span::styled(
            fit_display(group_label(item.submit_by_group), GROUP_WIDTH),
            THEME.muted_style(),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn homework_lines(item: &HomeworkItem, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    push_wrapped(
        &mut lines,
        item.title.clone(),
        Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        width,
    );
    push_wrapped(
        &mut lines,
        format!("课程：{}", item.course_name),
        THEME.muted_style(),
        width,
    );
    push_wrapped(
        &mut lines,
        format!("截止：{}", deadline_label(item.end_time.as_deref())),
        THEME.muted_style(),
        width,
    );
    // 狀態列由多個樣式組成（狀態色隨語意變），且短於最小面板寬度，因此不換行。
    // 詳情面板用的是無 `Wrap` 的 `Paragraph`：這一列一旦超寬就會被直接裁掉，
    // 故以下斷言鎖住「一定放得下」的假設（面板最小寬度見 `too_narrow`）。
    let status = Line::from(vec![
        Span::styled("状态：", THEME.muted_style()),
        Span::styled(item.state.label(), THEME.status_style(item.state.tone())),
        Span::styled(
            match item.submit_by_group {
                Some(true) => "　提交单位：小组",
                Some(false) => "　提交单位：个人",
                None => "　提交单位：未知",
            },
            THEME.muted_style(),
        ),
    ]);
    debug_assert!(
        status.width() <= width,
        "作业状态列宽度 {} 超过面板宽度 {width}",
        status.width()
    );
    lines.push(status);
    if let Some(note) = &item.note {
        push_wrapped(
            &mut lines,
            format!("说明：{note}"),
            THEME.muted_style(),
            width,
        );
    }
    // 作業說明：讓使用者不必按 `o` 開網頁就能看完題目內容。
    if let Some(description) = &item.description {
        push_description(&mut lines, description, ActivityKind::Homework, width);
    }
    lines
}

/// 自訂義任務列：優先級／內容／狀態／截止時間（與作業列共用欄位骨架）。
///
/// `checked` 為 `Some` 時代表目前在多選模式（顯示勾選框）。
fn task_item(
    task: &Task,
    now: DateTime<FixedOffset>,
    columns: RowColumns,
    checked: Option<bool>,
) -> ListItem<'static> {
    let state = task.state(now);
    let deadline_raw = task.deadline.map(|deadline| deadline.to_rfc3339());
    let deadline = deadline_list_label(deadline_raw.as_deref(), columns.compact);
    let mut spans = Vec::new();
    if let Some(checked) = checked {
        spans.push(Span::styled(
            if checked { "[x] " } else { "[ ] " },
            if checked {
                THEME.accent_style()
            } else {
                THEME.muted_style()
            },
        ));
    }
    spans.push(Span::styled(
        format!("{} ", fit_display(task.priority.label(), columns.label)),
        THEME.status_style(task.priority.tone()),
    ));
    spans.push(Span::styled(
        format!("{} ", fit_display(&task.content, columns.title)),
        Style::default().fg(THEME.text),
    ));
    spans.push(Span::styled(
        format!("{} ", fit_display(state.label(), columns.state)),
        THEME.status_style(state.tone()),
    ));
    spans.push(Span::styled(
        deadline_cell(&deadline, columns),
        THEME.muted_style(),
    ));
    if columns.group {
        // 任務沒有「小组」概念：留白以維持與作業列的欄位對齊。
        spans.push(Span::raw(" ".repeat(GROUP_WIDTH)));
    }
    ListItem::new(Line::from(spans))
}

/// 任務詳情（`enter` 展開的內容）。
fn task_lines(task: &Task, now: DateTime<FixedOffset>, width: usize) -> Vec<Line<'static>> {
    let state = task.state(now);
    let mut lines = Vec::new();
    push_wrapped(
        &mut lines,
        task.content.clone(),
        Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        width,
    );
    // 狀態列由多個樣式組成（顏色隨語意變），且短於最小面板寬度，因此不換行。
    let status = Line::from(vec![
        Span::styled("状态：", THEME.muted_style()),
        Span::styled(state.label(), THEME.status_style(state.tone())),
        Span::styled("　优先级：", THEME.muted_style()),
        Span::styled(
            task.priority.label(),
            THEME.status_style(task.priority.tone()),
        ),
    ]);
    debug_assert!(
        status.width() <= width,
        "任务状态列宽度 {} 超过面板宽度 {width}",
        status.width()
    );
    lines.push(status);
    let deadline_raw = task.deadline.map(|deadline| deadline.to_rfc3339());
    push_wrapped(
        &mut lines,
        format!("截止：{}", deadline_label(deadline_raw.as_deref())),
        THEME.muted_style(),
        width,
    );
    match &task.description {
        Some(description) => {
            push_wrapped(&mut lines, "描述：", THEME.muted_style(), width);
            push_multiline(
                &mut lines,
                description,
                Style::default().fg(THEME.text),
                width,
            );
        }
        None => push_wrapped(&mut lines, "描述：无", THEME.muted_style(), width),
    }
    lines
}
