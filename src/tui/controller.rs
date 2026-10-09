//! 畫面層業務邏輯：由使用者操作驅動的任務建構與頁面導航。
//!
//! 與 [`super::handler`] 的分工：handler 只做「按鍵 → 意圖」的映射；凡會
//! 建構 [`Job`] 或切換頁面／資源的邏輯都在這裡。事件套用（[`super::event`]）
//! 也共用這裡的 [`request`] 與 [`ensure_page`]。

use std::collections::HashSet;
use std::sync::mpsc::Sender;

use crate::credentials::{Credentials, Secret};
use crate::domain::todo::{self, SortMode, Task};
use crate::sites::lms::ActivityKind;
use crate::sync::config::SyncConfig;
use crate::task::Job;
use crate::tui::app::{
    App, FieldRole, FormKind, FormState, LmsLevel, NavItem, Screen, TaskBatchMenuState, TaskEntry,
    TaskField, TaskFormMode, TaskFormState, TaskMenuState, TermPickerState,
};
use crate::tui::text::InputLine;

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
            if force {
                // 強制刷新：週快取一律作廢（之後翻回任何一週都會重新查詢）。
                app.clear_schedule_weeks();
            }
            app.schedule.start_loading("正在加载课表与考勤记录…");
            let _ = jobs.send(Job::LoadSchedule { force });
        }
        NavItem::Homework => {
            app.homework
                .start_loading("正在汇总作业（需要逐门课程查询）…");
            let _ = jobs.send(Job::LoadHomework { force });
        }
        NavItem::Attendance => {
            if force {
                // 強制刷新：頁快取一律作廢（之後翻回任何一頁都會重新查詢）。
                app.clear_flow_pages();
            }
            let page = flow_page(app);
            app.attendance.start_loading("正在加载考勤流水…");
            let _ = jobs.send(Job::LoadFlow { page });
        }
        NavItem::Lms => request_lms(app, jobs, force),
    }
}

/// 思源學堂的載入請求。
///
/// `r` 的語意是「刷新目前畫面」：在活動層與詳情層要重載該層，不能把使用者彈
/// 回課程清單（原本無條件把 `level` 設回 `Courses`，`apply_courses` 因此走了
/// 「目前課程不存在」的分支，連選取的課程都會被重設為第一門）。只有層級對應
/// 的資源識別碼遺失時才逐層退回。
fn request_lms(app: &mut App, jobs: &Sender<Job>, force: bool) {
    match app.lms.level {
        LmsLevel::Detail => {
            if let Some(activity_id) = app.lms.detail_activity.clone() {
                let note = detail_loading_note(app.lms.detail.ready().map(|detail| detail.kind));
                // 保留舊詳情（stale）：重新查詢期間畫面不跳。
                app.lms.detail.start_loading(note);
                let _ = jobs.send(Job::LoadActivityDetail { activity_id, force });
                return;
            }
            app.lms.level = LmsLevel::Activities;
            request_lms(app, jobs, force);
        }
        LmsLevel::Activities => {
            if let Some(course_id) = app.lms.activities_course.clone() {
                app.lms.activities.start_loading("正在加载课程活动…");
                let _ = jobs.send(Job::LoadActivities { course_id, force });
                return;
            }
            app.lms.level = LmsLevel::Courses;
            request_lms(app, jobs, force);
        }
        LmsLevel::Courses => {
            app.lms.courses.start_loading("正在加载课程…");
            let _ = jobs.send(Job::LoadCourses { force });
        }
    }
}

/// 活動詳情的載入提示（作業才有提交記錄）。
fn detail_loading_note(kind: Option<ActivityKind>) -> &'static str {
    match kind {
        Some(ActivityKind::Homework) => "正在加载活动详情与提交记录…",
        _ => "正在加载活动详情…",
    }
}

/// 開啟目前選取項目的內容（`enter`）：展開詳情或進入下一層。
pub(super) fn activate(app: &mut App, jobs: &Sender<Job>) {
    match app.nav {
        NavItem::Schedule => app.schedule_detail = !app.schedule_detail,
        NavItem::Homework => {
            // 多選模式：enter 開啟批量操作選單，而不是切換詳情。
            if app.task_page.multi.is_some() {
                if selected_task_ids(app).is_empty() {
                    app.set_message("尚未选择任务（space 勾选）");
                } else {
                    app.set_screen(Screen::TaskBatchMenu(TaskBatchMenuState { index: 0 }));
                }
                return;
            }
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
                // 同理：換活動時不得沿用上一個活動的詳情。
                let note = detail_loading_note(Some(kind));
                if app.lms.detail_activity.as_deref() == Some(activity_id.as_str()) {
                    app.lms.detail.start_loading(note);
                } else {
                    app.lms.detail.reset_loading(note);
                }
                app.lms.detail_activity = Some(activity_id.clone());
                app.lms.level = LmsLevel::Detail;
                app.lms.detail_scroll.reset();
                let _ = jobs.send(Job::LoadActivityDetail {
                    activity_id,
                    force: false,
                });
            }
            LmsLevel::Detail => {}
        },
    }
}

/// 返回上一層（`esc`）：思源學堂逐層返回，其餘頁面收起詳情。
pub(super) fn escape(app: &mut App) {
    // 多選模式最優先（與提示列的「esc 退出多选」一致）：除了 `m` 之外，`esc`
    // 也能離開，否則提示說了卻沒反應，使用者會以為卡住。
    if app.task_page.multi.is_some() && app.nav == NavItem::Homework {
        app.task_page.multi = None;
        app.set_message("已退出多选");
        return;
    }
    match app.nav {
        NavItem::Lms => match app.lms.level {
            LmsLevel::Detail => app.lms.level = LmsLevel::Activities,
            LmsLevel::Activities => app.lms.level = LmsLevel::Courses,
            LmsLevel::Courses => {}
        },
        NavItem::Schedule => app.schedule_detail = false,
        NavItem::Homework => {
            // 篩選中時 esc 先清除篩選，再收起詳情（與 ui-ref 一致）。
            if app.task_page.filter.is_some() {
                app.task_page.filter = None;
                app.homework_state.select(Some(0));
                app.set_message("已清除筛选");
            } else {
                app.homework_detail = false;
            }
        }
        NavItem::Attendance => app.flow_detail = false,
    }
}

/// `[`／`]`：依目前頁面切換課表週次、考勤流水頁碼或分組。
///
/// 這三個動作在各頁面上都是「上一頁／下一頁」，因此共用同一組按鍵；分派由
/// 這裡負責（handler 只把按鍵轉成這個呼叫）。
pub(super) fn bracket(app: &mut App, jobs: &Sender<Job>, delta: i32) {
    match app.nav {
        NavItem::Schedule => change_schedule_week(app, jobs, delta),
        NavItem::Attendance => change_flow_page(app, jobs, delta),
        NavItem::Homework | NavItem::Lms => change_group(app, delta),
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

/// 套用任務頁的排序方式（`^L`）：立即生效並回到主畫面。
///
/// 排序只影響顯示順序；為了不讓游標跳到別的項目，切換前先記住目前選取的項目，
/// 切換後在清單中找回它（找不到時回到第一項）。
pub(super) fn set_task_sort(app: &mut App, mode: SortMode) {
    let previous = app.task_page_selected_id();
    app.task_page.sort = mode;
    app.set_screen(Screen::Main);
    app.anchor_task_selection(previous);
    app.set_message(match mode {
        SortMode::Default => "已恢复默认排序（任务在前、作业在后）".to_owned(),
        _ => format!("已按{}排序（任务与作业混合）", mode.label()),
    });
}

/// 切換課表週次（`[`／`]`）。
///
/// 該週已載入過（`App::schedule_weeks`）時直接顯示快取：翻週不再重查考勤，
/// 也不會再閃一次「載入中」——使用者要重新查詢時按 `r`（那時整個週快取作廢）。
/// 沒有快取時與切換課程／活動同理，舊週的課程不屬於目標週：清空內容
/// （`reset_loading`）而不是保留顯示，避免使用者以為看到的是目標週的課表。
/// 到邊界（第 1 週、最後一週）時不動作。
///
/// 目標週次另記在 `schedule_pending_week`：已在執行中的舊週載入不會被作廢
/// （切週指令要等它回報後才生效），其結果必須由 `event::apply_schedule`
/// 依此欄位丟棄。
///
/// 兩種情況都要把週次告訴工作者（`SetScheduleWeek`）：按 `r` 時送的是
/// `LoadSchedule`——工作者只能依自己的週次狀態決定要載入哪一週，快取命中的
/// 翻週若不同步，`r` 就會載入上一次真正查詢過的那一週。`reload` 指出介面是否
/// 還需要該週資料（快取命中時不需要）。
pub(super) fn change_schedule_week(app: &mut App, jobs: &Sender<Job>, delta: i32) {
    if app.nav != NavItem::Schedule {
        return;
    }
    let (Some(week), Some(total)) = (app.schedule_week, app.schedule_total) else {
        return;
    };
    let target = i32::try_from(week).unwrap_or(1) + delta;
    if target < 1 || target > i32::try_from(total).unwrap_or(1) {
        return;
    }

    let target = u32::try_from(target).unwrap_or(1);
    app.schedule_week = Some(target);
    app.schedule_pending_week = Some(target);
    let reload = !app.show_cached_schedule_week(target);
    if reload {
        app.schedule
            .reset_loading(format!("正在加载第 {target} 周…"));
    }
    let _ = jobs.send(Job::SetScheduleWeek {
        week: target,
        reload,
    });
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

/// 考勤流水目前「該顯示」的頁碼。
///
/// 使用者最後選定但尚未回報的頁碼優先（見 `App::flow_pending_page`），其次才是
/// 畫面上已有的資料頁碼（載入中保留的是舊資料），最後回退第 1 頁。
fn flow_page(app: &App) -> u32 {
    app.flow_pending_page
        .or_else(|| app.attendance.ready().map(|data| data.page))
        .unwrap_or(1)
}

/// 考勤流水分頁（`[`／`]`）；超出頁數範圍時不動作。
///
/// 該頁已載入過（`App::flow_pages`）時直接顯示快取：翻頁不再重查，也不會再閃
/// 一次「載入中」——使用者要重新查詢時按 `r`（那時整個頁快取作廢）。
///
/// 目標頁以 [`flow_page`] 為準：載入期間 `ready()` 仍是上一頁，拿它計算會讓
/// 連續按鍵（例如連按兩次 `]`）都算成同一頁而只前進一次。目標頁另記在
/// `App::flow_pending_page`：已在執行中的舊頁載入不會被作廢（翻頁指令要等它
/// 回報後才生效），其結果必須由 `event::apply_flow` 依此欄位丟棄——快取命中
/// 時同樣要記，否則那筆遲到的舊頁結果會把畫面換回使用者已經離開的那一頁。
pub(super) fn change_flow_page(app: &mut App, jobs: &Sender<Job>, delta: i32) {
    if app.nav != NavItem::Attendance {
        return;
    }
    let Some(total_pages) = app.attendance.ready().map(|data| data.total_pages) else {
        return;
    };
    let target = i32::try_from(flow_page(app)).unwrap_or(1) + delta;
    if target < 1 || target > i32::try_from(total_pages).unwrap_or(1) {
        return;
    }

    let target = u32::try_from(target).unwrap_or(1);
    app.flow_pending_page = Some(target);
    if app.show_cached_flow_page(target) {
        return;
    }
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
    let homework = match app.selected_entry() {
        Some(TaskEntry::Homework(item)) => Some((item.activity_id.clone(), item.course_id.clone())),
        _ => None,
    };
    let is_task = matches!(app.selected_entry(), Some(TaskEntry::Task(_)));
    let Some((activity_id, course_id)) = homework else {
        app.set_message(if is_task {
            "自定义任务没有可打开的网页"
        } else {
            "请先选择要打开的作业"
        });
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

// ── 自訂義任務（任務頁） ─────────────────────────────

/// 目前選取項目的識別資訊。
///
/// 借用 `app` 取得的選取項目無法在後續操作中存活（會與 `&mut app` 衝突），
/// 因此一律先轉成這個擁有所有權的小型列舉。
enum Selected {
    /// 自訂義任務。
    Task {
        /// 任務識別碼。
        id: u64,
        /// 任務內容（顯示於提示訊息）。
        content: String,
        /// 是否已完成。
        completed: bool,
    },
    /// 思源學堂作業（只能檢視，不能編輯或刪除）。
    Homework,
}

/// 讀取目前選取的項目。
fn selected(app: &App) -> Option<Selected> {
    app.selected_entry().map(|entry| match entry {
        TaskEntry::Task(task) => Selected::Task {
            id: task.id,
            content: task.content.clone(),
            completed: task.completed,
        },
        TaskEntry::Homework(_) => Selected::Homework,
    })
}

/// 開啟新增任務的表單（`^A`）。
pub(super) fn open_task_form(app: &mut App) {
    if app.nav != NavItem::Homework {
        return;
    }
    app.set_screen(Screen::TaskForm(Box::new(TaskFormState::add())));
}

/// 編輯目前選取的任務（`^E`）；作業列不可編輯。
pub(super) fn edit_selected_task(app: &mut App) {
    let task = match app.selected_entry() {
        Some(TaskEntry::Task(task)) => Some(task.clone()),
        _ => None,
    };
    let is_homework = matches!(app.selected_entry(), Some(TaskEntry::Homework(_)));
    match task {
        Some(task) => app.set_screen(Screen::TaskForm(Box::new(TaskFormState::edit(&task)))),
        None if is_homework => app.set_message("只能修改自定义任务"),
        None => {}
    }
}

/// 切換目前選取任務的完成狀態（`space`）；作業列不可標記。
pub(super) fn toggle_selected_task(app: &mut App, jobs: &Sender<Job>) {
    if app.nav != NavItem::Homework {
        return;
    }
    match selected(app) {
        Some(Selected::Task { id, completed, .. }) => {
            let _ = jobs.send(Job::SetTaskDone {
                id,
                done: !completed,
            });
        }
        Some(Selected::Homework) => app.set_message("只能标记自定义任务"),
        None => {}
    }
}

/// 刪除目前選取的任務（`^D`）：第一次按下要求再按一次，第二次才刪除。
pub(super) fn delete_selected_task(app: &mut App, jobs: &Sender<Job>) {
    if app.nav != NavItem::Homework {
        return;
    }
    match selected(app) {
        Some(Selected::Task { id, content, .. }) => {
            let confirmed = app
                .task_page
                .pending_delete
                .as_ref()
                .is_some_and(|(pending, _)| *pending == id);
            if confirmed {
                app.task_page.pending_delete = None;
                let _ = jobs.send(Job::DeleteTask { id });
            } else {
                app.set_message(format!("再按一次 ^D 删除「{content}」"));
                app.task_page.pending_delete = Some((id, content));
            }
        }
        Some(Selected::Homework) => app.set_message("只能删除自定义任务"),
        None => {}
    }
}

/// 進入或離開多選模式（`m`）。
pub(super) fn toggle_task_multi(app: &mut App) {
    if app.nav != NavItem::Homework {
        return;
    }
    if app.task_page.multi.is_some() {
        app.task_page.multi = None;
        app.set_message("已退出多选");
    } else {
        app.task_page.multi = Some(HashSet::new());
        app.set_screen(Screen::Main);
        app.set_message("多选模式：space 勾选 · enter 批量操作 · esc 退出");
    }
}

/// 多選模式：勾選或取消目前選取的任務（`space`）。
pub(super) fn toggle_task_selection(app: &mut App) {
    match selected(app) {
        Some(Selected::Task { id, .. }) => {
            if let Some(selection) = app.task_page.multi.as_mut()
                && !selection.remove(&id)
            {
                selection.insert(id);
            }
        }
        Some(Selected::Homework) => app.set_message("只能选择自定义任务"),
        None => {}
    }
}

/// 目前勾選的任務識別碼（依清單順序）。
///
/// 只回傳**目前可見**的項目（當前分組＋當前搜尋）：多選狀態可能跨分組或
/// 搜尋而保留（切回原分組時勾選仍在），但批量操作只應作用於使用者此刻看
/// 得到的任務，否則會刪改畫面上不存在的項目。
pub(super) fn selected_task_ids(app: &App) -> Vec<u64> {
    let Some(selection) = app.task_page.multi.as_ref() else {
        return Vec::new();
    };
    app.task_group_items(app.homework_group)
        .into_iter()
        .filter(|task| selection.contains(&task.id))
        .map(|task| task.id)
        .collect()
}

/// 開啟任務設置選單（`^T`）。
pub(super) fn open_task_menu(app: &mut App) {
    if app.nav != NavItem::Homework {
        return;
    }
    app.set_screen(Screen::TaskMenu(TaskMenuState { index: 0 }));
}

/// 開啟任務搜尋輸入框（`^F`；以目前的篩選字預填）。
pub(super) fn open_task_search(app: &mut App) {
    if app.nav != NavItem::Homework {
        return;
    }
    let input = InputLine::with_value(app.task_page.filter.clone().unwrap_or_default());
    app.task_page.search = Some(input);
    app.task_page.tag_cursor = None;
}

/// 搜尋框的標籤建議：以 `delta` 在既有標籤間循環並整段預填（`^F`）。
///
/// 一個標籤也沒有時什麼都不做——維持輸入內容，也不顯示任何提示。順序沿用任務
/// 本身的排序；第一次按 `↓` 由第一個開始、第一次按 `↑` 由最後一個開始，之後
/// 首尾循環。
pub(super) fn cycle_tag_suggestion(app: &mut App, delta: i32) {
    // 先備妥候選清單（擁有所有權），再取 `task_search` 的可變借用。
    let options = app.task_tag_options();
    let len = options.len();
    if len == 0 {
        app.task_page.tag_cursor = None;
        return;
    }
    let index = match app.task_page.tag_cursor {
        Some(index) => (index + len + if delta >= 0 { 1 } else { len - 1 }) % len,
        None if delta >= 0 => 0,
        None => len - 1,
    };
    app.task_page.tag_cursor = Some(index);
    if let Some(input) = app.task_page.search.as_mut() {
        input.set(options[index].clone());
    }
}

/// 清空任務表單目前聚焦的欄位（`^U`）。
pub(super) fn clear_focused_task_field(app: &mut App) {
    let Screen::TaskForm(form) = &mut app.screen else {
        return;
    };
    match form.focus {
        TaskField::Content => form.content.clear(),
        TaskField::Tag => form.tag.clear(),
        TaskField::Description => form.description.focused_line_mut().clear(),
        TaskField::Deadline => form.deadline.clear(),
        TaskField::Priority | TaskField::Completed => {}
    }
}

/// 送出任務表單（`^S`）；驗證失敗時就地顯示錯誤，不送出任務。
pub(super) fn submit_task_form(app: &mut App, jobs: &Sender<Job>) {
    let job = {
        let Screen::TaskForm(form) = &app.screen else {
            return;
        };
        match build_task_job(form) {
            Ok(job) => job,
            Err(message) => {
                if let Screen::TaskForm(form) = &mut app.screen {
                    form.error = Some(message);
                }
                return;
            }
        }
    };
    if let Screen::TaskForm(form) = &mut app.screen {
        form.busy = true;
        form.error = None;
    }
    let _ = jobs.send(job);
}

/// 將任務表單內容轉為任務；驗證失敗時回傳訊息。
fn build_task_job(form: &TaskFormState) -> Result<Job, String> {
    let content = form.content.value().trim().to_owned();
    if content.is_empty() {
        return Err("任务内容不能为空".to_owned());
    }
    let deadline = todo::parse_deadline_input(form.deadline.value())?;
    let tag = todo::normalize_tag(form.tag.value());
    if let Some(tag) = &tag
        && !todo::tag_fits(tag, "")
    {
        return Err(format!(
            "标签不能超过 {} 个汉字宽度",
            todo::TAG_MAX_WIDTH / 2
        ));
    }
    let description = match form.description.value().trim() {
        "" => None,
        text => Some(text.to_owned()),
    };
    let task = Task {
        id: 0,
        content,
        description,
        tag,
        deadline,
        priority: form.priority,
        completed: form.completed,
    };
    Ok(match form.mode {
        TaskFormMode::Add => Job::AddTask { task },
        TaskFormMode::Edit { id } => Job::UpdateTask { id, task },
    })
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
        FormKind::SyncConfig => Ok(Job::SetSyncConfig {
            config: sync_config_from(values)?,
        }),
    }
}

/// 由同步設定表單組出連線設定（驗證失敗時回傳訊息）。
pub(super) fn sync_config_from_form(form: &FormState) -> Result<SyncConfig, String> {
    sync_config_from(&FormValues::from_form(form))
}

/// 由表單值組出連線設定（驗證失敗時回傳訊息）。
fn sync_config_from(values: &FormValues) -> Result<SyncConfig, String> {
    let url = values.sync_url.trim();
    if url.is_empty() {
        return Err("请输入服务器地址".to_owned());
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("服务器地址必须以 http(s):// 开头".to_owned());
    }
    if values.sync_account.trim().is_empty() {
        return Err("请输入坚果云账号".to_owned());
    }
    if values.sync_app_password.is_empty() {
        return Err("请输入应用密码".to_owned());
    }
    Ok(SyncConfig::new(
        url,
        values.sync_account.trim(),
        values.sync_app_password.as_str(),
    ))
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
    /// 坚果云伺服器位址（非機密）。
    sync_url: String,
    /// 坚果云帳號（非機密）。
    sync_account: String,
    /// 坚果云應用密碼（自動零化）。
    sync_app_password: Secret,
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
            .field("sync_url", &!self.sync_url.is_empty())
            .field("sync_account", &!self.sync_account.is_empty())
            .field("sync_app_password", &!self.sync_app_password.is_empty())
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
                FieldRole::SyncUrl => values.sync_url = text.to_owned(),
                FieldRole::SyncAccount => values.sync_account = text.to_owned(),
                FieldRole::SyncAppPassword => values.sync_app_password = Secret::from(text),
            }
        }
        values
    }
}

#[cfg(test)]
#[path = "tests/controller_test.rs"]
mod controller_test;
