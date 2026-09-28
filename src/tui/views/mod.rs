//! 畫面繪製。

pub mod content;
pub mod main_view;
pub mod settings;
pub mod term_picker;

use ratatui::Frame;

use crate::tui::app::{App, Screen};

/// 依目前畫面繪製。
pub fn draw(frame: &mut Frame, app: &mut App) {
    match &app.screen {
        Screen::Setup(form) => {
            crate::tui::ui::draw_form(
                frame,
                form,
                "首次使用：设置加密口令与账号",
                "tab 切换字段 · enter 保存并登录 · ^u 清空当前字段 · ^c 退出",
                None,
            );
        }
        Screen::Unlock(form) => {
            crate::tui::ui::draw_form(
                frame,
                form,
                "解锁凭证",
                "enter 解锁 · ^c 退出（凭证以加密方式保存在本地，不会明文存储密码）",
                None,
            );
        }
        Screen::Login(screen) => crate::tui::ui::draw_login(frame, screen),
        Screen::SettingsForm(form) => {
            let title = match form.kind {
                crate::tui::app::FormKind::ChangeAccount => "修改账号",
                _ => "修改加密口令",
            };
            crate::tui::ui::draw_form(
                frame,
                form,
                title,
                "tab 切换字段 · enter 保存（需先验证原口令）· esc 返回",
                None,
            );
        }
        Screen::Main | Screen::Settings(_) | Screen::TermPicker(_) => main_view::draw(frame, app),
    }
}
