//! 繪製層測試：以 `TestBackend` 渲染表單與登入彈窗，驗證欄位版面與游標位置。
//!
//! 中文標籤的顯示寬度是字元數的兩倍，因此「標籤補白、值區寬度、游標欄位」必須
//! 由同一套版面計算決定；驗證碼與簡訊驗證的輸入框也必須真的畫出來。

use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use crate::config::AccessPolicy;
use crate::domain::homework::{HomeworkGroup, HomeworkInput, HomeworkItem, aggregate};
use crate::domain::semester::TermCode;
use crate::session::{AccessMode, SiteKind};
use crate::sites::attendance::FlowRecord;
use crate::sites::lms::{ActivityKind, LmsActivity, LmsCourse};
use crate::task::HomeworkIssue;
use crate::tui::app::{
    ActivityDetailView, App, FlowData, FormState, HomeworkData, LessonEntry, LmsLevel, LoginScreen,
    NavItem, Page, ScheduleData, Screen, SettingsState, TermPickerState,
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
    let offset = row.find(needle).expect("子字串应存在于该列");
    u16::try_from(Line::from(&row[..offset]).width()).unwrap_or(u16::MAX)
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
    assert_eq!(long.prefix, 26, "顯示寬度 24 + 間隔 2");
    assert_eq!(long.value, usize::from(FORM_INNER_WIDTH) - 26);

    // 窄視窗不得溢位。
    assert_eq!(field_layout(0, "加密口令").value, 0);
    assert_eq!(field_layout(10, "加密口令").value, 0);
}

#[test]
fn draws_form_with_cursor_at_value_column() {
    let mut form = FormState::login_retry();
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
        "「账号」顯示寬度 4，需補 10 欄"
    );
    assert_eq!(
        column_of(&row, "3120000001"),
        FORM_INNER_X + 16,
        "值應接在間隔後"
    );
    assert_eq!(
        backend.cursor_position().x,
        FORM_INNER_X + 16 + 10,
        "游標應位於已輸入文字之後"
    );
    assert_eq!(backend.cursor_position().y, row_y);
}

#[test]
fn masks_sensitive_values_in_form() {
    let mut form = FormState::login_retry();
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
        "密碼不得以明文顯示"
    );
    assert_eq!(column_of(&row, &masked), FORM_INNER_X + 16);
    assert_eq!(backend.cursor_position().x, FORM_INNER_X + 16 + 8);
}

#[test]
fn long_value_scrolls_and_keeps_cursor_inside_field() {
    let mut form = FormState::login_retry();
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

    assert_eq!(visible, window - 1, "應捲動到最尾端內容");
    assert_eq!(
        backend.cursor_position().x,
        value_column + u16::try_from(visible).unwrap_or(u16::MAX),
        "游標應落在可見內容尾端"
    );
    assert_eq!(backend.cursor_position().y, row_y);
}

#[test]
fn draws_credentials_form_with_previous_failure_note() {
    let screen = LoginScreen::Credentials {
        form: FormState::login_retry(),
        message: "登录失败：用户名或密码错误".to_owned(),
    };

    let terminal = draw(WIDTH, HEIGHT, |frame| draw_login(frame, &screen));
    let text = screen_text(terminal.backend());

    assert!(
        text.contains("上次登录失败：登录失败：用户名或密码错误"),
        "應顯示上一次的失敗訊息：\n{text}"
    );
    for label in ["账号", "密码", "加密口令"] {
        assert!(text.contains(label), "缺少欄位 {label}：\n{text}");
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
        "「验证码」顯示寬度 6，需補 8 欄"
    );
    assert_eq!(column_of(&row, "a1b2"), LOGIN_INNER_X + 16);
    assert_eq!(
        backend.cursor_position().x,
        LOGIN_INNER_X + 16 + 4,
        "游標應位於已輸入的驗證碼之後"
    );
    assert_eq!(backend.cursor_position().y, row_y);
    assert!(
        screen_text(backend).contains("验证码错误"),
        "錯誤訊息仍應顯示"
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
    assert!(text.contains("短信验证码"), "應畫出輸入框標籤：\n{text}");
    assert!(text.contains("138****8888"), "應顯示綁定手機號");

    // 空輸入且聚焦時，游標位於值區域起點。
    let (_, row) = find_row(backend, "短信验证码");
    assert_eq!(
        column_of(&row, "短信验证码"),
        LOGIN_INNER_X + 4,
        "「短信验证码」顯示寬度 10，需補 4 欄"
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
        submit_by_group: false,
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
        "應顯示載入進度：\n{text}"
    );
    assert!(
        text.contains("1 门课程缺少学期信息"),
        "應提示未納入課程：\n{text}"
    );
    assert!(
        text.contains("待办作业"),
        "預設分組應顯示未完成作業：\n{text}"
    );
    assert!(
        !text.contains("完成作业"),
        "已完成作業不應出現在未完成分組：\n{text}"
    );

    // 切換到「已完成」分組。
    app.homework_group = HomeworkGroup::Completed;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("完成作业"),
        "已完成分組應顯示已完成作業：\n{text}"
    );
    assert!(
        !text.contains("待办作业"),
        "未完成作業不應出現在已完成分組：\n{text}"
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
    assert!(text.contains("选择学期"), "應顯示標題：\n{text}");
    assert!(text.contains("考勤系统不可用"), "應顯示原因：\n{text}");
    assert!(
        text.contains("2026-2027 学年 第 1 学期"),
        "應列出學期：\n{text}"
    );
    assert!(text.contains("（建议）"), "應標示建議學期：\n{text}");
    assert!(text.contains("enter 确定"), "應顯示操作提示：\n{text}");
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
                submit_by_group: false,
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
        "應顯示已確認與待核實數量：\n{text}"
    );
    assert!(
        text.contains("用户信息解析失败"),
        "應顯示待核實原因：\n{text}"
    );
    assert!(text.contains("按 r 重试"), "應提示可重試：\n{text}");
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
        "未登入時底欄應顯示未登录與訪問策略：\n{text}"
    );

    // 思源學堂已登入：作業頁底欄只顯示實際訪問方式（不再贅述「已登录」）。
    app.set_site_mode(SiteKind::Lms, AccessMode::Direct);
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("[直连 自动]"),
        "登入後底欄應顯示訪問方式與策略：\n{text}"
    );
    assert!(
        !text.contains("已登录"),
        "登入狀態正常時不應佔用底欄版面：\n{text}"
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
        "保留舊資料時應在頁面內顯示更新失敗：\n{text}"
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
    assert!(text.contains("课表"), "底層側欄應保持可見：\n{text}");
    assert!(text.contains("未登录"), "底層底欄應保持可見：\n{text}");
    assert!(
        text.contains("正在登录考勤服务…"),
        "彈窗內容應顯示：\n{text}"
    );

    // 事件只更新彈窗內容：換成驗證碼畫面後，底層仍完整可見（不出現黑屏）。
    app.login = Some(Box::new(LoginScreen::Captcha {
        path: PathBuf::from("/tmp/captcha.png"),
        input: InputLine::with_value("a1b2"),
        error: None,
    }));
    let second = render(&mut app);
    let text = screen_text(second.backend());
    assert!(text.contains("课表"), "換畫面後底層仍應可見：\n{text}");
    assert!(text.contains("a1b2"), "新彈窗內容應顯示：\n{text}");
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
        description: None,
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
    assert!(text.contains("直播 0"), "應顯示各組計數：\n{text}");
    assert!(text.contains("作业 1"), "應顯示作業組計數：\n{text}");
    assert!(text.contains("资料 1"), "應顯示資料組計數：\n{text}");
    assert!(text.contains("活动 1"), "應只顯示目前分組的項目：\n{text}");

    // 空組顯示提示，且不顯示其他組的項目。
    app.lms.activity_group = crate::domain::activity::ActivityGroup::LectureLive;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("本组暂无活动"), "空組應顯示提示：\n{text}");
    assert!(!text.contains("活动 1"), "空組不應顯示其他組：\n{text}");
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
        end_time: Some("2026-10-01 12:00:00".to_owned()),
        submit_by_group: false,
        submissions: None,
        note: None,
    });

    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(text.contains("类型：直播"), "應顯示活動類型：\n{text}");
    assert!(!text.contains("待核实"), "非作業不應顯示待核实：\n{text}");
    assert!(
        !text.contains("提交记录"),
        "非作業不應顯示提交記錄區：\n{text}"
    );

    // 作業仍顯示提交狀態（未知時為待核实）。
    app.lms.detail = Page::Ready(ActivityDetailView {
        id: "2".to_owned(),
        title: "作业A".to_owned(),
        kind: ActivityKind::Homework,
        end_time: None,
        submit_by_group: false,
        submissions: None,
        note: None,
    });
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("提交记录：无法确认（待核实）"),
        "作業未知提交狀態應顯示待核实：\n{text}"
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
        "有效應為成功綠"
    );
    assert_eq!(
        backend.buffer()[(long_col, long_y)].fg,
        THEME.yellow,
        "未匹配應為警告黃"
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
    assert!(text.contains('…'), "窄畫面應以省略號截斷地點：\n{text}");
    assert!(text.contains("有效"), "狀態仍應可見：\n{text}");
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
        "載入中應顯示進度：\n{text}"
    );
    assert!(!text.contains("加载失败"), "載入中不得顯示失敗：\n{text}");
    assert!(
        !text.contains("没有未完成的作业"),
        "載入中不得顯示空結果：\n{text}"
    );

    // 終態空：明確顯示「本学期暂无作业」，而不是失敗。
    app.homework = Page::Ready(homework_data(Vec::new(), None));
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("本学期暂无作业"),
        "終態空應顯示明確空狀態：\n{text}"
    );
    assert!(!text.contains("加载失败"), "空結果不得顯示為失敗：\n{text}");
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
        "應顯示略過的課程數：\n{text}"
    );
    assert!(text.contains("按 r 重试"), "應提示可重試：\n{text}");
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
        "應顯示當前學期分區：\n{text}"
    );
    assert!(text.contains("历史课程"), "應顯示歷史分區：\n{text}");
    assert!(text.contains("学期未知"), "應顯示未知分區：\n{text}");

    let (current_y, _) = find_row(backend, "课程2");
    let (history_y, history_row) = find_row(backend, "课程1");
    let (unknown_y, _) = find_row(backend, "课程3");
    assert!(
        current_y < history_y && history_y < unknown_y,
        "順序應為當前學期 → 歷史 → 未知：\n{text}"
    );

    // 「历史课程」標題與上方課程之間應空出一列。
    let (header_y, _) = find_row(backend, "历史课程");
    assert_eq!(
        header_y,
        current_y + 2,
        "歷史分區標題前應留一列空白：\n{text}"
    );

    // 歷史課程以較淺灰色呈現；當前學期課程維持一般文字色。
    let history_col = column_of(&history_row, "课程1");
    assert_eq!(
        backend.buffer()[(history_col, history_y)].fg,
        THEME.muted,
        "歷史課程應為淺灰：\n{text}"
    );
    let (_, current_row) = find_row(backend, "课程2");
    let current_col = column_of(&current_row, "课程2");
    assert_eq!(
        backend.buffer()[(current_col, current_y)].fg,
        THEME.text,
        "當前學期課程應維持一般文字色：\n{text}"
    );
}

/// 斷言彈窗矩形：左右邊框逐列、下緣逐欄連續；上緣可能被標題（含空白填充）佔用，
/// 只驗證顏色；整個彈窗必須為不透明 surface 底色。
fn assert_popup_borders(backend: &TestBackend, popup: Rect) {
    let buffer = backend.buffer();
    for y in popup.y + 1..popup.y + popup.height - 1 {
        for x in [popup.x, popup.x + popup.width - 1] {
            let cell = &buffer[(x, y)];
            assert_ne!(cell.symbol(), " ", "邊框不得被打斷：(x={x}, y={y})");
            assert_eq!(cell.fg, THEME.accent, "邊框顏色：(x={x}, y={y})");
        }
    }
    for x in popup.x + 1..popup.x + popup.width - 1 {
        let cell = &buffer[(x, popup.y + popup.height - 1)];
        assert_ne!(cell.symbol(), " ", "下緣不得被打斷：(x={x})");
        assert_eq!(cell.fg, THEME.accent, "下緣顏色：(x={x})");
    }
    for x in popup.x + 1..popup.x + popup.width - 1 {
        let cell = &buffer[(x, popup.y)];
        // 上緣由粉色邊框與藍色標題組成；寬字元的尾隨格會被重置為空白。
        let symbol = cell.symbol();
        assert!(
            symbol == " " || cell.fg == THEME.accent || cell.fg == THEME.blue,
            "上緣不得殘留底層文字：(x={x}) symbol={symbol:?}"
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
                "彈窗必須不透明：(x={x}, y={y}) symbol={:?} bg={:?}",
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
                "彈窗內部不得有底層文字：(x={x}, y={y})"
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
    assert_eq!(first, second, "連續兩幀的彈窗內容必須一致");
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
    panic!("側邊欄找不到 {label}：\n{backend}");
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
    let homework = find_sidebar_row(backend, "作业");
    let attendance = find_sidebar_row(backend, "考勤流水");
    let lms = find_sidebar_row(backend, "思源学堂");

    assert!(
        schedule.contains("..."),
        "載入中應顯示三點動畫：{schedule:?}"
    );
    assert!(
        !schedule.contains("正在"),
        "側邊欄不得顯示載入說明：{schedule:?}"
    );

    // 標籤左對齊：所有項目共用同一個文字起始欄。
    let label_col = column_of(&schedule, "课表");
    assert_eq!(column_of(&homework, "作业"), label_col, "標籤應左對齊");
    assert_eq!(column_of(&attendance, "考勤流水"), label_col);
    assert_eq!(column_of(&lms, "思源学堂"), label_col);

    // 文字欄置中：以最寬標籤（考勤流水，寬 8）計算左右留白相等。
    let left_margin = usize::from(label_col);
    let right_margin = 20 - (left_margin + 8);
    assert_eq!(left_margin, right_margin, "文字欄應置中：{schedule:?}");

    // 三點緊貼文字正前方（不參與置中）。
    let dots_col = column_of(&schedule, "...");
    assert_eq!(
        usize::from(dots_col) + 4,
        usize::from(label_col),
        "三點應顯示在文字正前方：{schedule:?}"
    );

    // 相位 0：三點為空白，標籤欄位不得改變（動畫不造成跳動）。
    app.tick = 0;
    let terminal = draw(WIDTH, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let sidebar_blank = find_sidebar_row(terminal.backend(), "课表");
    assert!(
        !sidebar_blank.contains('.'),
        "相位 0 不應顯示點：{sidebar_blank:?}"
    );
    assert_eq!(
        column_of(&sidebar_blank, "课表"),
        label_col,
        "動畫不得讓標籤位移"
    );

    // 未載入的頁面永遠沒有指示燈。
    assert!(
        !homework.contains('.'),
        "未載入頁面不應顯示點：{homework:?}"
    );
}

/// 課表測試用課程。
fn lesson_entry(course: &str, classroom: &str, teacher: &str, label: &'static str) -> LessonEntry {
    LessonEntry {
        date: chrono::NaiveDate::from_ymd_opt(2026, 9, 29).expect("日期"),
        sections: "1-2".to_owned(),
        course_name: course.to_owned(),
        classroom: classroom.to_owned(),
        teacher: teacher.to_owned(),
        weeks: "1-16".to_owned(),
        status: None,
        label,
    }
}

/// 課表頁資料。
fn schedule_data(lessons: Vec<LessonEntry>) -> ScheduleData {
    ScheduleData {
        semester: "2026-2027-1".to_owned(),
        week: 4,
        lessons,
        skipped: 0,
    }
}

#[test]
fn schedule_rows_align_attendance_column() {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app.nav = NavItem::Schedule;
    app.schedule = Page::Ready(schedule_data(vec![
        lesson_entry("高等数学", "主楼A101", "张老师", "正常"),
        lesson_entry("思想道德与法治", "逸夫科学馆", "欧阳老师", "缺勤"),
        lesson_entry("大学物理", "中2-2201", "李老师", "请假"),
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
        "考勤狀態欄起點不得受課程、地點與教師長度影響：\n{normal_row}\n{absent_row}"
    );
    assert_eq!(
        column_of(&normal_row, "主楼A101"),
        column_of(&absent_row, "逸夫科学馆"),
        "地點欄起點必須一致：\n{normal_row}\n{absent_row}"
    );
    assert_eq!(
        backend.buffer()[(normal_col, normal_y)].fg,
        THEME.green,
        "正常應為成功綠"
    );
    assert_eq!(
        backend.buffer()[(absent_col, absent_y)].fg,
        THEME.red,
        "缺勤應為錯誤紅"
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
        "正常",
    )]));

    let terminal = draw(70, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("高等数学"), "課程仍應可見：\n{text}");
    assert!(text.contains("主楼A101"), "地點仍應可見：\n{text}");
    assert!(!text.contains("张老师"), "寬度不足時收起教師欄：\n{text}");
    assert!(text.contains("正常"), "考勤狀態仍應可見：\n{text}");
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
            end_time: Some("2026-10-08 23:59:59".to_owned()),
            submit_by_group: true,
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
        "寬畫面應顯示完整截止時間：\n{wide_row}"
    );
    assert!(
        wide_row.contains("小组"),
        "寬畫面應顯示提交單位：\n{wide_row}"
    );

    // 70 欄：收起「小组」欄與「截止」前綴（日期仍完整），標題以省略號截斷。
    let narrow = draw(70, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let (_, narrow_row) = find_row(narrow.backend(), "马克思");
    assert!(
        !narrow_row.contains("小组"),
        "窄畫面應收起提交單位欄：\n{narrow_row}"
    );
    assert!(
        !narrow_row.contains("截止"),
        "窄畫面應收起「截止」前綴：\n{narrow_row}"
    );
    assert!(
        narrow_row.contains("2026-10-08 23:59"),
        "窄畫面仍應保留完整日期：\n{narrow_row}"
    );
    assert!(
        narrow_row.contains('…'),
        "過長的文字應以省略號截斷：\n{narrow_row}"
    );

    // 64 欄：再壓縮日期（省略年份）。
    let compact = draw(64, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let (_, compact_row) = find_row(compact.backend(), "马克思");
    assert!(
        compact_row.contains("10-08 23:59"),
        "極窄畫面應壓縮為不含年份的日期：\n{compact_row}"
    );
    assert!(
        !compact_row.contains("2026-"),
        "壓縮後不應再顯示年份：\n{compact_row}"
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
            "待考勤",
        ),
        lesson_entry("体育-3", "塑胶田径场-田径场", "胡良楠", "待核实"),
    ]));
    // 選取第二列，避免高亮樣式影響第一列的擷取。
    app.schedule_state.select(Some(1));

    let terminal = draw(160, HEIGHT, |frame| {
        crate::tui::views::draw(frame, &mut app)
    });
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("毛泽东思想和中国特色社会主义理论体系概论"),
        "足夠寬時課程名稱應完整顯示：\n{text}"
    );
    assert!(
        text.contains("塑胶田径场-田径场"),
        "足夠寬時地點應完整顯示：\n{text}"
    );
    assert!(text.contains("赵金瑞"), "教師欄應完整顯示：\n{text}");
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
                end_time: Some("2026-10-12 15:59:59".to_owned()),
                submit_by_group: false,
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
        "足夠寬時課程名稱應完整顯示：\n{text}"
    );
    assert!(
        text.contains("第五章作业（含附件）"),
        "足夠寬時標題應完整顯示：\n{text}"
    );
    assert!(
        text.contains("截止 2026-10-12 15:59"),
        "截止時間應完整顯示：\n{text}"
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
        "正常",
    )]));

    let terminal = draw(64, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("终端过窄"), "過窄時應提示放大窗口：\n{text}");
    assert!(
        !text.contains("主楼A101"),
        "過窄時不應擠出殘缺的列表：\n{text}"
    );

    // 放寬兩欄即可正常顯示（不再提示）。
    let terminal = draw(66, HEIGHT, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(!text.contains("终端过窄"), "足夠寬時不應提示：\n{text}");
    assert!(text.contains("高等数"), "足夠寬時應顯示課表：\n{text}");
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
            end_time: Some("2026-10-08 23:59:59".to_owned()),
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
        "標題欄起點必須一致（類型欄以顯示寬度排版）：\n{first}\n{second}"
    );
    assert_eq!(
        column_of(&first, "2026-10-08"),
        column_of(&second, "2026-09-20"),
        "截止欄起點必須一致：\n{first}\n{second}"
    );
}

/// 造一個主畫面的 App（尺寸不足時 `too_small` 會在內容之前接手）。
fn main_screen_app() -> App {
    let mut app = App::new(AccessPolicy::Auto);
    app.set_screen(Screen::Main);
    app
}

#[test]
fn too_small_message_is_centered_and_colors_current_size() {
    // 48x18：寬度未達 64、高度剛好 18。提示應上下左右置中，
    // 且只有未達標的數字轉紅。
    let mut app = main_screen_app();

    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let backend = terminal.backend();
    let text = screen_text(backend);
    assert!(text.contains("终端太小了:"), "應顯示新版提示：\n{text}");
    assert!(!text.contains("窗口过小"), "舊版文案不應再出現：\n{text}");
    assert!(!text.contains("至少需要"), "不再顯示最低需求：\n{text}");

    let (title_y, title_row) = find_row(backend, "终端太小了");
    assert_eq!(title_y, 8, "第一行應垂直置中（(18-2)/2）：\n{text}");
    assert_eq!(
        column_of(&title_row, "终端太小了"),
        19,
        "第一行應水平置中（48/2 − 11/2）：\n{title_row}"
    );

    let (size_y, size_row) = find_row(backend, "宽 = ");
    assert_eq!(size_y, 9, "第二行應緊接在下一列：\n{text}");
    let line_start = column_of(&size_row, "宽 = ");
    assert_eq!(
        line_start, 16,
        "第二行應水平置中（48/2 − 16/2）：\n{size_row}"
    );

    let width_col = column_of(&size_row, "48");
    let height_col = column_of(&size_row, "高 = ") + 5;
    assert_eq!(
        backend.buffer()[(line_start, size_y)].fg,
        THEME.text,
        "標題文字應為白色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(width_col, size_y)].fg,
        THEME.red,
        "寬度 48 未達 64 應為紅色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(height_col, size_y)].fg,
        THEME.green,
        "高度 18 已達門檻應為綠色：\n{size_row}"
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
    assert_eq!(size_y, 6, "第二行應在提示帶的第二列：\n{size_row}");
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "宽 = ") + 5, size_y)].fg,
        THEME.green,
        "寬度 100 已達門檻應為綠色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "高 = ") + 5, size_y)].fg,
        THEME.red,
        "高度 12 未達 18 應為紅色：\n{size_row}"
    );

    // 48x24：太窄、夠高。
    let terminal = draw(48, 24, |frame| crate::tui::views::draw(frame, &mut app));
    let backend = terminal.backend();
    let (size_y, size_row) = find_row(backend, "宽 = 48");
    // 提示帶自 (24-2)/2 = 11 起算，第二行落在帶內第二列。
    assert_eq!(size_y, 12, "第二行應在提示帶的第二列：\n{size_row}");
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "宽 = ") + 5, size_y)].fg,
        THEME.red,
        "寬度 48 未達 64 應為紅色：\n{size_row}"
    );
    assert_eq!(
        backend.buffer()[(column_of(&size_row, "高 = ") + 5, size_y)].fg,
        THEME.green,
        "高度 24 已達門檻應為綠色：\n{size_row}"
    );
}

#[test]
fn too_small_message_survives_single_row_terminal() {
    let mut app = main_screen_app();
    let terminal = draw(40, 1, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("终端太小了"),
        "只剩一列時仍應顯示標題：\n{text}"
    );
    assert!(
        !text.contains("高 = "),
        "只放得下一列時不顯示第二行：\n{text}"
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
        "達到 64x18 門檻時不應顯示提示：\n{text}"
    );
}

#[test]
fn too_small_message_shown_on_startup_forms() {
    // 啟動時的解鎖／首次設定／設定表單與登入覆蓋層共用同一道尺寸守衛：
    // 過小的終端一律顯示放大提示，而不是把表單裁切到無法操作。
    let mut app = App::new(AccessPolicy::Auto); // 預設畫面＝解鎖表單
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("终端太小了"), "解鎖畫面應顯示提示：\n{text}");
    assert!(!text.contains("解锁凭证"), "過小時不應擠出表單：\n{text}");

    app.set_screen(Screen::Setup(FormState::setup()));
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(
        text.contains("终端太小了"),
        "首次設定畫面應顯示提示：\n{text}"
    );
    assert!(!text.contains("首次使用"), "過小時不應擠出表單：\n{text}");

    app.set_screen(Screen::SettingsForm(FormState::change_passphrase()));
    let terminal = draw(48, 18, |frame| crate::tui::views::draw(frame, &mut app));
    let text = screen_text(terminal.backend());
    assert!(text.contains("终端太小了"), "設定表單應顯示提示：\n{text}");
    assert!(
        !text.contains("修改加密口令"),
        "過小時不應擠出表單：\n{text}"
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
        "登入彈窗開啟時仍應顯示提示：\n{text}"
    );
    assert!(
        !text.contains("正在登录"),
        "過小時不應畫出登入彈窗：\n{text}"
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
        "達到門檻時不顯示提示：\n{text}"
    );
    assert!(text.contains("解锁凭证"), "應顯示解鎖表單標題：\n{text}");
    assert!(text.contains("加密口令"), "應顯示欄位標籤：\n{text}");
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
    assert!(!text.contains("终端太小了"), "足夠大時不顯示提示：\n{text}");
    assert!(
        text.contains("正在登录"),
        "足夠大時應顯示登入彈窗：\n{text}"
    );
}
