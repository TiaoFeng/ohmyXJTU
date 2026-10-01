//! 思源學堂頁：課程 / 活動 / 詳情三層瀏覽。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::domain::activity::ActivityGroup;
use crate::domain::course_list::{self, CourseRow};
use crate::model::ActivityDetailView;
use crate::sites::lms::{ActivityKind, LmsActivity};
use crate::text::fit_display;
use crate::tone::Tone;
use crate::tui::app::{App, LmsLevel};
use crate::tui::theme::THEME;
use crate::tui::ui::render_list;

use super::columns::{GROUP_WIDTH, RowColumns, RowNeeds, activity_columns, activity_min_row_width};
use super::{
    deadline_cell, deadline_label, deadline_list_label, empty, empty_note, group_label, row_width,
    submission_time_label, title_suffix, too_narrow,
};

/// 依目前資料繪製思源學堂頁（課程／活動／詳情三層）。
pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.lms.level {
        LmsLevel::Courses => {
            let title = app.lms.courses.ready().map_or_else(
                || "思源学堂".to_owned(),
                |courses| {
                    format!(
                        "思源学堂 · 课程 {} 门{}",
                        courses.len(),
                        title_suffix(app.updated_at.lms.as_ref(), app.lms.courses.is_loading())
                    )
                },
            );
            match app.lms.courses.ready() {
                None => empty(
                    frame,
                    area,
                    &title,
                    app.lms.courses.note(),
                    app.lms.courses.is_loading(),
                ),
                Some(courses) if courses.is_empty() => {
                    empty_note(frame, area, &title, "没有课程");
                }
                Some(_) => {
                    let rows = {
                        let Some(courses) = app.lms.courses.ready() else {
                            return;
                        };
                        course_list::course_rows(courses, app.lms.courses_term)
                    };
                    let items = rows.iter().map(course_row_item).collect::<Vec<_>>();
                    render_course_list(frame, area, &title, items, &mut app.course_state, &rows);
                }
            }
        }
        LmsLevel::Activities => {
            let course_name = app
                .lms
                .courses
                .ready()
                .and_then(|courses| courses.get(app.lms.course_index))
                .map_or_else(String::new, |course| course.name.clone());
            let title = format!(
                "{course_name} · 活动{}",
                title_suffix(app.updated_at.lms.as_ref(), app.lms.activities.is_loading())
            );

            match app.lms.activities.ready() {
                None => empty(
                    frame,
                    area,
                    &title,
                    app.lms.activities.note(),
                    app.lms.activities.is_loading(),
                ),
                Some(activities) if activities.is_empty() => {
                    empty_note(frame, area, &title, "该课程没有活动");
                }
                Some(_) => {
                    let width = row_width(area);
                    let needs = RowNeeds::of_activities(&app.lms_activities_in_group());
                    if let Some(columns) = activity_columns(width, needs) {
                        let tabs =
                            activity_tabs(&app.activity_group_counts(), app.lms.activity_group);
                        let items = app
                            .lms_activities_in_group()
                            .iter()
                            .map(|activity| activity_item(activity, columns))
                            .collect::<Vec<_>>();
                        activity_list(frame, area, &title, tabs, items, &mut app.activity_state);
                    } else {
                        too_narrow(frame, area, &title, activity_min_row_width(), width);
                    }
                }
            }
        }
        LmsLevel::Detail => {
            let title = app.lms.detail.ready().map_or_else(
                || "活动详情".to_owned(),
                |detail| format!("活动详情 · {}", detail.title),
            );
            match app.lms.detail.ready() {
                None => empty(
                    frame,
                    area,
                    &title,
                    app.lms.detail.note(),
                    app.lms.detail.is_loading(),
                ),
                Some(detail) => {
                    let lines = detail_lines(detail);
                    frame.render_widget(
                        Paragraph::new(lines)
                            .block(THEME.block(&title))
                            .style(THEME.base_style())
                            .wrap(Wrap { trim: false }),
                        area,
                    );
                }
            }
        }
    }
}

/// 課程列（歷史課程用較淺灰色，標題列不可選取）。
fn course_row_item(row: &CourseRow<'_>) -> ListItem<'static> {
    match row {
        CourseRow::Header(text) => ListItem::new(Line::from(Span::styled(
            format!("  {text}"),
            THEME.muted_style(),
        ))),
        CourseRow::Spacer => ListItem::new(Line::default()),
        CourseRow::Course {
            course, historical, ..
        } => {
            let name_style = if *historical {
                THEME.muted_style()
            } else {
                Style::default().fg(THEME.text).add_modifier(Modifier::BOLD)
            };
            ListItem::new(Line::from(vec![
                Span::styled(course.name.clone(), name_style),
                Span::styled(
                    format!("  {}", course.instructor_names()),
                    THEME.muted_style(),
                ),
                Span::styled(
                    format!("  {}", course.semester_label()),
                    THEME.muted_style(),
                ),
            ]))
        }
    }
}

/// 繪製課程清單：`state` 以「原始課程索引」為準，繪製時映射到含標題列的視覺位置。
///
/// 標題列不可選取；捲動位移沿用原 state 並在繪製後寫回。
fn render_course_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'static>>,
    state: &mut ListState,
    rows: &[CourseRow<'_>],
) {
    let mut visual = ListState::default();
    visual.select(
        state
            .selected()
            .and_then(|index| course_list::visual_index(rows, index)),
    );
    *visual.offset_mut() = state.offset();
    render_list(frame, area, title, items, &mut visual);
    *state.offset_mut() = visual.offset();
}

/// 活動列：類型／標題／截止時間／（小组）。
fn activity_item(activity: &LmsActivity, columns: RowColumns) -> ListItem<'static> {
    let deadline = deadline_list_label(activity.end_time.as_deref(), columns.compact);
    let mut spans = vec![
        Span::styled(
            format!("{} ", fit_display(activity.kind().label(), columns.label)),
            THEME.muted_style(),
        ),
        Span::styled(
            format!("{} ", fit_display(&activity.display_title(), columns.title)),
            Style::default().fg(THEME.text),
        ),
        Span::styled(deadline_cell(&deadline, columns), THEME.muted_style()),
    ];
    if columns.group {
        spans.push(Span::styled(
            fit_display(
                group_label(activity.submit_by_group.unwrap_or(false)),
                GROUP_WIDTH,
            ),
            THEME.muted_style(),
        ));
    }
    ListItem::new(Line::from(spans))
}

/// 活動分組標籤列（顯示各組計數，並以目前分組高亮）。
fn activity_tabs(counts: &[(ActivityGroup, usize)], current: ActivityGroup) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (group, count)) in counts.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", THEME.muted_style()));
        }
        let style = if *group == current {
            THEME.accent_style().add_modifier(Modifier::BOLD)
        } else {
            THEME.muted_style()
        };
        spans.push(Span::styled(format!("{} {count}", group.label()), style));
    }
    Line::from(spans)
}

/// 繪製帶分組標籤列的活動清單；目前分組為空時顯示提示。
fn activity_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    tabs: Line<'static>,
    items: Vec<ListItem<'static>>,
    state: &mut ListState,
) {
    let block = THEME.block(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [tabs_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    frame.render_widget(Paragraph::new(tabs).style(THEME.base_style()), tabs_area);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "本组暂无活动（[ / ] 切换分组）",
                THEME.muted_style(),
            )))
            .style(THEME.base_style()),
            list_area,
        );
        return;
    }

    let list = List::new(items)
        .highlight_style(THEME.highlight_style())
        .highlight_symbol("▍")
        .style(THEME.base_style());
    frame.render_stateful_widget(list, list_area, state);
}

fn detail_lines(detail: &ActivityDetailView) -> Vec<Line<'static>> {
    let mut meta = format!(
        "类型：{}　截止：{}",
        detail.kind.label(),
        deadline_label(detail.end_time.as_deref())
    );
    if detail.kind == ActivityKind::Homework {
        meta.push_str(&format!(
            "　提交单位：{}",
            if detail.submit_by_group {
                "小组"
            } else {
                "个人"
            }
        ));
    }
    let mut lines = vec![
        Line::from(Span::styled(
            detail.title.clone(),
            Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(meta, THEME.muted_style())),
    ];

    // 提交狀態只適用於作業；其他類型不顯示「待核实」。
    if detail.kind == ActivityKind::Homework {
        match &detail.submissions {
            Some(submissions) if submissions.is_empty() => {
                lines.push(Line::from(Span::styled(
                    "提交记录：暂无（视为未提交）",
                    THEME.status_style(Tone::Accent),
                )));
            }
            Some(submissions) => {
                lines.push(Line::from(Span::styled(
                    format!("提交记录：{} 条", submissions.len()),
                    THEME.status_style(Tone::Success),
                )));
                for submission in submissions.iter().take(5) {
                    let score = submission
                        .score
                        .as_ref()
                        .map_or(String::new(), |score| format!("　分数：{score}"));
                    lines.push(Line::from(Span::styled(
                        format!(
                            "· {}（最新版本：{}）{score}",
                            submission_time_label(submission.timestamp()),
                            if submission.is_latest_version.unwrap_or(false) {
                                "是"
                            } else {
                                "否"
                            }
                        ),
                        THEME.muted_style(),
                    )));
                }
            }
            None => {
                lines.push(Line::from(Span::styled(
                    "提交记录：无法确认（待核实）",
                    THEME.status_style(Tone::Warning),
                )));
            }
        }
    }

    if let Some(note) = &detail.note {
        lines.push(Line::from(Span::styled(note.clone(), THEME.error_style())));
    }
    lines
}
