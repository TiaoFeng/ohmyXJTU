//! 作業頁：未完成／已完成／待核实三分組清單與作業詳情。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, Paragraph, Wrap};

use crate::domain::homework::{HomeworkGroup, HomeworkItem};
use crate::sites::lms::ActivityKind;
use crate::text::fit_display;
use crate::tui::app::{App, HomeworkData, Page};
use crate::tui::theme::THEME;
use crate::tui::ui::render_list;

use super::columns::{GROUP_WIDTH, RowColumns, RowNeeds, homework_columns, homework_min_row_width};
use super::{
    deadline_cell, deadline_label, deadline_list_label, empty, group_label, panel_width,
    push_description, push_wrapped, row_width, scrolled_panel, split_detail, too_narrow,
};

/// 展開詳情時的框高範圍：內容區一半，並限制在可讀區間。
const DETAIL_MIN_HEIGHT: u16 = 7;
const DETAIL_MAX_HEIGHT: u16 = 14;

/// 依目前資料繪製作業頁。
pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    // 本幀若沒畫到詳情面板（載入中、空分組、終端過窄…），不得沿用上一幀的視窗
    // 資訊，否則底欄會一直提示「PgUp/PgDn 滚动」卻沒有東西可捲。
    app.homework_scroll.clear();
    // 標題：學期與各組計數。
    let title = match app.homework.ready() {
        Some(data) => {
            let term = data.term_label.as_deref().unwrap_or("未确定学期");
            format!(
                "作业 · {term} · 未完成 {} / 已完成 {} / 待核实 {}",
                data.group_count(HomeworkGroup::Unfinished),
                data.group_count(HomeworkGroup::Completed),
                data.group_count(HomeworkGroup::Unknown),
            )
        }
        None => "作业".to_owned(),
    };

    let Some(data) = app.homework.ready() else {
        empty(
            frame,
            area,
            &title,
            app.homework.note(),
            app.homework.is_loading(),
        );
        return;
    };

    // 分組標籤列＋載入狀態（更新失敗或有待核实項目時再加一行提示）。
    let header = homework_tabs(data, app.homework_group, app.homework.is_loading());
    let warning = homework_warning(app, data);
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

    // 展開詳情時加大面板：描述可能有好幾行，原本的 5 列內容區讀不了多少。
    let detail_height = (body_area.height / 2).clamp(DETAIL_MIN_HEIGHT, DETAIL_MAX_HEIGHT);
    let (list_area, detail_area) = split_detail(body_area, app.homework_detail, detail_height);
    let (items, detail) = {
        let Some(data) = app.homework.ready() else {
            return;
        };
        if data.group_count(app.homework_group) == 0 {
            empty_homework(frame, body_area, &title, app, data);
            return;
        }
        let visible = data.group_items(app.homework_group);
        let width = row_width(list_area);
        let Some(columns) = homework_columns(width, RowNeeds::of_homework(&visible)) else {
            too_narrow(frame, list_area, &title, homework_min_row_width(), width);
            return;
        };
        let index = app.page_selection().min(visible.len() - 1);
        let items = visible
            .iter()
            .map(|item| homework_item(item, columns))
            .collect::<Vec<_>>();
        // 詳情內容預先換行：列數必須已知，捲動位移才夾得住。
        let detail = detail_area.map(|area| homework_lines(visible[index], panel_width(area)));
        (items, detail)
    };
    render_list(frame, list_area, &title, items, &mut app.homework_state);
    if let (Some(area), Some(lines)) = (detail_area, detail) {
        scrolled_panel(frame, area, "作业详情", lines, &mut app.homework_scroll);
    }
}

/// 作業清單為空時的提示：載入中顯示進度，終態才是「沒有…」或「本学期暂无作业」。
///
/// 不經 [`super::empty`]：該函式將非載入中的文字視為失敗訊息，會讓空結果看起來像載入失敗。
fn empty_homework(frame: &mut Frame, area: Rect, title: &str, app: &App, data: &HomeworkData) {
    let message = if app.homework.is_loading() {
        app.homework.note().unwrap_or("正在汇总作业…").to_owned()
    } else if data.items.is_empty() {
        "本学期暂无作业".to_owned()
    } else {
        match app.homework_group {
            HomeworkGroup::Unfinished => "没有未完成的作业",
            HomeworkGroup::Completed => "没有已完成的作业",
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

/// 作業分組標籤列。
fn homework_tabs(data: &HomeworkData, group: HomeworkGroup, loading: bool) -> Line<'static> {
    let mut spans = Vec::new();
    for candidate in HomeworkGroup::ALL {
        let text = format!(" {} {} ", candidate.label(), data.group_count(candidate));
        let style = if candidate == group {
            THEME.highlight_style()
        } else {
            THEME.muted_style()
        };
        spans.push(Span::styled(text, style));
    }
    if let Some((done, total)) = data.progress {
        spans.push(Span::styled(
            format!("  加载中 {done}/{total} 门课程"),
            THEME.accent_style(),
        ));
    } else if loading {
        spans.push(Span::styled("  正在更新…", THEME.accent_style()));
    }
    if data.courses_skipped > 0 {
        spans.push(Span::styled(
            format!("  （{} 门课程缺少学期信息，未纳入）", data.courses_skipped),
            THEME.muted_style(),
        ));
    }
    Line::from(spans)
}

/// 提示列：更新失敗（保留舊資料時）或「已确认 / 待核实」。
fn homework_warning(app: &App, data: &HomeworkData) -> Option<Line<'static>> {
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

    let unknown = data.group_count(HomeworkGroup::Unknown);
    if unknown == 0 && data.courses_failed == 0 {
        return None;
    }
    let mut spans = Vec::new();
    if unknown > 0 {
        let confirmed = data.items.len() - unknown;
        let reason = data
            .issues
            .first()
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
    if data.courses_failed > 0 {
        if !spans.is_empty() {
            spans.push(Span::styled("；", THEME.muted_style()));
        }
        spans.push(Span::styled(
            format!("{} 门课程查询失败（按 r 重试）", data.courses_failed),
            THEME.error_style(),
        ));
    }
    Some(Line::from(spans))
}

/// 作業列：課程／標題／狀態／截止時間／（小组）。
fn homework_item(item: &HomeworkItem, columns: RowColumns) -> ListItem<'static> {
    let deadline = deadline_list_label(item.end_time.as_deref(), columns.compact);
    let mut spans = vec![
        Span::styled(
            format!("{} ", fit_display(&item.course_name, columns.label)),
            THEME.accent_style(),
        ),
        Span::styled(
            format!("{} ", fit_display(&item.title, columns.title)),
            Style::default().fg(THEME.text),
        ),
        Span::styled(
            format!("{} ", fit_display(item.state.label(), columns.state)),
            THEME.status_style(item.state.tone()),
        ),
        Span::styled(deadline_cell(&deadline, columns), THEME.muted_style()),
    ];
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
