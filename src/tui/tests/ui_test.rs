//! 繪製層測試：以 `TestBackend` 渲染表單與登入彈窗，驗證欄位版面與游標位置。
//!
//! 中文標籤的顯示寬度是字元數的兩倍，因此「標籤補白、值區寬度、游標欄位」必須
//! 由同一套版面計算決定；驗證碼與簡訊驗證的輸入框也必須真的畫出來。

use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::config::AccessPolicy;
use crate::domain::homework::{HomeworkGroup, HomeworkInput, aggregate};
use crate::domain::semester::TermCode;
use crate::tui::app::{
    App, FormState, HomeworkData, LoginScreen, NavItem, Page, Screen, TermPickerState,
};
use crate::tui::text::{InputLine, MASK_CHAR};

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
