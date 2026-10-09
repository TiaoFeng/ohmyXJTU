//! 背景事件的套用：事件 → 介面狀態。
//!
//! [`apply_event`] 是純分派表，每個事件由一個具名函式處理；登入覆蓋層的
//! 收斂規則集中在 [`set_login_error`] 與 [`apply_failure`]（任何失敗事件都
//! 必須讓停在「正在登入」的覆蓋層轉為可重試的失敗畫面）。

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::config::AccessPolicy;
use crate::domain::homework::HomeworkGroup;
use crate::domain::semester::TermCode;
use crate::domain::todo::Task;
use crate::model::{ActivityDetailView, FlowData, ScheduleData};
use crate::session::{AccessMode, SiteKind};
use crate::sites::lms::LmsActivity;
use crate::task::{CoursesData, Event, FailedTarget, HomeworkUpdate, Job, SyncStateView};
use crate::text::{MAX_INLINE_CHARS, sanitize_inline};

use super::app::{
    App, FormKind, FormState, HomeworkData, LmsLevel, LoginScreen, Page, Screen, SettingsState,
    SyncMenuState, TermPickerState,
};
use super::controller;
use super::text::InputLine;

/// 套用背景事件。
pub(crate) fn apply_event(app: &mut App, event: Event, jobs: &Sender<Job>) {
    match event {
        Event::VaultReady => apply_vault_ready(app, jobs),
        Event::LoginProgress(note) => {
            // 登入互動是覆蓋層：底層畫面保持不變，事件只更新彈窗內容。
            // 等待取消期間忽略：遲到的進度事件屬於正在被取消的那次登入。
            if !app.login_cancel_pending {
                app.login = Some(Box::new(LoginScreen::Progress { note }));
            }
        }
        Event::LoginNeedsCaptcha(path) => apply_needs_captcha(app, path),
        Event::LoginNeedsMfa { phone, sent } => apply_needs_mfa(app, phone, sent),
        Event::LoginFailed { site, message } => set_login_error(app, site, message),
        Event::LoginCancelled => apply_login_cancelled(app),
        Event::VerificationRetry { site, message } => apply_verification_retry(app, site, message),
        Event::LoginSucceeded { site, mode } => apply_login_succeeded(app, jobs, site, mode),
        Event::SessionsCleared { account_changed } => {
            app.clear_site_modes();
            app.invalidate_data(account_changed);
        }
        Event::AgreementAccepted => {
            // 協議已同意：關閉閱讀門，揭露底層畫面（首次設定或解鎖）。
            app.agreement = None;
        }
        Event::SessionExpired { site } => app.clear_site_mode(site),
        Event::SessionDisabled(message) => apply_session_disabled(app, message),
        Event::LoadingCancelled { target } => {
            // 進行中的載入被取消：解除載入中狀態，保留已取得的部分資料。
            app.cancel_loading(target);
        }
        Event::Schedule(data) => apply_schedule(app, *data),
        Event::Tasks(tasks) => apply_tasks(app, tasks),
        Event::Homework(update) => apply_homework(app, update),
        Event::HomeworkNeedsTerm {
            options,
            suggestion,
            reason,
        } => apply_homework_needs_term(app, options, suggestion, reason),
        Event::Flow(data) => apply_flow(app, *data),
        Event::CoursesTerm(term) => {
            // 只更新分區提示：課程清單本身不變，重新繪製即會依新學期重新分區。
            app.lms.courses_term = term;
        }
        Event::Courses(data) => apply_courses(app, data),
        Event::Activities {
            course_id,
            activities,
        } => apply_activities(app, course_id, activities),
        Event::ActivityDetail(detail) => apply_activity_detail(app, *detail),
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
        Event::CredentialSaveFailed(message) => apply_credential_save_failed(app, message),
        Event::PassphraseUpdated => apply_passphrase_updated(app),
        Event::AccessPolicyUpdated(policy) => apply_access_policy_updated(app, policy),
        Event::Notice(message) => app.set_message(message),
        Event::Warning(message) => app.set_warning_message(message),
        Event::SyncState(view) => apply_sync_state(app, *view),
        Event::SyncTestResult { ok, message } => apply_sync_test_result(app, ok, message),
        Event::SyncImported { message } => apply_sync_imported(app, message),
        Event::SyncDone { summary } => app.set_message(summary),
        Event::SyncConflict { message } => app.set_warning_message(message),
        Event::Failed {
            what,
            message,
            target,
            site,
            resource,
        } => apply_failure(app, what, message, target, site, resource),
    }
}

/// 憑證已就緒：清掉殘留的登入覆蓋層，進入主畫面，並請工作者在背景預熱。
///
/// 預載任務**先送**：工作者會先登入兩個站點，再把四個頁面的載入任務排進
/// 佇列，之後送出的目前頁面載入任務會在登入完成後才執行（登入進行中資料
/// 任務一律延後）。即使順序顛倒也不會出錯，但先送可以少一次無謂的
/// 「沒登入 → 失敗 → 重登」往返。
///
/// 預載失敗只留提示（`FailedTarget::Preload` 不動任何頁面），四個頁面維持
/// 未載入，進入該頁時仍會正常載入。
fn apply_vault_ready(app: &mut App, jobs: &Sender<Job>) {
    app.login = None;
    app.login_cancel_pending = false;
    let _ = jobs.send(Job::Preload);
    if app.is_main() {
        // 修改帳號後回到主畫面：舊資料屬於舊帳號，強制刷新目前頁面。
        let nav = app.nav;
        controller::request(app, jobs, nav, true);
    } else {
        app.ensure_main();
        controller::ensure_page(app, jobs);
    }
    app.set_message("凭证已就绪");
}

/// 同步設定狀態變更：更新顯示用狀態；若同步設定表單剛保存成功則關閉它。
fn apply_sync_state(app: &mut App, view: SyncStateView) {
    let configured = view.configured;
    let form_open = matches!(
        &app.screen,
        Screen::SettingsForm(form) if form.kind == FormKind::SyncConfig
    );
    app.sync = view;
    if configured && form_open {
        app.set_screen(Screen::SyncMenu(SyncMenuState::default()));
        app.set_message("坚果云同步已启用");
    }
}

/// 堅果雲連線測試結果：通過／失敗都以底欄訊息呈現。
fn apply_sync_test_result(app: &mut App, ok: bool, message: String) {
    if ok {
        app.set_message(format!("坚果云连接成功：{message}"));
    } else {
        app.set_warning_message(format!("坚果云连接失败：{message}"));
    }
}

/// 已從堅果雲導入：本機憑證檔已就緒，提示使用者輸入加密口令。
fn apply_sync_imported(app: &mut App, message: String) {
    app.set_message(message);
}

/// 需要圖片驗證碼：保留上一次的錯誤訊息，輸入框清空。
fn apply_needs_captcha(app: &mut App, path: PathBuf) {
    // 等待取消期間忽略：遲到的驗證碼事件屬於正在被取消的那次登入，
    // 不得把它重新彈出來。
    if app.login_cancel_pending {
        return;
    }
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

/// 需要簡訊驗證碼：保留已輸入的驗證碼與錯誤訊息（重送簡訊時不應清空）。
fn apply_needs_mfa(app: &mut App, phone: Option<String>, sent: bool) {
    // 等待取消期間忽略：遲到的簡訊驗證事件屬於正在被取消的那次登入，
    // 不得把它重新彈出來。
    if app.login_cancel_pending {
        return;
    }
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

/// 驗證碼填錯（可重試）：保留原本的驗證碼／簡訊輸入畫面，讓使用者就地重輸，
/// 不必重新輸入帳號密碼（登入流程仍在工作者端保留）。
fn apply_verification_retry(app: &mut App, site: SiteKind, message: String) {
    match app.login.as_deref_mut() {
        Some(LoginScreen::Mfa { input, error, .. })
        | Some(LoginScreen::Captcha { input, error, .. }) => {
            input.clear();
            *error = Some(message);
        }
        _ => set_login_error(app, site, message),
    }
}

/// 登入取消完成：關閉覆蓋層並清除等待狀態。
///
/// 即使先前有遲到的登入事件重開過覆蓋層（例如簡訊驗證），也在這裡一併
/// 關閉；之後的登入事件（新的登入流程）再正常開啟。
fn apply_login_cancelled(app: &mut App) {
    app.login = None;
    app.login_cancel_pending = false;
}

/// 登入成功：關閉覆蓋層、記錄站點訪問方式並確保目前頁面已載入。
fn apply_login_succeeded(
    app: &mut App,
    jobs: &Sender<Job>,
    site: SiteKind,
    mode: Option<AccessMode>,
) {
    app.login = None;
    app.login_cancel_pending = false;
    match mode {
        Some(mode) => app.set_site_mode(site, mode),
        None => app.clear_site_mode(site),
    }
    if !app.is_main() {
        app.set_screen(Screen::Main);
    }
    // 若目前頁面尚未載入（例如從失敗畫面重試成功），補一次載入。
    controller::ensure_page(app, jobs);
    app.set_message("登录成功");
}

/// 會話已停用（無法建立乾淨的新會話）：回到解鎖畫面，讓使用者重新輸入
/// 加密口令以建立一個全新的會話；此前不會再發出任何請求。
fn apply_session_disabled(app: &mut App, message: String) {
    app.login = None;
    app.login_cancel_pending = false;
    app.clear_site_modes();
    app.invalidate_data(true);
    let mut form = FormState::unlock();
    form.error = Some(message);
    app.set_screen(Screen::Unlock(form));
    app.set_error_message("会话已停用：请输入加密口令重新解锁");
}

fn apply_schedule(app: &mut App, data: ScheduleData) {
    // 遲到的舊週結果：切週指令要等已在執行中的舊週載入回報後才生效，那筆
    // 結果仍會送達介面。使用者已指定別的週次時丟棄它並保留「載入中」，等
    // 目標週的結果抵達；否則畫面會閃回舊週，甚至在新週載入失敗時停在舊週
    // 的課程資料與標題（見 `App::schedule_pending_week`）。
    if app
        .schedule_pending_week
        .is_some_and(|pending| pending != data.week)
    {
        return;
    }
    app.schedule_pending_week = None;
    // 週次與總週數先取出：切週載入期間資料已清空，標題仍能顯示目標週。
    app.schedule_week = Some(data.week);
    app.schedule_total = Some(data.total_weeks);
    // 記住這一週：下次翻回同一週時直接顯示，不必再查一次考勤。
    app.store_schedule_week(&data);
    app.schedule = Page::Ready(data);
    app.updated_at.schedule = Some(now_clock());
    app.schedule_state.select(Some(0));
    app.ensure_main();
}

/// 判定不出本學期時的作業頁訊息（作業頁可見時 `s` 直接可用）。
const NEEDS_TERM_MESSAGE: &str = "未确定本学期：按 s 选择要查看的学期";
/// 同上，但畫面上有彈窗時的訊息。
///
/// 彈窗獨占按鍵（`popup_owns_keys`），`s` 根本到不了主畫面；而且這是一則暫時
/// 訊息，使用者按下的任何鍵（包含用來關閉彈窗的 `esc`）都會先把它清掉，因此
/// 必須在文字裡說清楚下一步該做什麼。
const NEEDS_TERM_POPUP_MESSAGE: &str = "未确定本学期：请先关闭当前窗口，再按 s 选择学期";

/// 背景載入判定不出本學期：標記作業頁，並在可以的時候直接開啟學期選擇器。
///
/// 學期選擇器是彈窗：只有主畫面（沒有其他彈窗）時才直接開啟。使用者可能正在
/// 任務表單或設定裡輸入，把畫面換掉會丟掉輸入內容（與 [`apply_failure`] 的
/// 「彈窗就地處理」原則一致）；這種情況下只標記作業頁並留下提示，關掉手上的
/// 彈窗後按 `s` 仍可選擇（選項已記在 `App::term_options`）。
fn apply_homework_needs_term(
    app: &mut App,
    options: Vec<TermCode>,
    suggestion: Option<TermCode>,
    reason: String,
) {
    app.term_options = options.clone();
    app.homework.fail(NEEDS_TERM_MESSAGE);
    let picker = TermPickerState::new(options, suggestion, reason);
    match app.screen {
        // 主畫面，或選擇器已經開著（例如使用者已按 `s`）：顯示／更新它。
        Screen::Main | Screen::TermPicker(_) => app.set_screen(Screen::TermPicker(picker)),
        // 其他彈窗（任務表單、設定、排序提示…）：不要搶走畫面。
        _ => app.set_message(NEEDS_TERM_POPUP_MESSAGE),
    }
}

fn apply_flow(app: &mut App, data: FlowData) {
    // 遲到的舊頁結果：翻頁指令要等已在執行中的舊頁載入回報後才生效，那筆結果
    // 仍會送達介面。使用者已指定別的頁碼時丟棄它，否則會清掉「正在加载第 N
    // 页…」、把「更新於」往回寫，畫面也顯示成不是使用者要的那一頁（與課表的
    // 切週過濾同理，見 `App::flow_pending_page`）。
    if app
        .flow_pending_page
        .is_some_and(|pending| pending != data.page)
    {
        return;
    }
    app.flow_pending_page = None;
    // 記住這一頁：下次翻回同一頁時直接顯示，不必再查一次。
    app.store_flow_page(&data);
    app.attendance = Page::Ready(data);
    app.updated_at.attendance = Some(now_clock());
    app.flow_state.select(Some(0));
    app.ensure_main();
}

/// 作業更新（部分結果或終態）：部分結果維持載入中並保留已累積資料。
fn apply_homework(app: &mut App, update: HomeworkUpdate) {
    // 記下目前選取的項目：新結果可能重新排序或改動分組，選取要跟著識別碼。
    let previous = app.task_page_selected_id();
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
        courses_skipped: update.courses_skipped,
        term_options: update.term_options,
        items: update.items,
        issues: update.issues,
        courses_failed: update.courses_failed,
        progress,
    };
    app.term_options = data.term_options.clone();
    app.updated_at.homework = Some(now_clock());
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
    app.anchor_task_selection(previous);
    if finished {
        // 新一輪結果取代了畫面上的作業：詳情捲動回到頂端。
        app.homework_scroll.reset();
        app.set_message(format!(
            "已更新作业：未完成 {unfinished} 项（用时 {:.1}s）",
            elapsed.as_secs_f32()
        ));
    }
    app.ensure_main();
}

/// 任務快照更新：取代清單，並把選取錨定回原本的任務。
///
/// 任務可能因為標記完成而換分組（不在目前分組時就找不到），因此優先用識別碼
/// 重新定位；找不到才把索引夾在新長度內。
///
/// 表單**正在送出**（`busy`）時收到的快照就是那次操作的結果：面板必須關起來
///（否則會永遠停在「正在保存…」，連 `esc` 都被 busy 擋住）。尚未送出的表單
///（使用者剛按下 `^a`／`^e`）不得關閉——背景任務操作（例如 `space` 標記完成）
/// 的快照可能恰好在此時抵達，關掉會讓剛輸入的內容消失。
///
/// 這裡刻意不切換根畫面：解鎖時任務服務會先回報一次快照（`InitTasks` 排在
/// `VaultReady` 之前送出），那時介面還停在解鎖表單，不該被任務快照拉進主畫面
/// ——進入主畫面由 `VaultReady` 負責。
fn apply_tasks(app: &mut App, tasks: Vec<Task>) {
    let previous = app.task_page_selected_id();
    app.task_page.tasks = tasks;
    if let Screen::TaskForm(form) = &app.screen
        && form.busy
    {
        app.set_screen(Screen::Main);
    }
    // 已刪除的任務不再存在：把殘留的勾選一併清掉，避免多選集合持續累積。
    let alive: Vec<u64> = app.task_page.tasks.iter().map(|task| task.id).collect();
    if let Some(selection) = app.task_page.multi.as_mut() {
        selection.retain(|id| alive.contains(id));
    }
    app.anchor_task_selection(previous);
}

/// 課程清單更新：以穩定的課程識別碼重新定位目前課程。
fn apply_courses(app: &mut App, data: CoursesData) {
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

/// 活動清單更新；遲到的舊課程回應直接忽略。
fn apply_activities(app: &mut App, course_id: String, activities: Vec<LmsActivity>) {
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

/// 活動詳情更新；遲到的舊活動回應直接忽略。
fn apply_activity_detail(app: &mut App, detail: ActivityDetailView) {
    // 遲到的回應：使用者已經改看別的活動。
    if app.lms.detail_activity.as_deref() != Some(detail.id.as_str()) {
        return;
    }
    app.lms.detail = Page::Ready(detail);
    app.lms.detail_scroll.reset();
    app.updated_at.lms = Some(now_clock());
    app.ensure_main();
}

/// 憑證保存失敗：解除表單的「處理中」狀態並就地顯示錯誤，否則表單會卡住
/// 而連 Esc 都被忽略。
fn apply_credential_save_failed(app: &mut App, message: String) {
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
    app.set_error_message(message);
}

/// 修改口令成功：離開處理中狀態並回到設定選單。
fn apply_passphrase_updated(app: &mut App) {
    if matches!(
        &app.screen,
        Screen::SettingsForm(form) if form.kind == FormKind::ChangePassphrase
    ) {
        app.set_screen(Screen::Settings(SettingsState::open(app.access_policy)));
    }
    app.set_message("加密口令已更新");
}

/// 訪問模式已保存：設定彈窗若開著，更新草稿並解除「保存中」，彈窗不關閉。
fn apply_access_policy_updated(app: &mut App, policy: AccessPolicy) {
    app.access_policy = policy;
    if let Screen::Settings(state) = &mut app.screen {
        state.draft = Some(policy);
        state.saving = false;
    }
    app.set_message(format!(
        "访问模式已切换为 {}；按 r 刷新当前页面",
        policy.label()
    ));
}

/// 失敗事件：標記受影響的頁面並讓相關表單／覆蓋層收斂。
fn apply_failure(
    app: &mut App,
    what: String,
    message: String,
    target: FailedTarget,
    site: Option<SiteKind>,
    resource: Option<String>,
) {
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
            FailedTarget::Settings | FailedTarget::Sync => {
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
                    Screen::Setup(form) | Screen::Unlock(form) | Screen::SettingsForm(form) => {
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
            FailedTarget::Tasks => {
                // 任務保存失敗：表單就地顯示錯誤並解除「保存中」，否則只在底欄提示。
                if let Screen::TaskForm(form) = &mut app.screen {
                    form.busy = false;
                    form.error = Some(text.clone());
                }
            }
            _ => {
                // 資料任務失敗：只標記對應頁面，不影響根畫面。
            }
        }
        app.set_error_message(text);
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
///
/// 訊息可能直接來自伺服器（登入頁的 `el-alert` 文字，見 `auth::login`），因此
/// 與其他伺服器文字走同一套清理（[`sanitize_inline`]）：控制字元與超長內容
/// 都不該進入畫面。
fn set_login_error(app: &mut App, site: SiteKind, message: String) {
    let message = sanitize_inline(&message, MAX_INLINE_CHARS);
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

/// 套用背景事件（測試用：供跨模組的「工作者 → 介面」串接測試呼叫）。
#[cfg(test)]
pub(crate) fn apply_event_for_test(app: &mut App, event: Event) {
    let (jobs, _rx) = std::sync::mpsc::channel();
    apply_event(app, event, &jobs);
}

#[cfg(test)]
#[path = "tests/event_test.rs"]
mod event_test;
