//! 畫面層業務邏輯：由使用者操作驅動的任務建構與頁面導航。
//!
//! 與 [`super::handler`] 的分工：handler 只做「按鍵 → 意圖」的映射；凡會
//! 建構 [`Job`] 或切換頁面／資源的邏輯都在這裡。事件套用（[`super::event`]）
//! 也共用這裡的 [`request`] 與 [`ensure_page`]。

use std::sync::mpsc::Sender;

use crate::credentials::{Credentials, Secret};
use crate::sites::lms::ActivityKind;
use crate::task::Job;
use crate::tui::app::{
    App, FieldRole, FormKind, FormState, LmsLevel, NavItem, Screen, TermPickerState,
};

/// 加密口令的最短長度。
///
/// 僅適用於新設置的口令（首次設定、修改口令）；解鎖既有憑證不檢查長度，
/// 以免舊使用者的短口令被鎖死。
pub(super) const MIN_PASSPHRASE_LEN: usize = 8;

/// 首次進入尚未載入的頁面時自動查詢。
pub(super) fn ensure_page(app: &mut App, jobs: &Sender<Job>) {
    let needs_load = match app.nav {
        NavItem::Schedule => app.schedule.is_idle(),
        NavItem::Homework => app.homework.is_idle(),
        NavItem::Attendance => app.attendance.is_idle(),
        NavItem::Lms => app.lms.courses.is_idle(),
    };
    if needs_load {
        request(app, jobs, app.nav, false);
    }
}

/// 重新查詢指定頁面；`force` 為真時略過快取（使用者按 `r`）。
pub(super) fn request(app: &mut App, jobs: &Sender<Job>, nav: NavItem, force: bool) {
    match nav {
        NavItem::Schedule => {
            app.schedule.start_loading("正在加载课表与考勤记录…");
            let _ = jobs.send(Job::LoadSchedule);
        }
        NavItem::Homework => {
            app.homework
                .start_loading("正在汇总作业（需要逐门课程查询）…");
            let _ = jobs.send(Job::LoadHomework { force });
        }
        NavItem::Attendance => {
            let page = app.attendance.ready().map_or(1, |data| data.page);
            app.attendance.start_loading("正在加载考勤流水…");
            let _ = jobs.send(Job::LoadFlow { page });
        }
        NavItem::Lms => {
            app.lms.courses.start_loading("正在加载课程…");
            app.lms.level = LmsLevel::Courses;
            let _ = jobs.send(Job::LoadCourses { force });
        }
    }
}

/// 開啟目前選取項目的內容（`enter`）：展開詳情或進入下一層。
pub(super) fn activate(app: &mut App, jobs: &Sender<Job>) {
    match app.nav {
        NavItem::Schedule => app.schedule_detail = !app.schedule_detail,
        NavItem::Homework => {
            app.homework_detail = !app.homework_detail;
            // 展開或收起都回到頂端：下次展開時從標題開始讀。
            app.homework_scroll.reset();
        }
        NavItem::Attendance => app.flow_detail = !app.flow_detail,
        NavItem::Lms => match app.lms.level {
            LmsLevel::Courses => {
                let selected = app.page_selection();
                let Some(course_id) = app
                    .lms
                    .courses
                    .ready()
                    .and_then(|courses| courses.get(selected))
                    .map(|course| course.id.clone())
                else {
                    return;
                };
                app.lms.course_index = selected;
                // 換到不同課程時不得沿用前一門課的活動：舊資料屬於別的課程，
                // 在新回應抵達前顯示它，會讓使用者以為看到的是本課程的內容。
                if app.lms.activities_course.as_deref() == Some(course_id.as_str()) {
                    app.lms.activities.start_loading("正在加载课程活动…");
                } else {
                    app.lms.activities.reset_loading("正在加载课程活动…");
                }
                app.lms.activities_course = Some(course_id.clone());
                app.lms.level = LmsLevel::Activities;
                app.activity_state.select(Some(0));
                let _ = jobs.send(Job::LoadActivities {
                    course_id,
                    force: false,
                });
            }
            LmsLevel::Activities => {
                let selected = app.page_selection();
                let (activity_id, kind) = {
                    let items = app.lms_activities_in_group();
                    let Some(activity) = items.get(selected) else {
                        return;
                    };
                    (activity.id.clone(), activity.kind())
                };
                app.lms.activity_index = selected;
                // 同理：換活動時不得沿用上一個活動的詳情。
                let note = if kind == ActivityKind::Homework {
                    "正在加载活动详情与提交记录…"
                } else {
                    "正在加载活动详情…"
                };
                if app.lms.detail_activity.as_deref() == Some(activity_id.as_str()) {
                    app.lms.detail.start_loading(note);
                } else {
                    app.lms.detail.reset_loading(note);
                }
                app.lms.detail_activity = Some(activity_id.clone());
                app.lms.level = LmsLevel::Detail;
                app.lms.detail_scroll.reset();
                let _ = jobs.send(Job::LoadActivityDetail { activity_id });
            }
            LmsLevel::Detail => {}
        },
    }
}

/// 返回上一層（`esc`）：思源學堂逐層返回，其餘頁面收起詳情。
pub(super) fn escape(app: &mut App) {
    match app.nav {
        NavItem::Lms => match app.lms.level {
            LmsLevel::Detail => app.lms.level = LmsLevel::Activities,
            LmsLevel::Activities => app.lms.level = LmsLevel::Courses,
            LmsLevel::Courses => {}
        },
        NavItem::Schedule => app.schedule_detail = false,
        NavItem::Homework => app.homework_detail = false,
        NavItem::Attendance => app.flow_detail = false,
    }
}

/// 切換分組（`[`／`]`）：作業頁切作業分組、思源學堂活動頁切活動分組。
pub(super) fn change_group(app: &mut App, delta: i32) {
    if app.nav == NavItem::Homework {
        app.homework_group = if delta < 0 {
            app.homework_group.previous()
        } else {
            app.homework_group.next()
        };
        // 切換分組後重設選取，避免索引越界。
        app.set_selection(0);
        app.homework_scroll.reset();
        return;
    }
    if app.nav == NavItem::Lms && app.lms.level == LmsLevel::Activities {
        app.lms.activity_group = if delta < 0 {
            app.lms.activity_group.previous()
        } else {
            app.lms.activity_group.next()
        };
        app.set_selection(0);
    }
}

/// 詳情內容的捲動指令（PgUp／PgDn／Home／End）。
#[derive(Clone, Copy)]
pub(super) enum DetailScroll {
    /// 往上捲一頁。
    PageUp,
    /// 往下捲一頁。
    PageDown,
    /// 捲到頂端。
    Top,
    /// 捲到底端。
    Bottom,
}

/// 捲動目前頁面的詳情內容；沒有可捲動的詳情時不動作。
pub(super) fn scroll_detail(app: &mut App, command: DetailScroll) {
    let state = if app.nav == NavItem::Homework && app.homework_detail {
        &mut app.homework_scroll
    } else if app.nav == NavItem::Lms && app.lms.level == LmsLevel::Detail {
        &mut app.lms.detail_scroll
    } else {
        return;
    };
    match command {
        DetailScroll::PageUp => state.page(-1),
        DetailScroll::PageDown => state.page(1),
        DetailScroll::Top => state.to_top(),
        DetailScroll::Bottom => state.to_bottom(),
    }
}

/// 考勤流水分頁（`n`／`p`）；超出頁數範圍時不動作。
pub(super) fn change_flow_page(app: &mut App, jobs: &Sender<Job>, delta: i32) {
    if app.nav != NavItem::Attendance {
        return;
    }
    let (page, total_pages) = match app.attendance.ready() {
        Some(data) => (data.page, data.total_pages),
        None => return,
    };
    let target = i32::try_from(page).unwrap_or(1) + delta;
    if target < 1 || target > i32::try_from(total_pages).unwrap_or(1) {
        return;
    }

    let target = u32::try_from(target).unwrap_or(1);
    app.attendance
        .start_loading(format!("正在加载第 {target} 页…"));
    let _ = jobs.send(Job::LoadFlow { page: target });
}

/// 開啟目前選取項目的網頁（`o`）：作業頁與思源學堂活動／詳情層。
pub(super) fn open_activity(app: &mut App, jobs: &Sender<Job>) {
    match app.nav {
        NavItem::Homework => open_homework(app, jobs),
        NavItem::Lms => open_lms_activity(app, jobs),
        _ => {}
    }
}

/// 作業頁：開啟目前選取作業所屬課程的作業列表（前端網址由工作執行緒組出）。
fn open_homework(app: &mut App, jobs: &Sender<Job>) {
    let target = app.homework.ready().and_then(|data| {
        data.group_items(app.homework_group)
            .get(app.page_selection())
            .map(|item| (item.activity_id.clone(), item.course_id.clone()))
    });
    let Some((activity_id, course_id)) = target else {
        app.set_message("请先选择要打开的作业");
        return;
    };
    app.set_message("正在打开作业网页…");
    let _ = jobs.send(Job::OpenActivity {
        activity_id,
        course_id: Some(course_id),
        kind: ActivityKind::Homework,
    });
}

/// 思源學堂頁：開啟目前活動的網頁（活動與詳情層）；附上所在課程識別碼。
fn open_lms_activity(app: &mut App, jobs: &Sender<Job>) {
    let course_id = app
        .lms
        .courses
        .ready()
        .and_then(|courses| courses.get(app.lms.course_index))
        .map(|course| course.id.clone());
    let target = match app.lms.level {
        LmsLevel::Activities => {
            let selected = app.page_selection();
            app.lms_activities_in_group()
                .get(selected)
                .map(|activity| (activity.id.clone(), activity.kind()))
        }
        LmsLevel::Detail => app
            .lms
            .detail
            .ready()
            .map(|detail| (detail.id.clone(), detail.kind)),
        LmsLevel::Courses => None,
    };
    let Some((activity_id, kind)) = target else {
        app.set_message("请先选择要打开的活动");
        return;
    };
    app.set_message("正在解析活动网页…");
    let _ = jobs.send(Job::OpenActivity {
        activity_id,
        course_id,
        kind,
    });
}

/// 開啟學期選擇器（作業頁按 `s`）。
pub(super) fn open_term_picker(app: &mut App) {
    if app.nav != NavItem::Homework {
        return;
    }
    if app.term_options.is_empty() {
        app.set_message("尚无可选学期：请先加载一次作业列表");
        return;
    }
    let state = TermPickerState::new(
        app.term_options.clone(),
        None,
        "选择要查看的学期".to_owned(),
    );
    app.set_screen(Screen::TermPicker(state));
}

/// 送出表單；驗證失敗時就地顯示錯誤，不送出任務。
pub(super) fn submit_form(app: &mut App, jobs: &Sender<Job>) {
    let job = {
        let Some(form) = app.form_mut() else {
            return;
        };
        let values = FormValues::from_form(form);
        match build_job(form.kind, &values) {
            Ok(job) => job,
            Err(message) => {
                form.error = Some(message);
                return;
            }
        }
    };

    // 憑證操作：留在原表單顯示處理中；成功與失敗都由事件回到表單或主畫面。
    if matches!(
        app.screen,
        Screen::Setup(_) | Screen::Unlock(_) | Screen::SettingsForm(_)
    ) && let Some(form) = app.form_mut()
    {
        form.busy = true;
        form.error = None;
    }
    let _ = jobs.send(job);
}

/// 送出重新輸入的憑證；驗證失敗時把訊息寫回表單，不送出任務。
pub(super) fn submit_login_credentials(app: &mut App, jobs: &Sender<Job>) {
    let job = {
        let Some(form) = app.form_mut() else {
            return;
        };
        let values = FormValues::from_form(form);
        match build_job(form.kind, &values) {
            Ok(job) => {
                form.busy = true;
                form.error = None;
                job
            }
            Err(message) => {
                form.error = Some(message);
                return;
            }
        }
    };

    let _ = jobs.send(job);
}

/// 將表單內容轉為任務；驗證失敗時回傳訊息。
fn build_job(kind: FormKind, values: &FormValues) -> Result<Job, String> {
    match kind {
        FormKind::Setup => {
            validate_passphrase(&values.passphrase, &values.passphrase_confirm)?;
            if values.username.trim().is_empty() {
                return Err("账号不能为空".to_owned());
            }
            if values.password.is_empty() {
                return Err("密码不能为空".to_owned());
            }
            if values.password != values.password_confirm {
                return Err("两次输入的密码不一致".to_owned());
            }
            Ok(Job::CreateVault {
                passphrase: values.passphrase.clone(),
                credentials: Credentials::new(values.username.trim(), values.password.as_str()),
            })
        }
        FormKind::Unlock => {
            if values.passphrase.is_empty() {
                return Err("请输入加密口令".to_owned());
            }
            Ok(Job::Unlock {
                passphrase: values.passphrase.clone(),
            })
        }
        FormKind::LoginRetry(site) => {
            if values.username.trim().is_empty() {
                return Err("账号不能为空".to_owned());
            }
            if values.password.is_empty() {
                return Err("密码不能为空".to_owned());
            }
            if values.passphrase.is_empty() {
                return Err("请输入加密口令".to_owned());
            }
            Ok(Job::RetryWithAccount {
                site,
                credentials: Credentials::new(values.username.trim(), values.password.as_str()),
                passphrase: values.passphrase.clone(),
            })
        }
        FormKind::ChangeAccount => {
            if values.passphrase.is_empty() {
                return Err("请输入原加密口令".to_owned());
            }
            if values.username.trim().is_empty() {
                return Err("新账号不能为空".to_owned());
            }
            if values.password.is_empty() {
                return Err("新密码不能为空".to_owned());
            }
            if values.password != values.password_confirm {
                return Err("两次输入的密码不一致".to_owned());
            }
            Ok(Job::ChangeAccount {
                passphrase: values.passphrase.clone(),
                credentials: Credentials::new(values.username.trim(), values.password.as_str()),
            })
        }
        FormKind::ChangePassphrase => {
            if values.passphrase.is_empty() {
                return Err("请输入原加密口令".to_owned());
            }
            validate_passphrase(&values.password, &values.password_confirm)?;
            Ok(Job::ChangePassphrase {
                old: values.passphrase.clone(),
                new: values.password.clone(),
            })
        }
    }
}

fn validate_passphrase(passphrase: &str, confirm: &str) -> Result<(), String> {
    if passphrase.chars().count() < MIN_PASSPHRASE_LEN {
        return Err(format!("加密口令至少需要 {MIN_PASSPHRASE_LEN} 个字符"));
    }
    if passphrase != confirm {
        return Err("两次输入的加密口令不一致".to_owned());
    }
    Ok(())
}

/// 表單各欄位的值（依表單種類對應位置）。
#[derive(Default)]
pub(super) struct FormValues {
    /// 加密口令（自動零化）。
    passphrase: Secret,
    /// 確認加密口令（自動零化）。
    passphrase_confirm: Secret,
    /// 帳號（非機密，但一律不進 `Debug`）。
    username: String,
    /// 密碼（自動零化）。
    password: Secret,
    /// 確認密碼（自動零化）。
    password_confirm: Secret,
}

impl std::fmt::Debug for FormValues {
    /// 任何 `{:?}` 都只輸出欄位是否有值，不含帳號、密碼或口令內容。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FormValues")
            .field("passphrase", &!self.passphrase.is_empty())
            .field("passphrase_confirm", &!self.passphrase_confirm.is_empty())
            .field("username", &!self.username.is_empty())
            .field("password", &!self.password.is_empty())
            .field("password_confirm", &!self.password_confirm.is_empty())
            .finish()
    }
}

impl FormValues {
    /// 依欄位角色取值；欄位順序與標籤的調整不影響對應。
    pub(super) fn from_form(form: &FormState) -> Self {
        let mut values = Self::default();
        for field in &form.fields {
            let text = field.value.value();
            match field.role {
                FieldRole::Passphrase | FieldRole::OldPassphrase => {
                    values.passphrase = Secret::from(text);
                }
                FieldRole::PassphraseConfirm => {
                    values.passphrase_confirm = Secret::from(text);
                }
                FieldRole::Username | FieldRole::NewUsername => {
                    values.username = text.to_owned();
                }
                FieldRole::Password | FieldRole::NewPassword | FieldRole::NewPassphrase => {
                    values.password = Secret::from(text);
                }
                FieldRole::PasswordConfirm
                | FieldRole::NewPasswordConfirm
                | FieldRole::NewPassphraseConfirm => {
                    values.password_confirm = Secret::from(text);
                }
            }
        }
        values
    }
}

#[cfg(test)]
#[path = "tests/controller_test.rs"]
mod controller_test;
