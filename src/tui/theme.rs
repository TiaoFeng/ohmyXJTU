//! TUI 配色與樣式。
//!
//! 深色底、桃色高亮、藍色標題（opencode 風格），另外針對考勤與作業狀態
//! 定義語意色：正常為綠、遲到為黃、缺勤為紅、請假為藍、不確定為黃。

use ratatui::style::{Color, Modifier, Style};

/// 顏色主題。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// 視窗底色。
    pub base: Color,
    /// 彈窗底色。
    pub surface: Color,
    /// 邊框。
    pub border: Color,
    /// 一般文字。
    pub text: Color,
    /// 次要文字。
    pub muted: Color,
    /// 高亮（整列反白）。
    pub accent: Color,
    /// 標題。
    pub blue: Color,
    /// 正常／成功。
    pub green: Color,
    /// 錯誤。
    pub red: Color,
    /// 警告。
    pub yellow: Color,
}

/// 全域主題。
pub const THEME: Theme = Theme {
    base: Color::Rgb(20, 20, 20),
    surface: Color::Rgb(30, 30, 30),
    border: Color::Rgb(100, 100, 100),
    text: Color::Rgb(240, 240, 240),
    muted: Color::Rgb(150, 150, 150),
    accent: Color::Rgb(245, 169, 184),
    blue: Color::Rgb(91, 206, 250),
    green: Color::Rgb(120, 190, 32),
    red: Color::Rgb(203, 51, 59),
    yellow: Color::Rgb(255, 199, 44),
};

impl Theme {
    /// 一般底色。
    pub fn base_style(self) -> Style {
        Style::default().bg(self.base).fg(self.text)
    }

    /// 彈窗底色。
    pub fn surface_style(self) -> Style {
        Style::default().bg(self.surface).fg(self.text)
    }

    /// 次要文字。
    pub fn muted_style(self) -> Style {
        Style::default().fg(self.muted)
    }

    /// 面板標題。
    pub fn title_style(self) -> Style {
        Style::default().fg(self.blue).add_modifier(Modifier::BOLD)
    }

    /// 選取列。
    pub fn highlight_style(self) -> Style {
        Style::default()
            .bg(self.accent)
            .fg(self.base)
            .add_modifier(Modifier::BOLD)
    }

    /// 強調文字。
    pub fn accent_style(self) -> Style {
        Style::default().fg(self.accent)
    }

    /// 錯誤文字。
    pub fn error_style(self) -> Style {
        Style::default().fg(self.red)
    }

    /// 狀態標籤的顏色。
    pub fn status_style(self, label: &str) -> Style {
        let color = match label {
            "正常" => self.green,
            "迟到" => self.yellow,
            "缺勤" | "逾期" => self.red,
            "请假" => self.blue,
            "待考勤" | "待提交" => self.accent,
            "不考勤" => self.muted,
            _ => self.yellow,
        };
        Style::default().fg(color)
    }

    /// 面板外框（統一風格）。
    pub fn block(self, title: impl Into<String>) -> ratatui::widgets::Block<'static> {
        let title = title.into();
        ratatui::widgets::Block::bordered()
            .title(ratatui::text::Span::styled(
                format!(" {title} "),
                self.title_style(),
            ))
            .border_style(Style::default().fg(self.border))
            .style(self.base_style())
    }

    /// 彈窗外框。
    pub fn popup_block(self, title: impl Into<String>) -> ratatui::widgets::Block<'static> {
        let title = title.into();
        ratatui::widgets::Block::bordered()
            .title(ratatui::text::Span::styled(
                format!(" {title} "),
                self.title_style(),
            ))
            .border_style(Style::default().fg(self.accent))
            .style(self.surface_style())
    }
}
