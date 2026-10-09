//! 調度行為測試：控制任務優先、重複刷新合併、代際取消。
//!
//! 以真正的 [`Worker::run`] 執行緒搭配「可控閘門」的假客戶端，驗證長時間的
//! 資料載入不會阻塞設定操作、重複刷新會被合併，且訪問模式變更後，進行中的
//! 任務會在下一個步驟立即中止。

use std::sync::mpsc::channel as std_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::auth::rsa;
use crate::config::{AccessPolicy, Config};
use crate::http::fake::{FakeClient, json};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::session::AccessMode;
use crate::sites::attendance::{self, AttendanceSite};
use crate::sites::lms::{self, ActivityKind, LmsSite};

use super::fixtures::{LMS_POST, login_page, login_page_with_mfa, public_key_pem};
use super::*;

/// 事件等待時限。
const WAIT: Duration = Duration::from_secs(3);

/// 線程化的工作執行緒（真的跑 [`Worker::run`]）。
struct ThreadWorker {
    jobs: Sender<Job>,
    events: Receiver<Event>,
    handle: Option<thread::JoinHandle<()>>,
    _dir: TempDir,
}

impl ThreadWorker {
    fn new(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        Self::with_login(responder, true)
    }

    /// 只標記思源學堂已登入（考勤未登入；學期僅能由記憶解析）。
    fn lms_only(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        Self::with_login(responder, false)
    }

    fn with_login(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
        login_attendance: bool,
    ) -> Self {
        let dir = TempDir::new().expect("建立暂存目录");
        let vault = Vault::at(dir.path().join("credentials.vault"));
        let config = Config {
            access_policy: AccessPolicy::Direct,
            save_path: Some(dir.path().join("config.json")),
            ..Config::default()
        };

        let client = Arc::new(FakeClient::with_responder(responder));
        let direct: Arc<dyn HttpClient> = Arc::clone(&client) as Arc<dyn HttpClient>;
        let webvpn: Arc<dyn HttpClient> = client as Arc<dyn HttpClient>;
        let mut session = SessionManager::with_clients(&config, direct, webvpn);
        session.register(Box::new(AttendanceSite));
        session.register(Box::new(LmsSite));
        // 跳過登入流程：站點直接視為已登入。
        if login_attendance {
            session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
        }
        session.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());

        let (job_tx, job_rx) = std_channel();
        let (event_tx, event_rx) = std_channel();
        let tasks = crate::task::tasks::detached();
        let vault_path = vault.path().to_path_buf();
        let webdav: Arc<dyn HttpClient> =
            Arc::new(FakeClient::with_responder(|_: &HttpRequest| {
                Err(crate::error::AppError::config("测试未注入 WebDAV 后端"))
            }));
        let mut worker = Worker {
            jobs: job_rx,
            events: event_tx,
            vault,
            config,
            session: Some(session),
            // 憑證供自動重新登入使用（登入態失效的測試會走到 `begin_login`）。
            credentials: Some(Credentials::new("3120000001", "old-password")),
            flow: None,
            retry: None,
            pending_vault: None,
            login_site: None,
            login_submitted_credentials: false,
            login_failures: HashMap::new(),
            login_failure_key: None,
            captcha_path: None,
            pending_data: VecDeque::new(),
            generation: 0,
            homework_epoch: 0,
            relogin: AttemptBudgets::new(MAX_LOGIN_ATTEMPTS),
            retries: AttemptBudgets::new(MAX_ATTEMPTS),
            preload_pending: false,
            cache: LmsCache::default(),
            schedule_cache: None,
            schedule_week: None,
            known_term: None,
            chosen_term: None,
            timing: LoadTiming::default(),
            login_started: None,
            sync: crate::sync::config::SyncStore::at(dir.path().join("sync.vault")),
            webdav,
            pending_sync: None,
            sync_local: vec![
                (crate::sync::config::SyncFile::Credentials, vault_path),
                (
                    crate::sync::config::SyncFile::Tasks,
                    dir.path().join("tasks.vault"),
                ),
            ],
            shutdown: false,
            tasks,
        };

        let handle = thread::spawn(move || worker.run());

        Self {
            jobs: job_tx,
            events: event_rx,
            handle: Some(handle),
            _dir: dir,
        }
    }

    fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// 送出結束指令並等待工作執行緒真的結束。
    ///
    /// 用來確定「該送的請求都送完了」：若載入仍在進行，`run` 不會返回。
    fn shutdown_and_join(&mut self) {
        let _ = self.jobs.send(Job::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// 等待符合條件的事件（會依序消耗事件）。
    fn wait_event(&self, predicate: impl Fn(&Event) -> bool) -> bool {
        let deadline = Instant::now() + WAIT;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(remaining) {
                Ok(event) => {
                    if predicate(&event) {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
        false
    }

    /// 收集目前為止已送出的所有事件（通道閒置即停止）。
    fn drain_events(&self) -> Vec<Event> {
        let mut collected = Vec::new();
        while let Ok(event) = self.events.recv_timeout(Duration::from_millis(100)) {
            collected.push(event);
        }
        collected
    }
}

impl Drop for ThreadWorker {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 作業流程常用的假回應（課程兩門、無作業）。
fn courses_response() -> AppResult<HttpResponse> {
    Ok(json(serde_json::json!({ "courses": [
        { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        { "id": "2", "name": "操作系统", "semester": { "code": "2026-1" } },
    ]})))
}

/// 考勤當前學期的假回應。
fn semester_response() -> AppResult<HttpResponse> {
    Ok(json(serde_json::json!({ "code": 0, "data": [{
        "semesterId": "s-1",
        "academicYear": "2026-2027",
        "semesterName": "第一学期",
        "startDate": "2026-09-07",
    }]})))
}

/// 請求 URL 後綴的出現次數。
fn count(seen: &Arc<Mutex<Vec<String>>>, needle: &str) -> usize {
    seen.lock()
        .expect("lock")
        .iter()
        .filter(|url| url.ends_with(needle))
        .count()
}

/// 等待條件成立（輪詢至期限）。
fn wait_until(predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    predicate()
}

/// 等待計數穩定於期望值（避免後續載入尚未開始時提早判定）。
fn wait_settled(mut observe: impl FnMut() -> usize, expected: usize) -> bool {
    let deadline = Instant::now() + WAIT;
    let mut stable_since: Option<Instant> = None;
    while Instant::now() < deadline {
        if observe() == expected {
            match stable_since {
                Some(at) if at.elapsed() >= Duration::from_millis(250) => return true,
                Some(_) => {}
                None => stable_since = Some(Instant::now()),
            }
        } else {
            stable_since = None;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn control_jobs_interrupt_homework_between_steps() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    // 阻塞訊號需跨執行緒共用：Sender/Receiver 並非 Sync，以 Mutex 包裝。
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            // 六門課程：一批只抓前三門，第四門之後屬於下一批。
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
                { "id": "2", "name": "操作系统", "semester": { "code": "2026-1" } },
                { "id": "3", "name": "计算机网络", "semester": { "code": "2026-1" } },
                { "id": "4", "name": "数据库原理", "semester": { "code": "2026-1" } },
                { "id": "5", "name": "软件工程", "semester": { "code": "2026-1" } },
                { "id": "6", "name": "数字逻辑", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            // 第一門課程的請求先阻塞，讓測試有機會插入控制任務。
            started
                .lock()
                .expect("lock")
                .send(())
                .expect("发送开始信号");
            release_rx.lock().expect("lock").recv().expect("等待释放");
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") || url.ends_with("/courses/3/activities") {
            // 同一批內的其餘課程（與第一門同時送出）。
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: false });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    // 長查詢期間切換訪問模式：應在下一個步驟之間被處理。
    worker.send(Job::SetAccessPolicy(AccessPolicy::WebVpn));
    release_tx.send(()).expect("释放请求");

    assert!(
        worker
            .wait_event(|event| matches!(event, Event::AccessPolicyUpdated(AccessPolicy::WebVpn))),
        "设置应在单一请求内生效"
    );
    assert!(
        worker.wait_event(|event| matches!(event, Event::Notice(text) if text.contains("已取消"))),
        "代际变更后应取消剩余查询"
    );
    assert!(
        worker.wait_event(|event| matches!(
            event,
            Event::LoadingCancelled {
                target: FailedTarget::Homework
            }
        )),
        "取消后应通知界面解除加载中状态"
    );

    // 活動查詢以「一批（並行上限）門課程」為單位送出：取消發生在批次之後，
    // 因此在途的請求可能已經送出（結果一律丟棄），但**不得再啟動下一批**。
    let seen = seen.lock().expect("lock").clone();
    assert!(
        !seen
            .iter()
            .any(|url| url.ends_with("/courses/4/activities")),
        "取消后不得再启动下一批查询：{seen:?}"
    );
}

#[test]
fn detail_prefetch_does_not_starve_control_jobs() {
    // 一門課程含多項作業時，詳情預取必須以「一個波次（並行上限）」為界：
    // 第一個詳情請求被擋住期間送出結束指令，不得把整門課程的詳情全部抓完
    // 才處理。維持有界並行（一批最多 3 項），但不讓波次隨作業數成長。
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);

    let site_seen = Arc::clone(&seen);
    let mut worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            // 六項作業：一批詳情就會是兩個波次。
            return Ok(json(serde_json::json!({ "activities": (11..=16).map(|id| {
                serde_json::json!({
                    "id": id.to_string(), "type": "homework",
                    "title": format!("作业{id}"),
                    "end_time": "2099-12-31 23:59:59",
                })
            }).collect::<Vec<_>>() })));
        }
        if url.contains("/api/activities/") {
            if url.ends_with("/api/activities/11") {
                // 第一個詳情請求：擋住，讓測試有機會插入結束指令。
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({
                "id": "11", "type": "homework", "title": "作业",
                "end_time": "2099-12-31 23:59:59",
                "submit_by_group": false, "user_submit_count": 0,
            })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: false });
    started_rx.recv_timeout(WAIT).expect("应开始查询详情");
    worker.send(Job::Shutdown);
    release_tx.send(()).expect("释放请求");
    worker.shutdown_and_join();

    let requested = seen.lock().expect("lock").clone();
    let details = requested
        .iter()
        .filter(|url| url.contains("/api/activities/"))
        .count();
    assert!(
        details <= 3,
        "结束指令后不得继续抓取剩余详情（预取应以一个波次为界）：{requested:?}"
    );
}

#[test]
fn duplicate_refreshes_are_coalesced() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: true });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    // 長查詢期間重複按 r：應被合併成一次（含較弱的非強制請求，不得降級）。
    worker.send(Job::LoadHomework { force: true });
    worker.send(Job::LoadHomework { force: true });
    worker.send(Job::LoadHomework { force: false });
    release_tx.send(()).expect("释放请求");

    assert!(
        worker.wait_event(
            |event| matches!(event, Event::Homework(update) if update.progress.is_none())
        ),
        "应完成最终更新"
    );

    let counts = |needle: &str| {
        seen.lock()
            .expect("lock")
            .iter()
            .filter(|url| url.ends_with(needle))
            .count()
    };
    assert_eq!(counts("/api/my-courses"), 1, "重复刷新应被合并");
    assert_eq!(counts("/courses/1/activities"), 1);
    assert_eq!(counts("/courses/2/activities"), 1);
}

/// 非強制作業載入進行中按 `r`：強制刷新不得被吞掉，必須再跑一次完整重載。
#[test]
fn forced_refresh_during_non_forced_homework_load_is_not_swallowed() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: false });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    // 非強制載入進行中按 r：應保留一次強制重載。
    worker.send(Job::LoadHomework { force: true });
    release_tx.send(()).expect("释放请求");

    // 強制重載是第二次完整載入：最後一項請求（第二門課程的活動）穩定出現
    // 第二次，即代表重載確實執行完畢。
    assert!(
        wait_settled(|| count(&seen, "/courses/2/activities"), 2),
        "强制刷新必须再执行一次完整加载"
    );
    assert_eq!(
        count(&seen, "/api/my-courses"),
        2,
        "强制刷新必须重新查询课程"
    );
    assert_eq!(
        count(&seen, "/courses/1/activities"),
        2,
        "强制刷新必须绕过活动缓存"
    );
    assert_eq!(count(&seen, "/courses/2/activities"), 2);
}

/// 作業載入期間排入一組「非強制 + 強制」課程載入：佇列中的非強制任務應原位升級。
#[test]
fn forced_refresh_upgrades_a_queued_non_forced_load() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    // 預熱課程快取（此時不得阻塞）。
    worker.send(Job::LoadCourses { force: false });
    assert!(
        worker.wait_event(|event| matches!(event, Event::Courses(_))),
        "预热加载应完成"
    );

    // 作業載入進行中（第一門課程的活動查詢阻塞），同時排入非強制與強制課程載入。
    worker.send(Job::LoadHomework { force: false });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    worker.send(Job::LoadCourses { force: false });
    worker.send(Job::LoadCourses { force: true });
    release_tx.send(()).expect("释放请求");

    assert!(
        wait_settled(|| count(&seen, "/api/my-courses"), 2),
        "队列中的非强制加载应升级为强制并绕过缓存"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "最多一次重载");
}
/// 切換學期必須使進行中的作業載入失效，不得再回填舊學期的進度與結果。
#[test]
fn switching_term_cancels_the_running_load_without_old_results() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::lms_only(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    // 先選定學期甲：自動排入一次強制載入，並在第一門課程的活動查詢阻塞。
    worker.send(Job::SetHomeworkTerm {
        term: "2026-2027-1".to_owned(),
    });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    // 載入進行中切換到學期乙。
    worker.send(Job::SetHomeworkTerm {
        term: "2025-2026-2".to_owned(),
    });
    release_tx.send(()).expect("释放请求");

    assert!(
        wait_settled(|| count(&seen, "/api/my-courses"), 2),
        "切换学期后应再执行一次强制重载"
    );

    // 舊學期的載入必須在切換後立即中止：切換後不得再出現任何舊學期的進度
    // 或完成結果（切換前已發出的初始進度不在此限）。
    let events = worker.drain_events();
    let old_label = TermCode::parse("2026-2027-1").expect("学期").label();
    let new_term = TermCode::parse("2025-2026-2").expect("学期");
    let new_label = new_term.label();
    let switch_at = events
        .iter()
        .position(|event| matches!(event, Event::CoursesTerm(Some(term)) if *term == new_term))
        .expect("应收到学期切换事件");
    assert!(
        !events[switch_at..].iter().any(|event| matches!(
            event,
            Event::Homework(update) if update.term_label.as_deref() == Some(old_label.as_str())
        )),
        "切换学期后不得再回填旧学期的进度或结果：{:?}",
        &events[switch_at..]
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Homework(update) if update.progress.is_none()
                && update.term_label.as_deref() == Some(new_label.as_str())
        )),
        "应得到新学期的完成结果：{events:?}"
    );
}
/// 課程單步載入進行中收到強制刷新：排隊的下一筆必須是強制（不得被吞或降級）。
#[test]
fn forced_refresh_is_kept_behind_a_running_non_forced_course_load() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return courses_response();
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadCourses { force: false });
    started_rx.recv_timeout(WAIT).expect("课程应开始查询");
    // 非強制載入進行中：重複的非強制請求與一次強制刷新。
    worker.send(Job::LoadCourses { force: false });
    worker.send(Job::LoadCourses { force: true });
    release_tx.send(()).expect("释放请求");

    assert!(
        wait_settled(|| count(&seen, "/api/my-courses"), 2),
        "排队的课程加载应被升级为强制并重新查询"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "最多一次重载");
}

/// 活動單步載入進行中收到強制刷新：同樣必須保留強制任務。
#[test]
fn forced_refresh_is_kept_behind_a_running_non_forced_activity_load() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadActivities {
        course_id: "1".to_owned(),
        force: false,
    });
    started_rx.recv_timeout(WAIT).expect("活动应开始查询");
    worker.send(Job::LoadActivities {
        course_id: "1".to_owned(),
        force: false,
    });
    worker.send(Job::LoadActivities {
        course_id: "1".to_owned(),
        force: true,
    });
    release_tx.send(()).expect("释放请求");

    assert!(
        wait_settled(|| count(&seen, "/courses/1/activities"), 2),
        "排队的活动加载应被升级为强制并重新查询"
    );
    assert_eq!(count(&seen, "/courses/1/activities"), 2, "最多一次重载");
}

/// 學期載入中連續兩次選擇：佇列只留一筆強制重載（不得重複完整重載）。
#[test]
fn set_homework_term_keeps_a_single_forced_reload() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: false });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    // 載入進行中連續兩次選擇學期：都應被記住，但只留一筆強制重載。
    worker.send(Job::SetHomeworkTerm {
        term: "2025-2026-2".to_owned(),
    });
    worker.send(Job::SetHomeworkTerm {
        term: "2026-2027-1".to_owned(),
    });
    release_tx.send(()).expect("释放请求");

    for _ in 0..2 {
        assert!(
            worker.wait_event(
                |event| matches!(event, Event::Notice(text) if text.contains("已记住学期"))
            ),
            "两次选择都应被处理"
        );
    }
    // 恰有一次重載：課程查詢計數應穩定於 2（兩次選擇各排一筆的舊行為會出現第三次）。
    assert!(
        wait_settled(|| count(&seen, "/api/my-courses"), 2),
        "应恰有一次强制重载"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "不得出现第二次重载");
}

/// 強制載入進行中切換學期：必須再排入一次強制重載（切換不得被吞掉）。
#[test]
fn set_homework_term_reloads_while_a_forced_load_runs() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let signalled = Mutex::new(false);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::lms_only(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/courses/1/activities") {
            let mut signalled = signalled.lock().expect("lock");
            if !*signalled {
                *signalled = true;
                started
                    .lock()
                    .expect("lock")
                    .send(())
                    .expect("发送开始信号");
                release_rx.lock().expect("lock").recv().expect("等待释放");
            }
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    // 先選定初始學期：設定被記住並自動排入一次強制載入（考勤未登入，學期僅由記憶解析）。
    worker.send(Job::SetHomeworkTerm {
        term: "2026-2027-1".to_owned(),
    });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");

    // 強制載入進行中再次切換學期：必須再排入恰一次強制重載。
    worker.send(Job::SetHomeworkTerm {
        term: "2025-2026-2".to_owned(),
    });
    release_tx.send(()).expect("释放请求");

    assert!(
        wait_until(|| count(&seen, "/api/my-courses") == 2),
        "切换学期后应再执行一次强制重载"
    );
    assert!(
        worker.wait_event(|event| matches!(
            event,
            Event::Homework(update) if update.progress.is_none()
                && update.term_label.as_deref() == Some("2025-2026 学年 第 2 学期")
        )),
        "重载结果应为新学期"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "最多一次重载");
}

/// 長查詢期間按 `o` 開啟網頁：應在下一個步進邊界執行，不必等整輪載入。
#[test]
fn interactive_open_runs_between_homework_steps() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.ends_with("/courses/1/activities") {
            // 第一門課程的請求先阻塞，讓測試有機會插入 `o` 的開啟任務。
            started
                .lock()
                .expect("lock")
                .send(())
                .expect("发送开始信号");
            release_rx.lock().expect("lock").recv().expect("等待释放");
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: false });
    started_rx
        .recv_timeout(WAIT)
        .expect("作业应开始查询第一门课程");
    worker.send(Job::OpenActivity {
        activity_id: "42".to_owned(),
        course_id: Some("1".to_owned()),
        kind: ActivityKind::Homework,
    });
    release_tx.send(()).expect("释放请求");

    // 收集事件直到作業終態，再多收一輪尾隨事件：舊行為下 OpenUrl 會落在此時。
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("作业加载应在时限内完成");
        let event = worker.events.recv_timeout(remaining).expect("等待事件");
        let finished = matches!(&event, Event::Homework(update) if update.progress.is_none());
        events.push(event);
        if finished {
            break;
        }
    }
    events.extend(worker.drain_events());

    let open_at = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/course/1/homework"
            )
        })
        .expect("按 o 应开启作业网页");
    let done_at = events
        .iter()
        .position(|event| matches!(event, Event::Homework(update) if update.progress.is_none()))
        .expect("作业加载应有终态事件");
    assert!(
        open_at < done_at,
        "开启作业网页不应等待整轮加载：{events:?}"
    );

    let seen = seen.lock().expect("lock").clone();
    assert!(
        seen.iter()
            .any(|url| url.ends_with("/courses/2/activities")),
        "开启网页不得中断作业加载：{seen:?}"
    );
}

/// 長查詢期間按 `o` 觸發重新登入：本輪載入暫停並重新排隊，登入取消後自動續跑。
#[test]
fn interactive_open_pauses_homework_until_relogin_settles() {
    // 預先產生測試公鑰（2048 位元金鑰產生延遲變異大）：放到時限之外，
    // 否則重新登入流程會把金鑰產生的時間算進等待預算而間歇逾時。
    let _ = public_key_pem();

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::new(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == lms::LOGIN_URL {
            return Ok(HttpResponse::new(200, LMS_POST, login_page_with_mfa()));
        }
        if url.contains("/mfa/detect") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "state": "s-1", "need": true }
            })));
        }
        if url.contains("/initByType/securephone") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "securePhone": "138****1234", "gid": "g-1" }
            })));
        }
        if url.ends_with("/api/my-courses") {
            return courses_response();
        }
        if url.ends_with("/timetable/semesters") {
            return semester_response();
        }
        if url.contains("/api/lessons/7/player-url") {
            // 播放器網址查詢回報登入態失效：最終位址落在統一認證。
            return Ok(HttpResponse::new(
                200,
                "https://login.xjtu.edu.cn/cas/login?service=lms",
                login_page(),
            ));
        }
        if url.ends_with("/courses/1/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            // 第二門課程的請求先阻塞，讓測試有機會在載入中途插入 `o`。
            started
                .lock()
                .expect("lock")
                .send(())
                .expect("发送开始信号");
            release_rx.lock().expect("lock").recv().expect("等待释放");
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::LoadHomework { force: false });
    started_rx
        .recv_timeout(WAIT)
        .expect("作业应开始查询第二门课程");
    worker.send(Job::OpenActivity {
        activity_id: "7".to_owned(),
        course_id: Some("1".to_owned()),
        kind: ActivityKind::Lesson,
    });
    release_tx.send(()).expect("释放请求");

    // 等到登入流程停在簡訊驗證：此時本輪載入必須已暫停（沒有終態事件）。
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("登录应停在短信验证");
        let event = worker.events.recv_timeout(remaining).expect("等待事件");
        let needs_mfa = matches!(&event, Event::LoginNeedsMfa { .. });
        events.push(event);
        if needs_mfa {
            break;
        }
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Homework(update) if update.progress.is_none())),
        "暂停生效前不得出现作业终态事件：{events:?}"
    );
    assert_eq!(
        count(&seen, "/courses/2/activities"),
        1,
        "暂停期间不得继续查询：{:?}",
        seen.lock().expect("lock")
    );

    // 使用者取消登入：本輪載入應自動重跑並完成（已取得的資料走快取）。
    worker.send(Job::CancelLogin);
    assert!(
        wait_settled(|| count(&seen, "/timetable/semesters"), 2),
        "取消登录后作业加载应重新开始：{:?}",
        seen.lock().expect("lock")
    );

    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("重跑应在时限内完成");
        let event = worker.events.recv_timeout(remaining).expect("等待事件");
        let finished = matches!(&event, Event::Homework(update) if update.progress.is_none());
        events.push(event);
        if finished {
            break;
        }
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Homework(update) if update.progress.is_none())),
        "重新排队的作业加载应完成：{events:?}"
    );
}

/// 登入請求進行中按 esc 取消：遲到的開窗事件之後必須跟著取消完成，
/// 且取消完成之後不得再出現任何開窗事件（介面已據此關閉覆蓋層）。
#[test]
fn cancelling_a_login_in_flight_reports_completion_after_late_events() {
    // 預先產生測試公鑰（2048 位元金鑰產生延遲變異大）：放到時限之外。
    let _ = public_key_pem();

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std_channel::<()>();
    let (release_tx, release_rx) = std_channel::<()>();
    let started = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);

    let site_seen = Arc::clone(&seen);
    let worker = ThreadWorker::lms_only(move |request: &HttpRequest| {
        let url = request.url.clone();
        site_seen.lock().expect("lock").push(url.clone());

        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(
                200,
                attendance::LOGIN_URL,
                login_page_with_mfa(),
            ));
        }
        if url.contains("/mfa/detect") {
            // MFA 偵測回應先阻塞，讓測試有機會在請求進行中送出取消。
            started
                .lock()
                .expect("lock")
                .send(())
                .expect("发送开始信号");
            release_rx.lock().expect("lock").recv().expect("等待释放");
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "state": "s-1", "need": true }
            })));
        }
        if url.contains("/initByType/securephone") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "securePhone": "138****1234", "gid": "g-1" }
            })));
        }
        panic!("未预期的请求：{url}");
    });

    worker.send(Job::RetryLogin {
        site: SiteKind::Attendance,
    });
    started_rx.recv_timeout(WAIT).expect("应开始 MFA 检测");
    // 請求仍在進行中：此時取消（介面按 esc 後送出的就是這個任務）。
    worker.send(Job::CancelLogin);
    release_tx.send(()).expect("释放请求");

    // 收集事件直到取消完成。
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("取消应在时限内完成");
        let event = worker.events.recv_timeout(remaining).expect("等待事件");
        let cancelled = matches!(&event, Event::LoginCancelled);
        events.push(event);
        if cancelled {
            break;
        }
    }
    events.extend(worker.drain_events());

    let mfa_at = events
        .iter()
        .position(|event| matches!(event, Event::LoginNeedsMfa { .. }))
        .expect("MFA 检测完成后应要求短信验证（迟到的开窗事件）");
    let cancelled_at = events
        .iter()
        .position(|event| matches!(event, Event::LoginCancelled))
        .expect("取消必须回报完成");
    assert!(
        mfa_at < cancelled_at,
        "取消完成必须出现在迟到的登录事件之后：{events:?}"
    );
    assert!(
        !events[cancelled_at + 1..].iter().any(|event| matches!(
            event,
            Event::LoginProgress(_) | Event::LoginNeedsCaptcha(_) | Event::LoginNeedsMfa { .. }
        )),
        "取消完成之后不得再出现开窗事件：{events:?}"
    );
}
