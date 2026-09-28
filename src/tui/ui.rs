//! 共用繪製工具：置中彈窗、輸入框、提示列與尺寸守衛。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use crate::tui::app::{FormState, LoginScreen};
use crate::tui::text::InputLine;
use crate::tui::theme::THEME;

/// 視窗最小寬度。
pub const MIN_WIDTH: u16 = 64;
/// 視窗最小高度。
pub const MIN_HEIGHT: u16 = 18;
/// 表單標籤欄寬度。
const LABEL_WIDTH: u16 = 14;

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
    let value_width = area.width.saturating_sub(LABEL_WIDTH + 2) as usize;
    let (visible, column) = input_window(line, value_width);

    let value_span = if line.is_empty() && !focused {
        Span::styled(placeholder.to_owned(), THEME.muted_style())
    } else {
        Span::styled(visible, Style::default().fg(THEME.text))
    };

    let label_text = format!("{:>width$}  ", label, width = usize::from(LABEL_WIDTH));
    let text = Line::from(vec![Span::styled(label_text, label_style), value_span]);
    frame.render_widget(Paragraph::new(text).style(THEME.surface_style()), area);

    focused.then(|| (area.x + LABEL_WIDTH + 2 + column, area.y))
}

/// 繪製表單彈窗。
pub fn draw_form(frame: &mut Frame, form: &FormState, title: &str, hint: &str) {
    let content_height = u16::try_from(form.fields.len()).unwrap_or(0) * 2 + 6;
    let area = centered_rect(frame.area(), 76, content_height);
    frame.render_widget(Clear, area);

    let block = THEME.popup_block(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut constraints: Vec<Constraint> =
        form.fields.iter().map(|_| Constraint::Length(2)).collect();
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(2));
    let chunks = Layout::vertical(constraints).split(inner);

    let mut cursor = None;
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        if let Some(position) =
            draw_field(frame, chunks[index], field.label, &field.value, focused, "")
        {
            cursor = Some(position);
        }
    }

    let status_area = chunks[form.fields.len()];
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

    let hint_area = chunks[form.fields.len() + 1];
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
    let (title, lines, hint) = match screen {
        LoginScreen::Progress { note } => (
            "登录",
            vec![Line::from(Span::styled(note.clone(), THEME.muted_style()))],
            String::new(),
        ),
        LoginScreen::Failed { message } => (
            "登录失败",
            vec![Line::from(Span::styled(
                message.clone(),
                THEME.error_style(),
            ))],
            "enter 重试（重新输入账号密码） · q 退出".to_owned(),
        ),
        LoginScreen::Captcha { path, error, .. } => {
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
                ("验证码"),
                lines,
                "enter 提交 · r 换一张 · q 退出".to_owned(),
            )
        }
        LoginScreen::Mfa {
            phone, sent, error, ..
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
                ("短信验证"),
                lines,
                "s 发送验证码 · enter 提交 · q 退出".to_owned(),
            )
        }
    };

    let area = centered_rect(
        frame.area(),
        84,
        u16::try_from(lines.len()).unwrap_or(1) + 6,
    );
    frame.render_widget(Clear, area);
    let block = THEME.popup_block(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    frame.render_widget(
        Paragraph::new(lines)
            .style(THEME.surface_style())
            .wrap(Wrap { trim: true }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, THEME.muted_style())))
            .style(THEME.surface_style()),
        chunks[1],
    );
}

/// 尺寸不足時顯示提示，回傳是否可以直接結束繪製。
pub fn too_small(frame: &mut Frame) -> bool {
    let area = frame.area();
    if area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT {
        return false;
    }
    let message = format!(
        "终端窗口过小：当前 {}x{}，至少需要 {}x{}",
        area.width, area.height, MIN_WIDTH, MIN_HEIGHT
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(message, THEME.error_style())))
            .style(THEME.base_style()),
        area,
    );
    true
}
