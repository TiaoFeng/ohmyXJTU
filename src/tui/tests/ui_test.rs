//! 繪製層測試：以 `TestBackend` 渲染表單與登入彈窗，驗證欄位版面與游標位置。
//!
//! 中文標籤的顯示寬度是字元數的兩倍，因此「標籤補白、值區寬度、游標欄位」必須
//! 由同一套版面計算決定；驗證碼與簡訊驗證的輸入框也必須真的畫出來。

use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use crate::config::AccessPolicy;
use crate::domain::attendance_match::LessonAttendance;
use crate::domain::homework::{HomeworkGroup, HomeworkInput, HomeworkItem, aggregate};
use crate::domain::semester::TermCode;
use crate::domain::todo::{Priority, SortMode, Task};
use crate::model::{ActivityDetailView, FlowData, LessonEntry, ScheduleData};
use crate::session::{AccessMode, SiteKind};
use crate::sites::attendance::{AttendanceStatus, FlowRecord};
use crate::sites::lms::{
    ActivityContent, ActivityKind, BODY_NOT_OBJECT_NOTE, LmsActivity, LmsCourse, LmsSubmissionList,
    LmsUpload, TOP_LEVEL_BODY_NOTE,
};
use crate::task::HomeworkIssue;
use crate::tui::app::{
    AgreementState, App, FormState, HomeworkData, LmsLevel, LoginScreen, NavItem, Page, Screen,
    SettingsState, TaskBatchMenuState, TaskConfirmState, TaskFormState, TaskMenuState,
    TermPickerState,
};
use crate::tui::text::{InputLine, MASK_CHAR};
use crate::tui::theme::THEME;

use super::*;

/// 測試終端寬度。
const WIDTH: u16 = 100;
/// 測試終端高度。
const HEIGHT: u16 = 30;
/// 表單彈窗（76 寬）內框的起始欄位。
const FORM_INNER_X: u16 = (WIDTH - 76) / 2 + 1;
/// 表單彈窗的內框寬度。
const FORM_INNER_WIDTH: u16 = 76 - 2;
/// 驗證碼彈窗（84 寬）內框的起始欄位。
const LOGIN_INNER_X: u16 = (WIDTH - 84) / 2 + 1;

/// 以測試終端繪製畫面。
fn draw(width: u16, height: u16, render: impl FnOnce(&mut Frame)) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("建立测试终端");
    terminal.draw(render).expect("绘制画面");
    terminal
}

/// 取出某一列的畫面文字（寬字元的續格會被跳過，尾端空白去除）。
fn row_text(backend: &TestBackend, y: u16) -> String {
    let area = backend.buffer().area;
    let mut text = String::new();
    let mut skip = 0_u16;

    for x in area.x..area.x + area.width {
        let symbol = backend.buffer()[(x, y)].symbol();
        if skip == 0 {
            text.push_str(symbol);
            let width = u16::try_from(Line::from(symbol).width()).unwrap_or(0);
            skip = width.saturating_sub(1);
        } else {
            skip -= 1;
        }
    }

    text.trim_end().to_owned()
}

/// 畫面上所有文字。
fn screen_text(backend: &TestBackend) -> String {
    let area = backend.buffer().area;
    (area.y..area.y + area.height)
        .map(|y| row_text(backend, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 找出含有指定文字的列。
fn find_row(backend: &TestBackend, needle: &str) -> (u16, String) {
    let area = backend.buffer().area;
    for y in area.y..area.y + area.height {
        let row = row_text(backend, y);
        if row.contains(needle) {
            return (y, row);
        }
    }
    panic!("画面上找不到 {needle}:\n{backend}");
}

/// 子字串在該列中的起始欄位（以顯示寬度計算，不是位元組位移）。
fn column_of(row: &str, needle: &str) -> u16 {
    let offset = row.find(needle).expect("子字符串应存在于该列");
    u16::try_from(Line::from(&row[..offset]).width()).unwrap_or(u16::MAX)
}

/// 內容區（側邊欄之後）中，指定文字的起始欄位。
///
/// 側邊欄的導覽標籤可能與內容同名（例如「任务」），因此一律從內容區左界之後
/// 開始找，不能直接在整列文字上搜尋。
fn content_column(backend: &TestBackend, y: u16, content_x: u16, needle: &str) -> u16 {
    let area = backend.buffer().area;
    let mut text = String::new();
    let mut columns: Vec<u16> = Vec::new();
    let mut x = content_x + 1;

    while x < area.x + area.width {
        let symbol = backend.buffer()[(x, y)].symbol();
        for character in symbol.chars() {
            columns.push(x);
            text.push(character);
        }
        let width = u16::try_from(Line::from(symbol).width()).unwrap_or(1);
        x += width.max(1);
    }

    let offset = text.find(needle).expect("子字符串应存在于内容区");
    columns[text[..offset].chars().count()]
}

#[test]
fn field_layout_uses_display_width() {
    // 「加密口令」顯示寬度 8（字元數只有 4），右對齊標籤欄後再加間隔。
    let chinese = field_layout(FORM_INNER_WIDTH, "加密口令");
    assert_eq!(chinese.pad, 6);
    assert_eq!(chinese.prefix, 16);
    assert_eq!(chinese.value, usize::from(FORM_INNER_WIDTH) - 16);

    // 英文標籤的字元數即顯示寬度。
    let english = field_layout(FORM_INNER_WIDTH, "Password");
    assert_eq!(english.pad, 6);
    assert_eq!(english.prefix, 16);

    // 超過標籤欄寬的標籤不再補白，值區域等量縮減。
    let long = field_layout(FORM_INNER_WIDTH, "非常非常非常非常长的标签");
    assert_eq!(long.pad, 0);
    assert_eq!(long.prefix, 26, "显示宽度 24 + 间隔 2");
    assert_eq!(long.value, usize::from(FORM_INNER_WIDTH) - 26);

    // 窄視窗不得溢位。
    assert_eq!(field_layout(0, "加密口令").value, 0);
    assert_eq!(field_layout(10, "加密口令").value, 0);
}

#[test]
fn draws_form_with_cursor_at_value_column() {
    let mut form = FormState::login_retry(crate::session::SiteKind::Attendance);
    form.focus = 0;
    form.fields[0].value.set("3120000001");

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        draw_form(frame, &form, "重新输入账号密码", "hint", None);
    });
    let backend = terminal.backend();

    // 以輸入的值定位該列（標題也含「账号」，不能用標籤找）。
    let (row_y, row) = find_row(backend, "3120000001");
    assert_eq!(
        column_of(&row, "账号"),
        FORM_INNER_X + 10,
        "「账号」显示宽度 4，需补 10 栏"
    );
    assert_eq!(
        column_of(&row, "3120000001"),
        FORM_INNER_X + 16,
        "值应接在间隔后"
    );
    assert_eq!(
        backend.cursor_position().x,
        FORM_INNER_X + 16 + 10,
        "光标应位于已输入文字之后"
    );
    assert_eq!(backend.cursor_position().y, row_y);
}

#[test]
fn masks_sensitive_values_in_form() {
    let mut form = FormState::login_retry(crate::session::SiteKind::Attendance);
    form.focus = 1;
    form.fields[1].value.set("pw-12345");

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        draw_form(frame, &form, "重新输入账号密码", "hint", None);
    });
    let backend = terminal.backend();

    let masked = MASK_CHAR.to_string().repeat(8);
    let (_, row) = find_row(backend, &masked);
    assert!(
        !screen_text(backend).contains("pw-12345"),
        "密码不得以明文显示"
    );
    assert_eq!(column_of(&row, &masked), FORM_INNER_X + 16);
    assert_eq!(backend.cursor_position().x, FORM_INNER_X + 16 + 8);
}

#[test]
fn long_value_scrolls_and_keeps_cursor_inside_field() {
    let mut form = FormState::login_retry(crate::session::SiteKind::Attendance);
    form.focus = 0;
    form.fields[0].value.set("x".repeat(80));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        draw_form(frame, &form, "重新输入账号密码", "hint", None);
    });
    let backend = terminal.backend();

    let (row_y, row) = find_row(backend, "xxxx");
    let value_column = column_of(&row, "x");
    let visible = row.matches('x').count();
    let window = field_layout(FORM_INNER_WIDTH, "账号").value;

    assert_eq!(visible, window - 1, "应滚动到最尾端内容");
    assert_eq!(
        backend.cursor_position().x,
        value_column + u16::try_from(visible).unwrap_or(u16::MAX),
        "光标应落在可见内容尾端"
    );
    assert_eq!(backend.cursor_position().y, row_y);
}

#[test]
fn draws_credentials_form_with_previous_failure_note() {
    let screen = LoginScreen::Credentials {
        site: crate::session::SiteKind::Attendance,
        form: FormState::login_retry(crate::session::SiteKind::Attendance),
        message: "登录失败：用户名或密码错误".to_owned(),
    };

    let terminal = draw(WIDTH, HEIGHT, |frame| draw_login(frame, &screen));
    let text = screen_text(terminal.backend());

    assert!(
        text.contains("上次登录失败：登录失败：用户名或密码错误"),
        "应显示上一次的失败信息：\n{text}"
    );
    for label in ["账号", "密码", "加密口令"] {
        assert!(text.contains(label), "缺少字段 {label}：\n{text}");
    }
}

#[test]
fn draws_captcha_input_and_cursor() {
    let screen = LoginScreen::Captcha {
        path: PathBuf::from("/tmp/captcha.png"),
        input: InputLine::with_value("a1b2"),
        error: Some("验证码错误".to_owned()),
    };

    let terminal = draw(WIDTH, HEIGHT, |frame| draw_login(frame, &screen));
    let backend = terminal.backend();

    let (row_y, row) = find_row(backend, "a1b2");
    assert_eq!(
        column_of(&row, "验证码"),
        LOGIN_INNER_X + 8,
        "「验证码」显示宽度 6，需补 8 栏"
    );
    assert_eq!(column_of(&row, "a1b2"), LOGIN_INNER_X + 16);
    assert_eq!(
        backend.cursor_position().x,
        LOGIN_INNER_X + 16 + 4,
        "光标应位于已输入的验证码之后"
    );
    assert_eq!(backend.cursor_position().y, row_y);
    assert!(
        screen_text(backend).contains("验证码错误"),
        "错误信息仍应显示"
    );
}

#[test]
fn draws_mfa_input_and_placeholder_while_empty() {
    let empty = LoginScreen::Mfa {
        phone: Some("138****8888".to_owned()),
        sent: true,
        input: InputLine::new(),
        error: None,
    };

    let terminal = draw(WIDTH, HEIGHT, |frame| draw_login(frame, &empty));
    let backend = terminal.backend();
    let text = screen_text(backend);
    assert!(text.contains("短信验证码"), "应画出输入框标签：\n{text}");
    assert!(text.contains("138****8888"), "应显示绑定手机号");

    // 空輸入且聚焦時，游標位於值區域起點。
    let (_, row) = find_row(backend, "短信验证码");
    assert_eq!(
        column_of(&row, "短信验证码"),
        LOGIN_INNER_X + 4,
        "「短信验证码」显示宽度 10，需补 4 栏"
    );
    assert_eq!(backend.cursor_position().x, LOGIN_INNER_X + 16);
}

/// 作業頁測試用的輸入。
fn homework_input(title: &str, end_time: &str, submitted: usize) -> HomeworkInput {
    HomeworkInput {
        course_id: "1".to_owned(),
        course_name: "编译原理".to_owned(),
        activity_id: format!("a-{title}"),
        title: title.to_owned(),
        end_time: Some(end_time.to_owned()),
        description: None,
        submit_by_group: Some(false),
        submission_count: Some(submitted),
        note: None,
    }
}

#[test]
fn draws_homework_groups_and_switches_them() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[
            homework_input("待办作业", "2026-10-01 23:59:59", 0),
            homework_input("完成作业", "2026-09-01 23:59:59", 2),
        ],
        now,
    );

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(HomeworkData {
        term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
        term_source: Some("考勤系统"),
        courses_included: 2,
        courses_skipped: 1,
        term_options: Vec::new(),
        items,
        issues: Vec::new(),
        courses_failed: 0,
        progress: Some((1, 2)),
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("2026-2027 学年 第 1 学期"),
        "标题应显示学期：\n{text}"
    );
    assert!(text.contains("未完成 1"), "标题应显示分组计数：\n{text}");
    assert!(text.contains("已完成 1"), "标题应显示分组计数：\n{text}");
    assert!(
        text.contains("加载中 1/2 门课程"),
        "应显示加载进度：\n{text}"
    );
    assert!(
        text.contains("1 门课程缺少学期信息"),
        "应提示未纳入课程：\n{text}"
    );
    assert!(
        text.contains("待办作业"),
        "默认分组应显示未完成作业：\n{text}"
    );
    assert!(
        !text.contains("完成作业"),
        "已完成作业不应出现在未完成分组：\n{text}"
    );

    // 切換到「已完成」分組。
    app.homework_group = HomeworkGroup::Completed;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("完成作业"),
        "已完成分组应显示已完成作业：\n{text}"
    );
    assert!(
        !text.contains("待办作业"),
        "未完成作业不应出现在已完成分组：\n{text}"
    );
}

#[test]
fn draws_term_picker_popup() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.set_screen(Screen::TermPicker(TermPickerState::new(
        vec![
            TermCode::parse("2026-2027-1").expect("学期"),
            TermCode::parse("2025-2026-2").expect("学期"),
        ],
        Some(TermCode::parse("2026-2027-1").expect("学期")),
        "考勤系统不可用，且没有记住的学期".to_owned(),
    )));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("选择学期"), "应显示标题：\n{text}");
    assert!(text.contains("考勤系统不可用"), "应显示原因：\n{text}");
    assert!(
        text.contains("2026-2027 学年 第 1 学期"),
        "应列出学期：\n{text}"
    );
    assert!(text.contains("（建议）"), "应标示建议学期：\n{text}");
    assert!(text.contains("enter 确定"), "应显示操作提示：\n{text}");
}

#[test]
fn draws_homework_unknown_warning_with_reason() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[
            homework_input("待办作业", "2026-10-01 23:59:59", 0),
            HomeworkInput {
                course_id: "1".to_owned(),
                course_name: "编译原理".to_owned(),
                activity_id: "a-unknown".to_owned(),
                title: "待核实作业".to_owned(),
                end_time: Some("2026-10-02 23:59:59".to_owned()),
                description: None,
                submit_by_group: Some(false),
                submission_count: None,
                note: Some("无法确认提交状态：思源学堂用户信息解析失败".to_owned()),
            },
        ],
        now,
    );

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(HomeworkData {
        term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
        term_source: Some("考勤系统"),
        courses_included: 1,
        courses_skipped: 0,
        term_options: Vec::new(),
        items,
        issues: vec![HomeworkIssue {
            reason: "无法确认提交状态：思源学堂用户信息解析失败".to_owned(),
            count: 1,
        }],
        courses_failed: 0,
        progress: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("已确认 1 / 待核实 1"),
        "应显示已确认与待核实数量：\n{text}"
    );
    assert!(
        text.contains("用户信息解析失败"),
        "应显示待核实原因：\n{text}"
    );
    assert!(text.contains("按 r 重试"), "应提示可重试：\n{text}");
}

#[test]
fn footer_shows_site_session_state() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(HomeworkData::default());

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("[未登录 自动]"),
        "未登录时底栏应显示未登录与访问策略：\n{text}"
    );

    // 思源學堂已登入：作業頁底欄只顯示實際訪問方式（不再贅述「已登录」）。
    app.set_site_mode(SiteKind::Lms, AccessMode::Direct);
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("[直连 自动]"),
        "登录后底栏应显示访问方式与策略：\n{text}"
    );
    assert!(
        !text.contains("已登录"),
        "登录状态正常时不应占用底栏版面：\n{text}"
    );
}

#[test]
fn homework_failure_with_stale_data_shows_inline_warning() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(HomeworkData::default());
    app.homework.fail("网络连接失败");

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("更新失败：网络连接失败（按 r 重试）"),
        "保留旧数据时应在页面内显示更新失败：\n{text}"
    );
}

#[test]
fn login_overlay_keeps_main_view_behind_popup() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(HomeworkData::default());
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录考勤服务…".to_owned(),
    }));

    let render = |app: &mut App| draw(WIDTH, HEIGHT, |frame| crate::tui::views::draw(frame, app));

    let first = render(&mut app);
    let text = screen_text(first.backend());
    assert!(text.contains("课表"), "底层侧栏应保持可见：\n{text}");
    assert!(text.contains("未登录"), "底层底栏应保持可见：\n{text}");
    assert!(
        text.contains("正在登录考勤服务…"),
        "弹窗内容应显示：\n{text}"
    );

    // 事件只更新彈窗內容：換成驗證碼畫面後，底層仍完整可見（不出現黑屏）。
    app.login = Some(Box::new(LoginScreen::Captcha {
        path: PathBuf::from("/tmp/captcha.png"),
        input: InputLine::with_value("a1b2"),
        error: None,
    }));
    let second = render(&mut app);
    let text = screen_text(second.backend());
    assert!(text.contains("课表"), "换画面后底层仍应可见：\n{text}");
    assert!(text.contains("a1b2"), "新弹窗内容应显示：\n{text}");
}

/// 思源學堂測試用活動。
fn lms_activity(id: &str, kind: &str, end: Option<&str>) -> LmsActivity {
    LmsActivity {
        id: id.to_owned(),
        course_id: None,
        kind: kind.to_owned(),
        title: Some(format!("活动 {id}")),
        start_time: None,
        end_time: end.map(str::to_owned),
        submit_by_group: None,
        group_id: None,
        data: None,
        top_level_description: None,
        uploads: Vec::new(),
        user_submit_count: None,
        published: None,
    }
}

#[test]
fn draws_activity_groups_with_counts_and_empty_state() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities = Page::Ready(vec![
        lms_activity("1", "homework", Some("2026-10-01 23:59:59")),
        lms_activity("2", "material", None),
    ]);
    app.lms.activity_group = crate::domain::activity::ActivityGroup::Homework;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("直播 0"), "应显示各组计数：\n{text}");
    assert!(text.contains("作业 1"), "应显示作业组计数：\n{text}");
    assert!(text.contains("资料 1"), "应显示数据组计数：\n{text}");
    assert!(text.contains("活动 1"), "应只显示目前分组的项目：\n{text}");

    // 空組顯示提示，且不顯示其他組的項目。
    app.lms.activity_group = crate::domain::activity::ActivityGroup::LectureLive;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("本组暂无活动"), "空组应显示提示：\n{text}");
    assert!(!text.contains("活动 1"), "空组不应显示其他组：\n{text}");
}

#[test]
fn activity_detail_hides_submission_section_for_non_homework() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "直播课".to_owned(),
        kind: ActivityKind::LectureLive,
        description: None,
        end_time: Some("2026-10-01 12:00:00".to_owned()),
        submit_by_group: Some(false),
        submissions: None,
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("类型：直播"), "应显示活动类型：\n{text}");
    assert!(!text.contains("待核实"), "非作业不应显示待核实：\n{text}");
    assert!(
        !text.contains("提交记录"),
        "非作业不应显示提交记录区：\n{text}"
    );

    // 作業仍顯示提交狀態（未知時為待核实）。
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "2".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        description: None,
        end_time: None,
        submit_by_group: Some(false),
        submissions: None,
        note: None,
    });
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("提交记录：无法确认（待核实）"),
        "作业未知提交状态应显示待核实：\n{text}"
    );
}

#[test]
fn activity_detail_converts_submission_times_to_school_time() {
    // 脫敏樣本：UTC（`Z`）與未帶時區的提交時間混合。
    let list: LmsSubmissionList = serde_json::from_str(
        r#"{"list":[
            {"id":1,"submitted_at":"2026-09-20T02:00:00Z","is_latest_version":true},
            {"id":2,"submitted_at":"2026-09-21 09:30:00","is_latest_version":false}
        ]}"#,
    )
    .expect("脱敏样本");

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        description: None,
        end_time: Some("2026-09-25T15:59:59.000Z".to_owned()),
        submit_by_group: Some(false),
        submissions: Some(list.list),
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("2026-09-20 10:00"),
        "UTC 提交时间应换算为 +08:00：\n{text}"
    );
    assert!(
        text.contains("2026-09-21 09:30"),
        "未带时区的提交时间维持原样：\n{text}"
    );
    assert!(
        !text.contains("T02:00:00"),
        "不应显示原始 ISO 字符串：\n{text}"
    );
    assert!(
        text.contains("截止：2026-09-25 23:59"),
        "详情截止时间也应换算：\n{text}"
    );
}

fn flow_record(id: &str, place: Option<&str>, time: Option<&str>, effective: bool) -> FlowRecord {
    FlowRecord {
        id: id.to_owned(),
        classroom_name: place.map(str::to_owned),
        collect_time: time.map(str::to_owned),
        effective,
    }
}

#[test]
fn flow_rows_align_status_column_and_show_semantic_colors() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(FlowData {
        records: vec![
            flow_record("1", Some("主楼A101"), Some("2026-09-29 08:00:00"), true),
            flow_record(
                "2",
                Some("西迁博物馆报告厅"),
                Some("2026-09-29 09:00:00"),
                false,
            ),
            flow_record("3", None, Some("2026-09-29 10:00:00"), true),
        ],
        page: 1,
        total_pages: 1,
        total: 3,
    });
    // 選取第三列（空地點），避免高亮樣式覆蓋前兩列的狀態色。
    app.flow_state.select(Some(2));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();

    let (short_y, short_row) = find_row(backend, "主楼A101");
    let (long_y, long_row) = find_row(backend, "西迁博物馆报告厅");
    let (_, empty_row) = find_row(backend, "（无地点）");

    let short_col = column_of(&short_row, "有效");
    let long_col = column_of(&long_row, "未匹配");
    let empty_col = column_of(&empty_row, "有效");
    assert_eq!(
        short_col, long_col,
        "地点长度不得影响状态列起点：\n{short_row}\n{long_row}"
    );
    assert_eq!(short_col, empty_col, "空地点不得影响状态列起点");

    assert_eq!(
        backend.buffer()[(short_col, short_y)].fg,
        THEME.green,
        "有效应为成功绿"
    );
    assert_eq!(
        backend.buffer()[(long_col, long_y)].fg,
        THEME.yellow,
        "未匹配应为警告黄"
    );
}

#[test]
fn flow_rows_degrade_on_narrow_terminal() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(FlowData {
        records: vec![flow_record(
            "1",
            Some("西迁博物馆报告厅"),
            Some("2026-09-29 08:00:00"),
            true,
        )],
        page: 1,
        total_pages: 1,
        total: 1,
    });

    let terminal = draw(64, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains('…'), "窄画面应以省略号截断地点：\n{text}");
    assert!(text.contains("有效"), "状态仍应可见：\n{text}");
}

fn homework_data(items: Vec<HomeworkItem>, progress: Option<(usize, usize)>) -> HomeworkData {
    HomeworkData {
        term_label: Some("2026-2027 学年 第 1 学期".to_owned()),
        term_source: Some("考勤系统"),
        courses_included: 2,
        courses_skipped: 0,
        term_options: Vec::new(),
        items,
        issues: Vec::new(),
        courses_failed: 0,
        progress,
    }
}

#[test]
fn homework_empty_states_distinguish_loading_and_terminal() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;

    // 載入中（0 項）：應顯示進度說明，不得顯示為失敗或空結果。
    app.homework = Page::Loading {
        note: "正在汇总作业（已完成 0/2 门课程，累计 0 项）…".to_owned(),
        stale: Some(homework_data(Vec::new(), Some((0, 2)))),
    };
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("已完成 0/2 门课程"),
        "加载中应显示进度：\n{text}"
    );
    assert!(!text.contains("加载失败"), "加载中不得显示失败：\n{text}");
    assert!(
        !text.contains("没有未完成的作业"),
        "加载中不得显示空结果：\n{text}"
    );

    // 終態空：明確顯示「本学期暂无作业」，而不是失敗。
    app.homework = Page::Ready(homework_data(Vec::new(), None));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("本学期暂无作业"),
        "终态空应显示明确空状态：\n{text}"
    );
    assert!(!text.contains("加载失败"), "空结果不得显示为失败：\n{text}");
}

/// 「載入成功但沒有資料」的四個頁面都應顯示中性說明，不得誤標為載入失敗。
#[test]
fn no_data_states_are_plain_notes_not_failures() {
    // 課表：本週沒有課程。
    let mut app = main_screen_app();
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(Vec::new()));
    let text = main_text(&mut app);
    assert!(
        text.contains("该周没有课程安排"),
        "课表空结果应显示说明：\n{text}"
    );
    assert!(!text.contains("加载失败"), "空结果不得显示为失败：\n{text}");

    // 考勤流水：本頁沒有記錄。
    let mut app = main_screen_app();
    app.nav = NavItem::Attendance;
    app.attendance = Page::Ready(FlowData {
        records: Vec::new(),
        page: 1,
        total_pages: 1,
        total: 0,
    });
    let text = main_text(&mut app);
    assert!(
        text.contains("本页没有流水记录"),
        "流水空结果应显示说明：\n{text}"
    );
    assert!(!text.contains("加载失败"), "空结果不得显示为失败：\n{text}");

    // 思源學堂：沒有課程。
    let mut app = main_screen_app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    app.lms.courses = Page::Ready(Vec::new());
    let text = main_text(&mut app);
    assert!(text.contains("没有课程"), "课程空结果应显示说明：\n{text}");
    assert!(!text.contains("加载失败"), "空结果不得显示为失败：\n{text}");

    // 思源學堂：該課程沒有活動。
    let mut app = main_screen_app();
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activities = Page::Ready(Vec::new());
    let text = main_text(&mut app);
    assert!(
        text.contains("该课程没有活动"),
        "活动空结果应显示说明：\n{text}"
    );
    assert!(!text.contains("加载失败"), "空结果不得显示为失败：\n{text}");
}

/// 課表：有無法解析的課程時顯示提示，數字為零時不顯示。
#[test]
fn schedule_shows_skipped_course_warning() {
    let mut app = main_screen_app();
    app.nav = NavItem::Schedule;
    let mut data = schedule_data(vec![lesson_entry(
        "高等数学",
        "主楼A101",
        "张老师",
        AttendanceStatus::Normal,
    )]);
    data.skipped = 2;
    app.schedule = Page::Ready(data);
    let text = main_text(&mut app);
    assert!(
        text.contains("已跳过 2 门无法解析的课程"),
        "应显示跳过提示：\n{text}"
    );

    let mut app = main_screen_app();
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(vec![lesson_entry(
        "高等数学",
        "主楼A101",
        "张老师",
        AttendanceStatus::Normal,
    )]));
    let text = main_text(&mut app);
    assert!(!text.contains("已跳过"), "没有跳过时不应显示提示：\n{text}");
}

#[test]
fn homework_shows_failed_course_count() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    let mut data = homework_data(Vec::new(), None);
    data.courses_failed = 1;
    app.homework = Page::Ready(data);

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("1 门课程查询失败"),
        "应显示略过的课程数：\n{text}"
    );
    assert!(text.contains("按 r 重试"), "应提示可重试：\n{text}");
}

fn lms_course(id: &str, code: Option<&str>) -> LmsCourse {
    LmsCourse {
        id: id.to_owned(),
        name: format!("课程{id}"),
        course_code: None,
        instructors: Vec::new(),
        semester: code.map(|code| crate::sites::lms::models::LmsSemester {
            id: None,
            code: Some(code.to_owned()),
            name: None,
            real_name: None,
        }),
        academic_year: None,
    }
}

#[test]
fn lms_courses_partition_current_term_first() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    app.lms.courses = Page::Ready(vec![
        lms_course("1", Some("2025-2")), // 歷史
        lms_course("2", Some("2026-1")), // 當前學期
        lms_course("3", None),           // 學期未知
    ]);
    app.lms.courses_term = Some(TermCode::parse("2026-2027-1").expect("学期"));
    // 選取未知課程，避免高亮樣式覆蓋其他列的顏色斷言。
    app.course_state.select(Some(2));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let text = screen_text(backend);
    assert!(
        text.contains("当前学期 · 2026-2027 学年 第 1 学期"),
        "应显示当前学期分区：\n{text}"
    );
    assert!(text.contains("历史课程"), "应显示历史分区：\n{text}");
    assert!(text.contains("学期未知"), "应显示未知分区：\n{text}");

    let (current_y, _) = find_row(backend, "课程2");
    let (history_y, history_row) = find_row(backend, "课程1");
    let (unknown_y, _) = find_row(backend, "课程3");
    assert!(
        current_y < history_y && history_y < unknown_y,
        "顺序应为当前学期 → 历史 → 未知：\n{text}"
    );

    // 「历史课程」標題與上方課程之間應空出一列。
    let (header_y, _) = find_row(backend, "历史课程");
    assert_eq!(
        header_y,
        current_y + 2,
        "历史分区标题前应留一列空白：\n{text}"
    );

    // 歷史課程以較淺灰色呈現；當前學期課程維持一般文字色。
    let history_col = column_of(&history_row, "课程1");
    assert_eq!(
        backend.buffer()[(history_col, history_y)].fg,
        THEME.muted,
        "历史课程应为浅灰：\n{text}"
    );
    let (_, current_row) = find_row(backend, "课程2");
    let current_col = column_of(&current_row, "课程2");
    assert_eq!(
        backend.buffer()[(current_col, current_y)].fg,
        THEME.text,
        "当前学期课程应维持一般文字色：\n{text}"
    );
}

/// 斷言彈窗矩形：左右邊框逐列、下緣逐欄連續；上緣可能被標題（含空白填充）佔用，
/// 只驗證顏色；整個彈窗必須為不透明 surface 底色。
fn assert_popup_borders(backend: &TestBackend, popup: Rect) {
    let buffer = backend.buffer();
    for y in popup.y + 1..popup.y + popup.height - 1 {
        for x in [popup.x, popup.x + popup.width - 1] {
            let cell = &buffer[(x, y)];
            assert_ne!(cell.symbol(), " ", "边框不得被打断：(x={x}, y={y})");
            assert_eq!(cell.fg, THEME.accent, "边框颜色：(x={x}, y={y})");
        }
    }
    for x in popup.x + 1..popup.x + popup.width - 1 {
        let cell = &buffer[(x, popup.y + popup.height - 1)];
        assert_ne!(cell.symbol(), " ", "下缘不得被打断：(x={x})");
        assert_eq!(cell.fg, THEME.accent, "下缘颜色：(x={x})");
    }
    for x in popup.x + 1..popup.x + popup.width - 1 {
        let cell = &buffer[(x, popup.y)];
        // 上緣由粉色邊框與藍色標題組成；寬字元的尾隨格會被重置為空白。
        let symbol = cell.symbol();
        assert!(
            symbol == " " || cell.fg == THEME.accent || cell.fg == THEME.blue,
            "上缘不得残留底层文字：(x={x}) symbol={symbol:?}"
        );
    }
    for y in popup.y..popup.y + popup.height {
        for x in popup.x..popup.x + popup.width {
            let cell = &buffer[(x, y)];
            // 不透明＝沒有任何底層底色露出：底色只能是彈窗 surface、選取列 accent，
            // 或寬字元（標題）被 ratatui 重置且不獨立顯示的尾隨格。
            let continuation = cell.symbol() == " " && cell.fg == Color::Reset;
            assert!(
                continuation || cell.bg == THEME.surface || cell.bg == THEME.accent,
                "弹窗必须不透明：(x={x}, y={y}) symbol={:?} bg={:?}",
                cell.symbol(),
                cell.bg
            );
        }
    }
}

#[test]
fn popup_surface_is_opaque_over_text_background() {
    let popup = centered_rect(Rect::new(0, 0, WIDTH, HEIGHT), 60, 12);
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        // 滿版文字背景：彈窗覆蓋後不得有任何字元穿透。
        let full = frame.area();
        let lines: Vec<Line> = (0..full.height)
            .map(|_| Line::from("思源学堂课程活动作业考勤流水课表甲乙丙丁戊己庚辛壬癸"))
            .collect();
        frame.render_widget(Paragraph::new(lines).style(THEME.base_style()), full);
        popup_surface(frame, popup, "测试弹窗");
    });

    assert_popup_borders(terminal.backend(), popup);

    // 彈窗內部不得殘留底層文字。
    let buffer = terminal.backend().buffer();
    for y in popup.y + 1..popup.y + popup.height - 1 {
        for x in popup.x + 1..popup.x + popup.width - 1 {
            assert_eq!(
                buffer[(x, y)].symbol(),
                " ",
                "弹窗内部不得有底层文字：(x={x}, y={y})"
            );
        }
    }
}

#[test]
fn settings_popup_stays_continuous_over_dense_content() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Courses;
    app.lms.courses = Page::Ready(
        (1..=12)
            .map(|index| lms_course(&index.to_string(), Some("2026-1")))
            .collect(),
    );
    app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    assert_popup_borders(
        terminal.backend(),
        centered_rect(Rect::new(0, 0, WIDTH, HEIGHT), 60, 12),
    );
}

#[test]
fn popup_redraw_is_stable_across_frames() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));

    let snapshot = |app: &mut App| {
        let terminal = draw(WIDTH, HEIGHT, |frame| crate::tui::views::draw(frame, app));
        let popup = centered_rect(Rect::new(0, 0, WIDTH, HEIGHT), 60, 12);
        let buffer = terminal.backend().buffer();
        let mut cells = Vec::new();
        for y in popup.y..popup.y + popup.height {
            for x in popup.x..popup.x + popup.width {
                let cell = &buffer[(x, y)];
                cells.push((cell.symbol().to_owned(), cell.fg, cell.bg));
            }
        }
        cells
    };

    let first = snapshot(&mut app);
    let second = snapshot(&mut app);
    assert_eq!(first, second, "连续两帧的弹窗内容必须一致");
}

#[test]
fn popup_fits_minimal_terminal_size() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));

    let terminal = draw(MIN_WIDTH, MIN_HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    assert_popup_borders(
        terminal.backend(),
        centered_rect(Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT), 60, 12),
    );
}

#[test]
fn term_picker_and_login_overlays_keep_continuous_borders() {
    // 學期選擇器。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.set_screen(Screen::TermPicker(TermPickerState::new(
        vec![TermCode::parse("2026-2027-1").expect("学期")],
        None,
        "考勤不可用".to_owned(),
    )));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    assert_popup_borders(
        terminal.backend(),
        centered_rect(Rect::new(0, 0, WIDTH, HEIGHT), 62, 16),
    );

    // 登入覆蓋層（進度畫面：1 行內容 + 6 行固定高度）。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    }));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    assert_popup_borders(
        terminal.backend(),
        centered_rect(Rect::new(0, 0, WIDTH, HEIGHT), 84, 7),
    );
}

/// 找出側邊欄中含有指定文字的列，回傳側邊欄內文（避開同列的內容面板文字）。
fn find_sidebar_row(backend: &TestBackend, label: &str) -> String {
    let area = backend.buffer().area;
    for y in area.y..area.y + area.height {
        let row = row_text(backend, y);
        // 列格式：`│側邊欄內文││內容內文│`，第 2 段（索引 1）即側邊欄內文。
        if let Some(sidebar) = row.split('│').nth(1)
            && sidebar.contains(label)
        {
            return sidebar.to_owned();
        }
    }
    panic!("侧边栏找不到 {label}：\n{backend}");
}

#[test]
fn sidebar_left_aligns_labels_and_blinks_dots_before_text() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.schedule.start_loading("正在加载课表与考勤记录…");

    // 相位 3：三點顯示在文字正前方；不再出現被截斷的載入說明。
    app.tick = 3;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let schedule = find_sidebar_row(backend, "课表");
    let homework = find_sidebar_row(backend, "任务");
    let attendance = find_sidebar_row(backend, "考勤流水");
    let lms = find_sidebar_row(backend, "思源学堂");

    assert!(
        schedule.contains("..."),
        "加载中应显示三点动画：{schedule:?}"
    );
    assert!(
        !schedule.contains("正在"),
        "侧边栏不得显示加载说明：{schedule:?}"
    );

    // 標籤左對齊：所有項目共用同一個文字起始欄。
    let label_col = column_of(&schedule, "课表");
    assert_eq!(column_of(&homework, "任务"), label_col, "标签应左对齐");
    assert_eq!(column_of(&attendance, "考勤流水"), label_col);
    assert_eq!(column_of(&lms, "思源学堂"), label_col);

    // 文字欄置中：以最寬標籤（考勤流水，寬 8）計算左右留白相等。
    let left_margin = usize::from(label_col);
    let right_margin = 20 - (left_margin + 8);
    assert_eq!(left_margin, right_margin, "文字栏应置中：{schedule:?}");

    // 三點緊貼文字正前方（不參與置中）。
    let dots_col = column_of(&schedule, "...");
    assert_eq!(
        usize::from(dots_col) + 4,
        usize::from(label_col),
        "三点应显示在文字正前方：{schedule:?}"
    );

    // 相位 0：三點為空白，標籤欄位不得改變（動畫不造成跳動）。
    app.tick = 0;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let sidebar_blank = find_sidebar_row(terminal.backend(), "课表");
    assert!(
        !sidebar_blank.contains('.'),
        "相位 0 不应显示点：{sidebar_blank:?}"
    );
    assert_eq!(
        column_of(&sidebar_blank, "课表"),
        label_col,
        "动画不得让标签位移"
    );

    // 未載入的頁面永遠沒有指示燈。
    assert!(
        !homework.contains('.'),
        "未加载页面不应显示点：{homework:?}"
    );
}

/// 課表測試用課程。
fn lesson_entry(
    course: &str,
    classroom: &str,
    teacher: &str,
    attendance: impl Into<LessonAttendance>,
) -> LessonEntry {
    LessonEntry {
        date: chrono::NaiveDate::from_ymd_opt(2026, 9, 29).expect("日期"),
        sections: "1-2".to_owned(),
        start_section: 1,
        end_section: 2,
        course_name: course.to_owned(),
        classroom: classroom.to_owned(),
        teacher: teacher.to_owned(),
        weeks: "1-16".to_owned(),
        attendance: attendance.into(),
    }
}

/// 課表頁資料。
fn schedule_data(lessons: Vec<LessonEntry>) -> ScheduleData {
    ScheduleData {
        semester: "2026-2027-1".to_owned(),
        week: 4,
        total_weeks: 23,
        lessons,
        skipped: 0,
        notice: None,
    }
}

#[test]
fn schedule_rows_align_attendance_column() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(vec![
        lesson_entry("高等数学", "主楼A101", "张老师", AttendanceStatus::Normal),
        lesson_entry(
            "思想道德与法治",
            "逸夫科学馆",
            "欧阳老师",
            AttendanceStatus::Absent,
        ),
        lesson_entry("大学物理", "中2-2201", "李老师", AttendanceStatus::Leave),
    ]));
    // 選取第三列，避免高亮樣式蓋掉前兩列的狀態色。
    app.schedule_state.select(Some(2));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (normal_y, normal_row) = find_row(backend, "高等数学");
    let (absent_y, absent_row) = find_row(backend, "思想道德与法治");

    let normal_col = column_of(&normal_row, "正常");
    let absent_col = column_of(&absent_row, "缺勤");
    assert_eq!(
        normal_col, absent_col,
        "考勤状态栏起点不得受课程、地点与教师长度影响：\n{normal_row}\n{absent_row}"
    );
    assert_eq!(
        column_of(&normal_row, "主楼A101"),
        column_of(&absent_row, "逸夫科学馆"),
        "地点栏起点必须一致：\n{normal_row}\n{absent_row}"
    );
    assert_eq!(
        backend.buffer()[(normal_col, normal_y)].fg,
        THEME.green,
        "正常应为成功绿"
    );
    assert_eq!(
        backend.buffer()[(absent_col, absent_y)].fg,
        THEME.red,
        "缺勤应为错误红"
    );
}

#[test]
fn schedule_rows_hide_teacher_column_when_narrow() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(vec![lesson_entry(
        "高等数学",
        "主楼A101",
        "张老师",
        AttendanceStatus::Normal,
    )]));

    let terminal = draw(70, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("高等数学"), "课程仍应可见：\n{text}");
    assert!(text.contains("主楼A101"), "地点仍应可见：\n{text}");
    assert!(!text.contains("张老师"), "宽度不足时收起教师栏：\n{text}");
    assert!(text.contains("正常"), "考勤状态仍应可见：\n{text}");
}

#[test]
fn homework_rows_trade_group_and_title_detail_when_narrow() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            course_id: "2".to_owned(),
            course_name: "马克思主义基本原理概论".to_owned(),
            activity_id: "a-2".to_owned(),
            title: "社会实践报告与社会调查作业".to_owned(),
            end_time: Some("2026-10-08T15:59:59.000Z".to_owned()),
            description: None,
            submit_by_group: Some(true),
            submission_count: Some(0),
            note: None,
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));

    // 100 欄：完整欄位（含「截止」前綴與「小组」欄）。
    let wide = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let (_, wide_row) = find_row(wide.backend(), "马克思");
    assert!(
        wide_row.contains("截止 2026-10-08 23:59"),
        "宽画面应显示完整截止时间：\n{wide_row}"
    );
    assert!(
        wide_row.contains("小组"),
        "宽画面应显示提交单位：\n{wide_row}"
    );

    // 70 欄：收起「小组」欄與「截止」前綴（日期仍完整），標題以省略號截斷。
    let narrow = draw(70, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let (_, narrow_row) = find_row(narrow.backend(), "马克思");
    assert!(
        !narrow_row.contains("小组"),
        "窄画面应收起提交单位栏：\n{narrow_row}"
    );
    assert!(
        !narrow_row.contains("截止"),
        "窄画面应收起「截止」前缀：\n{narrow_row}"
    );
    assert!(
        narrow_row.contains("2026-10-08 23:59"),
        "窄画面仍应保留完整日期：\n{narrow_row}"
    );
    assert!(
        narrow_row.contains('…'),
        "过长的文字应以省略号截断：\n{narrow_row}"
    );

    // 64 欄：再壓縮日期（省略年份）。
    let compact = draw(64, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let (_, compact_row) = find_row(compact.backend(), "马克思");
    assert!(
        compact_row.contains("10-08 23:59"),
        "极窄画面应压缩为不含年份的日期：\n{compact_row}"
    );
    assert!(
        !compact_row.contains("2026-"),
        "压缩后不应再显示年份：\n{compact_row}"
    );
}

#[test]
fn schedule_rows_show_full_names_when_terminal_is_wide() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(vec![
        lesson_entry(
            "毛泽东思想和中国特色社会主义理论体系概论",
            "主楼B-204",
            "赵金瑞",
            LessonAttendance::Pending,
        ),
        lesson_entry(
            "体育-3",
            "塑胶田径场-田径场",
            "胡良楠",
            LessonAttendance::Unknown,
        ),
    ]));
    // 選取第二列，避免高亮樣式影響第一列的擷取。
    app.schedule_state.select(Some(1));

    let terminal = draw(160, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("毛泽东思想和中国特色社会主义理论体系概论"),
        "足够宽时课程名称应完整显示：\n{text}"
    );
    assert!(
        text.contains("塑胶田径场-田径场"),
        "足够宽时地点应完整显示：\n{text}"
    );
    assert!(text.contains("赵金瑞"), "教师栏应完整显示：\n{text}");
}

#[test]
fn homework_rows_show_full_names_when_terminal_is_wide() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[
            HomeworkInput {
                course_id: "1".to_owned(),
                course_name: "微电子电路基础".to_owned(),
                activity_id: "a-1".to_owned(),
                title: "第五章作业（含附件）".to_owned(),
                end_time: Some("2026-10-12T15:59:59.000Z".to_owned()),
                description: None,
                submit_by_group: Some(false),
                submission_count: Some(0),
                note: None,
            },
            homework_input("第一章作业", "2026-10-20 23:59:59", 0),
        ],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    // 選取第二列，避免高亮樣式影響第一列的擷取。
    app.homework_state.select(Some(1));

    let terminal = draw(160, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("微电子电路基础"),
        "足够宽时课程名称应完整显示：\n{text}"
    );
    assert!(
        text.contains("第五章作业（含附件）"),
        "足够宽时标题应完整显示：\n{text}"
    );
    assert!(
        text.contains("截止 2026-10-12 23:59"),
        "UTC 截止时间应换算为 +08:00 显示：\n{text}"
    );
}

#[test]
fn lists_ask_to_enlarge_terminal_when_too_narrow() {
    // 課表頁：64 欄（全域允許的最小寬度）時課程欄會縮到三個字以下，
    // 依規則改為提示放大窗口，而不是擠出殘缺的列表。
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(vec![lesson_entry(
        "高等数学",
        "主楼A101",
        "张老师",
        AttendanceStatus::Normal,
    )]));

    let terminal = draw(64, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("终端过窄"), "过窄时应提示放大窗口：\n{text}");
    assert!(
        !text.contains("主楼A101"),
        "过窄时不应挤出残缺的列表：\n{text}"
    );

    // 放寬兩欄即可正常顯示（不再提示）。
    let terminal = draw(66, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(!text.contains("终端过窄"), "足够宽时不应提示：\n{text}");
    assert!(text.contains("高等数"), "足够宽时应显示课表：\n{text}");
}

#[test]
fn activity_rows_align_title_and_deadline_columns() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Activities;
    app.lms.activity_group = crate::domain::activity::ActivityGroup::Homework;
    app.lms.activities = Page::Ready(vec![
        LmsActivity {
            id: "1".to_owned(),
            kind: "homework".to_owned(),
            title: Some("作业一".to_owned()),
            end_time: Some("2026-10-08T15:59:59.000Z".to_owned()),
            submit_by_group: Some(false),
            ..lms_activity("1", "homework", None)
        },
        LmsActivity {
            id: "2".to_owned(),
            kind: "homework".to_owned(),
            title: Some("很长很长的作业标题示例".to_owned()),
            end_time: Some("2026-09-20 12:00:00".to_owned()),
            submit_by_group: Some(true),
            ..lms_activity("2", "homework", None)
        },
    ]);

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (_, first) = find_row(backend, "作业一");
    let (_, second) = find_row(backend, "很长很长");
    assert_eq!(
        column_of(&first, "作业一"),
        column_of(&second, "很长很长"),
        "标题栏起点必须一致（类型栏以显示宽度排版）：\n{first}\n{second}"
    );
    assert_eq!(
        column_of(&first, "2026-10-08"),
        column_of(&second, "2026-09-20"),
        "截止栏起点必须一致：\n{first}\n{second}"
    );
    assert!(
        first.contains("2026-10-08 23:59"),
        "UTC 截止时间应换算为 +08:00：\n{first}"
    );
}

/// 造一個主畫面的 App（尺寸不足時 `too_small` 會在內容之前接手）。
fn main_screen_app() -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app
}

/// 以標準尺寸繪製主畫面並回傳畫面文字。
fn main_text(app: &mut App) -> String {
    let terminal = draw(WIDTH, HEIGHT, |frame| crate::tui::views::draw(frame, app));
    screen_text(terminal.backend())
}

#[test]
fn too_small_message_is_centered_and_colors_current_size() {
    // 48x18：寬度未達 64、高度剛好 18。提示應上下左右置中，
    // 且只有未達標的數字轉紅。
    let mut app = main_screen_app();

    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let backend = terminal.backend();
    let text = screen_text(backend);
    assert!(text.contains("终端太小了:"), "应显示新版提示：\n{text}");
    assert!(!text.contains("窗口过小"), "旧版文案不应再出现：\n{text}");
    assert!(!text.contains("至少需要"), "不再显示最低需求：\n{text}");

    let (title_y, title_row) = find_row(backend, "终端太小了");
    assert_eq!(title_y, 8, "第一行应垂直置中（(18-2)/2）：\n{text}");
    assert_eq!(
        column_of(&title_row, "终端太小了"),
        19,
        "第一行应水平置中（48/2 − 11/2）：\n{title_row}"
    );

    let (size_y, size_row) = find_row(backend, "宽 = ");
    assert_eq!(size_y, 9, "第二行应紧接在下一列：\n{text}");
    let line_start = column_of(&size_row, "宽 = ");
    assert_eq!(
        line_start, 16,
        "第二行应水平置中（48/2 − 16/2）：\n{size_row}"
    );

    let width_col = column_of(&size_row, "48");
    let height_col = column_of(&size_row, "高 = ") + 5;
    assert_eq!(
        backend.buffer()[(line_start, size_y)].fg,
        THEME.text,
        "标题文字应为白色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(width_col, size_y)].fg,
        THEME.red,
        "宽度 48 未达 64 应为红色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(height_col, size_y)].fg,
        THEME.green,
        "高度 18 已达门槛应为绿色：\n{size_row}"
    );
}

#[test]
fn too_small_message_flags_only_the_failing_dimension() {
    // 100x12：夠寬、太矮。
    let mut app = main_screen_app();
    let terminal = draw(100, 12, |frame| crate::tui::views::draw(frame, &mut app));
    let backend = terminal.backend();
    let (size_y, size_row) = find_row(backend, "宽 = 100");
    // 提示帶自 (12-2)/2 = 5 起算，第二行落在帶內第二列。
    assert_eq!(size_y, 6, "第二行应在提示带的第二列：\n{size_row}");
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "宽 = ") + 5, size_y)].fg,
        THEME.green,
        "宽度 100 已达门槛应为绿色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "高 = ") + 5, size_y)].fg,
        THEME.red,
        "高度 12 未达 18 应为红色：\n{size_row}"
    );

    // 48x24：太窄、夠高。
    let terminal = draw(48, 24, |frame| crate::tui::views::draw(frame, &mut app));
    let backend = terminal.backend();
    let (size_y, size_row) = find_row(backend, "宽 = 48");
    // 提示帶自 (24-2)/2 = 11 起算，第二行落在帶內第二列。
    assert_eq!(size_y, 12, "第二行应在提示带的第二列：\n{size_row}");
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "宽 = ") + 5, size_y)].fg,
        THEME.red,
        "宽度 48 未达 64 应为红色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "高 = ") + 5, size_y)].fg,
        THEME.green,
        "高度 24 已达门槛应为绿色：\n{size_row}"
    );
}

#[test]
fn too_small_message_survives_single_row_terminal() {
    let mut app = main_screen_app();
    let terminal = draw(40, 1, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("终端太小了"),
        "只剩一列时仍应显示标题：\n{text}"
    );
    assert!(
        !text.contains("高 = "),
        "只放得下一列时不显示第二行：\n{text}"
    );
}

#[test]
fn too_small_message_hidden_at_minimum_size() {
    let mut app = main_screen_app();
    let terminal = draw(MIN_WIDTH, MIN_HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        !text.contains("终端太小了"),
        "达到 64x18 门槛时不应显示提示：\n{text}"
    );
}

#[test]
fn too_small_message_shown_on_startup_forms() {
    // 啟動時的解鎖／首次設定／設定表單與登入覆蓋層共用同一道尺寸守衛：
    // 過小的終端一律顯示放大提示，而不是把表單裁切到無法操作。
    let mut app = App::new(AccessPolicy::Auto); // 預設畫面＝解鎖表單
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("终端太小了"), "解锁画面应显示提示：\n{text}");
    assert!(!text.contains("解锁凭证"), "过小时不应挤出表单：\n{text}");

    app.set_screen(Screen::Setup(FormState::setup()));
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("终端太小了"),
        "首次设置画面应显示提示：\n{text}"
    );
    assert!(!text.contains("首次使用"), "过小时不应挤出表单：\n{text}");

    app.set_screen(Screen::SettingsForm(FormState::change_passphrase()));
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("终端太小了"), "设置表单应显示提示：\n{text}");
    assert!(
        !text.contains("修改加密口令"),
        "过小时不应挤出表单：\n{text}"
    );

    // 登入互動覆蓋層也不能蓋掉提示。
    app.set_screen(Screen::Main);
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    }));
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("终端太小了"),
        "登录弹窗开启时仍应显示提示：\n{text}"
    );
    assert!(
        !text.contains("正在登录"),
        "过小时不应画出登录弹窗：\n{text}"
    );
}

#[test]
fn startup_form_visible_at_minimum_size() {
    // 剛好 64x18：解鎖表單照常顯示（守衛不啟用）。
    let mut app = App::new(AccessPolicy::Auto);
    let terminal = draw(MIN_WIDTH, MIN_HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        !text.contains("终端太小了"),
        "达到门槛时不显示提示：\n{text}"
    );
    assert!(text.contains("解锁凭证"), "应显示解锁表单标题：\n{text}");
    assert!(text.contains("加密口令"), "应显示字段标签：\n{text}");
}

#[test]
fn login_overlay_visible_when_terminal_is_large_enough() {
    // 放大後登入彈窗照常顯示（守衛只在過小時接手）。
    let mut app = main_screen_app();
    app.login = Some(Box::new(LoginScreen::Progress {
        note: "正在登录…".to_owned(),
    }));

    let terminal = draw(100, 30, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(!text.contains("终端太小了"), "足够大时不显示提示：\n{text}");
    assert!(
        text.contains("正在登录"),
        "足够大时应显示登录弹窗：\n{text}"
    );
}

#[test]
fn agreement_overlay_covers_screen_and_shows_document() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Setup(FormState::setup()));
    app.agreement = Some(Box::new(AgreementState::new()));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    let title = format!("用户协议（ohmyXJTU）v{}", crate::privacy::VERSION);

    assert!(text.contains(&title), "标题应含版本：\n{text}");
    assert!(text.contains("欢迎您使用"), "应显示协议开头：\n{text}");
    assert!(
        text.contains("请阅读至最底部"),
        "页脚应提示阅读进度：\n{text}"
    );
    assert!(
        !text.contains("首次使用：设置加密口令与账号"),
        "底层表单不得露出：\n{text}"
    );

    assert_popup_borders(terminal.backend(), Rect::new(0, 0, WIDTH, HEIGHT));
}

#[test]
fn agreement_footer_activates_after_reaching_bottom() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.agreement = Some(Box::new(AgreementState::new()));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("请阅读至最底部"),
        "初始应提示滚到底部：\n{text}"
    );
    assert!(
        !text.contains("同意并继续"),
        "未读完不得出现确认按钮：\n{text}"
    );

    // 跳到結尾後重繪：出現確認按鈕。
    app.agreement.as_mut().expect("阅读门").to_bottom();
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("[ 同意并继续 ]"),
        "到底部后应出现确认按钮：\n{text}"
    );
    assert!(text.contains("enter 同意并继续"), "应提示确认键：\n{text}");
    assert!(!text.contains("请阅读至最底部"));
}

#[test]
fn agreement_failure_is_shown_in_footer() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    let mut state = AgreementState::new();
    state.fail("写入配置文件失败".to_owned());
    app.agreement = Some(Box::new(state));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let (y, row) = find_row(terminal.backend(), "保存失败");
    assert!(
        row.contains("保存失败：写入配置文件失败（按 enter 重试）"),
        "{row}"
    );
    assert_eq!(
        terminal.backend().buffer()[(1, y)].fg,
        THEME.red,
        "错误信息应以红色显示"
    );
}

/// 開啟閱讀門並捲到協議中第一個含 `needle` 的列。
///
/// 捲動需要已知的視窗高度與總列數，因此先繪製一次再捲動，最後重繪並回傳。
fn agreement_showing(app: &mut App, needle: &str) -> Terminal<TestBackend> {
    drop(draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut *app)
    }));
    let lines = crate::privacy::wrap(crate::privacy::document(), usize::from(WIDTH) - 2);
    let index = i32::try_from(
        lines
            .iter()
            .position(|line| line.text.contains(needle))
            .unwrap_or_else(|| panic!("协议中找不到 {needle}")),
    )
    .expect("列号可转为 i32");
    app.agreement.as_mut().expect("阅读门").scroll_by(index);
    draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut *app)
    })
}

#[test]
fn agreement_table_cards_align_labels_with_their_values() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.agreement = Some(Box::new(AgreementState::new()));

    let terminal = agreement_showing(&mut app, "login.xjtu.edu.cn");

    // 卡片標題：符號為次要色、主機名為強調色。
    let (title_y, title_row) = find_row(terminal.backend(), "▸ login.xjtu.edu.cn");
    assert_eq!(column_of(&title_row, "▸"), 1, "外框后即为卡片标题");
    assert_eq!(
        terminal.backend().buffer()[(1, title_y)].fg,
        THEME.muted,
        "标题符号以次要色呈现"
    );
    assert_eq!(
        terminal.backend().buffer()[(3, title_y)].fg,
        THEME.accent,
        "主机名以强调色呈现"
    );

    // 欄位列：標籤與值同行，值自固定欄起算。
    let (label_y, label_row) = find_row(terminal.backend(), "会上传的内容");
    assert!(label_row.contains("账号、RSA 加密后的密码"), "{label_row}");
    assert_eq!(column_of(&label_row, "会上传的内容"), 3, "缩进 2 ＋ 外框 1");
    assert_eq!(terminal.backend().buffer()[(1, label_y)].fg, THEME.muted);
    assert_eq!(terminal.backend().buffer()[(17, label_y)].fg, THEME.text);

    // 續行以標籤欄懸掛縮排，仍屬同一欄位。
    let next = row_text(terminal.backend(), label_y + 1);
    assert!(
        next.contains("所需的会话与状态字段"),
        "续行应接续同一栏的值：{next}"
    );
    assert_eq!(column_of(&next, "所需的会话与状态字段"), 17);
    assert_eq!(
        terminal.backend().buffer()[(17, label_y + 1)].fg,
        THEME.text
    );
}

#[test]
fn agreement_table_grid_aligns_columns_when_it_fits() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.agreement = Some(Box::new(AgreementState::new()));

    let terminal = agreement_showing(&mut app, "平台");

    // 短表格排成對齊表格：標頭兩欄同行、使用標題色。
    let (header_y, header) = find_row(terminal.backend(), "平台");
    assert!(header.contains("目录"), "标头应与另一栏同行：{header}");
    assert_eq!(
        terminal.backend().buffer()[(1, header_y)].fg,
        THEME.blue,
        "标头列使用标题色"
    );

    // 資料列的第二欄與標頭的第二欄起點一致。
    let (_, row) = find_row(terminal.backend(), "Linux");
    assert!(row.contains("~/.local/share/ohmyXJTU/"), "{row}");
    assert_eq!(
        column_of(&row, "~/.local/share/ohmyXJTU/"),
        column_of(&header, "目录"),
        "两列的字段起点必须一致"
    );

    let rule = row_text(terminal.backend(), header_y + 1);
    assert!(rule.contains("───"), "标头下方应为表格细线：{rule}");
}

/// 詳情計數必須與作業彙總的「有效提交」語義一致（單一判據 is_effective）。
#[test]
fn activity_detail_counts_effective_submissions_consistently() {
    let list: LmsSubmissionList = serde_json::from_str(
        r#"{"list":[
            {"id":1,"submitted_at":"2026-09-20T02:00:00Z","is_latest_version":true},
            {"id":2,"submitted_at":"2026-09-21 09:30:00","is_latest_version":false},
            {"id":3,"submitted_at":"2026-09-22 09:30:00"}
        ]}"#,
    )
    .expect("脱敏样本");

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        description: None,
        end_time: None,
        submit_by_group: Some(false),
        submissions: Some(list.list),
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("提交记录：2 条有效（另有 1 条历史版本）"),
        "计数应与汇总语义一致：\n{text}"
    );
    assert!(
        text.contains("最新版本：未知"),
        "未知的 is_latest_version 应显示「未知」：\n{text}"
    );
}

/// 全部都是舊版本時不得宣稱有有效提交（與彙總的「待提交」一致）。
#[test]
fn activity_detail_reports_no_effective_submissions() {
    let list: LmsSubmissionList = serde_json::from_str(
        r#"{"list":[
            {"id":1,"is_latest_version":false},
            {"id":2,"is_latest_version":"false"}
        ]}"#,
    )
    .expect("脱敏样本");

    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        description: None,
        end_time: None,
        submit_by_group: Some(false),
        submissions: Some(list.list),
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("提交记录：暂无有效提交（另有 2 条历史版本）"),
        "全为旧版本时不得显示为有效提交：\n{text}"
    );
}

/// 提交單位未知時不得顯示為「个人」。
#[test]
fn homework_detail_shows_unknown_submission_unit() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            course_id: "1".to_owned(),
            course_name: "编译原理".to_owned(),
            activity_id: "a-1".to_owned(),
            title: "缺单位作业".to_owned(),
            end_time: None,
            description: None,
            submit_by_group: None,
            submission_count: Some(0),
            note: None,
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("提交单位：未知"),
        "不得把未知提交单位显示为个人：\n{text}"
    );
}

/// 學期外的課表提示列（搭配空狀態）必須實際顯示。
#[test]
fn schedule_renders_notice_above_empty_state() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    let mut data = schedule_data(Vec::new());
    data.notice = Some("本学期已结束（2027-01-17）".to_owned());
    app.schedule = Page::Ready(data);

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("本学期已结束（2027-01-17）"),
        "应显示学期外提示：\n{text}"
    );
    assert!(
        text.contains("该周没有课程安排"),
        "空状态提示应保留：\n{text}"
    );
}

/// 標題顯示週次與總週數（`第 N/M 周`）。
#[test]
fn schedule_title_shows_the_week_and_total() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(Vec::new()));
    app.schedule_week = Some(4);
    app.schedule_total = Some(23);

    let text = main_text(&mut app);
    assert!(
        text.contains("第 4/23 周"),
        "标题应显示周次与总周数：\n{text}"
    );
}

/// 切週載入期間：標題立即顯示目標週，內容清空並顯示載入說明。
#[test]
fn switching_week_shows_the_target_week_while_loading() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule_week = Some(6);
    app.schedule_total = Some(23);
    app.schedule.reset_loading("正在加载第 6 周…");

    let text = main_text(&mut app);
    assert!(
        text.contains("第 6/23 周"),
        "标题应立即显示目标周：\n{text}"
    );
    assert!(
        text.contains("正在加载第 6 周…"),
        "内容应显示加载说明：\n{text}"
    );
    assert!(
        text.contains("[ ] 切换周次"),
        "底栏应提示切周按键：\n{text}"
    );
}

/// 作業說明顯示於詳情框：純文字、保留換行、不留 HTML 標籤。
#[test]
fn homework_detail_shows_activity_description() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: Some("第一章习题\n交到邮箱".to_owned()),
                has_media: false,
                has_links: false,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("第一章作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("作业描述："), "应显示描述区块：\n{text}");
    assert!(text.contains("第一章习题"), "应显示描述内容：\n{text}");
    assert!(text.contains("交到邮箱"), "描述换行应保留：\n{text}");
    assert!(!text.contains("<p>"), "不应残留 HTML 标签：\n{text}");
}

/// 沒有說明時不顯示描述區塊（不得把空值當成內容）。
#[test]
fn homework_detail_hides_description_section_when_absent() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[homework_input("第一章作业", "2026-10-01 23:59:59", 0)],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        !text.contains("作业描述"),
        "没有说明时不应显示描述区块：\n{text}"
    );
}

/// 長說明可捲動：列數由繪製回寫，捲到底後應看得到結尾。
#[test]
fn homework_detail_scrolls_long_description() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let description = (1..=30)
        .map(|line| format!("第{line}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: Some(description),
                has_media: false,
                has_links: false,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("长作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("第1行"), "初始应显示说明开头：\n{text}");
    assert!(!text.contains("第30行"), "初始不应显示说明结尾：\n{text}");
    assert!(app.homework_scroll.scrollable(), "内容超长时应可滚动");

    app.homework_scroll.to_bottom();
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("第30行"), "滚到底应显示说明结尾：\n{text}");
    assert!(
        !text.contains("第1行"),
        "滚到底后开头应已离开画面：\n{text}"
    );
}

/// 整份說明只有一張圖片：必須標註含圖片並提示開網頁，而不是看起來沒有說明。
#[test]
fn homework_detail_marks_image_only_description() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: None,
                has_media: true,
                has_links: false,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("图片作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("作业描述："), "应显示描述区块：\n{text}");
    assert!(text.contains("说明含图片"), "应标注说明含图片：\n{text}");
    assert!(
        text.contains("按 o 打开思源学堂后自行查看"),
        "应提示到思源学堂看原文：\n{text}"
    );
}

/// 說明含連結（`href` 目標不在純文字裡）：應標註並提示開網頁。
#[test]
fn homework_detail_marks_link_only_description() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: Some("下载附件".to_owned()),
                has_media: false,
                has_links: true,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("链接作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("下载附件"), "应显示锚文字：\n{text}");
    assert!(text.contains("说明含链接"), "应标注含链接：\n{text}");
    assert!(
        text.contains("按 o 打开思源学堂后自行查看"),
        "应提示到思源学堂看原文：\n{text}"
    );
}

/// 圖片與連結並存：提示合併為一句。
#[test]
fn homework_detail_combines_media_and_link_hint() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: None,
                has_media: true,
                has_links: true,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("图文作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("说明含图片、链接"),
        "图片与链接并存时应合并提示：\n{text}"
    );
}

/// 底欄在詳情可捲動時提示捲動鍵（終端夠寬時才看得見完整提示）。
#[test]
fn footer_hints_scrolling_when_detail_is_scrollable() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let description = (1..=30)
        .map(|line| format!("第{line}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: Some(description),
                has_media: false,
                has_links: false,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("长作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(200, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("PgUp/PgDn 滚动"),
        "底栏应提示滚动键：\n{text}"
    );
}

/// 沒畫到詳情面板時（空分組）不得沿用上一幀的捲動資訊，否則底欄會一直提示
/// 捲動鍵卻沒有東西可捲。
#[test]
fn footer_hides_scroll_hint_when_the_panel_is_not_drawn() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    // 唯一一筆作業已提交（屬「已完成」分組），預設的「未完成」分組為空。
    let items = aggregate(&[homework_input("已交作业", "2026-10-01 23:59:59", 1)], now);
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;
    // 假裝上一幀的詳情很長（可捲動）。
    app.homework_scroll.sync(5, 20);
    assert!(app.homework_scroll.scrollable(), "前置条件：上一帧可滚动");

    let terminal = draw(200, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("没有未完成的作业"),
        "应显示空分组提示：\n{text}"
    );
    assert!(
        !text.contains("PgUp/PgDn 滚动"),
        "没有详情面板时不应提示滚动：\n{text}"
    );
}

/// 提示列依重要度取捨：畫面專屬的操作（尤其捲動）在小終端也看得到。
///
/// 回歸：提示列以往把畫面專屬的操作接在固定的長前綴之後，終端稍窄就會被裁掉
/// （實測 100 欄時連「enter 收起详情」都只剩半個字），使用者因此看不到「這份
/// 說明還能往下讀」。
#[test]
fn footer_keeps_page_hints_on_narrow_terminals() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let description = (1..=30)
        .map(|line| format!("第{line}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: Some(description),
                has_media: false,
                has_links: false,
                attachments: Vec::new(),
                issue: None,
            }),
            ..homework_input("长作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    for width in [64, 80] {
        let terminal = draw(width, HEIGHT, |frame| {
            crate::tui::views::draw(frame, &mut app)
        });
        let footer = row_text(terminal.backend(), HEIGHT - 1);
        assert!(
            footer.contains("PgUp/PgDn 滚动"),
            "{width} 栏应看得到滚动提示：{footer:?}"
        );
        assert!(
            footer.contains("[ ] 分组"),
            "{width} 栏应看得到页面操作：{footer:?}"
        );
    }

    // 放不下的通用提示整段捨去（並標示還有未顯示的提示）。
    let terminal = draw(64, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let footer = row_text(terminal.backend(), HEIGHT - 1);
    assert!(
        !footer.contains("^P 账户设置"),
        "应舍去放不下的提示：{footer:?}"
    );
    assert!(footer.contains('…'), "应标注还有未显示的提示：{footer:?}");

    // 寬終端仍列出完整提示（提示段數隨功能增加，這裡留出足夠的欄寬）。
    let terminal = draw(220, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let footer = row_text(terminal.backend(), HEIGHT - 1);
    assert!(
        footer.contains("^P 账户设置"),
        "宽终端应显示完整提示：{footer:?}"
    );
    assert!(!footer.contains('…'), "全部显示时不应有省略号：{footer:?}");
}

/// 思源學堂活動詳情顯示說明，且長說明可捲動。
#[test]
fn activity_detail_shows_description_and_scrolls() {
    let description = (1..=30)
        .map(|line| format!("第{line}行"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        description: Some(ActivityContent {
            text: Some(description),
            has_media: false,
            has_links: false,
            attachments: Vec::new(),
            issue: None,
        }),
        end_time: None,
        submit_by_group: Some(false),
        submissions: None,
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("作业描述："), "应显示描述区块：\n{text}");
    assert!(text.contains("第1行"), "初始应显示说明开头：\n{text}");
    assert!(!text.contains("第30行"), "初始不应显示说明结尾：\n{text}");

    app.lms.detail_scroll.to_bottom();
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("第30行"), "滚到底应显示说明结尾：\n{text}");
    assert!(
        !text.contains("第1行"),
        "滚到底后开头应已离开画面：\n{text}"
    );
}

/// 說明區塊標題依活動類型：作業為「作业描述」，其他為「内容」。
#[test]
fn activity_detail_labels_description_by_kind() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "课程简介".to_owned(),
        kind: ActivityKind::Material,
        description: Some(ActivityContent {
            text: Some("课程介绍".to_owned()),
            has_media: true,
            has_links: false,
            attachments: Vec::new(),
            issue: None,
        }),
        end_time: None,
        submit_by_group: None,
        submissions: None,
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("内容："),
        "资料类型应使用「内容」标题：\n{text}"
    );
    assert!(text.contains("课程介绍"), "应显示正文：\n{text}");
    assert!(text.contains("说明含图片"), "应标注说明含图片：\n{text}");
    assert!(
        !text.contains("作业描述"),
        "非作业不得使用作业描述标题：\n{text}"
    );
}

/// 測試用附件。
fn upload(name: &str, size: Option<u64>) -> LmsUpload {
    LmsUpload {
        name: Some(name.to_owned()),
        size,
    }
}

/// 附件逐項列出名稱與大小，並與圖片／連結共用同一句提示。
#[test]
fn homework_detail_lists_attachments() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: None,
                has_media: false,
                has_links: false,
                attachments: vec![
                    upload("题目.pdf", Some(1_234_567)),
                    upload("参考答案.docx", Some(24_576)),
                ],
                issue: None,
            }),
            ..homework_input("附件作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("附件（2）："), "应列出附件数量：\n{text}");
    assert!(
        text.contains("题目.pdf（1.2 MB）"),
        "应显示附件名称与大小：\n{text}"
    );
    assert!(
        text.contains("参考答案.docx（24 KB）"),
        "应显示附件名称与大小：\n{text}"
    );
    assert!(
        text.contains("说明含附件"),
        "附件内容无法在终端显示，应提示开网页：\n{text}"
    );
}

/// 附件過多時只列前面幾項，並註明剩餘數量。
#[test]
fn homework_detail_caps_the_attachment_list() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let attachments = (1..=7)
        .map(|index| upload(&format!("附件{index}.pdf"), None))
        .collect::<Vec<_>>();
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: None,
                has_media: false,
                has_links: false,
                attachments,
                issue: None,
            }),
            ..homework_input("多附件作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, 40, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("附件（7）："), "数量应为全部附件数：\n{text}");
    assert!(text.contains("附件5.pdf"), "应列出前几项：\n{text}");
    assert!(!text.contains("附件6.pdf"), "超出的附件不应列出：\n{text}");
    assert!(text.contains("…另有 2 个"), "应注明剩余数量：\n{text}");
}

/// 讀不出正文時顯示原因：不得靜默地看起來「這項活動沒有說明」。
///
/// 這是實網驗收的診斷入口——欄位假設與實際回應不符時，畫面會直接說出原因。
#[test]
fn homework_detail_reports_unreadable_description() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            description: Some(ActivityContent {
                text: None,
                has_media: false,
                has_links: false,
                attachments: Vec::new(),
                issue: Some(BODY_NOT_OBJECT_NOTE),
            }),
            ..homework_input("异常作业", "2026-10-01 23:59:59", 0)
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("作业描述："), "标题仍应显示：\n{text}");
    assert!(
        text.contains("未取得说明正文"),
        "应显示读取失败的原因：\n{text}"
    );
    assert!(text.contains("不是对象"), "应指出具体原因：\n{text}");
}

/// 思源學堂活動詳情同樣顯示讀取失敗的原因（共用同一套說明區塊）。
#[test]
fn activity_detail_reports_unreadable_description() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Lms;
    app.lms.level = LmsLevel::Detail;
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "1".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        description: Some(ActivityContent {
            text: None,
            has_media: false,
            has_links: false,
            attachments: Vec::new(),
            issue: Some(TOP_LEVEL_BODY_NOTE),
        }),
        end_time: None,
        submit_by_group: Some(false),
        submissions: None,
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("未取得说明正文"), "应显示原因：\n{text}");
    assert!(text.contains("顶层"), "应指出正文实际位置：\n{text}");
}

// ── 任務頁（自訂義任務） ─────────────────────────────────

/// 測試用任務。
fn todo(id: u64, content: &str, priority: Priority, completed: bool) -> Task {
    Task {
        id,
        content: content.to_owned(),
        description: None,
        tag: None,
        deadline: None,
        priority,
        completed,
    }
}

/// 任務頁測試用資料（任務段在前、作業段在後）。
fn task_page_app(tasks: Vec<Task>, items: Vec<HomeworkItem>) -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.task_page.tasks = tasks;
    app.homework = Page::Ready(homework_data(items, None));
    app
}

#[test]
fn task_page_separates_sections_with_a_spacer_and_pink_headers() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(&[homework_input("待办作业", "2026-10-01 23:59:59", 0)], now);
    let mut app = task_page_app(vec![todo(1, "写实验报告", Priority::High, false)], items);

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let text = screen_text(backend);

    let (task_y, _) = find_row(backend, "  任务");
    let (homework_y, _) = find_row(backend, "  作业");
    assert!(task_y < homework_y, "任务段应排在作业段之前：\n{text}");
    assert_eq!(task_y + 3, homework_y, "任务列后应留一列空白再接着作业段");

    // 空白列只判斷內容區：側邊欄的導覽標籤與此無關。
    let (_, title_row) = find_row(backend, "┌ 任务");
    let content_x = column_of(&title_row, "┌ 任务");
    let area = backend.buffer().area;
    assert!(
        (content_x + 1..area.x + area.width - 1)
            .all(|x| backend.buffer()[(x, task_y + 2)].symbol() == " "),
        "两段之间应留一列空白：\n{text}"
    );

    let (_, task_entry_row) = find_row(backend, "写实验报告");
    let (_, homework_entry_row) = find_row(backend, "待办作业");
    assert!(task_entry_row.contains('高'), "任务列应显示优先级");
    assert!(
        !task_entry_row.contains("  任务"),
        "分段标题与任务列必须是不同行"
    );
    assert!(!homework_entry_row.is_empty());

    // 兩段標題同色（粉紅強調），不以深淺灰區分。
    assert_eq!(
        backend.buffer()[(content_column(backend, task_y, content_x, "任务"), task_y)].fg,
        THEME.accent,
        "任务分段标题应为强调色"
    );
    assert_eq!(
        backend.buffer()[(
            content_column(backend, homework_y, content_x, "作业"),
            homework_y
        )]
            .fg,
        THEME.accent,
        "作业分段标题应与任务同色"
    );
}

#[test]
fn task_rows_show_priority_state_and_deadline_without_greying_out() {
    let mut task = todo(1, "写实验报告", Priority::High, false);
    // 用遠未來日期：固定日期一旦跨過就會被判為「逾期」，測試不再決定性。
    task.deadline = Some(
        chrono::DateTime::parse_from_rfc3339("2099-12-31T12:00:00+08:00").expect("固定截止时间"),
    );
    let app_tasks = vec![task, todo(2, "复习", Priority::Low, false)];
    let mut app = task_page_app(app_tasks, Vec::new());
    // 選取第二列：反白列會覆蓋文字色，顏色斷言必須在未選取的列上進行。
    app.homework_state.select(Some(1));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (row_y, row) = find_row(backend, "写实验报告");
    assert!(row.contains('高'), "应显示优先级：\n{row}");
    assert!(row.contains("待完成"), "应显示状态：\n{row}");
    assert!(row.contains("2099-12-31 12:00"), "应显示截止时间：\n{row}");

    // 未完成的任務維持一般文字色（不以灰色弱化）。
    assert_eq!(
        backend.buffer()[(column_of(&row, "写实验报告"), row_y)].fg,
        THEME.text,
        "任务内容应为一般文字色"
    );

    // 已完成的任務移到「已完成」分組，顏色不變。
    app.homework_group = HomeworkGroup::Completed;
    app.task_page.tasks[1].completed = true;
    app.task_page
        .tasks
        .push(todo(3, "自习", Priority::Low, true));
    app.homework_state.select(Some(1));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (done_y, done_row) = find_row(backend, "复习");
    assert!(done_row.contains("已完成"), "应显示已完成：\n{done_row}");
    assert_eq!(
        backend.buffer()[(column_of(&done_row, "复习"), done_y)].fg,
        THEME.text,
        "已完成任务不得以灰色呈现"
    );
}

#[test]
fn task_page_marks_multi_select_checkboxes() {
    let mut app = task_page_app(
        vec![
            todo(1, "写实验报告", Priority::High, false),
            todo(2, "复习", Priority::Low, false),
        ],
        Vec::new(),
    );
    app.task_page.multi = Some(HashSet::from([1]));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("[x] "), "已勾选的任务应显示勾选框：\n{text}");
    assert!(text.contains("[ ] "), "未勾选的任务应显示空框：\n{text}");
    assert!(text.contains("space 勾选"), "底栏应提示多选按键：\n{text}");
}

#[test]
fn task_detail_panel_shows_the_description_and_scrolls() {
    let mut task = todo(1, "写实验报告", Priority::Medium, false);
    task.description = Some(
        (1..=30)
            .map(|index| format!("第 {index} 行说明"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let mut app = task_page_app(vec![task], Vec::new());
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let text = screen_text(backend);
    assert!(text.contains("任务详情"), "应显示任务详情标题：\n{text}");
    assert!(text.contains("描述："), "应显示描述栏：\n{text}");
    assert!(text.contains("第 1 行说明"), "应显示描述内容：\n{text}");
    assert!(text.contains("优先级："), "详情应同时显示优先级：\n{text}");
    assert!(
        text.contains("PgUp/PgDn 滚动"),
        "内容超过面板高度时应提示滚动：\n{text}"
    );
    assert!(app.homework_scroll.scrollable(), "面板应可滚动");
}

/// 逾期任務的狀態列（標籤與語意色）與詳情的截止時間列。
///
/// 截止時間固定在 2020 年（永遠早於執行當下），因此不依賴測試執行的日期。
/// 顏色斷言取的是**詳情面板**：清單列被選取時會套用高亮樣式（覆寫字色），
/// 詳情面板沒有高亮，才看得到語意色。
#[test]
fn task_page_shows_overdue_state_and_deadline() {
    let mut overdue = todo(1, "过期的任务", Priority::High, false);
    overdue.deadline =
        Some(chrono::DateTime::parse_from_rfc3339("2020-01-02T08:00:00+08:00").expect("固定时间"));
    let mut app = task_page_app(vec![overdue], Vec::new());
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let text = screen_text(backend);

    let (_, row) = find_row(backend, "过期的任务");
    assert!(row.contains("逾期"), "应显示逾期状态：\n{row}");

    let (status_y, status_row) = find_row(backend, "状态：");
    assert!(
        status_row.contains("逾期"),
        "详情应显示逾期状态：\n{status_row}"
    );
    assert_eq!(
        backend.buffer()[(column_of(&status_row, "逾期"), status_y)].fg,
        THEME.red,
        "逾期应以错误色（危险）呈现"
    );
    assert!(
        text.contains("截止：2020-01-02 08:00"),
        "详情应显示任务的截止时间（不是「无」）：\n{text}"
    );
}

/// 作業詳情的「说明」列與小組提交單位（`submit_by_group == Some(true)`）。
#[test]
fn homework_detail_shows_note_and_group_submission() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(
        &[HomeworkInput {
            course_id: "1".to_owned(),
            course_name: "编译原理".to_owned(),
            activity_id: "a-1".to_owned(),
            title: "小组作业".to_owned(),
            end_time: None,
            description: None,
            submit_by_group: Some(true),
            submission_count: Some(0),
            note: Some("需提交 PDF".to_owned()),
        }],
        now,
    );
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Homework;
    app.homework = Page::Ready(homework_data(items, None));
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("提交单位：小组"),
        "应显示小组提交单位：\n{text}"
    );
    assert!(text.contains("说明：需提交 PDF"), "应显示说明列：\n{text}");
}

/// 多選模式的勾選框欄在作業列也要佔位（作業不可勾選），兩種列的欄位才對齊。
#[test]
fn multi_select_keeps_task_and_homework_columns_aligned() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(&[homework_input("待办作业", "2026-10-01 23:59:59", 0)], now);
    let mut app = task_page_app(vec![todo(1, "写实验报告", Priority::High, false)], items);
    app.task_page.multi = Some(HashSet::new());

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (_, task_row) = find_row(backend, "写实验报告");
    let (_, homework_row) = find_row(backend, "待办作业");
    assert_eq!(
        column_of(&task_row, "写实验报告"),
        column_of(&homework_row, "待办作业"),
        "作业列应以空白补上勾选框栏，维持与任务列对齐"
    );
}

/// 最小終端尺寸下任務頁仍完整顯示清單（不提示放大視窗）。
///
/// 任務頁的欄寬需求（32 欄）小於最小終端尺寸能給的內容寬度，所以「终端过窄」
/// 在實務上不會觸發；這個測試鎖住那個前提——改動 `MIN_WIDTH`、側欄寬度或欄寬
/// 下限時會在這裡失敗。
#[test]
fn task_page_fits_at_the_minimum_terminal_size() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(&[homework_input("待办作业", "2099-12-31 23:59:59", 0)], now);
    let mut app = task_page_app(vec![todo(1, "写实验报告", Priority::High, false)], items);

    let terminal = draw(
        crate::tui::ui::MIN_WIDTH,
        crate::tui::ui::MIN_HEIGHT,
        |frame| crate::tui::views::draw(frame, &mut app),
    );
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("写实验报告"),
        "最小尺寸仍应显示任务列：\n{text}"
    );
    assert!(
        !text.contains("终端过窄"),
        "任务页在最小尺寸下仍放得下，不应提示放大窗口：\n{text}"
    );
}

#[test]
fn task_page_search_hint_and_empty_state() {
    let mut app = task_page_app(
        vec![todo(1, "写实验报告", Priority::High, false)],
        Vec::new(),
    );
    app.task_page.filter = Some("不存在的任务".to_owned());

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("筛选“不存在的任务”：0 项"),
        "应显示筛选提示与匹配数：\n{text}"
    );
    assert!(
        text.contains("没有匹配的条目"),
        "无匹配时应显示说明：\n{text}"
    );
    assert!(
        text.contains("（esc 清除）"),
        "筛选提示应说明如何清除：\n{text}"
    );
}

#[test]
fn task_form_popup_shows_fields_and_inline_error() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::TaskForm(Box::new(TaskFormState::add())));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("添加任务"), "应显示弹窗标题：\n{text}");
    for label in ["内容", "标签", "描述", "截止", "优先级", "完成"] {
        assert!(text.contains(label), "表单应显示 {label} 字段：\n{text}");
    }
    assert!(
        text.contains("（可留空，6 个汉字以内）"),
        "标签字段应显示长度上限提示：\n{text}"
    );
    assert!(
        text.contains("2026-12-31 12:30（可留空）"),
        "截止字段应显示格式提示：\n{text}"
    );
    assert!(text.contains("^s 保存"), "应显示保存按键提示：\n{text}");
    assert!(
        text.contains("tab 切换字段"),
        "应显示字段切换提示：\n{text}"
    );

    // 驗證失敗：錯誤就地顯示，欄位仍在。
    if let Screen::TaskForm(form) = &mut app.screen {
        form.error = Some("任务内容不能为空".to_owned());
    }
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("任务内容不能为空"),
        "应就地显示错误：\n{text}"
    );
    assert!(text.contains("内容"), "错误不得清空表单：\n{text}");
}

#[test]
fn task_menu_and_confirm_popups_render() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::TaskMenu(TaskMenuState { index: 1 }));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("任务设置"), "应显示设置弹窗标题：\n{text}");
    assert!(
        text.contains("多选（批量操作）"),
        "应列出多选选项：\n{text}"
    );
    assert!(
        text.contains("删除所有已完成的任务"),
        "应列出删除已完成选项：\n{text}"
    );

    app.set_screen(Screen::TaskConfirm(TaskConfirmState { count: 3 }));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("确认删除"), "应显示确认弹窗标题：\n{text}");
    assert!(text.contains("3"), "应显示待删除数量：\n{text}");
    assert!(text.contains("个已完成任务？"), "应显示确认问句：\n{text}");
    assert!(text.contains("y 确认"), "应提示确认按键：\n{text}");
}

#[test]
fn task_page_search_box_shows_the_query_and_cursor() {
    let mut app = task_page_app(
        vec![todo(1, "写实验报告", Priority::High, false)],
        Vec::new(),
    );
    app.task_page.search = Some(InputLine::with_value("报告"));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (row_y, row) = find_row(backend, "搜索：报告");
    let (tabs_y, _) = find_row(backend, "未完成 1");
    assert!(
        tabs_y < row_y,
        "搜索框应在分组标签列之后：\n{}",
        screen_text(backend)
    );

    // 游標應緊接在輸入內容之後（前綴 1+4+2 欄，內容為兩個全形字＝4 欄）。
    let content_x = column_of(&row, "搜索");
    assert_eq!(backend.cursor_position().y, row_y, "游标应在搜索框那一列");
    assert_eq!(
        backend.cursor_position().x,
        content_x + 10,
        "游标应在输入内容之后：\n{row}"
    );
}

#[test]
fn task_page_hides_the_search_box_when_no_search_is_open() {
    let mut app = task_page_app(
        vec![todo(1, "写实验报告", Priority::High, false)],
        Vec::new(),
    );

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(!text.contains("搜索："), "未搜尋時不應出现搜尋框：\n{text}");
}

#[test]
fn task_batch_menu_lists_the_batch_operations() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::TaskBatchMenu(TaskBatchMenuState { index: 2 }));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("批量操作"), "应显示弹窗标题：\n{text}");
    for label in ["标记完成", "标记未完成", "删除"] {
        assert!(text.contains(label), "应列出 {label}：\n{text}");
    }
    assert!(text.contains("esc 返回"), "应提示返回按键：\n{text}");
}

#[test]
fn task_form_edit_mode_prefills_and_shows_busy_state() {
    let mut app = App::new(AccessPolicy::Auto);
    let mut task = todo(7, "写实验报告", Priority::High, true);
    task.tag = Some("实验".to_owned());
    app.set_screen(Screen::TaskForm(Box::new(TaskFormState::edit(&task))));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("编辑任务"), "编辑模式标题不同：\n{text}");
    assert!(text.contains("写实验报告"), "应预填内容：\n{text}");
    let (_, tag_row) = find_row(terminal.backend(), "标签");
    assert!(tag_row.contains("实验"), "应预填标签：\n{tag_row}");
    assert!(text.contains("高"), "应显示优先级：\n{text}");
    assert!(text.contains("已完成"), "应显示完成状态：\n{text}");

    if let Screen::TaskForm(form) = &mut app.screen {
        form.busy = true;
    }
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("正在保存…"), "保存中应显示提示：\n{text}");
}

// ── 任務頁排序（^L） ─────────────────────────────────────

/// 該列在內容區（側邊欄右框線之後）是否以分段標題開頭。
///
/// 側邊欄也有「任务」標籤，因此不能直接比對整列文字。
fn has_section_header(row: &str) -> bool {
    row.split_once('│')
        .is_some_and(|(_, rest)| rest.starts_with("│  任务") || rest.starts_with("│  作业"))
}

#[test]
fn sorted_task_page_mixes_rows_without_section_headers() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T12:00:00+08:00").expect("固定时间");
    let items = aggregate(&[homework_input("待办作业", "2026-10-01 23:59:59", 0)], now);
    let mut app = task_page_app(vec![todo(1, "写实验报告", Priority::Low, false)], items);
    app.task_page.sort = SortMode::Priority;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let text = screen_text(backend);
    let area = backend.buffer().area;

    // 混合排序：內容區不再出現分段標題。
    let headers: Vec<String> = (area.y..area.y + area.height)
        .map(|y| row_text(backend, y))
        .filter(|row| has_section_header(row))
        .collect();
    assert!(headers.is_empty(), "混合排序不应出现分段标题：{headers:?}");

    // 作業（高）排在低優先級任務之前，兩列相鄰（沒有標題或空白列夾在中間）。
    let (homework_y, _) = find_row(backend, "待办作业");
    let (task_y, _) = find_row(backend, "写实验报告");
    assert!(
        homework_y < task_y,
        "作业的优先级视为高，应排在低优先级任务前：\n{text}"
    );
    assert_eq!(homework_y + 1, task_y, "混合排序不应插入空白列：\n{text}");

    // 標題列說明目前的排序方式（分段標題消失後仍看得出在按什麼排序）。
    let (_, tabs_row) = find_row(backend, "排序：优先级");
    assert!(
        tabs_row.contains("任务与作业混合"),
        "标题列应说明混合排序：\n{tabs_row}"
    );
}

#[test]
fn sort_prompt_replaces_the_footer_with_the_sort_keys() {
    let mut app = task_page_app(
        vec![todo(1, "写实验报告", Priority::High, false)],
        Vec::new(),
    );
    // 排序提示開啟時，底欄只顯示排序相關的按鍵。
    app.set_screen(Screen::Sort);
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let footer = row_text(backend, HEIGHT - 1);
    assert!(
        footer.contains("排序：[p] 优先级"),
        "底栏应显示排序按键：{footer}"
    );
    assert!(
        footer.contains("[d] 截止时间"),
        "底栏应显示排序按键：{footer}"
    );
    assert!(footer.contains("esc 取消"), "底栏应显示取消提示：{footer}");
    assert!(
        !footer.contains("^a 添加"),
        "排序提示应取代一般按键提示：{footer}"
    );

    // 關閉排序提示後恢復一般提示；提示段數多，這裡用寬終端確認 `^L` 仍在提示列中。
    app.set_screen(Screen::Main);
    let terminal = draw(220, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let footer = row_text(terminal.backend(), HEIGHT - 1);
    assert!(footer.contains("^L 排序"), "任务页应提示 ^L：{footer}");
    assert!(
        !footer.contains("排序：[p]"),
        "关闭后不应再显示排序提示：{footer}"
    );
}

/// 通知尚未消失時按下 `^L`：底欄必須立刻換成排序提示（而不是繼續顯示通知）。
#[test]
fn pressing_sort_key_hides_the_pending_notice_immediately() {
    let (jobs, _rx) = std::sync::mpsc::channel();
    let mut app = task_page_app(
        vec![todo(1, "写实验报告", Priority::High, false)],
        Vec::new(),
    );
    app.set_message("作业已更新（用时 3.2s）");

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let before = row_text(terminal.backend(), HEIGHT - 1);
    assert!(
        before.contains("作业已更新"),
        "测试前提：通知应显示在底栏：{before}"
    );

    crate::tui::handler::handle_key(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('l'),
            crossterm::event::KeyModifiers::CONTROL,
        ),
        &jobs,
    );
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let after = row_text(terminal.backend(), HEIGHT - 1);
    assert!(
        after.contains("排序：[p] 优先级"),
        "按下 ^L 后底栏应立刻显示排序按键：{after}"
    );
    assert!(
        !after.contains("作业已更新"),
        "旧通知不应继续盖住画面提示：{after}"
    );
}

// ── 任務標籤 ───────────────────────────────────────────

#[test]
fn task_rows_show_the_tag_after_the_priority() {
    let mut tagged = todo(1, "写实验报告", Priority::High, false);
    tagged.tag = Some("实验".to_owned());
    // 選取第二列：反白列會覆蓋文字色，顏色斷言必須在未選取的列上進行。
    let mut app = task_page_app(
        vec![tagged, todo(2, "复习", Priority::Low, false)],
        Vec::new(),
    );
    app.homework_state.select(Some(1));

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let backend = terminal.backend();
    let (row_y, row) = find_row(backend, "写实验报告");

    let priority_x = column_of(&row, "高");
    let tag_x = column_of(&row, "・实验");
    assert_eq!(
        tag_x,
        priority_x + 2,
        "全角中点应紧接在优先级之后、标签紧接中点：\n{row}"
    );
    // 優先級用語意色、標籤用一般文字色。
    assert_eq!(backend.buffer()[(priority_x, row_y)].fg, THEME.red);
    assert_eq!(
        backend.buffer()[(tag_x, row_y)].fg,
        THEME.text,
        "标签应为一般文字色"
    );

    // 沒有標籤的任務列不顯示中點。
    let (_, plain) = find_row(backend, "复习");
    assert!(!plain.contains('・'), "没有标签时不显示中点：\n{plain}");
}

#[test]
fn task_detail_shows_the_tag_or_none() {
    let mut tagged = todo(1, "写实验报告", Priority::High, false);
    tagged.tag = Some("实验".to_owned());
    let mut app = task_page_app(
        vec![tagged, todo(2, "复习", Priority::Low, false)],
        Vec::new(),
    );
    app.homework_detail = true;

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("标签：实验"), "详情应显示标签：\n{text}");

    app.homework_state.select(Some(1));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("标签：无"), "没有标签时显示“无”：\n{text}");
}

#[test]
fn footer_shows_tag_suggestion_hint_only_when_tags_exist() {
    let mut app = task_page_app(
        vec![todo(1, "写实验报告", Priority::High, false)],
        Vec::new(),
    );
    app.task_page.search = Some(InputLine::with_value(""));

    let terminal = draw(120, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let footer = row_text(terminal.backend(), HEIGHT - 1);
    assert!(
        !footer.contains("选标签"),
        "一个标签也没有时不应提示：{footer:?}"
    );

    app.task_page.tasks[0].tag = Some("实验".to_owned());
    let terminal = draw(120, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let footer = row_text(terminal.backend(), HEIGHT - 1);
    assert!(
        footer.contains("↑/↓ 选标签"),
        "有标签时应提示上下键：{footer:?}"
    );
}
