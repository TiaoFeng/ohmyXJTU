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
use super::{detail_panel, empty, empty_note, row_width, split_detail, title_suffix, too_narrow};

/// 依目前資料繪製課表頁。
pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app.schedule.ready().map_or_else(
        || "课表".to_owned(),
        |data| {
            format!(
                "课表 · {} · 第 {} 周{}",
                data.semester,
                data.week,
                title_suffix(app.updated_at.schedule.as_ref(), app.schedule.is_loading())
            )
        },
    );

    // 無法解析星期資訊的課程不會出現在清單中：留一列提示，避免使用者誤以為
    // 課表完整（與作業頁的警示列同型）。
    let (warning, has_lessons) = {
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
        let warning = (data.skipped > 0).then(|| {
            Line::from(Span::styled(
                format!(" 已跳过 {} 门无法解析的课程", data.skipped),
                THEME.muted_style(),
            ))
        });
        (warning, !data.lessons.is_empty())
    };
    let header_height = u16::from(warning.is_some());
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(3)]).areas(area);
    if let Some(line) = warning {
        frame.render_widget(Paragraph::new(line).style(THEME.base_style()), header_area);
    }

    if !has_lessons {
        empty_note(frame, body_area, &title, "本周没有课程安排");
        return;
    }

    let (list_area, detail_area) = split_detail(body_area, app.schedule_detail);
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
        lesson.label.to_owned(),
        THEME.status_style(lesson.label),
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
            Span::styled(lesson.label, THEME.status_style(lesson.label)),
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
