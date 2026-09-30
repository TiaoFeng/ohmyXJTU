//! 用户协议覆蓋層：滿版顯示協議全文，讀到底部後按 enter 才能同意。
//!
//! 排版由 [`crate::privacy`] 完成（輕量 Markdown 整理＋顯示寬度換行），
//! 本模組只負責上色與版面：內文獨占畫面，頁腳顯示閱讀進度、確認按鈕與
//! 保存錯誤。呼叫端（[`super::draw`]）已確保此畫面開啟時不繪製底層。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::privacy::{self, DocLine, LineKind};
use crate::tui::app::AgreementState;
use crate::tui::text::truncate_display;
use crate::tui::theme::THEME;
use crate::tui::ui::popup_surface;

/// 依列種類決定文字樣式。
fn line_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Title => THEME.accent_style().add_modifier(Modifier::BOLD),
        LineKind::Heading => THEME.title_style(),
        LineKind::Quote => THEME.muted_style(),
        LineKind::Body | LineKind::Bullet | LineKind::Table | LineKind::Rule => {
            Style::default().fg(THEME.text)
        }
    }
}

/// 將文件列轉為可繪製的列（分隔線依寬度鋪滿）。
fn rendered_line(line: &DocLine, width: usize) -> Line<'static> {
    if line.kind == LineKind::Rule {
        Line::styled("─".repeat(width), THEME.muted_style())
    } else {
        Line::styled(line.text.clone(), line_style(line.kind))
    }
}

/// 繪製協議閱讀門（滿版、不透明）。
pub fn draw(frame: &mut Frame, state: &mut AgreementState) {
    let title = format!("用户协议（ohmyXJTU）v{}", privacy::VERSION);
    let inner = popup_surface(frame, frame.area(), &title);

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    let text_area = chunks[0];
    let footer_area = chunks[1];

    let width = usize::from(text_area.width);
    let wrapped = privacy::wrap(privacy::document(), width);
    let total = wrapped.len();
    let lines: Vec<Line<'static>> = wrapped
        .iter()
        .map(|line| rendered_line(line, width))
        .collect();

    let paragraph = Paragraph::new(lines)
        .style(THEME.surface_style())
        .scroll((state.scroll(), 0));
    frame.render_widget(paragraph, text_area);

    // 回寫版面資訊：夾取捲動位置並更新「已讀到底部」。
    state.sync_layout(text_area.height, u16::try_from(total).unwrap_or(u16::MAX));

    draw_footer(frame, footer_area, state);
}

/// 頁腳：狀態列與按鍵提示。
fn draw_footer(frame: &mut Frame, area: Rect, state: &AgreementState) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let width = usize::from(area.width);

    let (status, style) = if let Some(error) = &state.error {
        (
            format!("保存失败：{error}（按 enter 重试）"),
            THEME.error_style(),
        )
    } else if state.saving {
        ("正在保存…".to_owned(), THEME.muted_style())
    } else if state.can_confirm() {
        ("[ 同意并继续 ]".to_owned(), THEME.highlight_style())
    } else {
        (
            format!("请阅读至最底部（已读 {}%）", state.progress()),
            THEME.muted_style(),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::styled(truncate_display(&status, width), style))
            .style(THEME.surface_style()),
        rows[0],
    );

    let hint = if state.saving {
        "请稍候…"
    } else if state.can_confirm() {
        "enter 同意并继续 · esc 不同意并退出"
    } else {
        "↓/j 下一行 · PgDn 翻页 · End 到底部 · esc 退出"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(hint, THEME.muted_style())).style(THEME.surface_style()),
        rows[1],
    );
}
