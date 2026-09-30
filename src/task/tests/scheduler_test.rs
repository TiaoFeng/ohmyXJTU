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
        // 跳過登入流程：兩站直接視為已登入。
        session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
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
            cache: LmsCache::default(),
            known_term: None,
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
    // 長查詢期間重複按 r：應被合併成一次。
    worker.send(Job::LoadHomework { force: true });
    worker.send(Job::LoadHomework { force: true });
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
