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

use crate::config::{AccessPolicy, Config};
use crate::http::fake::{FakeClient, json};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::session::AccessMode;

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
            credentials: None,
            flow: None,
            retry: None,
            pending_vault: None,
            captcha_path: None,
            pending_data: VecDeque::new(),
            generation: 0,
            relogin_attempts: 0,
            cache: LmsCache::default(),
            known_term: None,
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
