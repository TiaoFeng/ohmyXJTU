//! 按鍵處理：把終端事件轉成狀態變更與背景任務。

use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::credentials::{Credentials, Secret};
use crate::session::SiteKind;
use crate::sites::lms::ActivityKind;
use crate::task::Job;
use crate::tui::app::{
    App, FormKind, FormState, LmsLevel, LoginScreen, NavItem, Screen, SettingsState,
    TermPickerState,
};
use crate::tui::text::InputLine;

/// 加密口令的最短長度。
///
/// 僅適用於新設置的口令（首次設定、修改口令）；解鎖既有憑證不檢查長度，
/// 以免舊使用者的短口令被鎖死。
const MIN_PASSPHRASE_LEN: usize = 8;

/// 登入畫面的動作。
enum LoginAction {
    Quit,
    Retry(SiteKind),
    EditAccount(SiteKind),
    BackToFailed(SiteKind),
    /// 關閉登入覆蓋層（回到底層畫面，可繼續按 `r` 刷新等操作）。
    Dismiss,
    SubmitCredentials,
    SubmitCaptcha(String),
    RefreshCaptcha,
    SendMfaCode,
    VerifyMfaCode(String),
}

/// 處理單一按鍵事件。
pub fn handle_key(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    // Ctrl+C 一律可退出（含協議閱讀門與登入覆蓋層）。
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    // 用户协议閱讀門開啟時攔截其餘所有按鍵（含 Ctrl+P／Ctrl+U），
    // 底層畫面與表單完全隔離。
    if app.agreement.is_some() {
        handle_agreement(app, key, jobs);
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('p') => {
                toggle_settings(app);
                return;
            }
            KeyCode::Char('u') => {
                if let Some(form) = form_mut(app)
                    && let Some(field) = form.focused_mut()
                {
                    field.value.clear();
                }
                return;
            }
            _ => {}
        }
    }

    // 登入覆蓋層開啟時（進度、驗證碼、簡訊、失敗、重新輸入憑證），
    // 所有按鍵都交給登入畫面處理，底層畫面維持不變。
    if app.login.is_some() {
        handle_login(app, key, jobs);
        return;
    }

    match app.screen {
        Screen::Setup(_) | Screen::Unlock(_) | Screen::SettingsForm(_) => {
            handle_form(app, key, jobs);
        }
        Screen::Settings(_) => handle_settings(app, key, jobs),
        Screen::TermPicker(_) => handle_term_picker(app, key, jobs),
        Screen::Main => handle_main(app, key, jobs),
    }
}

/// 處理貼上事件：把文字插入目前聚焦的輸入欄位（含登入覆蓋層與各表單）。
///
/// 單行輸入不接受換行；貼上的換行與回車一律忽略，避免貼上內容意外送出表單。
pub fn handle_paste(app: &mut App, text: &str) {
    // 協議閱讀門開啟時不接受輸入。
    if app.agreement.is_some() {
        return;
    }
    // 登入覆蓋層優先於底層表單。
    if let Some(screen) = app.login.as_mut() {
        match screen.as_mut() {
            LoginScreen::Captcha { input, .. } | LoginScreen::Mfa { input, .. } => {
                insert_text(input, text);
            }
            LoginScreen::Credentials { form, .. } => {
                if let Some(field) = form.focused_mut() {
                    insert_text(&mut field.value, text);
                }
            }
            _ => {}
        }
        return;
    }
    if let Some(form) = form_mut(app)
        && let Some(field) = form.focused_mut()
    {
        insert_text(&mut field.value, text);
    }
}

/// 把貼上的文字插入單行輸入框（忽略換行）。
fn insert_text(line: &mut InputLine, text: &str) {
    for character in text
        .chars()
        .filter(|character| !matches!(character, '\n' | '\r'))
    {
        line.insert(character);
    }
}

/// `Ctrl+P`：開啟或關閉帳戶設定（登入覆蓋層或協議閱讀門開啟時不生效）。
fn toggle_settings(app: &mut App) {
    if app.login.is_some() || app.agreement.is_some() {
        return;
    }
    match app.screen {
        Screen::Settings(_) | Screen::TermPicker(_) => app.set_screen(Screen::Main),
        Screen::Main => app.set_screen(Screen::Settings(SettingsState::open(app.access_policy))),
        _ => {}
    }
}

// ── 用户协议 ─────────────────────────────────────────

/// 用户协议閱讀門：捲動、翻頁、同意（需讀到底部）與退出。
fn handle_agreement(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    // 保存中：忽略所有輸入，避免重複提交。
    if app.agreement.as_ref().is_some_and(|state| state.saving) {
        return;
    }

    match key.code {
        // 不同意：直接退出，不記錄任何同意狀態。
        KeyCode::Esc | KeyCode::Char('q') => app.quit = true,
        KeyCode::Up | KeyCode::Char('k') => scroll_agreement(app, -1),
        KeyCode::Down | KeyCode::Char('j') => scroll_agreement(app, 1),
        KeyCode::PageUp => page_agreement(app, -1),
        KeyCode::PageDown | KeyCode::Char(' ') => page_agreement(app, 1),
        KeyCode::Home | KeyCode::Char('g') => {
            if let Some(state) = app.agreement.as_mut() {
                state.to_top();
            }
        }
        KeyCode::End | KeyCode::Char('G') => {
            if let Some(state) = app.agreement.as_mut() {
                state.to_bottom();
            }
        }
        KeyCode::Enter | KeyCode::Char('\n') => {
            let Some(state) = app.agreement.as_mut() else {
                return;
            };
            // 未讀到底部前 enter 不作用（頁腳會顯示原因）。
            if state.can_confirm() {
                state.start_saving();
                let _ = jobs.send(Job::AcceptAgreement);
            }
        }
        _ => {}
    }
}

/// 協議逐行捲動（正為向下）。
fn scroll_agreement(app: &mut App, delta: i32) {
    if let Some(state) = app.agreement.as_mut() {
        state.scroll_by(delta);
    }
}

/// 協議翻頁（`pages` 為 +1／-1）。
fn page_agreement(app: &mut App, pages: i32) {
    if let Some(state) = app.agreement.as_mut() {
        state.page_by(pages);
    }
}

// ── 表單 ─────────────────────────────────────────────

fn handle_form(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    // 送出中：忽略所有輸入，避免重複提交。
    if form_mut(app).is_some_and(|form| form.busy) {
        return;
    }
    let kind = form_mut(app).map(|form| form.kind);

    match key.code {
        KeyCode::Esc => {
            if matches!(
                kind,
                Some(FormKind::ChangeAccount | FormKind::ChangePassphrase)
            ) {
                app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));
            }
        }
        KeyCode::Enter | KeyCode::Char('\n') => submit_form(app, jobs),
        _ => {
            let Some(form) = form_mut(app) else {
                return;
            };
            if form.busy {
                return;
            }
            match key.code {
                KeyCode::Tab | KeyCode::Down => form.focus_next(),
                KeyCode::BackTab | KeyCode::Up => form.focus_previous(),
                _ => {
                    if let Some(field) = form.focused_mut() {
                        edit_line(&mut field.value, key);
                    }
                }
            }
        }
    }
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

fn submit_form(app: &mut App, jobs: &Sender<Job>) {
    let job = {
        let Some(form) = form_mut(app) else {
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
    ) && let Some(form) = form_mut(app)
    {
        form.busy = true;
        form.error = None;
    }
    let _ = jobs.send(job);
}

/// 表單各欄位的值（依表單種類對應位置）。
#[derive(Default)]
struct FormValues {
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
    fn from_form(form: &FormState) -> Self {
        let at = |index: usize| {
            form.fields
                .get(index)
                .map_or_else(String::new, |field| field.value.value().to_owned())
        };
        let secret_at = |index: usize| Secret::from(at(index));
        match form.kind {
            FormKind::Setup => Self {
                passphrase: secret_at(0),
                passphrase_confirm: secret_at(1),
                username: at(2),
                password: secret_at(3),
                password_confirm: secret_at(4),
            },
            FormKind::Unlock => Self {
                passphrase: secret_at(0),
                ..Self::default()
            },
            FormKind::LoginRetry(_) => Self {
                username: at(0),
                password: secret_at(1),
                passphrase: secret_at(2),
                ..Self::default()
            },
            FormKind::ChangeAccount => Self {
                passphrase: secret_at(0),
                username: at(1),
                password: secret_at(2),
                password_confirm: secret_at(3),
                ..Self::default()
            },
            FormKind::ChangePassphrase => Self {
                passphrase: secret_at(0),
                password: secret_at(1),
                password_confirm: secret_at(2),
                ..Self::default()
            },
        }
    }
}

fn form_mut(app: &mut App) -> Option<&mut FormState> {
    if let Some(screen) = app.login.as_mut()
        && let LoginScreen::Credentials { form, .. } = screen.as_mut()
    {
        return Some(form);
    }
    match &mut app.screen {
        Screen::Setup(form) | Screen::Unlock(form) | Screen::SettingsForm(form) => Some(form),
        _ => None,
    }
}

// ── 登入 ─────────────────────────────────────────────

fn handle_login(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    let mut action: Option<LoginAction> = None;

    if let Some(screen) = app.login.as_mut() {
        match screen.as_mut() {
            LoginScreen::Progress { .. } => match key.code {
                // 登入往返期間（可能長達預設的 HTTP 逾時）允許取消，
                // 否則 r、換頁、Ctrl+P 全部無效。
                KeyCode::Esc => action = Some(LoginAction::Dismiss),
                KeyCode::Char('q') => action = Some(LoginAction::Quit),
                _ => {}
            },
            LoginScreen::Failed { site, .. } => match key.code {
                KeyCode::Enter => action = Some(LoginAction::Retry(*site)),
                KeyCode::Char('e') => action = Some(LoginAction::EditAccount(*site)),
                // 關閉覆蓋層：讓使用者回到原本的頁面（可換頁或按 r 重試）。
                KeyCode::Esc => action = Some(LoginAction::Dismiss),
                KeyCode::Char('q') => action = Some(LoginAction::Quit),
                _ => {}
            },
            LoginScreen::Credentials { site, form, .. } => {
                if form.busy {
                    return;
                }
                match key.code {
                    KeyCode::Esc => action = Some(LoginAction::BackToFailed(*site)),
                    KeyCode::Enter => action = Some(LoginAction::SubmitCredentials),
                    KeyCode::Tab | KeyCode::Down => form.focus_next(),
                    KeyCode::BackTab | KeyCode::Up => form.focus_previous(),
                    _ => {
                        if let Some(field) = form.focused_mut() {
                            edit_line(&mut field.value, key);
                        }
                    }
                }
            }
            LoginScreen::Captcha { input, .. } => match key.code {
                KeyCode::Enter => {
                    let code = input.value().trim().to_owned();
                    if !code.is_empty() {
                        input.clear();
                        action = Some(LoginAction::SubmitCaptcha(code));
                    }
                }
                KeyCode::Char('r') if input.is_empty() => {
                    action = Some(LoginAction::RefreshCaptcha);
                }
                // 驗證碼填錯後仍可放棄本次登入（換帳號時才能拿回舊帳號）。
                KeyCode::Esc => action = Some(LoginAction::Dismiss),
                KeyCode::Char('q') if input.is_empty() => action = Some(LoginAction::Quit),
                _ => edit_line(input, key),
            },
            LoginScreen::Mfa { input, .. } => match key.code {
                KeyCode::Enter => {
                    let code = input.value().trim().to_owned();
                    if !code.is_empty() {
                        input.clear();
                        action = Some(LoginAction::VerifyMfaCode(code));
                    }
                }
                KeyCode::Char('s') if input.is_empty() => action = Some(LoginAction::SendMfaCode),
                // 驗證碼填錯後仍可放棄本次登入（換帳號時才能拿回舊帳號）。
                KeyCode::Esc => action = Some(LoginAction::Dismiss),
                KeyCode::Char('q') if input.is_empty() => action = Some(LoginAction::Quit),
                _ => edit_line(input, key),
            },
        }
    }

    match action {
        Some(LoginAction::Quit) => app.quit = true,
        Some(LoginAction::Retry(site)) => {
            app.login = Some(Box::new(LoginScreen::Progress {
                note: "正在重试登录…".to_owned(),
            }));
            let _ = jobs.send(Job::RetryLogin { site });
        }
        Some(LoginAction::EditAccount(site)) => open_credentials_form(app, site),
        Some(LoginAction::Dismiss) => {
            app.login = None;
            // 底層若是送出中的表單（例如修改帳號失敗），一併解除處理中狀態，
            // 否則關閉覆蓋層後表單會卡在「正在處理」而無法再操作。
            if let Some(form) = form_mut(app) {
                form.busy = false;
            }
            // 一併取消工作者端的登入流程：否則登入互動期間被延後的資料任務
            //（例如接著按 r 重新整理）永遠不會執行。
            let _ = jobs.send(Job::CancelLogin);
            app.set_message("已关闭登录提示：网络恢复后可按 r 重试");
        }
        Some(LoginAction::BackToFailed(site)) => {
            let message = login_message(app);
            app.login = Some(Box::new(LoginScreen::Failed { site, message }));
        }
        Some(LoginAction::SubmitCredentials) => submit_login_credentials(app, jobs),
        Some(LoginAction::SubmitCaptcha(code)) => {
            let _ = jobs.send(Job::SubmitCaptcha(Secret::from(code)));
        }
        Some(LoginAction::RefreshCaptcha) => {
            let _ = jobs.send(Job::RefreshCaptcha);
        }
        Some(LoginAction::SendMfaCode) => {
            let _ = jobs.send(Job::SendMfaCode);
        }
        Some(LoginAction::VerifyMfaCode(code)) => {
            let _ = jobs.send(Job::VerifyMfaCode(Secret::from(code)));
        }
        None => {}
    }
}

/// 目前登入畫面的失敗訊息（供回復上一層或帶入表單）。
fn login_message(app: &App) -> String {
    match app.login.as_deref() {
        Some(LoginScreen::Failed { message, .. } | LoginScreen::Credentials { message, .. }) => {
            message.clone()
        }
        _ => String::new(),
    }
}

/// 開啟「重新輸入账号密码」表單，並帶上原本的失敗訊息。
fn open_credentials_form(app: &mut App, site: SiteKind) {
    let message = login_message(app);
    app.login = Some(Box::new(LoginScreen::Credentials {
        site,
        form: FormState::login_retry(site),
        message,
    }));
}

/// 送出重新輸入的憑證；驗證失敗時把訊息寫回表單，不送出任務。
fn submit_login_credentials(app: &mut App, jobs: &Sender<Job>) {
    let job = {
        let Some(form) = form_mut(app) else {
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

// ── 帳戶設定 ─────────────────────────────────────────

fn handle_settings(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    let Screen::Settings(mut state) = app.screen else {
        return;
    };

    match key.code {
        KeyCode::Esc => app.set_screen(Screen::Main),
        KeyCode::Up | KeyCode::Char('k') => {
            state.previous();
            app.set_screen(Screen::Settings(state));
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.next();
            app.set_screen(Screen::Settings(state));
        }
        // 左鍵上一個、右鍵下一個；只調整草稿，不立即送出。
        KeyCode::Left | KeyCode::Char('h') => {
            if state.index == SettingsState::POLICY_INDEX {
                state.cycle_policy(app.access_policy, -1);
                app.set_screen(Screen::Settings(state));
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if state.index == SettingsState::POLICY_INDEX {
                state.cycle_policy(app.access_policy, 1);
                app.set_screen(Screen::Settings(state));
            }
        }
        KeyCode::Enter => match state.index {
            0 => app.set_screen(Screen::SettingsForm(FormState::change_account())),
            1 => app.set_screen(Screen::SettingsForm(FormState::change_passphrase())),
            // 訪問模式：enter 提交草稿；未變更或保存中則不送任務。
            _ => {
                if !state.saving && state.policy_dirty(app.access_policy) {
                    let draft = state.policy(app.access_policy);
                    state.saving = true;
                    app.set_screen(Screen::Settings(state));
                    let _ = jobs.send(Job::SetAccessPolicy(draft));
                } else {
                    app.set_screen(Screen::Settings(state));
                }
            }
        },
        _ => {}
    }
}

// ── 主畫面 ───────────────────────────────────────────

/// 學期選擇器（作業頁）：上下選擇、enter 確認、esc 取消。
fn handle_term_picker(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    let Screen::TermPicker(state) = &mut app.screen else {
        return;
    };

    match key.code {
        KeyCode::Esc => app.set_screen(Screen::Main),
        KeyCode::Up | KeyCode::Char('k') => state.previous(),
        KeyCode::Down | KeyCode::Char('j') => state.next(),
        KeyCode::Enter => {
            let selected = state.selected();
            if let Some(term) = selected {
                let _ = jobs.send(Job::SetHomeworkTerm {
                    term: term.to_string(),
                });
            }
            app.set_screen(Screen::Main);
        }
        _ => {}
    }
}

fn handle_main(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Left | KeyCode::Char('h') => {
            app.nav_previous();
            ensure_page(app, jobs);
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
            app.nav_next();
            ensure_page(app, jobs);
        }
        KeyCode::BackTab => {
            app.nav_previous();
            ensure_page(app, jobs);
        }
        KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Enter => activate(app, jobs),
        KeyCode::Esc => escape(app),
        KeyCode::Char('r') => {
            // 手動刷新：略過快取重新查詢。
            let nav = app.nav;
            request(app, jobs, nav, true);
        }
        KeyCode::Char('[') => change_group(app, -1),
        KeyCode::Char(']') => change_group(app, 1),
        KeyCode::Char('o') => open_activity(app, jobs),
        KeyCode::Char('s') => open_term_picker(app),
        KeyCode::Char('n') => change_flow_page(app, jobs, 1),
        KeyCode::Char('p') => change_flow_page(app, jobs, -1),
        _ => {}
    }
}

/// 切換分組（`[`／`]`）：作業頁切作業分組、思源學堂活動頁切活動分組。
fn change_group(app: &mut App, delta: i32) {
    if app.nav == NavItem::Homework {
        app.homework_group = if delta < 0 {
            app.homework_group.previous()
        } else {
            app.homework_group.next()
        };
        // 切換分組後重設選取，避免索引越界。
        app.set_selection(0);
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

/// 開啟目前選取項目的網頁（`o`）：作業頁與思源學堂活動／詳情層。
fn open_activity(app: &mut App, jobs: &Sender<Job>) {
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
fn open_term_picker(app: &mut App) {
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

/// 首次進入尚未載入的頁面時自動查詢。
pub(crate) fn ensure_page(app: &mut App, jobs: &Sender<Job>) {
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
pub fn request(app: &mut App, jobs: &Sender<Job>, nav: NavItem, force: bool) {
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

fn activate(app: &mut App, jobs: &Sender<Job>) {
    match app.nav {
        NavItem::Schedule => app.schedule_detail = !app.schedule_detail,
        NavItem::Homework => app.homework_detail = !app.homework_detail,
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
                let _ = jobs.send(Job::LoadActivityDetail { activity_id });
            }
            LmsLevel::Detail => {}
        },
    }
}

fn escape(app: &mut App) {
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

fn change_flow_page(app: &mut App, jobs: &Sender<Job>, delta: i32) {
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

/// 單行輸入的共用編輯邏輯。
///
/// 帶 CONTROL 或 ALT 修飾的字元一律不插入：crossterm 對 `Ctrl+A` 之類的按鍵
/// 同樣回報 `Char('a')`，若只看 `code` 會把控制鍵當成普通字元寫進欄位
/// （包含口令欄）。
fn edit_line(line: &mut InputLine, key: KeyEvent) {
    match key.code {
        KeyCode::Backspace => {
            line.backspace();
        }
        KeyCode::Delete => {
            line.delete();
        }
        KeyCode::Left => {
            line.move_left();
        }
        KeyCode::Right => {
            line.move_right();
        }
        KeyCode::Home => line.move_home(),
        KeyCode::End => line.move_end(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            line.insert(character);
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "tests/handler_test.rs"]
mod handler_test;
