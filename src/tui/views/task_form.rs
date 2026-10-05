//! 新增／編輯任務的表單彈窗。
//!
//! 樣式沿用 `ui-ref`：欄位標籤固定寬度、聚焦欄位高亮、`tab` 切換欄位、
//! `enter` 在描述欄換行、`^s` 保存、`esc` 取消。驗證或保存失敗就在提示列
//! 就地顯示（表單與輸入內容都保留，不必重新輸入）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::text::pad_display;
use crate::tone::Tone;
use crate::tui::app::{TaskField, TaskFormMode, TaskFormState};
use crate::tui::text::InputLine;
use crate::tui::theme::THEME;
use crate::tui::ui::{centered_rect, hint_line, input_window, popup_surface};

/// 彈窗寬度（含邊框）。
const FORM_WIDTH: u16 = 64;
/// 彈窗高度（含邊框）：內容 1＋標籤 1＋描述 3＋截止 1＋優先級 1＋完成 1＋空行 1＋提示 1＋邊框 2。
const FORM_HEIGHT: u16 = 12;
/// 標籤欄寬度（顯示欄，含「: 」）。
const LABEL_WIDTH: u16 = 12;
/// 描述欄的可見行數。
const DESC_ROWS: usize = 3;

/// 繪製任務表單。
pub fn draw(frame: &mut Frame, form: &mut TaskFormState) {
    let area = centered_rect(frame.area(), FORM_WIDTH, FORM_HEIGHT);
    let inner = popup_surface(frame, area, form_title(form));

    let [
        content_area,
        tag_area,
        desc_area,
        deadline_area,
        priority_area,
        completed_area,
        _,
        hint_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(DESC_ROWS as u16),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    draw_text_field(
        frame,
        "内容",
        &form.content,
        None,
        content_area,
        form.focus == TaskField::Content,
    );
    draw_text_field(
        frame,
        "标签",
        &form.tag,
        Some("（可留空，6 个汉字以内）"),
        tag_area,
        form.focus == TaskField::Tag,
    );
    draw_description(frame, form, desc_area);
    draw_text_field(
        frame,
        "截止",
        &form.deadline,
        Some("2026-12-31 12:30（可留空）"),
        deadline_area,
        form.focus == TaskField::Deadline,
    );
    draw_choice_field(
        frame,
        "优先级",
        form.priority.label(),
        THEME.status_style(form.priority.tone()),
        priority_area,
        form.focus == TaskField::Priority,
    );
    let (completed, tone) = if form.completed {
        ("已完成", Tone::Success)
    } else {
        ("未完成", Tone::Accent)
    };
    draw_choice_field(
        frame,
        "完成",
        completed,
        THEME.status_style(tone),
        completed_area,
        form.focus == TaskField::Completed,
    );

    let hint = match &form.error {
        Some(error) => Line::from(Span::styled(
            error.clone(),
            THEME.error_style().add_modifier(Modifier::BOLD),
        )),
        None if form.busy => Line::from(Span::styled("正在保存…", THEME.accent_style())),
        None => hint_line("tab 切换字段 · enter 换行（描述）· ←/→ 切换选项 · ^s 保存 · esc 取消"),
    };
    frame.render_widget(Paragraph::new(hint).style(THEME.surface_style()), hint_area);
}

/// 彈窗標題。
fn form_title(form: &TaskFormState) -> &'static str {
    match form.mode {
        TaskFormMode::Add => "添加任务",
        TaskFormMode::Edit { .. } => "编辑任务",
    }
}

/// 欄位標籤（固定寬度，聚焦時以強調色標示）。
fn label_span(label: &str, focused: bool) -> Span<'static> {
    let padded = pad_display(label, usize::from(LABEL_WIDTH) - 2);
    let style = if focused {
        THEME.accent_style().add_modifier(Modifier::BOLD)
    } else {
        THEME.muted_style()
    };
    Span::styled(format!("{padded}: "), style)
}

/// 單行文字欄位（聚焦時在游標處顯示編輯游標）。
fn draw_text_field(
    frame: &mut Frame,
    label: &str,
    line: &InputLine,
    placeholder: Option<&str>,
    area: Rect,
    focused: bool,
) {
    let value_width = usize::from(area.width.saturating_sub(LABEL_WIDTH));
    let (visible, cursor) = input_window(line, value_width);
    let value_span = match (line.is_empty(), placeholder) {
        (true, Some(hint)) => Span::styled(hint.to_owned(), THEME.muted_style()),
        (true, None) => Span::raw(""),
        (false, _) => Span::styled(visible, Style::default().fg(THEME.text)),
    };
    let rendered = Line::from(vec![label_span(label, focused), value_span]);
    frame.render_widget(Paragraph::new(rendered).style(THEME.surface_style()), area);
    if focused && value_width > 0 {
        frame.set_cursor_position((area.x + LABEL_WIDTH + cursor, area.y));
    }
}

/// 描述欄（多行；只顯示游標附近 [`DESC_ROWS`] 行，過寬的行水平捲動）。
fn draw_description(frame: &mut Frame, form: &mut TaskFormState, area: Rect) {
    let focused = form.focus == TaskField::Description;
    let value_width = usize::from(area.width.saturating_sub(LABEL_WIDTH));
    let start = form.description.window_start(DESC_ROWS);
    let cursor_row = form.description.row();

    for index in 0..DESC_ROWS {
        let row_area = Rect {
            y: area.y + index as u16,
            ..area
        };
        let mut spans = vec![if index == 0 {
            label_span("描述", focused)
        } else {
            Span::raw(" ".repeat(LABEL_WIDTH as usize))
        }];

        let line = form.description.line(start + index);
        let cursor = match line {
            Some(line) => {
                let (visible, cursor) = input_window(line, value_width);
                if line.is_empty() && start + index == 0 {
                    spans.push(Span::styled(
                        "（可留空，enter 换行）".to_owned(),
                        THEME.muted_style(),
                    ));
                } else {
                    spans.push(Span::styled(visible, Style::default().fg(THEME.text)));
                }
                cursor
            }
            None => 0,
        };
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(THEME.surface_style()),
            row_area,
        );
        if focused && value_width > 0 && start + index == cursor_row {
            frame.set_cursor_position((row_area.x + LABEL_WIDTH + cursor, row_area.y));
        }
    }
}

/// 選項欄位（左右鍵切換）。
fn draw_choice_field(
    frame: &mut Frame,
    label: &str,
    value: &str,
    value_style: Style,
    area: Rect,
    focused: bool,
) {
    let arrow = if focused {
        THEME.accent_style()
    } else {
        THEME.muted_style()
    };
    let line = Line::from(vec![
        label_span(label, focused),
        Span::styled("< ", arrow),
        Span::styled(value.to_owned(), value_style.add_modifier(Modifier::BOLD)),
        Span::styled(" >", arrow),
    ]);
    frame.render_widget(Paragraph::new(line).style(THEME.surface_style()), area);
}
