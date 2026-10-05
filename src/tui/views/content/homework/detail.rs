//! 任務頁詳情面板的內容：思源學堂作業與自訂義任務。
//!
//! 兩者都是「預先換行後的列」：詳情面板不做自動換行，列數必須在繪製前已知，
//! 捲動位移才夾得住（見內容區的 `scrolled_panel`）。

use chrono::{DateTime, FixedOffset};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::domain::homework::HomeworkItem;
use crate::domain::todo::Task;
use crate::sites::lms::ActivityKind;
use crate::tui::theme::THEME;

use super::super::{deadline_label, push_description, push_multiline, push_wrapped};

/// 作業詳情：標題／課程／截止／狀態／說明／作業描述。
pub(super) fn homework_lines(item: &HomeworkItem, width: usize) -> Vec<Line<'static>> {
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

/// 任務詳情（`enter` 展開的內容）。
pub(super) fn task_lines(
    task: &Task,
    now: DateTime<FixedOffset>,
    width: usize,
) -> Vec<Line<'static>> {
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
    push_wrapped(
        &mut lines,
        format!("标签：{}", task.display_tag().unwrap_or("无")),
        THEME.muted_style(),
        width,
    );
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
