//! 應用狀態測試：導航、選取範圍與載入狀態。

use chrono::NaiveDate;

use super::*;
use crate::domain::attendance_match::LessonAttendance;
use crate::domain::homework::{HomeworkInput, aggregate};
use crate::domain::todo::{PageRow, Priority, TASKS_HEADER, Task};
use crate::model::{FlowData, LessonEntry, ScheduleData};
use crate::sites::attendance::{AttendanceStatus, FlowRecord};
use crate::sites::lms::LmsCourse;

fn lesson(start_section: u32, end_section: u32) -> LessonEntry {
    LessonEntry {
        date: NaiveDate::from_ymd_opt(2026, 9, 14).expect("日期"),
        sections: format!("{start_section}-{end_section}"),
        start_section,
        end_section,
        course_name: "高等数学".to_owned(),
        classroom: "主楼A101".to_owned(),
        teacher: "张老师".to_owned(),
        weeks: "1-16".to_owned(),
        attendance: LessonAttendance::Recorded(AttendanceStatus::Normal),
    }
}

fn app_with_schedule(len: usize) -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.schedule = Page::Ready(ScheduleData {
        semester: "2026-2027-1".to_owned(),
        week: 2,
        total_weeks: 23,
        lessons: (0..len)
            .map(|index| lesson(1, u32::try_from(index + 1).expect("节次")))
            .collect(),
        skipped: 0,
        notice: None,
    });
    app.schedule_week = Some(2);
    app.schedule_total = Some(23);
    app
}

#[test]
fn navigates_pages_in_a_loop() {
    let mut app = App::new(AccessPolicy::Auto);
    assert_eq!(app.nav, NavItem::Schedule);

    app.nav_previous();
    assert_eq!(app.nav, NavItem::Lms, "应循环到最后一个页面");
    app.nav_next();
    assert_eq!(app.nav, NavItem::Schedule, "应循环回第一个页面");

    for _ in 0..NavItem::ALL.len() {
        app.nav_next();
    }
    assert_eq!(app.nav, NavItem::Schedule);
}

#[test]
fn selection_wraps_around_page_length() {
    let mut app = app_with_schedule(3);
    assert_eq!(app.page_len(), 3);

    app.select_previous();
    assert_eq!(app.page_selection(), 2, "在第一项上应循环到最后一项");

    app.select_next();
    assert_eq!(app.page_selection(), 0, "在最后一项上应循环回第一项");
}

#[test]
fn selection_normalizes_stale_index_before_wrapping() {
    let mut app = app_with_schedule(3);
    // 越界索引在畫面上一律夾到最後一項；移動應從該可見位置出發。
    app.schedule_state.select(Some(9));
    app.select_next();
    assert_eq!(app.page_selection(), 0, "应从可见的最后一项循环回开头");

    app.schedule_state.select(Some(9));
    app.select_previous();
    assert_eq!(app.page_selection(), 1, "应从可见的最后一项往前一项");
}

#[test]
fn selection_stays_on_single_item_page() {
    let mut app = app_with_schedule(1);
    app.select_next();
    assert_eq!(app.page_selection(), 0);
    app.select_previous();
    assert_eq!(app.page_selection(), 0);
}

#[test]
fn selection_wraps_across_lms_courses_and_flow_pages() {
    let mut app = App::new(AccessPolicy::Auto);
    app.nav = NavItem::Lms;
    app.lms.courses = Page::Ready(vec![course("1"), course("2")]);
    app.select_previous();
    assert_eq!(app.page_selection(), 1, "课程层应循环到最后一门");

    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(FlowData {
        records: vec![flow_record("1"), flow_record("2"), flow_record("3")],
        page: 1,
        total_pages: 1,
        total: 3,
    });
    app.select_previous();
    assert_eq!(app.page_selection(), 2, "流水页应循环到最后一笔");
}

fn course(id: &str) -> LmsCourse {
    LmsCourse {
        id: id.to_owned(),
        name: format!("课程{id}"),
        course_code: None,
        instructors: Vec::new(),
        semester: None,
        academic_year: None,
    }
}

fn flow_record(id: &str) -> FlowRecord {
    FlowRecord {
        id: id.to_owned(),
        classroom_name: Some("主楼A101".to_owned()),
        collect_time: Some("08:00".to_owned()),
        effective: true,
    }
}

#[test]
fn selection_is_safe_on_empty_pages() {
    let mut app = App::new(AccessPolicy::Auto);
    assert_eq!(app.page_len(), 0);
    app.select_next();
    app.select_previous();
    assert_eq!(app.page_selection(), 0);
}

#[test]
fn fail_target_marks_only_the_target_page() {
    let mut app = App::new(AccessPolicy::Auto);
    app.schedule.start_loading("正在加载…");
    app.lms.detail.start_loading("正在加载…");

    app.fail_target(FailedTarget::Schedule, "网络超时");

    assert!(
        matches!(&app.schedule, Page::Failed { message, .. } if message == "网络超时"),
        "目标页面应标记为失败"
    );
    assert!(app.lms.detail.is_loading(), "其他页面不受影响");
}

#[test]
fn refreshing_keeps_stale_data() {
    let mut app = app_with_schedule(2);
    app.schedule.start_loading("正在刷新…");
    assert_eq!(app.page_len(), 2, "刷新中仍显示旧资料");
    assert!(app.schedule.is_loading());
    assert_eq!(app.schedule.note(), Some("正在刷新…"));

    app.schedule.fail("网络超时");
    assert_eq!(app.page_len(), 2, "失败后仍保留旧资料");
    assert!(app.schedule.ready().is_some());
}

#[test]
fn settings_menu_cycles() {
    let mut state = SettingsState::default();
    state.previous();
    assert_eq!(state.index, SettingsState::COUNT - 1);
    state.next();
    assert_eq!(state.index, 0);
    assert_eq!(
        SettingsState::label(SettingsState::POLICY_INDEX),
        "访问模式"
    );
}

#[test]
fn messages_expire_only_after_ttl() {
    let mut app = App::new(AccessPolicy::Auto);
    assert!(app.message_text().is_none());

    app.set_message("登录成功");
    app.expire_message();
    assert_eq!(
        app.message_text(),
        Some("登录成功"),
        "刚设置的信息不应立即过期"
    );
    assert_eq!(app.message_text(), Some("登录成功"));
}

#[test]
fn animation_tick_advances_for_loading_dots() {
    let mut app = App::new(AccessPolicy::Auto);
    assert_eq!(app.tick, 0, "动画由第 0 相位开始");
    app.advance_tick();
    assert_eq!(app.tick, 1);
    app.advance_tick();
    assert_eq!(app.tick, 2);
}

// ── 用户协议閱讀門 ───────────────────────────────────

#[test]
fn agreement_requires_reaching_bottom_before_confirming() {
    let mut state = AgreementState::new();
    assert!(!state.can_confirm(), "版面未知时不得确认");

    state.sync_layout(10, 50);
    state.scroll_by(5);
    assert_eq!(state.scroll(), 5);
    assert!(!state.can_confirm(), "尚未到底部不得确认");

    state.scroll_by(1000);
    assert_eq!(state.scroll(), 40, "滚动应夹取到最大位置");
    assert!(state.can_confirm(), "到底部后可确认");

    // 黏性：讀到底部後捲回上方不應失去確認資格。
    state.scroll_by(-1000);
    assert_eq!(state.scroll(), 0);
    assert!(state.can_confirm());
}

#[test]
fn agreement_confirms_when_document_fits_viewport() {
    let mut state = AgreementState::new();
    state.sync_layout(30, 20);
    assert!(state.can_confirm(), "整份文件可见即视为已读完");
    assert_eq!(state.progress(), 100);
}

#[test]
fn agreement_ignores_scroll_before_first_layout() {
    let mut state = AgreementState::new();
    state.scroll_by(10);
    state.page_by(1);
    state.to_bottom();
    assert_eq!(state.scroll(), 0, "版面未知时不得滚动");
    assert!(!state.can_confirm(), "版面未知时不得确认");
    assert_eq!(state.progress(), 0);
}

#[test]
fn agreement_reclamps_after_resize() {
    let mut state = AgreementState::new();
    state.sync_layout(10, 50);
    state.to_bottom();
    assert_eq!(state.scroll(), 40);

    // 視窗變高：最大捲动位置縮小，位置應被夾取。
    state.sync_layout(45, 50);
    assert_eq!(state.scroll(), 5);
    assert!(state.can_confirm());

    // 視窗比文件更高：回到頂端且仍視為讀完。
    state.sync_layout(60, 50);
    assert_eq!(state.scroll(), 0);
    assert!(state.can_confirm());
}

#[test]
fn agreement_pages_by_viewport() {
    let mut state = AgreementState::new();
    state.sync_layout(10, 50);
    state.page_by(1);
    assert_eq!(state.scroll(), 10);
    state.page_by(-1);
    assert_eq!(state.scroll(), 0);
    state.to_bottom();
    state.to_top();
    assert_eq!(state.scroll(), 0);
}

#[test]
fn agreement_saving_blocks_confirm_and_failure_recovers() {
    let mut state = AgreementState::new();
    state.sync_layout(10, 50);
    state.to_bottom();
    assert!(state.can_confirm());

    state.start_saving();
    assert!(!state.can_confirm(), "保存中不得重复确认");

    state.fail("写入配置文件失败".to_owned());
    assert!(!state.saving);
    assert_eq!(state.error.as_deref(), Some("写入配置文件失败"));
    assert!(state.can_confirm(), "失败后可重试确认");
}

// ── 課程清單導航（依畫面可見順序）─────────────────────

/// 課程列（帶學期碼，供分區測試）。
fn course_with_term(id: &str, code: &str) -> LmsCourse {
    LmsCourse {
        id: id.to_owned(),
        name: format!("课程{id}"),
        course_code: None,
        instructors: Vec::new(),
        semester: Some(crate::sites::lms::models::LmsSemester {
            id: None,
            code: Some(code.to_owned()),
            name: None,
            real_name: None,
        }),
        academic_year: None,
    }
}

#[test]
fn course_navigation_follows_the_displayed_partition_order() {
    let mut app = App::new(AccessPolicy::Auto);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    // 原始順序：歷史(索引 0) → 本學期(索引 1)；畫面會把本學期課程置頂。
    app.lms.courses = Page::Ready(vec![
        course_with_term("1", "2025-2"),
        course_with_term("2", "2026-1"),
    ]);
    app.lms.courses_term = TermCode::parse("2026-2027-1");
    // 畫面第一門＝本學期課程（真實索引 1）。
    app.course_state.select(Some(1));

    // 畫面上的下一門是歷史課程（真實索引 0），不是不存在的原始索引 2。
    app.select_next();
    assert_eq!(app.page_selection(), 0, "应依画面顺序移到历史课程");

    // 再下一門循環回本學期課程。
    app.select_next();
    assert_eq!(app.page_selection(), 1, "应循环回本学期课程");

    // 反向：從本學期課程往前是歷史課程。
    app.select_previous();
    assert_eq!(app.page_selection(), 0);
}

/// 清空敏感欄位依「欄位角色」判定：口令與密碼清空、帳號保留。
#[test]
fn clear_secrets_follows_field_roles() {
    let forms = [
        FormState::setup(),
        FormState::unlock(),
        FormState::login_retry(SiteKind::Attendance),
        FormState::change_account(),
        FormState::change_passphrase(),
    ];
    for mut form in forms {
        for field in &mut form.fields {
            field.value.set("secret");
        }
        form.clear_secrets();
        for field in &form.fields {
            if field.role.is_secret() {
                assert!(field.value.is_empty(), "{:?} 应清空", field.role);
            } else {
                assert_eq!(field.value.value(), "secret", "{:?} 应保留", field.role);
            }
        }
    }
}

// ── 任務頁列模型 ─────────────────────────────────────────

/// 測試用任務。
fn todo(id: u64, content: &str, completed: bool) -> Task {
    Task {
        id,
        content: content.to_owned(),
        description: None,
        tag: None,
        deadline: None,
        priority: Priority::Low,
        completed,
    }
}

#[test]
fn task_page_combines_tasks_and_homework_in_one_list() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let input = HomeworkInput {
        course_id: "1".to_owned(),
        course_name: "编译原理".to_owned(),
        activity_id: "a-1".to_owned(),
        title: "第一次作业".to_owned(),
        end_time: Some("2026-10-01 23:59:59".to_owned()),
        description: None,
        submit_by_group: Some(false),
        submission_count: Some(0),
        note: None,
    };

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.tasks = vec![todo(1, "写实验报告", false), todo(2, "复习", true)];
    app.homework = Page::Ready(HomeworkData {
        items: aggregate(&[input], now),
        ..HomeworkData::default()
    });

    assert_eq!(app.page_len(), 2, "未完成分组应含一任务与一作业");
    assert_eq!(
        app.task_page_group_counts().get(HomeworkGroup::Unfinished),
        2
    );
    assert_eq!(
        app.task_page_group_counts().get(HomeworkGroup::Completed),
        1,
        "分组计数应包含自訂義任务"
    );

    let rows = app.task_page_rows();
    assert!(
        matches!(rows.first(), Some(PageRow::Header(text)) if *text == TASKS_HEADER),
        "任务段应排在最前面"
    );
    assert!(
        matches!(rows.get(1), Some(PageRow::Task(_))),
        "任务列应紧接在任务标题之后"
    );
    assert!(
        rows.iter().any(|row| matches!(row, PageRow::Spacer)),
        "两段之间应有一列空白"
    );
    assert!(
        rows.iter().any(|row| matches!(row, PageRow::Homework(_))),
        "作业段应接在任务段之后"
    );

    // 任務在前、作業在後：第一列是任務，第二列是作業，且首尾循環。
    assert_eq!(app.page_selection(), 0);
    app.select_next();
    assert_eq!(app.page_selection(), 1, "下一列应是作业");
    app.select_next();
    assert_eq!(app.page_selection(), 0, "非空清单应首尾循环");

    // 篩選同時作用於任務與作業，但不影響分組計數。
    app.task_filter = Some("第一次作业".to_owned());
    assert_eq!(app.page_len(), 1, "筛选后只剩匹配的作业");
    assert_eq!(app.task_filter_matches(), 1);
    assert_eq!(
        app.task_page_group_counts().get(HomeworkGroup::Unfinished),
        2,
        "分组计数不受筛选影响"
    );
}

// ── 任務頁混合排序 ───────────────────────────────────────

/// 任務頁的測試資料：兩個任務（一高一低）與兩項作業（一有一無截止時間）。
fn sortable_app(sort: SortMode) -> App {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let input = |activity_id: &str, title: &str, end_time: Option<&str>| HomeworkInput {
        course_id: "1".to_owned(),
        course_name: "编译原理".to_owned(),
        activity_id: activity_id.to_owned(),
        title: title.to_owned(),
        end_time: end_time.map(str::to_owned),
        description: None,
        submit_by_group: Some(false),
        submission_count: Some(0),
        note: None,
    };

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.tasks = vec![
        Task {
            deadline: Some(
                chrono::DateTime::parse_from_rfc3339("2026-12-31T20:00:00+08:00")
                    .expect("固定时间"),
            ),
            priority: Priority::High,
            ..todo(1, "整理笔记", false)
        },
        todo(2, "写实验报告", false),
    ];
    app.homework = Page::Ready(HomeworkData {
        items: aggregate(
            &[
                input("a-1", "第一次作业", Some("2026-12-30T12:00:00Z")),
                input("a-2", "第二次作业", None),
            ],
            now,
        ),
        ..HomeworkData::default()
    });
    app.task_sort = sort;
    app
}

/// 目前項目的標籤（供排序斷言）。
fn entry_label(entry: TaskEntry<'_>) -> String {
    match entry {
        TaskEntry::Task(task) => format!("任务:{}", task.content),
        TaskEntry::Homework(item) => format!("作业:{}", item.title),
    }
}

#[test]
fn default_sort_keeps_tasks_before_homework_with_sections() {
    let app = sortable_app(SortMode::Default);
    let labels: Vec<String> = app
        .task_page_entries()
        .into_iter()
        .map(entry_label)
        .collect();
    assert_eq!(
        labels,
        vec![
            "任务:整理笔记",
            "任务:写实验报告",
            "作业:第一次作业",
            "作业:第二次作业"
        ],
        "默认排序维持任务在前、作业在后"
    );
    assert!(
        app.task_page_rows()
            .iter()
            .any(|row| matches!(row, PageRow::Header(_))),
        "默认排序仍显示分段标题"
    );
}

#[test]
fn priority_sort_mixes_tasks_and_homework_without_sections() {
    let app = sortable_app(SortMode::Priority);
    let labels: Vec<String> = app
        .task_page_entries()
        .into_iter()
        .map(entry_label)
        .collect();
    assert_eq!(
        labels,
        vec![
            "作业:第一次作业", // 高（作业）・截止较早
            "任务:整理笔记",   // 高・截止较晚
            "作业:第二次作业", // 高（作业）但无截止
            "任务:写实验报告", // 低
        ],
        "作业的优先级一律视为高，任务与作业混在一起"
    );

    let rows = app.task_page_rows();
    assert_eq!(rows.len(), app.page_len(), "混合排序不再插入标题或空白列");
    assert!(
        rows.iter().all(|row| row.is_selectable()),
        "混合排序的每一列都可以选取：{:?}",
        rows.len()
    );
    assert_eq!(app.page_len(), 4);
}

#[test]
fn sorted_entries_follow_the_selection_and_keep_filtering() {
    let mut app = sortable_app(SortMode::Deadline);
    let labels: Vec<String> = app
        .task_page_entries()
        .into_iter()
        .map(entry_label)
        .collect();
    assert_eq!(
        labels,
        vec![
            "作业:第一次作业", // 2026-12-30 20:00
            "任务:整理笔记",   // 2026-12-31 20:00
            "作业:第二次作业", // 无截止（高）
            "任务:写实验报告", // 无截止（低）
        ]
    );

    // 選取索引對應的是排序後的清單（不是任務在前、作業在後）。
    app.set_selection(0);
    assert_eq!(
        app.selected_entry().map(entry_label),
        Some("作业:第一次作业".to_owned())
    );
    app.set_selection(2);
    assert_eq!(
        app.selected_entry().map(entry_label),
        Some("作业:第二次作业".to_owned())
    );

    // 分組與搜尋在混合排序下依然生效（搜尋會同時過濾任務與作業）。
    app.task_filter = Some("整理".to_owned());
    let labels: Vec<String> = app
        .task_page_entries()
        .into_iter()
        .map(entry_label)
        .collect();
    assert_eq!(labels, vec!["任务:整理笔记"]);
    assert_eq!(app.page_len(), 1);
    assert_eq!(
        app.selected_entry().map(entry_label),
        None,
        "清單變短後越界的索引不應指向別的項目"
    );
}
