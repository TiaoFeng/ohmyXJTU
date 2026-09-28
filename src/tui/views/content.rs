//! 內容面板：課表、作業、考勤流水與思源學堂四個頁面。

use chrono::{Datelike as _, NaiveDate};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, Paragraph, Wrap};

use crate::domain::homework::{HomeworkItem, parse_time};
use crate::sites::attendance::FlowRecord;
use crate::sites::lms::LmsCourse;
use crate::tui::app::{ActivityDetailView, App, LessonEntry, LmsLevel, NavItem};
use crate::tui::theme::THEME;
use crate::tui::views::main_view::render_list;

/// 依目前頁面繪製內容。
pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.nav {
        NavItem::Schedule => schedule(frame, area, app),
        NavItem::Homework => homework(frame, area, app),
        NavItem::Attendance => flow(frame, area, app),
        NavItem::Lms => lms(frame, area, app),
    }
}

// ── 課表 ─────────────────────────────────────────────

fn schedule(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app.schedule.ready().map_or_else(
        || "课表".to_owned(),
        |data| format!("课表 · {} · 第 {} 周", data.semester, data.week),
    );

    match app.schedule.ready() {
        None => {
            empty(
                frame,
                area,
                &title,
                app.schedule.note(),
                app.schedule.is_loading(),
            );
        }
        Some(data) if data.lessons.is_empty() => {
            empty(frame, area, &title, Some("本周没有课程安排"), false);
        }
        Some(_) => {
            let (list_area, detail_area) = split_detail(area, app.schedule_detail);
            let (items, detail) = {
                let data = app.schedule.ready().expect("已确认存在课表数据");
                let index = app.page_selection().min(data.lessons.len() - 1);
                let items = data.lessons.iter().map(lesson_item).collect::<Vec<_>>();
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
    }
}

fn lesson_item(lesson: &LessonEntry) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{} ", lesson.date.format("%m-%d")),
            THEME.muted_style(),
        ),
        Span::styled(
            format!("{:<3} ", weekday_label(lesson.date)),
            THEME.muted_style(),
        ),
        Span::styled(format!("{:>4}  ", lesson.sections), THEME.muted_style()),
        Span::styled(
            lesson.course_name.clone(),
            Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", lesson.classroom), THEME.muted_style()),
        Span::styled(format!("  {}", lesson.teacher), THEME.muted_style()),
        Span::styled(
            format!("  {}", lesson.label),
            THEME.status_style(lesson.label),
        ),
    ]))
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

// ── 作業 ─────────────────────────────────────────────

fn homework(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app.homework.ready().map_or_else(
        || "作业".to_owned(),
        |items| format!("作业 · 待处理 {} 项", items.len()),
    );

    match app.homework.ready() {
        None => {
            empty(
                frame,
                area,
                &title,
                app.homework.note(),
                app.homework.is_loading(),
            );
        }
        Some(items) if items.is_empty() => {
            empty(frame, area, &title, Some("没有待提交的作业"), false);
        }
        Some(_) => {
            let (list_area, detail_area) = split_detail(area, app.homework_detail);
            let (items, detail) = {
                let data = app.homework.ready().expect("已确认存在作业数据");
                let index = app.page_selection().min(data.len() - 1);
                let items = data.iter().map(homework_item).collect::<Vec<_>>();
                let detail = app.homework_detail.then(|| homework_lines(&data[index]));
                (items, detail)
            };
            render_list(frame, list_area, &title, items, &mut app.homework_state);
            if let (Some(area), Some(lines)) = (detail_area, detail) {
                detail_panel(frame, area, "作业详情", lines);
            }
        }
    }
}

fn homework_item(item: &HomeworkItem) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<10} ", truncate(&item.course_name, 10)),
            THEME.accent_style(),
        ),
        Span::styled(item.title.clone(), Style::default().fg(THEME.text)),
        Span::styled(
            format!("  {}", item.state.label()),
            THEME.status_style(item.state.label()),
        ),
        Span::styled(
            format!("  截止 {}", deadline_label(item.end_time.as_deref())),
            THEME.muted_style(),
        ),
        Span::styled(
            if item.submit_by_group { "  小组" } else { "" },
            THEME.muted_style(),
        ),
    ]))
}

fn homework_lines(item: &HomeworkItem) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            item.title.clone(),
            Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("课程：{}", item.course_name),
            THEME.muted_style(),
        )),
        Line::from(Span::styled(
            format!("截止：{}", deadline_label(item.end_time.as_deref())),
            THEME.muted_style(),
        )),
        Line::from(vec![
            Span::styled("状态：", THEME.muted_style()),
            Span::styled(item.state.label(), THEME.status_style(item.state.label())),
            Span::styled(
                if item.submit_by_group {
                    "　提交单位：小组"
                } else {
                    "　提交单位：个人"
                },
                THEME.muted_style(),
            ),
        ]),
    ]
}

// ── 考勤流水 ─────────────────────────────────────────

fn flow(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app.attendance.ready().map_or_else(
        || "考勤流水".to_owned(),
        |data| {
            format!(
                "考勤流水 · 第 {}/{} 页（共 {} 条）",
                data.page, data.total_pages, data.total
            )
        },
    );

    match app.attendance.ready() {
        None => {
            empty(
                frame,
                area,
                &title,
                app.attendance.note(),
                app.attendance.is_loading(),
            );
        }
        Some(data) if data.records.is_empty() => {
            empty(frame, area, &title, Some("本页没有流水记录"), false);
        }
        Some(_) => {
            let (list_area, detail_area) = split_detail(area, app.flow_detail);
            let (items, detail) = {
                let data = app.attendance.ready().expect("已确认存在流水数据");
                let index = app.page_selection().min(data.records.len() - 1);
                let items = data.records.iter().map(flow_item).collect::<Vec<_>>();
                let detail = app.flow_detail.then(|| flow_lines(&data.records[index]));
                (items, detail)
            };
            render_list(frame, list_area, &title, items, &mut app.flow_state);
            if let (Some(area), Some(lines)) = (detail_area, detail) {
                detail_panel(frame, area, "流水详情", lines);
            }
        }
    }
}

fn flow_item(record: &FlowRecord) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!(
                "{:<20} ",
                record.collect_time.as_deref().unwrap_or("（无时间）")
            ),
            Style::default().fg(THEME.text),
        ),
        Span::styled(
            format!(
                "{:<16} ",
                record.classroom_name.as_deref().unwrap_or("（无地点）")
            ),
            THEME.muted_style(),
        ),
        Span::styled(
            flow_status_label(record),
            THEME.status_style(flow_status_label(record)),
        ),
    ]))
}

fn flow_lines(record: &FlowRecord) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            format!("时间：{}", record.collect_time.as_deref().unwrap_or("未知")),
            THEME.muted_style(),
        )),
        Line::from(Span::styled(
            format!(
                "地点：{}",
                record.classroom_name.as_deref().unwrap_or("未知")
            ),
            THEME.muted_style(),
        )),
        Line::from(vec![
            Span::styled("状态：", THEME.muted_style()),
            Span::styled(
                flow_status_label(record),
                THEME.status_style(flow_status_label(record)),
            ),
        ]),
    ]
}

fn flow_status_label(record: &FlowRecord) -> &'static str {
    if record.effective {
        "有效"
    } else {
        "未匹配"
    }
}

// ── 思源學堂 ─────────────────────────────────────────

fn lms(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.lms.level {
        LmsLevel::Courses => {
            let title = app.lms.courses.ready().map_or_else(
                || "思源学堂".to_owned(),
                |courses| format!("思源学堂 · 课程 {} 门", courses.len()),
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
                    empty(frame, area, &title, Some("没有课程"), false);
                }
                Some(_) => {
                    let items = {
                        let courses = app.lms.courses.ready().expect("已确认存在课程数据");
                        courses.iter().map(course_item).collect::<Vec<_>>()
                    };
                    render_list(frame, area, &title, items, &mut app.course_state);
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
            let title = format!("{course_name} · 活动");

            match app.lms.activities.ready() {
                None => empty(
                    frame,
                    area,
                    &title,
                    app.lms.activities.note(),
                    app.lms.activities.is_loading(),
                ),
                Some(activities) if activities.is_empty() => {
                    empty(frame, area, &title, Some("该课程没有活动"), false);
                }
                Some(_) => {
                    let items = {
                        let activities = app.lms.activities.ready().expect("已确认存在活动数据");
                        activities
                            .iter()
                            .map(|activity| {
                                let kind = activity.kind().label();
                                ListItem::new(Line::from(vec![
                                    Span::styled(format!("{kind:<6} "), THEME.muted_style()),
                                    Span::styled(
                                        activity.display_title(),
                                        Style::default().fg(THEME.text),
                                    ),
                                    Span::styled(
                                        format!(
                                            "  截止 {}",
                                            deadline_label(activity.end_time.as_deref())
                                        ),
                                        THEME.muted_style(),
                                    ),
                                    Span::styled(
                                        if activity.submit_by_group.unwrap_or(false) {
                                            "  小组"
                                        } else {
                                            ""
                                        },
                                        THEME.muted_style(),
                                    ),
                                ]))
                            })
                            .collect::<Vec<_>>()
                    };
                    render_list(frame, area, &title, items, &mut app.activity_state);
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

fn course_item(course: &LmsCourse) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(
            course.name.clone(),
            Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        ),
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

fn detail_lines(detail: &ActivityDetailView) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            detail.title.clone(),
            Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "类型：{}　截止：{}　提交单位：{}",
                detail.kind,
                deadline_label(detail.end_time.as_deref()),
                if detail.submit_by_group {
                    "小组"
                } else {
                    "个人"
                }
            ),
            THEME.muted_style(),
        )),
    ];

    match &detail.submissions {
        Some(submissions) if submissions.is_empty() => {
            lines.push(Line::from(Span::styled(
                "提交记录：暂无（视为未提交）",
                THEME.status_style("待提交"),
            )));
        }
        Some(submissions) => {
            lines.push(Line::from(Span::styled(
                format!("提交记录：{} 条", submissions.len()),
                THEME.status_style("正常"),
            )));
            for submission in submissions.iter().take(5) {
                let score = submission
                    .score
                    .as_ref()
                    .map_or(String::new(), |score| format!("　分数：{score}"));
                lines.push(Line::from(Span::styled(
                    format!(
                        "· {}（最新版本：{}）{score}",
                        submission.timestamp().unwrap_or("未知时间"),
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
                THEME.status_style("待核实"),
            )));
        }
    }

    if let Some(note) = &detail.note {
        lines.push(Line::from(Span::styled(note.clone(), THEME.error_style())));
    }
    lines
}

// ── 共用 ─────────────────────────────────────────────

fn split_detail(area: Rect, open: bool) -> (Rect, Option<Rect>) {
    if !open {
        return (area, None);
    }
    let [list, detail] = Layout::vertical([Constraint::Min(6), Constraint::Length(7)]).areas(area);
    (list, Some(detail))
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

fn deadline_label(value: Option<&str>) -> String {
    parse_time(value).map_or_else(
        || value.map_or_else(|| "无截止时间".to_owned(), |raw| raw.trim().to_owned()),
        |time| time.format("%Y-%m-%d %H:%M").to_string(),
    )
}

fn fallback(value: &str) -> &str {
    if value.trim().is_empty() {
        "未知"
    } else {
        value
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}
