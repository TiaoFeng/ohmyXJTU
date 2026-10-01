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
use crate::sites::attendance::AttendanceSite;
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
            relogin: ReloginBudgets::default(),
            cache: LmsCache::default(),
            known_term: None,
            chosen_term: None,
            shutdown: false,
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
            return courses_response();
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
        if url.ends_with("/courses/2/activities") {
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
        "取消后应通知介面解除载入中状态"
    );

    let seen = seen.lock().expect("lock").clone();
    assert!(
        !seen
            .iter()
            .any(|url| url.ends_with("/courses/2/activities")),
        "取消后不应继续查询下一门课程：{seen:?}"
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
        "強制刷新必須再執行一次完整載入"
    );
    assert_eq!(
        count(&seen, "/api/my-courses"),
        2,
        "強制刷新必須重新查詢課程"
    );
    assert_eq!(
        count(&seen, "/courses/1/activities"),
        2,
        "強制刷新必須繞過活動快取"
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
        "預熱載入應完成"
    );

    // 作業載入進行中（第一門課程的活動查詢阻塞），同時排入非強制與強制課程載入。
    worker.send(Job::LoadHomework { force: false });
    started_rx.recv_timeout(WAIT).expect("作业应开始查询");
    worker.send(Job::LoadCourses { force: false });
    worker.send(Job::LoadCourses { force: true });
    release_tx.send(()).expect("释放请求");

    assert!(
        wait_settled(|| count(&seen, "/api/my-courses"), 2),
        "佇列中的非強制載入應升級為強制並繞過快取"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "最多一次重載");
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
        "切換學期後應再執行一次強制重載"
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
        .expect("應收到學期切換事件");
    assert!(
        !events[switch_at..].iter().any(|event| matches!(
            event,
            Event::Homework(update) if update.term_label.as_deref() == Some(old_label.as_str())
        )),
        "切換學期後不得再回填舊學期的進度或結果：{:?}",
        &events[switch_at..]
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Homework(update) if update.progress.is_none()
                && update.term_label.as_deref() == Some(new_label.as_str())
        )),
        "應得到新學期的完成結果：{events:?}"
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
        "排隊的課程載入應被升級為強制並重新查詢"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "最多一次重載");
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
        "排隊的活動載入應被升級為強制並重新查詢"
    );
    assert_eq!(count(&seen, "/courses/1/activities"), 2, "最多一次重載");
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
            "兩次選擇都應被處理"
        );
    }
    // 恰有一次重載：課程查詢計數應穩定於 2（兩次選擇各排一筆的舊行為會出現第三次）。
    assert!(
        wait_settled(|| count(&seen, "/api/my-courses"), 2),
        "應恰有一次強制重載"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "不得出現第二次重載");
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
        "切換學期後應再執行一次強制重載"
    );
    assert!(
        worker.wait_event(|event| matches!(
            event,
            Event::Homework(update) if update.progress.is_none()
                && update.term_label.as_deref() == Some("2025-2026 学年 第 2 学期")
        )),
        "重載結果應為新學期"
    );
    assert_eq!(count(&seen, "/api/my-courses"), 2, "最多一次重載");
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
