//! 畫面繪製。

pub mod agreement;
pub mod content;
pub mod main_view;
pub mod settings;
pub mod task_form;
pub mod task_menu;
pub mod term_picker;

use ratatui::Frame;

use crate::tui::app::{App, Screen};

/// 依目前畫面繪製。
///
/// 終端小於最小尺寸時，所有畫面（含啟動時的解鎖／首次設定表單與登入覆蓋層）
/// 一律改為顯示置中的放大提示：表單在過小的終端會被裁切而無法操作。
pub fn draw(frame: &mut Frame, app: &mut App) {
    if crate::tui::ui::too_small(frame) {
        return;
    }

    // 用户协议閱讀門：獨占畫面（不繪製底層，避免底下表單設定的游標
    // 位置殘留在協議內容上）。
    if let Some(state) = app.agreement.as_mut() {
        agreement::draw(frame, state);
        return;
    }

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
        Screen::Main
        | Screen::Sort
        | Screen::Settings(_)
        | Screen::TermPicker(_)
        | Screen::TaskMenu(_)
        | Screen::TaskBatchMenu(_)
        | Screen::TaskConfirm(_)
        | Screen::TaskForm(_) => main_view::draw(frame, app),
    }

    // 登入互動覆蓋層：底層（表單或主畫面）先完整繪製，彈窗只覆蓋自身區域，
    // 避免整屏清空造成黑底與閃爍。
    if let Some(screen) = &app.login {
        crate::tui::ui::draw_login(frame, screen);
    }
}
