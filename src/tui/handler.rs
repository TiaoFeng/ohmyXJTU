//! 按鍵處理：把終端事件轉成畫面意圖。
//!
//! 需要建構任務或切換頁面／資源的動作委派給 [`super::controller`]；
//! 本檔只保留按鍵到意圖的映射與輸入編輯。

use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::credentials::Secret;
use crate::domain::todo::{self, SortMode};
use crate::session::SiteKind;
use crate::task::Job;
use crate::tui::app::{
    App, FormKind, FormState, LoginScreen, NavItem, Screen, SettingsState, TaskBatchOp,
    TaskConfirmState, TaskField, TaskMenuKind,
};
use crate::tui::text::InputLine;

use super::controller;

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
    // 按下任何按鍵都算一次操作：先清掉上一則暫時訊息，讓底部提示列立刻換成
    // 「目前畫面」的快捷鍵。否則像「作业已更新…」這種會停留數秒的通知，會蓋住
    // 剛切換過去的畫面提示（例如按下 `^L` 進入排序提示卻看不到排序按鍵），
    // 讓使用者以為操作沒有生效。動作本身可以再設定新的訊息。
    app.clear_message();

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
                if app.form_mut().is_some() {
                    if let Some(form) = app.form_mut()
                        && let Some(field) = form.focused_mut()
                    {
                        field.value.clear();
                    }
                } else {
                    // 任務表單不是憑證表單：清空它目前聚焦的欄位。
                    controller::clear_focused_task_field(app);
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
        Screen::TaskMenu(_) => handle_task_menu(app, key),
        Screen::TaskBatchMenu(_) => handle_task_batch_menu(app, key, jobs),
        Screen::TaskConfirm(_) => handle_task_confirm(app, key, jobs),
        Screen::TaskForm(_) => handle_task_form(app, key, jobs),
        Screen::Sort => handle_sort(app, key),
        Screen::Main => handle_main(app, key, jobs),
    }
}

/// 排序提示（`^L`）：`p` 優先級、`d` 截止時間、`n` 預設，`esc` 取消。
///
/// 選定後立即套用並回到主畫面；其他按鍵一律忽略（與 `ui-ref` 相同）。
fn handle_sort(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        app.set_screen(Screen::Main);
        return;
    }
    match plain_char(&key).map(|character| character.to_ascii_lowercase()) {
        Some('p') => controller::set_task_sort(app, SortMode::Priority),
        Some('d') => controller::set_task_sort(app, SortMode::Deadline),
        Some('n') => controller::set_task_sort(app, SortMode::Default),
        _ => {}
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
    // 任務搜尋輸入框優先於底層畫面。
    if let Some(input) = app.task_page.search.as_mut() {
        insert_text(input, text);
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
    // 任務表單：描述欄接受換行，其餘欄位忽略換行。標籤欄另有寬度上限。
    if let Screen::TaskForm(form) = &mut app.screen {
        match form.focus {
            TaskField::Content => insert_text(&mut form.content, text),
            TaskField::Tag => insert_tag_text(&mut form.tag, text),
            TaskField::Deadline => insert_text(&mut form.deadline, text),
            TaskField::Description => {
                for character in text.chars() {
                    if character == '\r' {
                        continue;
                    }
                    form.description.insert(character);
                }
            }
            TaskField::Priority | TaskField::Completed => {}
        }
        return;
    }
    if let Some(form) = app.form_mut()
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

/// 把貼上的文字插入標籤欄：超出長度上限即停（貼上自動截斷）。
fn insert_tag_text(line: &mut InputLine, text: &str) {
    for character in text
        .chars()
        .filter(|character| !matches!(character, '\n' | '\r'))
    {
        if !todo::tag_fits(line.value(), &character.to_string()) {
            break;
        }
        line.insert(character);
    }
}

/// 標籤欄的輸入編輯：字元超出寬度上限時直接不收（超出的字打不進去）。
fn edit_tag_line(line: &mut InputLine, key: KeyEvent) {
    if let KeyCode::Char(character) = key.code
        && plain_char(&key).is_some()
        && !todo::tag_fits(line.value(), &character.to_string())
    {
        return;
    }
    edit_line(line, key);
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
    if app.form_mut().is_some_and(|form| form.busy) {
        return;
    }
    let kind = app.form_mut().map(|form| form.kind);

    match key.code {
        KeyCode::Esc => {
            if matches!(
                kind,
                Some(FormKind::ChangeAccount | FormKind::ChangePassphrase)
            ) {
                app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));
            }
        }
        KeyCode::Enter | KeyCode::Char('\n') => controller::submit_form(app, jobs),
        _ => {
            let Some(form) = app.form_mut() else {
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
            // 等待工作者回報取消完成；期間遲到的登入事件不得重開覆蓋層
            //（它們屬於這次被取消的登入）。
            app.login_cancel_pending = true;
            // 底層若是送出中的表單（例如修改帳號失敗），一併解除處理中狀態，
            // 否則關閉覆蓋層後表單會卡在「正在處理」而無法再操作。
            if let Some(form) = app.form_mut() {
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
        Some(LoginAction::SubmitCredentials) => controller::submit_login_credentials(app, jobs),
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
            SettingsState::ACCOUNT_INDEX => {
                app.set_screen(Screen::SettingsForm(FormState::change_account()));
            }
            SettingsState::PASSPHRASE_INDEX => {
                app.set_screen(Screen::SettingsForm(FormState::change_passphrase()));
            }
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

/// 是否為帶 Ctrl 的指定字元（`^D` 之類的組合鍵）。
fn is_ctrl_char(key: &KeyEvent, character: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(character)
}

/// 是否為不含 Ctrl／Alt 的普通字元鍵。
fn plain_char(key: &KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(character) => Some(character),
        _ => None,
    }
}

/// 在任務選單（設置／批量操作）中移動選取。
fn move_menu_index(app: &mut App, delta: i32, len: usize) {
    if len == 0 {
        return;
    }
    let step = if delta >= 0 { 1 } else { len - 1 };
    match &mut app.screen {
        Screen::TaskMenu(state) => state.index = (state.index + step) % len,
        Screen::TaskBatchMenu(state) => state.index = (state.index + step) % len,
        _ => {}
    }
}

/// 任務設置彈窗（`^T`）：多選與刪除已完成。
fn handle_task_menu(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => move_menu_index(app, -1, TaskMenuKind::ALL.len()),
        KeyCode::Down | KeyCode::Char('j') => move_menu_index(app, 1, TaskMenuKind::ALL.len()),
        KeyCode::Esc => app.set_screen(Screen::Main),
        KeyCode::Enter | KeyCode::Char('\n') => {
            let index = match &app.screen {
                Screen::TaskMenu(state) => state.index,
                _ => return,
            };
            let Some(kind) = TaskMenuKind::ALL.get(index) else {
                return;
            };
            match kind {
                TaskMenuKind::Multi => {
                    controller::toggle_task_multi(app);
                }
                TaskMenuKind::DeleteCompleted => {
                    let count = app
                        .task_page
                        .tasks
                        .iter()
                        .filter(|task| task.completed)
                        .count();
                    if count == 0 {
                        app.set_screen(Screen::Main);
                        app.set_message("没有已完成的任务");
                        return;
                    }
                    app.set_screen(Screen::TaskConfirm(TaskConfirmState { count }));
                }
            }
        }
        _ => {}
    }
}

/// 多選後的批量操作選單。
fn handle_task_batch_menu(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => move_menu_index(app, -1, TaskBatchOp::ALL.len()),
        KeyCode::Down | KeyCode::Char('j') => move_menu_index(app, 1, TaskBatchOp::ALL.len()),
        KeyCode::Esc => app.set_screen(Screen::Main),
        KeyCode::Enter | KeyCode::Char('\n') => {
            let index = match &app.screen {
                Screen::TaskBatchMenu(state) => state.index,
                _ => return,
            };
            let Some(op) = TaskBatchOp::ALL.get(index) else {
                return;
            };
            let ids = controller::selected_task_ids(app);
            app.task_page.multi = None;
            app.set_screen(Screen::Main);
            if ids.is_empty() {
                app.set_message("选中的任务已不存在");
                return;
            }
            let job = match op {
                TaskBatchOp::Done => Job::SetTasksDone { ids, done: true },
                TaskBatchOp::Undone => Job::SetTasksDone { ids, done: false },
                TaskBatchOp::Delete => Job::DeleteTasks { ids },
            };
            let _ = jobs.send(job);
        }
        _ => {}
    }
}

/// 刪除已完成任務的二次確認（`y` 確認、`n`／`esc` 取消）。
fn handle_task_confirm(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    match plain_char(&key) {
        Some('y' | 'Y') => {
            app.set_screen(Screen::Main);
            let _ = jobs.send(Job::DeleteCompletedTasks);
        }
        Some('n' | 'N') => {
            app.set_screen(Screen::Main);
            app.set_message("已取消删除");
        }
        _ => {
            if key.code == KeyCode::Esc {
                app.set_screen(Screen::Main);
                app.set_message("已取消删除");
            }
        }
    }
}

/// 任務表單（新增／編輯）。
///
/// `tab` 切換欄位、描述欄 `enter` 換行、左右鍵切換選項、`^s` 保存、`esc` 取消。
fn handle_task_form(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    // 送出中：忽略所有輸入，避免重複提交；但 `esc` 仍可關閉面板——保存已在
    // 背景進行（任務服務獨立一條執行緒），不應把使用者鎖在面板裡。
    if matches!(&app.screen, Screen::TaskForm(form) if form.busy) {
        if key.code == KeyCode::Esc {
            app.set_screen(Screen::Main);
            app.set_message("面板已关闭，保存仍在进行");
        }
        return;
    }
    // 保存用組合鍵：描述欄的 enter 已經被「換行」佔用。
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if key.code == KeyCode::Char('s') {
            controller::submit_task_form(app, jobs);
        }
        return;
    }

    let Screen::TaskForm(form) = &mut app.screen else {
        return;
    };
    let form = form.as_mut();
    match key.code {
        KeyCode::Esc => {
            app.set_screen(Screen::Main);
            return;
        }
        KeyCode::Tab => {
            form.focus_next();
            return;
        }
        KeyCode::BackTab => {
            form.focus_previous();
            return;
        }
        _ => {}
    }
    match form.focus {
        TaskField::Content => edit_line(&mut form.content, key),
        TaskField::Tag => edit_tag_line(&mut form.tag, key),
        TaskField::Deadline => edit_line(&mut form.deadline, key),
        TaskField::Description => match key.code {
            KeyCode::Enter | KeyCode::Char('\n') => form.description.insert('\n'),
            KeyCode::Up => form.description.move_up(),
            KeyCode::Down => form.description.move_down(),
            KeyCode::Left => form.description.move_left(),
            KeyCode::Right => form.description.move_right(),
            KeyCode::Home => form.description.move_home(),
            KeyCode::End => form.description.move_end(),
            KeyCode::Backspace => form.description.backspace(),
            KeyCode::Delete => form.description.delete(),
            KeyCode::Char(character) if plain_char(&key).is_some() => {
                form.description.insert(character);
            }
            _ => {}
        },
        TaskField::Priority => match key.code {
            KeyCode::Left | KeyCode::Up => form.priority = form.priority.previous(),
            KeyCode::Right | KeyCode::Down => form.priority = form.priority.next(),
            _ => {}
        },
        TaskField::Completed => match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                form.completed = !form.completed;
            }
            _ => {}
        },
    }
}

/// 任務搜尋輸入框：`enter` 套用、`esc` 取消，`↑`／`↓` 在既有標籤間循環預填，
/// 其餘按鍵編輯輸入。
fn handle_task_search(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter | KeyCode::Char('\n') => {
            let keyword = app
                .task_page
                .search
                .as_ref()
                .map(|input| input.value().trim().to_owned());
            app.task_page.filter = keyword.filter(|keyword| !keyword.is_empty());
            app.task_page.search = None;
            app.task_page.tag_cursor = None;
            app.homework_state.select(Some(0));
            let matches = app.task_filter_matches();
            match &app.task_page.filter {
                Some(keyword) => app.set_message(format!(
                    "筛选“{keyword}”：当前分组 {matches} 项（esc 清除）"
                )),
                None => app.set_message("已清除筛选"),
            }
        }
        KeyCode::Esc => {
            app.task_page.search = None;
            app.task_page.tag_cursor = None;
            // 輸入框是以現有篩選預填的：esc 只放棄這次編輯，不會動到已套用的
            // 篩選（要清除請在主畫面按 esc），訊息必須說清楚。
            app.set_message(if app.task_page.filter.is_some() {
                "已取消编辑（保留现有筛选）"
            } else {
                "已取消编辑"
            });
        }
        // 上下鍵在既有標籤間循環預填；一個標籤也沒有時等同沒反應（也不提示）。
        KeyCode::Up | KeyCode::Down => {
            let delta = if key.code == KeyCode::Down { 1 } else { -1 };
            controller::cycle_tag_suggestion(app, delta);
        }
        _ => {
            // 手動編輯之後重新開始選取，下一次上下鍵由頭／尾重新起算。
            app.task_page.tag_cursor = None;
            if let Some(input) = app.task_page.search.as_mut() {
                edit_line(input, key);
            }
        }
    }
}

fn handle_main(app: &mut App, key: KeyEvent, jobs: &Sender<Job>) {
    // 任務頁的搜尋輸入框開啟時獨占按鍵（enter 套用、esc 取消）。
    if app.nav == NavItem::Homework && app.task_page.search.is_some() {
        handle_task_search(app, key);
        return;
    }
    // `^D` 的二次確認：任何其他按鍵都會取消（與提示訊息保持一致）。
    if app.task_page.pending_delete.is_some() && !is_ctrl_char(&key, 'd') {
        app.task_page.pending_delete = None;
    }
    // 任務頁的組合鍵（新增、編輯、刪除、搜尋、設置）。
    if app.nav == NavItem::Homework && key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('a') => {
                controller::open_task_form(app);
                return;
            }
            KeyCode::Char('e') => {
                controller::edit_selected_task(app);
                return;
            }
            KeyCode::Char('d') => {
                controller::delete_selected_task(app, jobs);
                return;
            }
            KeyCode::Char('f') => {
                controller::open_task_search(app);
                return;
            }
            KeyCode::Char('t') => {
                controller::open_task_menu(app);
                return;
            }
            KeyCode::Char('l') => {
                app.set_screen(Screen::Sort);
                return;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Left | KeyCode::Char('h') => {
            app.nav_previous();
            controller::ensure_page(app, jobs);
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
            app.nav_next();
            controller::ensure_page(app, jobs);
        }
        KeyCode::BackTab => {
            app.nav_previous();
            controller::ensure_page(app, jobs);
        }
        KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Enter => controller::activate(app, jobs),
        KeyCode::Esc => controller::escape(app),
        // 任務頁的單鍵操作（多選為普通鍵 `m`：`^M` 與 enter 同碼，不可靠）。
        KeyCode::Char(' ')
            if app.nav == NavItem::Homework
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if app.task_page.multi.is_some() {
                controller::toggle_task_selection(app);
            } else {
                controller::toggle_selected_task(app, jobs);
            }
        }
        KeyCode::Char('m')
            if app.nav == NavItem::Homework
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            controller::toggle_task_multi(app);
        }
        KeyCode::Char('r') => {
            // 手動刷新：略過快取重新查詢。
            let nav = app.nav;
            controller::request(app, jobs, nav, true);
        }
        // 課表頁切換週次；作業頁與思源學堂活動層切換分組。
        KeyCode::Char('[') => {
            if app.nav == NavItem::Schedule {
                controller::change_schedule_week(app, jobs, -1);
            } else {
                controller::change_group(app, -1);
            }
        }
        KeyCode::Char(']') => {
            if app.nav == NavItem::Schedule {
                controller::change_schedule_week(app, jobs, 1);
            } else {
                controller::change_group(app, 1);
            }
        }
        KeyCode::Char('o') => controller::open_activity(app, jobs),
        KeyCode::Char('s') => controller::open_term_picker(app),
        KeyCode::Char('n') => controller::change_flow_page(app, jobs, 1),
        KeyCode::Char('p') => controller::change_flow_page(app, jobs, -1),
        // 詳情內容偏長時可捲動（作業頁詳情與思源學堂活動詳情）。
        KeyCode::PageUp => controller::scroll_detail(app, controller::DetailScroll::PageUp),
        KeyCode::PageDown => controller::scroll_detail(app, controller::DetailScroll::PageDown),
        KeyCode::Home => controller::scroll_detail(app, controller::DetailScroll::Top),
        KeyCode::End => controller::scroll_detail(app, controller::DetailScroll::Bottom),
        _ => {}
    }
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
