//! 任務頁的列建構：分段標題、思源學堂作業列與自訂義任務列。
//!
//! 兩種列共用同一組欄位骨架（欄寬定義在上層的 `columns`），因此放在同一個
//! 檔案，讓「欄位對齊」的差異一眼可見：作業有狀態欄與小組欄，任務有優先級與
//! 標籤欄。

use chrono::{DateTime, FixedOffset};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

use crate::domain::homework::HomeworkItem;
use crate::domain::todo::Task;
use crate::text::fit_display;
use crate::tui::theme::THEME;

use super::super::columns::{GROUP_WIDTH, RowColumns, TAG_SEPARATOR, TASK_PRIORITY_WIDTH};
use super::super::{deadline_cell, deadline_list_label, group_label};

/// 分段標題列（強調色；不可選取）。
pub(super) fn section_header_item(text: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("  {text}"),
        THEME.accent_style().add_modifier(Modifier::BOLD),
    )))
}

/// 作業列：課程／標題／狀態／截止時間／（小组）。
///
/// `checkbox` 為 `Some` 時代表目前在多選模式；作業列不可勾選，以空白佔位
/// 維持與任務列相同的欄位對齊。
pub(super) fn homework_item(
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

/// 自訂義任務列：優先級／內容／狀態／截止時間（與作業列共用欄位骨架）。
///
/// `checked` 為 `Some` 時代表目前在多選模式（顯示勾選框）。
pub(super) fn task_item(
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
    // 標籤欄：優先級（語意色）＋（有標籤時）「・標籤」（一般文字色）。標籤在欄寬
    // 不足時被截掉，優先級永遠可見。
    spans.push(Span::styled(
        task.priority.label().to_owned(),
        THEME.status_style(task.priority.tone()),
    ));
    let tag_width = columns.label.saturating_sub(TASK_PRIORITY_WIDTH);
    let tag_cell = match task.display_tag() {
        Some(tag) if tag_width > 0 => fit_display(&format!("{TAG_SEPARATOR}{tag}"), tag_width),
        _ => " ".repeat(tag_width),
    };
    spans.push(Span::styled(tag_cell, Style::default().fg(THEME.text)));
    spans.push(Span::raw(" "));
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
