//! TUI 啟動、主迴圈與事件套用。

pub mod app;
pub mod handler;
pub mod text;
pub mod theme;
pub mod ui;
pub mod views;

use std::io::Stdout;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::credentials::Vault;
use crate::domain::homework::HomeworkGroup;
use crate::error::{AppError, AppResult};
use crate::session::SiteKind;
use crate::task::{self, Event, FailedTarget, Job};

use app::{
    AgreementState, App, FormKind, FormState, HomeworkData, LmsLevel, LoginScreen, Page, Screen,
    SettingsState, TermPickerState,
};
use text::InputLine;

/// 事件輪詢間隔。
const TICK: Duration = Duration::from_millis(200);

/// 啟動 TUI。
pub fn run() -> AppResult<()> {
    let config = Config::load_or_create()?;
    let vault = Vault::at_default_path()?;
    let vault_exists = vault.exists();

    let (jobs, events) = task::spawn(config.clone(), vault)?;

    let mut app = App::new(config.access_policy);
    if !vault_exists {
        app.set_screen(Screen::Setup(FormState::setup()));
    }
    // 尚未同意本版用户协议：先顯示閱讀門。底層畫面（首次設定或解鎖）已就緒，
    // 同意後直接揭露。
    if !config.privacy_accepted(crate::privacy::VERSION) {
        app.agreement = Some(Box::new(AgreementState::new()));
    }

    // 刻意不用 `?` 提前返回：任何情況下都要還原終端狀態。
    // `try_init` 本身失敗時（例如無法查詢終端尺寸）可能已開啟原始模式，
    // 也要先盡力還原再回報錯誤。
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = ratatui::try_restore();
            return Err(AppError::Tui(err.to_string()));
        }
    };
    // 啟用 bracketed paste：貼上內容會以單一 Paste 事件送達，不會被拆成
    // 一連串按鍵（貼上含換行的內容時也不會意外送出表單）。
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::EnableBracketedPaste
    );
    let loop_result = main_loop(&mut terminal, &mut app, &events, &jobs);
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste
    );
    let restore_result = ratatui::try_restore().map_err(|err| AppError::Tui(err.to_string()));

    let _ = jobs.send(Job::Shutdown);

    loop_result.and(restore_result)
}

/// 安裝 panic hook：還原終端、輸出單行錯誤訊息後結束行程。
///
/// 訊息不含 panic 內容（避免任何潛在敏感資料外洩），只保留發生位置；
/// 任何執行緒 panic 都會終止行程，避免背景執行緒死亡後介面卡死。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // 盡力還原終端：panic 可能發生在進入原始模式或替代畫面之後。
        let _ = ratatui::try_restore();
        // `try_restore` 不會重現游標，而 `exit` 又會跳過 `Terminal::Drop` 的補救；
        // 這裡一併關閉 bracketed paste 並顯示游標。
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        );
        let location = info
            .location()
            .map(|location| (location.file(), location.line()));
        eprintln!("{}", panic_hook_message(location));
        std::process::exit(101);
    }));
}

/// panic 時的單行錯誤訊息（不含 panic payload）。
fn panic_hook_message(location: Option<(&str, u32)>) -> String {
    match location {
        Some((file, line)) => format!("错误：程序发生内部错误（{file}:{line}），已退出。"),
        None => "错误：程序发生内部错误，已退出。".to_owned(),
    }
}

fn main_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    events: &Receiver<Event>,
    jobs: &Sender<Job>,
) -> AppResult<()> {
    loop {
        app.expire_message();
        app.advance_tick();
        terminal
            .draw(|frame| views::draw(frame, app))
            .map_err(|err| AppError::Tui(err.to_string()))?;

        if event::poll(TICK).map_err(|err| AppError::Tui(err.to_string()))? {
            match event::read().map_err(|err| AppError::Tui(err.to_string()))? {
                // 長按（Repeat）與單次按下（Press）都應作用；Release 忽略。
                CrosstermEvent::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    handler::handle_key(app, key, jobs);
                }
                // bracketed paste：整段貼上不應被拆成一連串按鍵。
                CrosstermEvent::Paste(text) => handler::handle_paste(app, &text),
                _ => {}
            }
        }

        loop {
            match events.try_recv() {
                Ok(event) => apply_event(app, event, jobs),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // 背景工作執行緒已結束：之後不會再有任何事件，頁面會永遠停在
                    // 「載入中」且按鍵無效。明確提示使用者退出重啟，而不是靜默停滯。
                    app.set_message("后台任务已停止，请按 q 退出后重新启动");
                    break;
                }
            }
        }

        // 事件要求的瀏覽器開啟：在這裡執行，錯誤以狀態訊息回報。
        if let Some(url) = app.pending_open.take() {
            match crate::system::browser::open_url(&url) {
                Ok(()) => app.set_message("已在浏览器打开：如需登录请在浏览器完成登录"),
                Err(err) => app.set_message(format!("打开网页失败：{err}")),
            }
        }

        if app.quit {
            return Ok(());
        }
    }
}

/// 套用背景事件（測試用：供跨模組的「工作者 → 介面」串接測試呼叫）。
#[cfg(test)]
pub(crate) fn apply_event_for_test(app: &mut App, event: Event) {
    let (jobs, _rx) = std::sync::mpsc::channel();
    apply_event(app, event, &jobs);
}

/// 套用背景事件。
fn apply_event(app: &mut App, event: Event, jobs: &Sender<Job>) {
    match event {
        Event::VaultReady => {
            // 憑證已就緒：清掉殘留的登入覆蓋層，直接進入主畫面
            //（不預先登入任何站點），由目前頁面按需觸發惰性登入。
            app.login = None;
            if app.is_main() {
                // 修改帳號後回到主畫面：舊資料屬於舊帳號，強制刷新目前頁面。
                let nav = app.nav;
                handler::request(app, jobs, nav, true);
            } else {
                app.ensure_main();
                handler::ensure_page(app, jobs);
            }
            app.set_message("凭证已就绪");
        }
        Event::LoginProgress(note) => {
            // 登入互動是覆蓋層：底層畫面保持不變，事件只更新彈窗內容。
            app.login = Some(Box::new(LoginScreen::Progress { note }));
        }
        Event::LoginNeedsCaptcha(path) => {
            app.captcha_path = Some(path.clone());
            let previous_error = match app.login.as_deref() {
                Some(LoginScreen::Captcha { error, .. }) => error.clone(),
                _ => None,
            };
            app.login = Some(Box::new(LoginScreen::Captcha {
                path,
                input: InputLine::new(),
                error: previous_error,
            }));
        }
        Event::LoginNeedsMfa { phone, sent } => {
            let (input, error) = match app.login.as_deref() {
                Some(LoginScreen::Mfa { input, error, .. }) => (input.clone(), error.clone()),
                _ => (InputLine::new(), None),
            };
            app.login = Some(Box::new(LoginScreen::Mfa {
                phone,
                sent,
                input,
                error,
            }));
        }
        Event::LoginFailed { site, message } => {
            set_login_error(app, site, message);
        }
        Event::VerificationRetry { site, message } => {
            // 驗證碼填錯：保留原本的驗證碼／簡訊輸入畫面，讓使用者就地重輸，
            // 不必重新輸入帳號密碼（登入流程仍在工作者端保留）。
            match app.login.as_deref_mut() {
                Some(LoginScreen::Mfa { input, error, .. })
                | Some(LoginScreen::Captcha { input, error, .. }) => {
                    input.clear();
                    *error = Some(message);
                }
                _ => set_login_error(app, site, message),
            }
        }
        Event::LoginSucceeded { site, mode } => {
            app.login = None;
            match mode {
                Some(mode) => app.set_site_mode(site, mode),
                None => app.clear_site_mode(site),
            }
            if !app.is_main() {
                app.set_screen(Screen::Main);
            }
            // 若目前頁面尚未載入（例如從失敗畫面重試成功），補一次載入。
            handler::ensure_page(app, jobs);
            app.set_message("登录成功");
        }
        Event::SessionsCleared { account_changed } => {
            app.clear_site_modes();
            app.invalidate_data(account_changed);
        }
        Event::AgreementAccepted => {
            // 協議已同意：關閉閱讀門，揭露底層畫面（首次設定或解鎖）。
            app.agreement = None;
        }
        Event::SessionExpired { site } => {
            app.clear_site_mode(site);
        }
        Event::SessionDisabled(message) => {
            // 會話已停用（無法建立乾淨的新會話）：回到解鎖畫面，讓使用者
            // 重新輸入加密口令以建立一個全新的會話；此前不會再發出任何請求。
            app.login = None;
            app.clear_site_modes();
            app.invalidate_data(true);
            let mut form = FormState::unlock();
            form.error = Some(message);
            app.set_screen(Screen::Unlock(form));
            app.set_message("会话已停用：请输入加密口令重新解锁");
        }
        Event::LoadingCancelled { target } => {
            // 進行中的載入被取消：解除載入中狀態，保留已取得的部分資料。
            app.cancel_loading(target);
        }
        Event::Schedule(data) => {
            app.schedule = Page::Ready(*data);
            app.updated_at.schedule = Some(now_clock());
            app.schedule_state.select(Some(0));
            app.ensure_main();
        }
        Event::Homework(update) => {
            let progress = update.progress;
            let finished = progress.is_none();
            let elapsed = update.elapsed;
            let unfinished = update
                .items
                .iter()
                .filter(|item| item.state.group() == HomeworkGroup::Unfinished)
                .count();
            let data = HomeworkData {
                term_label: update.term_label,
                term_source: update.term_source.map(|source| source.label()),
                courses_included: update.courses_included,
                courses_skipped: update.courses_skipped,
                term_options: update.term_options,
                items: update.items,
                issues: update.issues,
                courses_failed: update.courses_failed,
                progress,
            };
            app.term_options = data.term_options.clone();
            app.updated_at.homework = Some(now_clock());
            let len = data.group_count(app.homework_group);
            app.homework = match progress {
                // 部分結果：頁面維持載入中（資料持續可顯示），終態才轉為就緒。
                Some((done, total)) => Page::Loading {
                    note: format!(
                        "正在汇总作业（已完成 {done}/{total} 门课程，累计 {} 项）…",
                        data.items.len()
                    ),
                    stale: Some(data),
                },
                None => Page::Ready(data),
            };
            // 夾取選取索引，避免分組內容變動後越界。
            let selected = app
                .homework_state
                .selected()
                .unwrap_or(0)
                .min(len.saturating_sub(1));
            app.homework_state.select(Some(selected));
            if finished {
                app.set_message(format!(
                    "作业已更新：未完成 {unfinished} 项（用时 {:.1}s）",
                    elapsed.as_secs_f32()
                ));
            }
            app.ensure_main();
        }
        Event::HomeworkNeedsTerm {
            options,
            suggestion,
            reason,
        } => {
            app.term_options = options.clone();
            app.homework.fail("未确定本学期：按 s 选择要查看的学期");
            app.set_screen(Screen::TermPicker(TermPickerState::new(
                options, suggestion, reason,
            )));
        }
        Event::Flow(data) => {
            app.attendance = Page::Ready(*data);
            app.updated_at.attendance = Some(now_clock());
            app.flow_state.select(Some(0));
            app.ensure_main();
        }
        Event::CoursesTerm(term) => {
            // 只更新分區提示：課程清單本身不變，重新繪製即會依新學期重新分區。
            app.lms.courses_term = term;
        }
        Event::Courses(data) => {
            let count = data.courses.len();
            app.lms.courses_term = data.current_term;
            app.lms.courses = Page::Ready(data.courses);
            app.updated_at.lms = Some(now_clock());
            // 列表順序可能改變：以穩定的課程識別碼重新定位目前課程，否則活動層
            // 的標題與 `o` 會指向另一門課。仍在課程層時維持既有行為（選第一項）。
            let anchored = match (app.lms.level, app.lms.activities_course.as_deref()) {
                (LmsLevel::Courses, _) | (_, None) => None,
                (_, Some(id)) => app
                    .lms
                    .courses
                    .ready()
                    .and_then(|courses| courses.iter().position(|course| course.id == id)),
            };
            match anchored {
                Some(index) => {
                    app.lms.course_index = index;
                    app.course_state.select(Some(index));
                }
                None => {
                    // 目前課程已不在新的清單中（或本來就在課程層）：回到課程列表，
                    // 不讓活動層停留在一個已不存在的課程上。
                    if app.lms.level != LmsLevel::Courses {
                        app.lms.level = LmsLevel::Courses;
                    }
                    app.lms.course_index = 0;
                    app.course_state.select(Some(0));
                }
            }
            // 刻意不改動 `lms.level`（僅在目前課程消失時才回到清單）：使用者
            // 可能在刷新完成前已進入活動或詳情層，資料更新不應把他的導航拉回。
            app.set_message(format!("共 {count} 门课程"));
            app.ensure_main();
        }
        Event::Activities {
            course_id,
            activities,
        } => {
            // 遲到的回應：使用者已經切到別的課程，這批活動不屬於目前畫面。
            if app.lms.activities_course.as_deref() != Some(course_id.as_str()) {
                return;
            }
            app.lms.activities = Page::Ready(activities);
            app.updated_at.lms = Some(now_clock());
            // 目前分組為空時，改顯示第一個有內容的分組（依顯示順序）。
            let current = app.lms.activity_group;
            let counts = app.activity_group_counts();
            let current_empty = counts
                .iter()
                .find(|(group, _)| *group == current)
                .is_none_or(|(_, count)| *count == 0);
            if current_empty && let Some((group, _)) = counts.iter().find(|(_, count)| *count > 0) {
                app.lms.activity_group = *group;
            }
            app.activity_state.select(Some(0));
            app.ensure_main();
        }
        Event::ActivityDetail(detail) => {
            // 遲到的回應：使用者已經改看別的活動。
            if app.lms.detail_activity.as_deref() != Some(detail.id.as_str()) {
                return;
            }
            app.lms.detail = Page::Ready(*detail);
            app.updated_at.lms = Some(now_clock());
            app.ensure_main();
        }
        Event::OpenUrl(url) => {
            // 實際啟動瀏覽器交由主迴圈執行（測試不觸發外部程序）。
            app.pending_open = Some(url);
            app.set_message("正在打开浏览器…");
        }
        Event::AccountUpdated => {
            // 修改帳號成功：離開表單；資料由隨後的事件重新載入。
            if matches!(app.screen, Screen::SettingsForm(_)) {
                app.set_screen(Screen::Main);
            }
            app.set_message("账号已更新");
        }
        Event::CredentialSaveFailed(message) => {
            // 帳號已驗證成功，但憑證寫入保險庫失敗：解除表單的「處理中」
            // 狀態並就地顯示錯誤，否則表單會卡住而連 Esc 都被忽略。
            match &mut app.screen {
                Screen::Setup(form) | Screen::Unlock(form) | Screen::SettingsForm(form) => {
                    form.busy = false;
                    form.error = Some(message.clone());
                }
                _ => {}
            }
            if let Some(LoginScreen::Credentials { form, .. }) = app.login.as_deref_mut() {
                form.busy = false;
                form.error = Some(message.clone());
            }
            app.set_message(message);
        }
        Event::PassphraseUpdated => {
            // 修改口令成功：離開處理中狀態並回到設定選單。
            if matches!(
                &app.screen,
                Screen::SettingsForm(form) if form.kind == FormKind::ChangePassphrase
            ) {
                app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));
            }
            app.set_message("加密口令已更新");
        }
        Event::AccessPolicyUpdated(policy) => {
            app.access_policy = policy;
            // 設定彈窗若開著：更新草稿並解除「保存中」，彈窗不關閉。
            if let Screen::Settings(state) = &mut app.screen {
                state.draft = Some(policy);
                state.saving = false;
            }
            app.set_message(format!(
                "访问模式已切换为 {}；按 r 刷新当前页面",
                policy.label()
            ));
        }
        Event::Notice(message) => {
            app.set_message(message);
        }
        Event::Failed {
            what,
            message,
            target,
            site,
            resource,
        } => {
            let login_site = site.unwrap_or(SiteKind::Attendance);
            // 遲到的舊資源失敗：使用者已切到別的課程／活動，前一個資源的失敗
            // 不得標記目前畫面（與成功回應的識別碼檢查一致）；但登入流程的
            // 終態處理仍要執行（見下方），否則自動重登失敗後「正在登入」的
            // 覆蓋層會永遠留著。
            if !failed_resource_is_stale(app, target, resource.as_deref()) {
                // 錯誤只標記受影響的頁面；其他頁面保持原狀。
                app.fail_target(target, &message);
                let text = format!("{what}失败：{message}");
                match target {
                    FailedTarget::Login => set_login_error(app, login_site, text.clone()),
                    FailedTarget::Settings => {
                        // 設定保存失敗：保留彈窗與草稿，僅解除「保存中」；
                        // 設定表單失敗則就地顯示錯誤並恢復輸入。
                        match &mut app.screen {
                            Screen::Settings(state) => state.saving = false,
                            Screen::SettingsForm(form) => {
                                form.busy = false;
                                form.error = Some(text.clone());
                            }
                            _ => {}
                        }
                    }
                    FailedTarget::Credentials => {
                        // 憑證操作失敗：留在可編輯表單就地顯示錯誤；敏感欄位清空、帳號保留。
                        match &mut app.screen {
                            Screen::Setup(form)
                            | Screen::Unlock(form)
                            | Screen::SettingsForm(form) => {
                                form.busy = false;
                                form.error = Some(text.clone());
                                form.clear_secrets();
                            }
                            _ => {}
                        }
                    }
                    FailedTarget::Agreement => {
                        // 協議同意保存失敗：留在閱讀畫面就地顯示錯誤，可重試。
                        if let Some(state) = app.agreement.as_mut() {
                            state.fail(message.clone());
                        }
                    }
                    _ => {
                        // 資料任務失敗：只標記對應頁面，不影響根畫面。
                    }
                }
                app.set_message(text);
            }
            // 登入嘗試若以失敗收場（包含自動重登連開始都做不到，例如離線），
            // 覆蓋層必須離開「正在登入」：進度畫面只接受 q，否則使用者會被
            // 卡在一個不會再有後續事件的畫面（連 r 都無法刷新）。
            if matches!(app.login.as_deref(), Some(LoginScreen::Progress { .. })) {
                app.login = Some(Box::new(LoginScreen::Failed {
                    site: login_site,
                    message: format!("登录未完成：{message}"),
                }));
            }
        }
    }
}

/// 目前時鐘（顯示更新時間用）。
fn now_clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 失敗事件是否屬於已經離開的資源（課程活動或活動詳情）。
///
/// 只有帶資源識別碼且與目前選取不符時才算遲到；其他頁面不受影響。
fn failed_resource_is_stale(app: &App, target: FailedTarget, resource: Option<&str>) -> bool {
    match target {
        FailedTarget::Activities => {
            resource.is_some_and(|id| app.lms.activities_course.as_deref() != Some(id))
        }
        FailedTarget::ActivityDetail => {
            resource.is_some_and(|id| app.lms.detail_activity.as_deref() != Some(id))
        }
        _ => false,
    }
}

/// 顯示登入錯誤：憑證表單就地顯示，其餘登入畫面回到失敗畫面。
fn set_login_error(app: &mut App, site: SiteKind, message: String) {
    match app.login.as_deref_mut() {
        Some(LoginScreen::Credentials { form, .. }) => {
            form.busy = false;
            form.error = Some(message);
        }
        // 已有其他登入畫面（進度、驗證碼、簡訊、失敗）：就地切換成失敗畫面。
        Some(_) => app.login = Some(Box::new(LoginScreen::Failed { site, message })),
        // 使用者已關閉覆蓋層：不要用遲到的失敗事件把彈窗重新彈出來。
        None => {}
    }
}

#[cfg(test)]
#[path = "tests/event_test.rs"]
mod event_test;

#[cfg(test)]
#[path = "tests/panic_hook_test.rs"]
mod panic_hook_test;
