//! 應用狀態測試：導航、選取範圍與載入狀態。

use chrono::NaiveDate;

use super::*;
use crate::sites::attendance::{AttendanceStatus, FlowRecord};
use crate::sites::lms::LmsCourse;

fn lesson(sections: &str) -> LessonEntry {
    LessonEntry {
        date: NaiveDate::from_ymd_opt(2026, 9, 14).expect("日期"),
        sections: sections.to_owned(),
        course_name: "高等数学".to_owned(),
        classroom: "主楼A101".to_owned(),
        teacher: "张老师".to_owned(),
        weeks: "1-16".to_owned(),
        status: Some(AttendanceStatus::Normal),
        label: "正常",
    }
}

fn app_with_schedule(len: usize) -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.schedule = Page::Ready(ScheduleData {
        semester: "2026-2027-1".to_owned(),
        week: 2,
        lessons: (0..len)
            .map(|index| lesson(&format!("1-{}", index + 1)))
            .collect(),
        skipped: 0,
    });
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
    assert_eq!(app.page_selection(), 0, "應從可見的最後一項循環回開頭");

    app.schedule_state.select(Some(9));
    app.select_previous();
    assert_eq!(app.page_selection(), 1, "應從可見的最後一項往前一項");
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
    assert_eq!(app.page_selection(), 1, "课程層应循环到最后一门");

    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(FlowData {
        records: vec![flow_record("1"), flow_record("2"), flow_record("3")],
        page: 1,
        total_pages: 1,
        total: 3,
    });
    app.select_previous();
    assert_eq!(app.page_selection(), 2, "流水頁应循环到最后一笔");
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
    assert_eq!(SettingsState::label(2), "访问模式");
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
        "刚设置的訊息不应立即过期"
    );
    assert_eq!(app.message_text(), Some("登录成功"));
}

#[test]
fn animation_tick_advances_for_loading_dots() {
    let mut app = App::new(AccessPolicy::Auto);
    assert_eq!(app.tick, 0, "動畫由第 0 相位開始");
    app.advance_tick();
    assert_eq!(app.tick, 1);
    app.advance_tick();
    assert_eq!(app.tick, 2);
}
