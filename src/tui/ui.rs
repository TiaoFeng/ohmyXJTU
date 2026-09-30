//! 共用繪製工具：置中彈窗、輸入框、提示列與尺寸守衛。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::tui::app::{FormState, LoginScreen};
use crate::tui::text::InputLine;
use crate::tui::theme::THEME;

/// 視窗最小寬度。
pub const MIN_WIDTH: u16 = 64;
/// 視窗最小高度。
pub const MIN_HEIGHT: u16 = 18;
/// 表單標籤欄的顯示寬度（以終端列計，不是字元數）。
const LABEL_WIDTH: u16 = 14;
/// 標籤與值之間的間隔。
const LABEL_GAP: u16 = 2;

/// 置中且夾在畫面內的矩形。
pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// 繪製不透明彈窗表面：清除底層 → 以 surface 底色鋪滿整塊 → 畫粉色邊框。
///
/// 回傳內框區域。所有彈窗（表單、設定、學期選擇器、登入覆蓋層）共用此契約，
/// 保證框線逐列連續且底層文字不會穿透；不可只依賴底層恰好沒有文字。
pub fn popup_surface(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    frame.render_widget(Clear, area);
    // 先以彈窗底色鋪滿整個矩形，再畫邊框。
    frame.render_widget(Block::default().style(THEME.surface_style()), area);
    let block = THEME.popup_block(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// 輸入框的可見內容與游標欄位（水平捲動，含寬字元）。
pub fn input_window(line: &InputLine, width: usize) -> (String, u16) {
    if width == 0 {
        return (String::new(), 0);
    }
    let graphemes = line.display_graphemes();
    let cursor = line.cursor().min(graphemes.len());
    let start = if cursor >= width {
        cursor + 1 - width
    } else {
        0
    };
    let end = (start + width).min(graphemes.len());

    let prefix: String = graphemes[start..cursor].concat();
    let visible: String = graphemes[start..end].concat();
    let column = Line::from(prefix).width() as u16;
    (visible, column)
}

/// 單行欄位的版面（以終端顯示欄為單位）。
struct FieldLayout {
    /// 標籤前綴的總寬度（含右對齊填充與間隔）。
    prefix: u16,
    /// 標籤左側的填充空白數。
    pad: u16,
    /// 值區域可用寬度。
    value: usize,
}

/// 以顯示寬度計算欄位版面：中文標籤的寬度是字元數的兩倍，
/// 因此補白、值區寬度與游標欄位都必須用同一套計算結果。
fn field_layout(area_width: u16, label: &str) -> FieldLayout {
    let label_width = u16::try_from(Line::from(label).width()).unwrap_or(u16::MAX);
    let pad = LABEL_WIDTH.saturating_sub(label_width);
    let prefix = label_width.saturating_add(pad).saturating_add(LABEL_GAP);

    FieldLayout {
        prefix,
        pad,
        value: usize::from(area_width.saturating_sub(prefix)),
    }
}

/// 繪製單行輸入欄位，並在聚焦時放置終端游標。
pub fn draw_field(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    line: &InputLine,
    focused: bool,
    placeholder: &str,
) -> Option<(u16, u16)> {
    let label_style = if focused {
        THEME.accent_style().add_modifier(Modifier::BOLD)
    } else {
        THEME.muted_style()
    };
    let layout = field_layout(area.width, label);
    let (visible, column) = input_window(line, layout.value);

    let value_span = if line.is_empty() && !focused {
        Span::styled(placeholder.to_owned(), THEME.muted_style())
    } else {
        Span::styled(visible, Style::default().fg(THEME.text))
    };

    let label_text = format!(
        "{}{}{}",
        " ".repeat(usize::from(layout.pad)),
        label,
        " ".repeat(usize::from(LABEL_GAP))
    );
    let text = Line::from(vec![Span::styled(label_text, label_style), value_span]);
    frame.render_widget(Paragraph::new(text).style(THEME.surface_style()), area);

    focused.then(|| {
        (
            area.x.saturating_add(layout.prefix).saturating_add(column),
            area.y,
        )
    })
}

/// 繪製表單彈窗；`note` 不為空時顯示在欄位上方（例如上一次的登入失敗原因）。
pub fn draw_form(frame: &mut Frame, form: &FormState, title: &str, hint: &str, note: Option<&str>) {
    let note_rows = u16::from(note.is_some()) * 2;
    let content_height = u16::try_from(form.fields.len()).unwrap_or(0) * 2 + note_rows + 6;
    let area = centered_rect(frame.area(), 76, content_height);
    let inner = popup_surface(frame, area, title);

    let mut constraints: Vec<Constraint> = Vec::new();
    if note.is_some() {
        constraints.push(Constraint::Length(2));
    }
    constraints.extend(form.fields.iter().map(|_| Constraint::Length(2)));
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(2));
    let chunks = Layout::vertical(constraints).split(inner);

    let offset = usize::from(note.is_some());
    if let Some(note) = note {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                note.to_owned(),
                THEME.error_style(),
            )))
            .style(THEME.surface_style())
            .wrap(Wrap { trim: true }),
            chunks[0],
        );
    }

    let mut cursor = None;
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        if let Some(position) = draw_field(
            frame,
            chunks[offset + index],
            field.label,
            &field.value,
            focused,
            "",
        ) {
            cursor = Some(position);
        }
    }

    let status_area = chunks[offset + form.fields.len()];
    let status = match (&form.error, form.busy) {
        (Some(error), _) => Line::from(Span::styled(error.clone(), THEME.error_style())),
        (None, true) => Line::from(Span::styled("正在处理…", THEME.muted_style())),
        (None, false) => Line::from(Span::styled("", THEME.muted_style())),
    };
    frame.render_widget(
        Paragraph::new(status)
            .style(THEME.surface_style())
            .wrap(Wrap { trim: true }),
        status_area,
    );

    let hint_area = chunks[offset + form.fields.len() + 1];
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint.to_owned(),
            THEME.muted_style(),
        )))
        .style(THEME.surface_style())
        .wrap(Wrap { trim: true }),
        hint_area,
    );

    if let Some((x, y)) = cursor {
        frame.set_cursor_position((x, y));
    }
}

/// 繪製登入互動彈窗。
pub fn draw_login(frame: &mut Frame, screen: &LoginScreen) {
    if let LoginScreen::Credentials { form, message } = screen {
        let note = format!("上次登录失败：{message}");
        draw_form(
            frame,
            form,
            "重新输入账号密码",
            "tab 切换字段 · enter 登录（成功后写入凭证） · esc 返回 · ^u 清空当前字段",
            Some(&note),
        );
        return;
    }

    let (title, lines, hint, field) = match screen {
        LoginScreen::Progress { note } => (
            "登录",
            vec![Line::from(Span::styled(note.clone(), THEME.muted_style()))],
            String::new(),
            None,
        ),
        LoginScreen::Failed { message } => (
            "登录失败",
            vec![Line::from(Span::styled(
                message.clone(),
                THEME.error_style(),
            ))],
            "enter 重试（使用已保存的账号密码） · e 重新输入账号密码 · esc 关闭 · q 退出"
                .to_owned(),
            None,
        ),
        LoginScreen::Captcha { path, error, input } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    "登录需要图片验证码。",
                    Style::default().fg(THEME.text),
                )),
                Line::from(Span::styled(
                    format!("验证码图片：{}", path.display()),
                    THEME.muted_style(),
                )),
                Line::from(Span::styled(
                    "可用图片查看器打开该文件后输入其中的字符。",
                    THEME.muted_style(),
                )),
            ];
            if let Some(error) = error {
                lines.push(Line::from(Span::styled(error.clone(), THEME.error_style())));
            }
            (
                "验证码",
                lines,
                "enter 提交 · r 换一张 · q 退出".to_owned(),
                Some(("验证码", input, "输入图片中的字符")),
            )
        }
        LoginScreen::Mfa {
            phone,
            sent,
            input,
            error,
        } => {
            let phone = phone.as_deref().unwrap_or("（未知号码）");
            let mut lines = vec![Line::from(Span::styled(
                format!("登录需要短信验证，绑定手机号：{phone}"),
                Style::default().fg(THEME.text),
            ))];
            if *sent {
                lines.push(Line::from(Span::styled(
                    "验证码已发送，请查看短信并输入。",
                    THEME.muted_style(),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "按 s 发送验证码。",
                    THEME.muted_style(),
                )));
            }
            if let Some(error) = error {
                lines.push(Line::from(Span::styled(error.clone(), THEME.error_style())));
            }
            (
                "短信验证",
                lines,
                "s 发送验证码 · enter 提交 · q 退出".to_owned(),
                Some(("短信验证码", input, "输入短信中的验证码")),
            )
        }
        LoginScreen::Credentials { .. } => return,
    };

    let field_rows = if field.is_some() { 2 } else { 0 };
    let area = centered_rect(
        frame.area(),
        84,
        u16::try_from(lines.len()).unwrap_or(1) + field_rows + 6,
    );
    let inner = popup_surface(frame, area, title);

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(field_rows),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.surface_style())
            .wrap(Wrap { trim: true }),
        chunks[0],
    );

    let mut cursor = None;
    if let Some((label, input, placeholder)) = field {
        cursor = draw_field(frame, chunks[1], label, input, true, placeholder);
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, THEME.muted_style())))
            .style(THEME.surface_style()),
        chunks[2],
    );

    if let Some((x, y)) = cursor {
        frame.set_cursor_position((x, y));
    }
}

/// 「终端太小」提示的文字：兩行白色文案，兩個數字依是否達到門檻上色
///（達標綠、未達標紅），讓使用者一眼看出是太窄還是太矮。
fn too_small_lines(width: u16, height: u16) -> Vec<Line<'static>> {
    let text = |content: &str| Span::styled(content.to_owned(), Style::default().fg(THEME.text));
    let number = |value: u16, min: u16| {
        let color = if value >= min { THEME.green } else { THEME.red };
        Span::styled(value.to_string(), Style::default().fg(color))
    };
    vec![
        Line::from(text("终端太小了:")),
        Line::from(vec![
            text("宽 = "),
            number(width, MIN_WIDTH),
            text("  高 = "),
            number(height, MIN_HEIGHT),
        ]),
    ]
}

/// 尺寸不足時顯示提示，回傳是否可以直接結束繪製。
///
/// 提示上下左右都置中：先以底色鋪滿整個畫面，再於垂直置中的文字帶上繪製兩行。
/// 所有畫面共用同一門檻（由 `views::draw` 在最上層呼叫）。
pub fn too_small(frame: &mut Frame) -> bool {
    let area = frame.area();
    if area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT {
        return false;
    }

    let lines = too_small_lines(area.width, area.height);
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let band = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: area.width,
        height,
    };

    frame.render_widget(Block::default().style(THEME.base_style()), area);
    frame.render_widget(
        Paragraph::new(lines).centered().style(THEME.base_style()),
        band,
    );
    true
}

#[cfg(test)]
#[path = "tests/ui_test.rs"]
mod ui_test;
