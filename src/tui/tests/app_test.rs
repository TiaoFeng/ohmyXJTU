//! 應用狀態測試：導航、選取範圍與載入狀態。

use chrono::NaiveDate;

use super::*;
use crate::sites::attendance::AttendanceStatus;

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
fn selection_is_clamped_to_page_length() {
    let mut app = app_with_schedule(3);
    assert_eq!(app.page_len(), 3);

    app.select_previous();
    assert_eq!(app.page_selection(), 0, "在第一项上不应越界");

    app.select_next();
    app.select_next();
    app.select_next();
    assert_eq!(app.page_selection(), 2, "在最后一项上不应越界");
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
fn marks_loading_pages_as_failed() {
    let mut app = App::new(AccessPolicy::Auto);
    app.schedule.start_loading("正在加载…");
    app.lms.detail.start_loading("正在加载…");
    app.homework = Page::Ready(Vec::new());

    app.fail_loading("网络超时");

    assert!(matches!(app.schedule, Page::Failed(ref message) if message == "网络超时"));
    assert!(matches!(app.lms.detail, Page::Failed(_)));
    // 已載入的頁面不受影響。
    assert!(app.homework.ready().is_some());
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
