//! 內容面板：課表、作業、考勤流水與思源學堂四個頁面。

use chrono::{Datelike as _, NaiveDate};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::domain::activity::ActivityGroup;
use crate::domain::course_list::{self, CourseRow};
use crate::domain::homework::{HomeworkGroup, HomeworkItem, parse_time};
use crate::sites::attendance::FlowRecord;
use crate::sites::lms::{ActivityKind, LmsActivity};
use crate::tui::app::{
    ActivityDetailView, App, HomeworkData, LessonEntry, LmsLevel, NavItem, Page,
};
use crate::tui::text::{display_width, fit_display, fit_display_start};
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
        |data| {
            format!(
                "课表 · {} · 第 {} 周{}",
                data.semester,
                data.week,
                title_suffix(app.updated_at.schedule.as_ref(), app.schedule.is_loading())
            )
        },
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
            let width = row_width(list_area);
            let (items, detail) = {
                let data = app.schedule.ready().expect("已确认存在课表数据");
                let Some(columns) = schedule_columns(width, ScheduleNeeds::of(&data.lessons))
                else {
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
    }
}

/// 星期欄寬（「周一」為兩個全角字）。
const WEEKDAY_WIDTH: usize = 4;
/// 節次欄寬（最寬為 `11-12`，靠右對齊）。
const SECTIONS_WIDTH: usize = 5;
/// 考勤狀態欄寬（最寬標籤為三個全角字，如「待考勤」）。
const STATUS_WIDTH: usize = 6;
/// 課表日期欄寬（`MM-DD`）。
const DATE_WIDTH: usize = 5;
/// 課表七欄之間的單欄間隔數。
const SCHEDULE_GAPS: usize = 6;
/// 課表課程欄的最小寬度（三個全角字，低於此值即代表終端過窄）。
const SCHEDULE_COURSE_MIN_WIDTH: usize = 6;
/// 課表地點欄的最小寬度。
const SCHEDULE_CLASSROOM_MIN_WIDTH: usize = 8;
/// 課表教師欄的最小寬度（低於此值時整欄收起）。
const SCHEDULE_TEACHER_MIN_WIDTH: usize = 6;

/// 課表清單的文字欄內容需求（可見列的最大顯示寬）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ScheduleNeeds {
    /// 課程名稱需求寬度。
    course: usize,
    /// 地點需求寬度。
    classroom: usize,
    /// 教師需求寬度。
    teacher: usize,
}

impl ScheduleNeeds {
    /// 由本週課程組出需求。
    fn of(lessons: &[LessonEntry]) -> Self {
        Self {
            course: lessons
                .iter()
                .map(|lesson| display_width(&lesson.course_name))
                .max()
                .unwrap_or(0),
            classroom: lessons
                .iter()
                .map(|lesson| display_width(&lesson.classroom))
                .max()
                .unwrap_or(0),
            teacher: lessons
                .iter()
                .map(|lesson| display_width(&lesson.teacher))
                .max()
                .unwrap_or(0),
        }
    }
}

/// 課表清單的欄寬（單位為終端顯示欄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScheduleColumns {
    /// 課程欄寬。
    course: usize,
    /// 地點欄寬。
    classroom: usize,
    /// 教師欄寬（`0` 代表不顯示）。
    teacher: usize,
}

/// 依可用寬度與內容需求決定課表欄寬；終端過窄而無法完整顯示時回傳 `None`。
///
/// 日期、星期、節次與考勤狀態為固定欄；欄位自左而右緊密排列，剩餘寬度留在
/// 列尾。課程、地點與教師欄都以可見內容的寬度為準（空間足夠即完整顯示）；
/// 空間不足時先隱藏教師欄、再縮地點，讓課程名稱最後才縮（下限為三個全角字）。
fn schedule_columns(available: usize, needs: ScheduleNeeds) -> Option<ScheduleColumns> {
    let fixed = schedule_fixed_width();
    let mut classroom = needs.classroom.max(SCHEDULE_CLASSROOM_MIN_WIDTH);
    let mut teacher = if needs.teacher == 0 {
        0
    } else {
        needs.teacher.max(SCHEDULE_TEACHER_MIN_WIDTH)
    };
    let mut rest = available.saturating_sub(fixed + classroom + teacher);
    while rest < needs.course && (teacher > 0 || classroom > SCHEDULE_CLASSROOM_MIN_WIDTH) {
        if teacher > SCHEDULE_TEACHER_MIN_WIDTH {
            teacher -= 2;
        } else if teacher > 0 {
            teacher = 0;
        } else {
            classroom -= 2;
        }
        rest = available.saturating_sub(fixed + classroom + teacher);
    }
    if rest < SCHEDULE_COURSE_MIN_WIDTH {
        return None;
    }
    Some(ScheduleColumns {
        course: rest.min(needs.course.max(SCHEDULE_COURSE_MIN_WIDTH)),
        classroom,
        teacher,
    })
}

/// 課表固定欄寬合計（含欄間隔）。
fn schedule_fixed_width() -> usize {
    DATE_WIDTH + WEEKDAY_WIDTH + SECTIONS_WIDTH + STATUS_WIDTH + SCHEDULE_GAPS
}

/// 課表清單可完整顯示所需的最小列寬。
fn schedule_min_row_width() -> usize {
    schedule_fixed_width() + SCHEDULE_CLASSROOM_MIN_WIDTH + SCHEDULE_COURSE_MIN_WIDTH
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

// ── 作業 ─────────────────────────────────────────────

fn homework(frame: &mut Frame, area: Rect, app: &mut App) {
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

    let (list_area, detail_area) = split_detail(body_area, app.homework_detail);
    let (items, detail) = {
        let data = app.homework.ready().expect("已确认存在作业数据");
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
        let detail = app.homework_detail.then(|| homework_lines(visible[index]));
        (items, detail)
    };
    render_list(frame, list_area, &title, items, &mut app.homework_state);
    if let (Some(area), Some(lines)) = (detail_area, detail) {
        detail_panel(frame, area, "作业详情", lines);
    }
}

/// 作業清單為空時的提示：載入中顯示進度，終態才是「沒有…」或「本学期暂无作业」。
///
/// 不經 [`empty`]：該函式將非載入中的文字視為失敗訊息，會讓空結果看起來像載入失敗。
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

/// 作業狀態欄寬（最寬標籤「待提交」「已完成」「待核实」為三個全角字）。
const HOMEWORK_STATE_WIDTH: usize = 6;
/// 「小组」欄寬。
const GROUP_WIDTH: usize = 4;
/// 完整截止時間欄寬（`YYYY-MM-DD HH:MM`）。
const DEADLINE_FULL_WIDTH: usize = 16;
/// 精簡截止時間欄寬（`MM-DD HH:MM`）。
const DEADLINE_COMPACT_WIDTH: usize = 11;
/// 截止時間欄的「截止」前綴。
const DEADLINE_PREFIX: &str = "截止 ";
/// 標題欄的最小寬度（三個全角字）。
const TITLE_MIN_WIDTH: usize = 6;
/// 作業課程欄的最小寬度（三個全角字，低於此值即代表終端過窄）。
const HOMEWORK_COURSE_MIN_WIDTH: usize = 6;
/// 活動類型欄寬（最寬標籤「课程内容」為四個全角字）。
const ACTIVITY_KIND_WIDTH: usize = 8;

/// 作業與活動清單的欄寬（單位為終端顯示欄）。
///
/// 兩者共用「標籤欄＋標題＋（狀態）＋截止時間＋（小组）」的骨架：作業的標籤
/// 欄是課程名稱、含狀態欄；活動的標籤欄是活動類型、沒有狀態欄（`state == 0`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowColumns {
    /// 首欄（課程名稱或活動類型）欄寬。
    label: usize,
    /// 標題欄寬。
    title: usize,
    /// 狀態欄寬；`0` 代表不顯示狀態欄。
    state: usize,
    /// 截止時間欄寬。
    deadline: usize,
    /// 是否顯示「截止」前綴。
    deadline_prefix: bool,
    /// 是否使用不含年份的精簡日期。
    compact: bool,
    /// 是否顯示「小组」欄。
    group: bool,
}

/// 作業／活動清單的文字欄內容需求（可見列的最大顯示寬）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RowNeeds {
    /// 標籤欄（課程名稱或活動類型）需求寬度。
    label: usize,
    /// 標題欄需求寬度。
    title: usize,
}

impl RowNeeds {
    /// 由作業列組出需求。
    fn of_homework(items: &[&HomeworkItem]) -> Self {
        Self {
            label: items
                .iter()
                .map(|item| display_width(&item.course_name))
                .max()
                .unwrap_or(0),
            title: items
                .iter()
                .map(|item| display_width(&item.title))
                .max()
                .unwrap_or(0),
        }
    }

    /// 由活動列組出需求（類型欄為固定寬度）。
    fn of_activities(activities: &[&LmsActivity]) -> Self {
        Self {
            label: ACTIVITY_KIND_WIDTH,
            title: activities
                .iter()
                .map(|activity| display_width(&activity.display_title()))
                .max()
                .unwrap_or(0),
        }
    }
}

/// 作業清單欄寬；終端過窄而無法完整顯示時回傳 `None`。
fn homework_columns(available: usize, needs: RowNeeds) -> Option<RowColumns> {
    row_columns(
        available,
        HOMEWORK_COURSE_MIN_WIDTH,
        HOMEWORK_STATE_WIDTH,
        needs,
    )
}

/// 活動清單欄寬（類型欄固定寬度、沒有狀態欄）；終端過窄時回傳 `None`。
fn activity_columns(available: usize, needs: RowNeeds) -> Option<RowColumns> {
    row_columns(available, ACTIVITY_KIND_WIDTH, 0, needs)
}

/// 作業清單可完整顯示所需的最小列寬。
fn homework_min_row_width() -> usize {
    row_min_row_width(HOMEWORK_COURSE_MIN_WIDTH, HOMEWORK_STATE_WIDTH)
}

/// 活動清單可完整顯示所需的最小列寬。
fn activity_min_row_width() -> usize {
    row_min_row_width(ACTIVITY_KIND_WIDTH, 0)
}

/// 依可用寬度與內容需求決定欄寬。
///
/// 欄位自左而右緊密排列，用不到的寬度留在**列尾**——不像以往把標題欄撐滿、
/// 將後半欄位推到畫面右側。標籤欄與標題欄以可見內容的寬度為準，空間足夠即
/// 完整顯示；不足時先依序犧牲「小组」欄、截止前綴與年份，再依內容需求比例
/// 縮減，連下限都放不下時回傳 `None`（由呼叫端提示放大終端）。
fn row_columns(
    available: usize,
    label_min: usize,
    state: usize,
    needs: RowNeeds,
) -> Option<RowColumns> {
    /// 依偏好排序的欄位組合：（顯示「小组」欄、顯示「截止」前綴、截止欄寬、寬裕量）。
    ///
    /// 「寬裕量」是標籤欄與標題欄下限之外的額外寬度：空間足夠寬鬆才採用該
    /// 組合，否則退而求其次（寧可先少顯示前綴或年份，也不把兩個文字欄壓到下限）。
    const TIERS: [(bool, bool, usize, usize); 4] = [
        (true, true, DEADLINE_FULL_WIDTH, 8),
        (false, true, DEADLINE_FULL_WIDTH, 8),
        (false, false, DEADLINE_FULL_WIDTH, 4),
        (false, false, DEADLINE_COMPACT_WIDTH, 0),
    ];
    let minimum = label_min + TITLE_MIN_WIDTH;
    let mut fallback = None;
    for (group, prefix, deadline, slack) in TIERS {
        let rest = available.saturating_sub(row_fixed_width(state, deadline, prefix, group));
        if rest < minimum {
            continue;
        }
        let columns = row_columns_at(rest, label_min, state, deadline, prefix, group, needs);
        if rest >= minimum + slack {
            return Some(columns);
        }
        // 後續組合的可分配寬度只會更大，先記住目前最寬的可行組合。
        fallback = Some(columns);
    }
    fallback
}

/// 依欄位組合計算各欄寬（`rest` 為標籤欄與標題欄可分配的寬度）。
fn row_columns_at(
    rest: usize,
    label_min: usize,
    state: usize,
    deadline: usize,
    deadline_prefix: bool,
    group: bool,
    needs: RowNeeds,
) -> RowColumns {
    let (label, title) = split_text_widths(rest, label_min, needs);
    RowColumns {
        label,
        title,
        state,
        deadline,
        deadline_prefix,
        compact: deadline == DEADLINE_COMPACT_WIDTH,
        group,
    }
}

/// 在標籤欄與標題欄之間分配寬度。
///
/// 空間足夠時兩欄都取內容需求寬度（完整顯示，剩餘寬度留在列尾）；不足時依
/// 內容需求比例分配，並確保標題欄不低於下限。
fn split_text_widths(rest: usize, label_min: usize, needs: RowNeeds) -> (usize, usize) {
    let label_need = needs.label.max(label_min);
    let title_need = needs.title.max(TITLE_MIN_WIDTH);
    if label_need + title_need <= rest {
        return (label_need, title_need);
    }
    let total = label_need + title_need;
    let label = (rest * label_need / total).clamp(label_min, rest.saturating_sub(TITLE_MIN_WIDTH));
    (label, rest - label)
}

/// 指定欄位組合的固定寬度（不含標籤欄與標題欄，含所有欄間隔）。
fn row_fixed_width(state: usize, deadline: usize, prefix: bool, group: bool) -> usize {
    state
        + deadline
        + if prefix {
            display_width(DEADLINE_PREFIX)
        } else {
            0
        }
        + if group { GROUP_WIDTH } else { 0 }
        // 間隔數＝欄數 − 1（標籤｜標題｜（狀態）｜截止｜（小组））。
        + 2
        + usize::from(state > 0)
        + usize::from(group)
}

/// 清單可完整顯示所需的最小列寬（標籤欄與標題欄均取下限）。
fn row_min_row_width(label_min: usize, state: usize) -> usize {
    row_fixed_width(state, DEADLINE_COMPACT_WIDTH, false, false) + label_min + TITLE_MIN_WIDTH
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
            THEME.status_style(item.state.label()),
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

/// 截止時間欄內容：可含「截止」前綴，後面還有欄位時補上間隔。
fn deadline_cell(deadline: &str, columns: RowColumns) -> String {
    let mut cell = String::new();
    if columns.deadline_prefix {
        cell.push_str(DEADLINE_PREFIX);
    }
    cell.push_str(&fit_display(deadline, columns.deadline));
    if columns.group {
        cell.push(' ');
    }
    cell
}

/// 提交單位欄文字（個人作業留白，維持欄位寬度）。
fn group_label(submit_by_group: bool) -> &'static str {
    if submit_by_group { "小组" } else { "" }
}

/// 列表中顯示的截止時間（`compact` 為真時省略年份）。
fn deadline_list_label(value: Option<&str>, compact: bool) -> String {
    parse_time(value).map_or_else(
        || value.map_or_else(|| "无截止时间".to_owned(), |raw| raw.trim().to_owned()),
        |time| {
            time.format(if compact {
                "%m-%d %H:%M"
            } else {
                "%Y-%m-%d %H:%M"
            })
            .to_string()
        },
    )
}

fn homework_lines(item: &HomeworkItem) -> Vec<Line<'static>> {
    let mut lines = vec![
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
    ];
    if let Some(note) = &item.note {
        lines.push(Line::from(Span::styled(
            format!("说明：{note}"),
            THEME.muted_style(),
        )));
    }
    lines
}

// ── 考勤流水 ─────────────────────────────────────────

fn flow(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app.attendance.ready().map_or_else(
        || "考勤流水".to_owned(),
        |data| {
            format!(
                "考勤流水 · 第 {}/{} 页（共 {} 条）{}",
                data.page,
                data.total_pages,
                data.total,
                title_suffix(
                    app.updated_at.attendance.as_ref(),
                    app.attendance.is_loading()
                )
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
                // 清單有邊框與高亮符號：欄寬需以實際列寬計算，狀態欄才能對齊。
                let columns = flow_columns(row_width(list_area));
                let items = data
                    .records
                    .iter()
                    .map(|record| flow_item(record, columns))
                    .collect::<Vec<_>>();
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

/// 流水清單的時間／地點欄寬（單位為終端顯示欄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlowColumns {
    /// 時間欄寬。
    time: usize,
    /// 地點欄寬。
    place: usize,
}

/// 依可用寬度決定欄寬：先縮地點、再縮時間，下限分別為 8／14 欄。
///
/// 極窄畫面維持下限，超出的部分交由清單自然裁切；狀態欄因此固定起點。
fn flow_columns(available: usize) -> FlowColumns {
    const TIME_DEFAULT: usize = 20;
    const PLACE_DEFAULT: usize = 16;
    const TIME_MIN: usize = 14;
    const PLACE_MIN: usize = 8;
    // 「未匹配」的顯示寬度（3 個全角字）。
    const STATUS: usize = 6;
    // 時間與地點、地點與狀態之間的空白。
    const GAPS: usize = 2;

    let place = available
        .saturating_sub(TIME_DEFAULT + STATUS + GAPS)
        .clamp(PLACE_MIN, PLACE_DEFAULT);
    let time = if place <= PLACE_MIN {
        available
            .saturating_sub(place + STATUS + GAPS)
            .clamp(TIME_MIN, TIME_DEFAULT)
    } else {
        TIME_DEFAULT
    };
    FlowColumns { time, place }
}

fn flow_item(record: &FlowRecord, columns: FlowColumns) -> ListItem<'static> {
    let time = fit_display(
        record.collect_time.as_deref().unwrap_or("（无时间）"),
        columns.time,
    );
    let place = fit_display(
        record.classroom_name.as_deref().unwrap_or("（无地点）"),
        columns.place,
    );
    ListItem::new(Line::from(vec![
        Span::styled(format!("{time} "), Style::default().fg(THEME.text)),
        Span::styled(format!("{place} "), THEME.muted_style()),
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
                    empty(frame, area, &title, Some("没有课程"), false);
                }
                Some(_) => {
                    let rows = {
                        let courses = app.lms.courses.ready().expect("已确认存在课程数据");
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
                    empty(frame, area, &title, Some("该课程没有活动"), false);
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
    }

    if let Some(note) = &detail.note {
        lines.push(Line::from(Span::styled(note.clone(), THEME.error_style())));
    }
    lines
}

// ── 共用 ─────────────────────────────────────────────

/// 清單列以外的固定佔用欄寬：外框左右欄線與選取列的高亮符號。
const ROW_CHROME_WIDTH: u16 = 3;

/// 清單列的可用顯示寬度：扣除外框左右欄線與選取列的高亮符號。
///
/// `render_list` 以 `▍` 作為高亮符號，ratatui 會為每一列保留該欄寬；欄寬
/// 計算若漏扣，最寬的內容會在最後一欄被裁掉。
fn row_width(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(ROW_CHROME_WIDTH))
}

/// 終端過窄、清單無法完整顯示時顯示的提示。
///
/// `required` 與 `available` 都是列寬（顯示欄）；訊息換算成使用者看到的終端
/// 欄數（含側邊欄與外框），方便對照要放大到多寬。
fn too_narrow(frame: &mut Frame, area: Rect, title: &str, required: usize, available: usize) {
    // 側邊欄寬度＝畫面總寬 − 內容區寬度。
    let sidebar = usize::from(frame.area().width.saturating_sub(area.width));
    let chrome = sidebar + usize::from(ROW_CHROME_WIDTH);
    let message = format!(
        "终端过窄，无法完整显示列表：请放大窗口（当前 {} 栏，本页至少需要 {} 栏）",
        available + chrome,
        required + chrome
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(message, THEME.muted_style())))
            .block(THEME.block(title))
            .style(THEME.base_style())
            .wrap(Wrap { trim: true }),
        area,
    );
}

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

/// 完整截止時間文字（詳情面板用，列表請用 [`deadline_list_label`]）。
fn deadline_label(value: Option<&str>) -> String {
    deadline_list_label(value, false)
}

fn fallback(value: &str) -> &str {
    if value.trim().is_empty() {
        "未知"
    } else {
        value
    }
}

/// 標題後綴：更新時間與載入狀態。
fn title_suffix(updated: Option<&String>, loading: bool) -> String {
    let mut suffix = String::new();
    if let Some(at) = updated {
        suffix.push_str(" · 更新于 ");
        suffix.push_str(at);
    }
    if loading {
        suffix.push_str(" · 更新中…");
    }
    suffix
}

#[cfg(test)]
#[path = "tests/content_test.rs"]
mod content_test;
