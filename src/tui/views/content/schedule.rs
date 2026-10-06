//! 課表頁：本週課程清單與課程詳情。

use chrono::{Datelike as _, NaiveDate};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, Paragraph};

use crate::model::LessonEntry;
use crate::text::{fit_display, fit_display_start};
use crate::tui::app::App;
use crate::tui::theme::THEME;
use crate::tui::ui::render_list;

use super::columns::{
    SECTIONS_WIDTH, ScheduleColumns, ScheduleNeeds, WEEKDAY_WIDTH, schedule_columns,
    schedule_min_row_width,
};
use super::{
    DETAIL_HEIGHT, detail_panel, empty, empty_note, row_width, split_detail, title_suffix,
    too_narrow,
};

/// 清單區域的最小高度（標題與提示列之外的空間）。
const BODY_MIN_HEIGHT: u16 = 3;

/// 依目前資料繪製課表頁。
pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = schedule_title(app);

    // 學期外提示與無法解析課程的警示都不屬於清單本身：各留一列提示，避免
    // 使用者誤解頁面內容（與作業頁的警示列同型）。
    let (header, has_lessons) = {
        let Some(data) = app.schedule.ready() else {
            empty(
                frame,
                area,
                &title,
                app.schedule.note(),
                app.schedule.is_loading(),
            );
            return;
        };
        let mut header = Vec::new();
        if let Some(notice) = &data.notice {
            header.push(Line::from(Span::styled(
                format!(" {notice}"),
                THEME.muted_style(),
            )));
        }
        if data.skipped > 0 {
            header.push(Line::from(Span::styled(
                format!(" 已跳过 {} 门无法解析的课程", data.skipped),
                THEME.muted_style(),
            )));
        }
        (header, !data.lessons.is_empty())
    };
    let header_height = u16::try_from(header.len()).unwrap_or(u16::MAX);
    let [header_area, body_area] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(BODY_MIN_HEIGHT),
    ])
    .areas(area);
    if !header.is_empty() {
        frame.render_widget(
            Paragraph::new(header).style(THEME.base_style()),
            header_area,
        );
    }

    if !has_lessons {
        empty_note(frame, body_area, &title, "该周没有课程安排");
        return;
    }

    let (list_area, detail_area) = split_detail(body_area, app.schedule_detail, DETAIL_HEIGHT);
    let width = row_width(list_area);
    let (items, detail) = {
        let Some(data) = app.schedule.ready() else {
            return;
        };
        let Some(columns) = schedule_columns(width, ScheduleNeeds::of(&data.lessons)) else {
            too_narrow(frame, list_area, &title, schedule_min_row_width(), width);
            return;
        };
        let index = app.page_selection().min(data.lessons.len() - 1);
        let items = data
            .lessons
            .iter()
            .map(|lesson| lesson_item(lesson, columns))
            .collect::<Vec<_>>();
        let detail = app
            .schedule_detail
            .then(|| lesson_lines(&data.lessons[index]));
        (items, detail)
    };
    render_list(frame, list_area, &title, items, &mut app.schedule_state);
    if let (Some(area), Some(lines)) = (detail_area, detail) {
        detail_panel(frame, area, "课程详情", lines);
    }
}

/// 標題：學期（已知時）、週次（`第 N/M 周`）與更新狀態。
///
/// 切週載入期間資料已清空，但週次與總週數仍在 `App` 上，標題因此能立即反映
/// 使用者選擇的週次（與考勤流水「第 N/M 页」同型）。
fn schedule_title(app: &App) -> String {
    let suffix = title_suffix(app.updated_at.schedule.as_ref(), app.schedule.is_loading());
    match app.schedule.ready() {
        Some(data) => format!(
            "课表 · {} · 第 {}/{} 周{suffix}",
            data.semester,
            app.schedule_week.unwrap_or(data.week),
            app.schedule_total.unwrap_or(data.total_weeks),
        ),
        None => match (app.schedule_week, app.schedule_total) {
            (Some(week), Some(total)) => format!("课表 · 第 {week}/{total} 周{suffix}"),
            _ => "课表".to_owned(),
        },
    }
}

/// 課表列：日期／星期／節次／課程／地點／（教師）／考勤狀態。
///
/// 各欄以顯示寬度排版（中文佔兩欄），因此不同長度的課程名稱、地點與教師
/// 都不會讓考勤狀態欄左右浮動。
fn lesson_item(lesson: &LessonEntry, columns: ScheduleColumns) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{} ", lesson.date.format("%m-%d")),
            THEME.muted_style(),
        ),
        Span::styled(
            format!(
                "{} ",
                fit_display(weekday_label(lesson.date), WEEKDAY_WIDTH)
            ),
            THEME.muted_style(),
        ),
        Span::styled(
            format!("{} ", fit_display_start(&lesson.sections, SECTIONS_WIDTH)),
            THEME.muted_style(),
        ),
        Span::styled(
            format!("{} ", fit_display(&lesson.course_name, columns.course)),
            Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} ", fit_display(&lesson.classroom, columns.classroom)),
            THEME.muted_style(),
        ),
    ];
    if columns.teacher > 0 {
        spans.push(Span::styled(
            format!("{} ", fit_display(&lesson.teacher, columns.teacher)),
            THEME.muted_style(),
        ));
    }
    spans.push(Span::styled(
        lesson.attendance.label().to_owned(),
        THEME.status_style(lesson.attendance.tone()),
    ));
    ListItem::new(Line::from(spans))
}

fn lesson_lines(lesson: &LessonEntry) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                lesson.course_name.clone(),
                Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("　第 {} 节", lesson.sections), THEME.accent_style()),
        ]),
        Line::from(Span::styled(
            format!(
                "日期：{} {}　地点：{}　教师：{}",
                lesson.date,
                weekday_label(lesson.date),
                fallback(&lesson.classroom),
                fallback(&lesson.teacher)
            ),
            THEME.muted_style(),
        )),
        Line::from(Span::styled(
            format!("周次：{}", lesson.weeks),
            THEME.muted_style(),
        )),
        Line::from(vec![
            Span::styled("考勤：", THEME.muted_style()),
            Span::styled(
                lesson.attendance.label(),
                THEME.status_style(lesson.attendance.tone()),
            ),
        ]),
    ]
}

fn weekday_label(date: NaiveDate) -> &'static str {
    match date.weekday() {
        chrono::Weekday::Mon => "周一",
        chrono::Weekday::Tue => "周二",
        chrono::Weekday::Wed => "周三",
        chrono::Weekday::Thu => "周四",
        chrono::Weekday::Fri => "周五",
        chrono::Weekday::Sat => "周六",
        chrono::Weekday::Sun => "周日",
    }
}

fn fallback(value: &str) -> &str {
    if value.trim().is_empty() {
        "未知"
    } else {
        value
    }
}
