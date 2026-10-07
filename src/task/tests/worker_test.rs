//! 背景工作執行緒測試：憑證重輸、待重試任務與保險庫寫入時機。
//!
//! 以假 HTTP 客戶端離線組出「登入頁 → 公鑰 → 提交帳密 → 業務收尾」的完整流程，
//! 驗證重新輸入的憑證只在登入成功後才寫入保險庫，且登入失敗不會遺失待重試的任務。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::auth::{AccountType, LoginReply, rsa};
use crate::config::AccessPolicy;
use crate::domain::attendance_match::LessonAttendance;
use crate::domain::homework::HomeworkState;
use crate::domain::semester::{TermCode, TermSource};
use crate::domain::todo::{Priority, Task};
use crate::error::NetworkKind;
use crate::http::fake::{FakeClient, html, json};
use crate::http::{Body, HttpClient, HttpRequest, HttpResponse, Method};
use crate::session::AccessMode;
use crate::sites::attendance::{AttendanceSite, AttendanceStatus};
use crate::sites::lms::LmsSite;
use crate::sites::lms::{BODY_FIELD_TYPE_NOTE, BODY_NOT_OBJECT_NOTE};
use crate::sites::{attendance, lms};
use crate::task::protocol::{DataKey, HomeworkUpdate};
use crate::task::tasks::store::TaskStore;
use crate::tui::app::{App, LoginScreen};
use crate::tui::text::InputLine;

use super::fixtures::{
    ATTENDANCE_EXCHANGE, ATTENDANCE_POST, ATTENDANCE_TARGET, LMS_COURSES, LMS_HOME, LMS_POST,
    TARGET_BODY, login_page, login_page_with_mfa, public_key_pem,
};
use super::*;

/// 假的站點流程；`public_key_failures` 表示前幾次公鑰請求回傳非 PEM 正文。
fn fake_flow(
    public_key_failures: usize,
) -> impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static {
    let fetches = AtomicUsize::new(0);

    move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == rsa::PUBLIC_KEY_URL {
            return if fetches.fetch_add(1, Ordering::SeqCst) < public_key_failures {
                Ok(html("<html><body>请先登录</body></html>"))
            } else {
                Ok(HttpResponse::new(
                    200,
                    rsa::PUBLIC_KEY_URL,
                    public_key_pem(),
                ))
            };
        }

        match url {
            attendance::LOGIN_URL => Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page())),
            lms::LOGIN_URL => Ok(HttpResponse::new(200, LMS_POST, login_page())),
            ATTENDANCE_POST => Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY)),
            LMS_POST => Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY)),
            ATTENDANCE_EXCHANGE => Ok(json(
                serde_json::json!({ "code": 0, "data": { "tokenValue": "token-1" } }),
            )),
            LMS_HOME => Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY)),
            LMS_COURSES => Ok(json(serde_json::json!({ "courses": [] }))),
            _ => Ok(html("")),
        }
    }
}

/// 假 HTTP 伺服器的回應函式（可共用給多個假客戶端實例）。
type SharedResponder = Arc<dyn Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync>;

/// 每次呼叫都產生一個新假客戶端的後端工廠。
type BackendFactory = Arc<dyn Fn() -> AppResult<Arc<dyn HttpClient>> + Send + Sync>;

/// 建立一個共用回應函式的假客戶端。
fn fake_client(responder: SharedResponder) -> Arc<dyn HttpClient> {
    Arc::new(FakeClient::with_responder(move |request| {
        responder(request)
    }))
}

/// 測試用工作執行緒：以假客戶端取代真實網路與資料目錄。
struct Harness {
    worker: Worker,
    events: Receiver<Event>,
    vault: Vault,
    /// 保持任務通道開啟（資料任務會檢查通道是否斷開）。
    _jobs: Sender<Job>,
    /// 任務服務的送出端（測試直接以它送任務操作）。
    _task_jobs: Sender<Job>,
    /// 持有暫存目錄，離開作用域時自動刪除。
    _dir: TempDir,
}

impl Harness {
    fn new(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        Self::from_shared(Arc::new(responder), None)
    }

    /// 以「每次重建後端都換新客戶端」的工廠建立，並以 `built` 觀察重建次數。
    ///
    /// 用於驗證換帳號／回復舊帳號時確實換掉了 cookie jar（假客戶端本身不存
    /// cookie，只能以「是否換了實例」來觀察）。
    fn rebuildable(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
        built: Arc<AtomicUsize>,
    ) -> Self {
        let responder: SharedResponder = Arc::new(responder);
        let factory_responder = Arc::clone(&responder);
        let factory: BackendFactory = Arc::new(move || {
            built.fetch_add(1, Ordering::SeqCst);
            Ok(fake_client(Arc::clone(&factory_responder)))
        });
        Self::from_shared(responder, Some(factory))
    }

    /// 以「第 `fail_from` 次呼叫起必定失敗」的工廠建立。
    ///
    /// 用於驗證重建失敗時不會繼續沿用被污染的會話。
    fn with_broken_factory(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
        calls: Arc<AtomicUsize>,
        fail_from: usize,
    ) -> Self {
        let responder: SharedResponder = Arc::new(responder);
        let factory_responder = Arc::clone(&responder);
        let factory: BackendFactory = Arc::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) >= fail_from {
                return Err(AppError::config("测试注入：无法建立新的后端"));
            }
            Ok(fake_client(Arc::clone(&factory_responder)))
        });
        Self::from_shared(responder, Some(factory))
    }

    fn from_shared(responder: SharedResponder, factory: Option<BackendFactory>) -> Self {
        let dir = TempDir::new().expect("建立暂存目录");
        let vault = Vault::at(dir.path().join("credentials.vault"));

        let config = Config {
            access_policy: AccessPolicy::Direct,
            save_path: Some(dir.path().join("config.json")),
            ..Config::default()
        };

        let credentials = Credentials::new("3120000001", "old-password");
        let mut session = match factory {
            None => {
                let direct: Arc<dyn HttpClient> = fake_client(Arc::clone(&responder));
                let webvpn: Arc<dyn HttpClient> = fake_client(responder);
                SessionManager::with_clients(&config, direct, webvpn)
            }
            Some(factory) => {
                SessionManager::with_client_factories(&config, Arc::clone(&factory), factory)
                    .expect("建立会话管理器")
            }
        };
        session.register(Box::new(AttendanceSite));
        session.register(Box::new(LmsSite));
        session.set_credentials(credentials.clone());

        let (job_tx, jobs) = channel();
        let (events, event_rx) = channel();

        // 任務服務：與正式程式一樣獨立一條通道（任務操作不與網路排隊）。
        let (task_tx, task_rx) = channel();
        crate::task::tasks::serve(
            events.clone(),
            job_tx.clone(),
            task_rx,
            dir.path().join("tasks.vault"),
        )
        .expect("启动任务服务");
        let tasks = crate::task::tasks::TaskHandle::new(task_tx.clone());

        Self {
            worker: Worker {
                jobs,
                events,
                vault: vault.clone(),
                config,
                session: Some(session),
                credentials: Some(credentials),
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
                shutdown: false,
                tasks,
            },
            events: event_rx,
            vault,
            _jobs: job_tx,
            _task_jobs: task_tx,
            _dir: dir,
        }
    }

    /// 預先寫入舊憑證。
    fn seed_vault(&self, passphrase: &str, credentials: &Credentials) {
        self.vault
            .store(passphrase, credentials)
            .expect("写入测试凭据");
    }

    /// 測試用設定檔路徑。
    fn config_path(&self) -> std::path::PathBuf {
        self._dir.path().join("config.json")
    }

    /// 測試用任務檔路徑。
    fn tasks_path(&self) -> std::path::PathBuf {
        self._dir.path().join("tasks.vault")
    }

    /// 直接標記考勤與思源學堂都已登入（跳過登入流程）。
    fn login_both_sites(&mut self) {
        let session = self.worker.session.as_mut().expect("会话已建立");
        session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
        session.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());
    }

    /// 只標記思源學堂已登入。
    fn login_lms_only(&mut self) {
        let session = self.worker.session.as_mut().expect("会话已建立");
        session.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());
    }

    /// 模擬介面送出任務（進通道，尚未被工作者取出）。
    fn send_job(&self, job: Job) {
        self._jobs.send(job).expect("发送任务");
    }

    /// 執行任務（錯誤處理比照 [`Worker::run`]）。
    fn dispatch(&mut self, job: Job) -> AppResult<()> {
        let what = job.label();
        let target = failed_target_of(&job);
        let site = self.worker.login_site_of(&job);
        let resource = resource_of(&job);
        match self.worker.dispatch(job) {
            Ok(()) => Ok(()),
            Err(err) => {
                // 與 [`Worker::handle_control`] 一致：驗證碼填錯由工作者回報成
                // 可重試事件，不再另發一般失敗。
                if !matches!(err, AppError::VerificationRetry(_)) {
                    self.worker.emit(Event::Failed {
                        what,
                        message: err.to_string(),
                        target,
                        site,
                        resource,
                    });
                }
                Err(err)
            }
        }
    }

    /// 取出目前為止的所有事件。
    fn drain_events(&mut self) -> Vec<Event> {
        let mut collected = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            collected.push(event);
        }
        collected
    }

    /// 收集事件：先等最多 `first` 等到第一則事件，之後只收連續空檔之前的後續事件。
    ///
    /// 任務服務是獨立執行緒：任務快照與提示比工作者的回報晚（解鎖後的第一次
    /// 載入要跑 Argon2id，在測試建置下可能要數百毫秒），因此不能只靠固定等待。
    fn collect_events(&mut self, first: Duration, idle: Duration) -> Vec<Event> {
        let deadline = Instant::now() + first;
        let mut collected = Vec::new();
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(remaining) {
            Ok(event) => collected.push(event),
            Err(_) => return collected,
        }
        loop {
            match self.events.recv_timeout(idle) {
                Ok(event) => collected.push(event),
                Err(_) => return collected,
            }
        }
    }

    /// 等到解鎖完成：憑證就緒與任務快照都收到為止。
    ///
    /// 任務服務是獨立執行緒：`VaultReady` 與任務快照的先後順序不固定，因此
    /// 兩個都要等到。
    fn wait_until_unlocked(&mut self) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut collected = Vec::new();
        let mut ready = false;
        let mut tasks = false;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(event) = self.events.recv_timeout(remaining) else {
                break;
            };
            match &event {
                Event::VaultReady => ready = true,
                Event::Tasks(_) => tasks = true,
                _ => {}
            }
            collected.push(event);
            if ready && tasks {
                break;
            }
        }
        collected
    }

    /// 送出任務操作給任務服務，並收集它回報的事件。
    fn dispatch_task(&mut self, job: Job) -> Vec<Event> {
        self._task_jobs.send(job).expect("送出任务操作");
        self.collect_events(Duration::from_secs(20), Duration::from_millis(150))
    }

    /// 事件中是否出現符合條件的項目（取出後即不再保留）。
    fn saw(&mut self, predicate: impl Fn(&Event) -> bool) -> bool {
        self.drain_events().iter().any(predicate)
    }
}

/// 建立 Harness 並預先放入舊憑證。
fn harness(
    responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
) -> Harness {
    let harness = Harness::new(responder);
    harness.seed_vault("secret123", &Credentials::new("3120000001", "old-password"));
    harness
}

#[test]
fn rejects_wrong_passphrase_without_touching_the_vault() {
    let mut harness = harness(fake_flow(0));

    let result = harness.dispatch(Job::RetryWithAccount {
        site: SiteKind::Attendance,
        credentials: Credentials::new("3120000002", "new-password"),
        passphrase: "wrong-passphrase".into(),
    });

    assert!(
        matches!(result, Err(AppError::WrongPassphrase)),
        "口令错误时应拒绝任务，实际：{result:?}"
    );
    let stored = harness.vault.load("secret123").expect("旧凭据应保持不变");
    assert_eq!(stored.username, "3120000001");
    assert_eq!(stored.password, "old-password");
    assert!(
        harness.saw(|event| matches!(event, Event::Failed { .. })),
        "应当回报失败事件"
    );
}

#[test]
fn unlock_failure_reports_credentials_target() {
    let mut harness = harness(fake_flow(0));

    let result = harness.dispatch(Job::Unlock {
        passphrase: "wrong-passphrase".into(),
    });

    assert!(
        matches!(result, Err(AppError::WrongPassphrase)),
        "口令错误时应拒绝任务，实际：{result:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Credentials,
                ..
            }
        )),
        "解锁失败应归为凭证操作，让界面回到解锁表单"
    );
}

#[test]
fn job_debug_never_leaks_credentials() {
    // 任何 `{:?}` 都不得輸出明文口令或帳號密碼。
    let unlock = Job::Unlock {
        passphrase: "super-secret-passphrase".into(),
    };
    let debug = format!("{unlock:?}");
    assert!(
        !debug.contains("super-secret-passphrase"),
        "Debug 不得泄漏口令：{debug}"
    );

    let create = Job::CreateVault {
        passphrase: "another-secret".into(),
        credentials: Credentials::new("3120000001", "pw-secret-value"),
    };
    let debug = format!("{create:?}");
    assert!(!debug.contains("another-secret"), "口令泄漏：{debug}");
    assert!(!debug.contains("pw-secret-value"), "密码泄漏：{debug}");
    assert!(!debug.contains("3120000001"), "账号泄漏：{debug}");

    let change = Job::ChangePassphrase {
        old: "old-secret".into(),
        new: "new-secret".into(),
    };
    let debug = format!("{change:?}");
    assert!(!debug.contains("old-secret"), "旧口令泄漏：{debug}");
    assert!(!debug.contains("new-secret"), "新口令泄漏：{debug}");

    // 一次性驗證碼同樣不得以明文進入 `Debug`。
    let captcha = format!("{:?}", Job::SubmitCaptcha("4821".into()));
    assert!(!captcha.contains("4821"), "验证码泄漏：{captcha}");
    let mfa = format!("{:?}", Job::VerifyMfaCode("654321".into()));
    assert!(!mfa.contains("654321"), "短信验证码泄漏：{mfa}");
}

#[test]
fn saves_credentials_only_after_login_succeeds() {
    let mut harness = harness(fake_flow(0));

    harness
        .dispatch(Job::RetryWithAccount {
            site: SiteKind::Attendance,
            credentials: Credentials::new("3120000002", "new-password"),
            passphrase: "secret123".into(),
        })
        .expect("登录应当成功");

    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000002");
    assert_eq!(stored.password, "new-password");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::LoginSucceeded {
                site: SiteKind::Attendance,
                mode: Some(AccessMode::Direct),
            }
        )),
        "登录成功事件应携带站点与访问方式"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AccountUpdated)),
        "应当回报账号已更新"
    );
}

#[test]
fn keeps_old_credentials_when_the_new_ones_are_rejected() {
    let mut harness = harness(|request: &HttpRequest| {
        match request.url.as_str() {
            url if url.starts_with(attendance::LOGIN_URL) => {
                Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()))
            }
            rsa::PUBLIC_KEY_URL => Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            )),
            // 帳密被拒：伺服器回 401。
            _ => Ok(HttpResponse::new(401, ATTENDANCE_POST, "<html></html>")),
        }
    });

    harness
        .dispatch(Job::RetryWithAccount {
            site: SiteKind::Attendance,
            credentials: Credentials::new("3120000002", "wrong-password"),
            passphrase: "secret123".into(),
        })
        .expect("登录被拒属于预期结果，不应是任务错误");

    assert!(
        harness.saw(|event| matches!(event, Event::LoginFailed { .. })),
        "应当回报登录失败"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.password, "old-password", "失败不得覆盖旧凭据");
}

#[test]
fn change_account_saves_new_credentials_only_after_login_succeeds() {
    let mut harness = harness(fake_flow(0));

    harness
        .dispatch(Job::ChangeAccount {
            passphrase: "secret123".into(),
            credentials: Credentials::new("3120000002", "new-password"),
        })
        .expect("登录应当成功");

    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000002");
    assert_eq!(stored.password, "new-password");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AccountUpdated)),
        "应当回报账号已更新"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::SessionsCleared {
                account_changed: true
            }
        )),
        "换账号应清除旧账号的资料与缓存"
    );
}

#[test]
fn change_account_keeps_old_credentials_when_login_is_rejected() {
    let mut harness = harness(|request: &HttpRequest| match request.url.as_str() {
        url if url.starts_with(attendance::LOGIN_URL) => {
            Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()))
        }
        rsa::PUBLIC_KEY_URL => Ok(HttpResponse::new(
            200,
            rsa::PUBLIC_KEY_URL,
            public_key_pem(),
        )),
        _ => Ok(HttpResponse::new(401, ATTENDANCE_POST, "<html></html>")),
    });

    harness
        .dispatch(Job::ChangeAccount {
            passphrase: "secret123".into(),
            credentials: Credentials::new("3120000002", "wrong-password"),
        })
        .expect("登录被拒属于预期结果，不应是任务错误");

    assert!(
        harness.saw(|event| matches!(event, Event::LoginFailed { .. })),
        "应当回报登录失败"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "失败不得覆盖旧凭据");
    assert_eq!(stored.password, "old-password", "失败不得覆盖旧凭据");
}

#[test]
fn change_account_rejects_wrong_passphrase_without_touching_the_vault() {
    let mut harness = harness(fake_flow(0));

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "wrong-passphrase".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });

    assert!(
        matches!(result, Err(AppError::WrongPassphrase)),
        "口令错误时应拒绝任务，实际：{result:?}"
    );
    let stored = harness.vault.load("secret123").expect("旧凭据应保持不变");
    assert_eq!(stored.username, "3120000001");
    assert_eq!(stored.password, "old-password");
}

#[test]
fn public_key_failure_is_recoverable_by_retrying() {
    let mut harness = harness(fake_flow(1));

    let first = harness.dispatch(Job::RetryLogin {
        site: SiteKind::Attendance,
    });
    assert!(
        matches!(first, Err(AppError::Protocol(_))),
        "公钥不是 PEM 时应报告协议错误，实际：{first:?}"
    );
    assert!(
        harness.saw(|event| matches!(event, Event::Failed { .. })),
        "应当回报失败事件"
    );

    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("重试应当成功");
    assert!(
        harness.saw(|event| matches!(event, Event::LoginSucceeded { .. })),
        "第二次重试应当登录成功"
    );
}

#[test]
fn pending_data_job_survives_failed_relogin_and_resumes_afterwards() {
    let mut harness = harness(fake_flow(1));
    // 模擬「載入作業時登入態失效」：任務已排入待重試。
    harness.worker.retry = Some(Job::LoadHomework { force: true });

    let failed = harness.dispatch(Job::RetryLogin {
        site: SiteKind::Attendance,
    });
    assert!(failed.is_err(), "公钥失败时重新登录应当失败");
    assert!(
        matches!(harness.worker.retry, Some(Job::LoadHomework { .. })),
        "登录失败不得丢失待重试的任务"
    );

    // 第二次重試：考勤登入成功後應自動續跑作業載入（思源學堂課程為空）。
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("重试应当成功");
    assert!(
        harness.saw(|event| matches!(event, Event::Homework(update) if update.items.is_empty())),
        "待重试的作业任务应在登录成功后自动续跑"
    );
    assert!(harness.worker.retry.is_none(), "任务续跑后不应继续保留");
}

/// 巢狀排空通道時吃到的結束指令同樣要停止目前任務。
///
/// `drain_channel` 是遞迴的（`Job::Preload` 會再排空一次）：內層吃到
/// `Job::Shutdown` 只設定旗標並回 `false`，外層拿到的卻是「通道已排空」，
/// 於是仍會照常送出請求——程式正在退出，那些請求與結果都沒有意義。
#[test]
fn a_nested_drain_still_stops_the_running_task() {
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let mut harness = harness(move |_request: &HttpRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(html(""))
    });
    // 兩個站點都已登入：`Preload` 會直接排入四個頁面的載入並排空通道。
    let session = harness.worker.session.as_mut().expect("会话已建立");
    session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    session.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());

    // 通道裡依序排著「預載」與「結束」：預載的巢狀排空會吃掉結束指令。
    harness.send_job(Job::Preload);
    harness.send_job(Job::Shutdown);

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("数据任务不应冒泡为错误");

    assert!(harness.worker.shutdown, "应记下结束指令");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "结束指令之后不得再发出请求"
    );
}

/// 等待重登的任務只有一個槽：新的失敗不得讓前一個任務靜默消失。
///
/// 修復前 `report_data_failure` 直接覆寫 `retry`：被覆寫的任務收不到任何事件，
/// 它那一頁就停在「載入中」——而 `ensure_page` 只在頁面尚未載入時才重新請求，
/// 使用者若不回到該頁按 `r` 就再也無法恢復。
#[test]
fn a_data_failure_settles_the_task_already_waiting_to_relogin() {
    // 公鑰取不到 → 自動重登連開始都做不到，本次失敗會走「回報原任務」的路徑。
    let mut harness = harness(fake_flow(1));
    // 上一個任務（課表）已因登入態失效排入待重試的槽。
    harness.worker.retry = Some(Job::LoadSchedule { force: false });

    // 第二個任務（思源學堂課程）也回報登入態失效：尚未登入時不用任何網路往返。
    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("数据任务失败不应冒泡为任务错误");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::LoadingCancelled {
                target: FailedTarget::Schedule
            }
        )),
        "被取代的等待任务必须收敛，否则该页永远停在加载中：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Courses,
                ..
            }
        )),
        "新的任务仍应照常回报失败：{events:?}"
    );
    assert!(
        harness.worker.retry.is_none(),
        "重登失败后不应保留待重试任务"
    );
}

// ── 自動重登上限（避免「重登→重試→再失效」的無上限迴圈）──

/// 「站點持續回報登入態失效」的假站點：資料端點永遠回傳統一認證頁，
/// 登入端點則正常成功；可用計數器觀察重登與資料請求次數。
fn always_expired_responses(
    login_posts: Arc<AtomicUsize>,
    lms_requests: Arc<AtomicUsize>,
) -> impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static {
    move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        match url {
            lms::LOGIN_URL => Ok(HttpResponse::new(200, LMS_POST, login_page())),
            LMS_POST => {
                login_posts.fetch_add(1, Ordering::SeqCst);
                Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY))
            }
            LMS_HOME => Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY)),
            url if url.ends_with("/user/index") => {
                Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY))
            }
            LMS_COURSES => {
                lms_requests.fetch_add(1, Ordering::SeqCst);
                // 最終位址落在統一認證：站點層判定登入態已失效。
                Ok(HttpResponse::new(
                    200,
                    "https://login.xjtu.edu.cn/cas/login?service=lms",
                    login_page().as_bytes(),
                ))
            }
            _ => Ok(html("")),
        }
    }
}

#[test]
fn automatic_relogin_is_bounded_per_load() {
    let login_posts = Arc::new(AtomicUsize::new(0));
    let lms_requests = Arc::new(AtomicUsize::new(0));
    let mut harness = harness(always_expired_responses(
        Arc::clone(&login_posts),
        Arc::clone(&lms_requests),
    ));
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("放弃自动重试不属于任务错误");

    // 初次加载 + 一次自动重登后的重试 = 恰两次课程请求。
    assert_eq!(
        lms_requests.load(Ordering::SeqCst),
        2,
        "自动重试应恰执行一次"
    );
    assert_eq!(
        login_posts.load(Ordering::SeqCst),
        1,
        "同一轮加载只允许一次自动重登"
    );
    assert!(harness.worker.retry.is_none(), "放弃后不应保留待重试任务");
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::Failed {
                message,
                target: FailedTarget::Courses,
                ..
            } if message.contains("自动重新登录")
        )),
        "应回报自动重登后仍失败"
    );
}

#[test]
fn new_load_resets_the_relogin_budget() {
    let login_posts = Arc::new(AtomicUsize::new(0));
    let lms_requests = Arc::new(AtomicUsize::new(0));
    let mut harness = harness(always_expired_responses(
        Arc::clone(&login_posts),
        Arc::clone(&lms_requests),
    ));
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("第一次加载应当在放弃后正常结束");
    assert_eq!(
        login_posts.load(Ordering::SeqCst),
        1,
        "第一次加载恰一次自动重登"
    );

    // 全新的刷新请求（相当于使用者按 r）：额度重置，可再自动重登一次。
    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("第二次加载同样有界");
    assert_eq!(
        login_posts.load(Ordering::SeqCst),
        2,
        "新的请求应重新获得一次自动重登额度"
    );
}

/// 連線層錯誤（逾時、連不上）會自動重送同一個請求，額度用完才回報失敗。
///
/// 校內服務偶發逾時是常態；沒有自動重試時，一次抖動就要使用者手動按 `r`。
#[test]
fn connection_errors_are_retried_before_reporting_failure() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let mut harness = harness(move |request: &HttpRequest| {
        if request.url.as_str() == LMS_COURSES {
            counter.fetch_add(1, Ordering::SeqCst);
            return Err(AppError::network_kind(
                NetworkKind::Timeout,
                "请求超时".to_owned(),
            ));
        }
        Ok(html(""))
    });
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("放弃重试后不属于任务错误");

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        MAX_ATTEMPTS as usize,
        "应以总共 {MAX_ATTEMPTS} 次尝试为上限"
    );
    let events = harness.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Notice(message) if message.contains("正在重试")))
            .count(),
        MAX_ATTEMPTS as usize - 1,
        "每次重试都应有提示"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Courses,
                message,
                ..
            } if message.contains("请求超时")
        )),
        "额度用完才回报失败"
    );
}

/// 以假站點準備好「Auto 模式 + 考勤直連 + 探測結果」的組合。
///
/// 正式流程在登入時才做校園網探測；測試直接觸發一次，讓探測結果（含被拒絕的
/// 狀態碼）進快取，再標記考勤已登入。
fn harness_in_auto_with_probe(probe_status: u16) -> Harness {
    let mut harness = harness(move |request: &HttpRequest| {
        // 探測網址即考勤站點的登入入口（見 `session::manager::CAMPUS_PROBE_URL`）。
        if request.url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(
                probe_status,
                request.url.clone(),
                b"probe".to_vec(),
            ));
        }
        if request.url.contains("bk-kq.xjtu.edu.cn") {
            return Ok(HttpResponse::new(
                403,
                request.url.clone(),
                b"forbidden".to_vec(),
            ));
        }
        Ok(html(""))
    });
    harness.worker.config.access_policy = AccessPolicy::Auto;
    let session = harness.worker.session.as_mut().expect("会话已建立");
    session.set_access_policy(AccessPolicy::Auto);
    assert_eq!(
        session.resolve_access_mode(SiteKind::Attendance).unwrap(),
        AccessMode::Direct,
        "探測有回應時先走直連"
    );
    session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    harness
}

/// 直連請求和探測被**同一個**狀態碼拒絕：Auto 模式下允許一次 WebVPN 回退。
#[test]
fn a_rejection_matching_the_probe_falls_back_to_webvpn() {
    let mut harness = harness_in_auto_with_probe(403);
    assert_eq!(
        harness
            .worker
            .session
            .as_ref()
            .expect("会话已建立")
            .probe_rejection(),
        Some(403),
        "探測被 403 拒絕時應留下訊號"
    );

    let _ = harness.dispatch(Job::LoadSchedule { force: true });

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(message) if message.contains("已改用 WebVPN 重试")
        )),
        "應提示已改走 WebVPN：{events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::Failed { message, .. } if message.contains("异常状态码")
        )),
        "回退後不應直接回報狀態碼錯誤：{events:?}"
    );
}

/// 探測成功但業務請求被 4xx 拒絕：屬業務錯誤，不得擅自改走 WebVPN。
#[test]
fn a_business_rejection_without_a_probe_signal_does_not_fall_back() {
    let mut harness = harness_in_auto_with_probe(200);
    assert_eq!(
        harness
            .worker
            .session
            .as_ref()
            .expect("会话已建立")
            .probe_rejection(),
        None
    );

    let _ = harness.dispatch(Job::LoadSchedule { force: true });

    let events = harness.drain_events();
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::Notice(message) if message.contains("已改用 WebVPN")
        )),
        "不得回退：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Schedule,
                message,
                ..
            } if message.contains("异常状态码")
        )),
        "應直接回報業務失敗：{events:?}"
    );
}

/// 短暫的連線抖動：第二次嘗試成功就當作成功，不留任何失敗訊息。
#[test]
fn a_transient_connection_error_recovers_on_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let mut harness = harness(move |request: &HttpRequest| {
        if request.url.as_str() == LMS_COURSES {
            return if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(AppError::network_kind(
                    NetworkKind::Connect,
                    "连接失败".to_owned(),
                ))
            } else {
                Ok(json(serde_json::json!({ "courses": [] })))
            };
        }
        Ok(html(""))
    });
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("短暂失败应自动恢复");

    assert_eq!(attempts.load(Ordering::SeqCst), 2, "第二次尝试即成功");
    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Courses(_))),
        "恢复后应回报课程"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. })),
        "恢复后不应回报失败"
    );
}

/// 新的使用者请求重新取得完整重试额度（相当于按 `r` 之后又遇到一次抖动）。
#[test]
fn a_new_request_resets_the_connection_retry_budget() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    let mut harness = harness(move |request: &HttpRequest| {
        if request.url.as_str() == LMS_COURSES {
            counter.fetch_add(1, Ordering::SeqCst);
            return Err(AppError::network_kind(
                NetworkKind::Dns,
                "域名解析失败".to_owned(),
            ));
        }
        Ok(html(""))
    });
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("第一次加载在有界重试后结束");
    assert_eq!(attempts.load(Ordering::SeqCst), MAX_ATTEMPTS as usize);

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("第二次加载同样有界");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        MAX_ATTEMPTS as usize * 2,
        "新的请求应重新取得完整重试额度"
    );
}

/// 解鎖後預載：四個頁面依「由便宜到貴」的順序排入，作業放最後。
///
/// 工作者同一時間只跑一個資料任務；作業彙總要逐門課程查活動與提交狀態，
/// 可能花上十秒，排在前面會讓使用者切到其他頁面時乾等。
#[test]
fn preload_queues_the_four_pages_cheapest_first() {
    let mut harness = harness(|_request: &HttpRequest| panic!("已登录时预载不应发出任何请求"));
    harness.login_both_sites();

    harness.dispatch(Job::Preload).expect("预载应成功");

    let queued: Vec<String> = harness.worker.pending_data.iter().map(Job::label).collect();
    assert_eq!(
        queued,
        vec!["课表", "考勤流水", "思源学堂", "作业"],
        "应由便宜到贵依序排入"
    );
}

/// 預載只登入尚未登入的站點，已登入的不重複登入。
#[test]
fn preload_logs_in_the_site_that_is_not_logged_in_yet() {
    let mut harness = harness(fake_flow(0));
    harness.login_lms_only();

    harness.dispatch(Job::Preload).expect("预载应成功");

    let session = harness.worker.session.as_ref().expect("会话");
    assert!(
        session.is_logged_in(SiteKind::Attendance),
        "应登录尚未登录的考勤"
    );
    assert!(
        session.is_logged_in(SiteKind::Lms),
        "已登录的思源学堂应保持"
    );
    assert_eq!(
        harness.worker.pending_data.len(),
        4,
        "两站就绪后排入四个页面"
    );
}

/// 預載的登入失敗：以 `FailedTarget::Preload` 回報（介面只留提示、不動頁面），
/// 且不排入任何載入任務——登入都沒成功，排了也只是徒勞。
#[test]
fn preload_login_failure_reports_without_queueing_any_load() {
    let mut harness = harness(|_request: &HttpRequest| {
        Err(AppError::network_kind(
            NetworkKind::Dns,
            "域名解析失败".to_owned(),
        ))
    });

    harness
        .dispatch(Job::Preload)
        .expect_err("离线时预载应失败");

    assert!(
        harness.worker.pending_data.is_empty(),
        "登录失败不应排入任何载入任务"
    );
    let events = harness.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Notice(message) if message.contains("正在重试")))
            .count(),
        MAX_ATTEMPTS as usize - 1,
        "登入失败也应有自动重试提示"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                what,
                target: FailedTarget::Preload,
                ..
            } if what == "预载"
        )),
        "应以预载为目标回报失败"
    );
}

/// 已經有登入在進行時，預載靜默等待，不重啟它，並記下待補做。
///
/// 重啟會覆寫 `flow`，把使用者正在輸入的驗證流程丟掉。預載只能等這次登入
/// 收尾：由預載自己發起的登入會帶著 `Job::Preload` 續跑，其他任務發起的
/// 登入則靠 `preload_pending` 補做（見下一個測試）。
#[test]
fn preload_waits_for_a_login_that_is_already_running() {
    let logins = Arc::new(AtomicUsize::new(0));
    let mut harness = harness(mfa_login_responses(Arc::clone(&logins)));
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("登录应停在短信验证");
    assert_eq!(logins.load(Ordering::SeqCst), 1, "测试前提：登入已开始");

    harness.dispatch(Job::Preload).expect("预载应静默等待");

    assert_eq!(logins.load(Ordering::SeqCst), 1, "不应重启进行中的登入");
    assert!(
        harness.worker.pending_data.is_empty(),
        "登入完成前不应排入任何载入任务"
    );
    assert!(harness.worker.preload_pending, "应记下待补做的预载");
}

/// 由其他任務發起的登入結束後，被擋下的預載會補做。
///
/// 回歸：修復前 `preload` 在 `flow` 存在時只回報成功就結束，而收尾的
/// `finish_login` 只續跑 `flow.retry`／`retry` 兩者（此時皆為 `None`），
/// 四頁預載從此不再發生。
#[test]
fn preload_blocked_by_another_login_is_resumed_when_it_finishes() {
    let mut harness = harness(|_request: &HttpRequest| panic!("两站已登录时预载不应发出请求"));
    harness.login_both_sites();
    // 模擬「先前被進行中的登入擋下」。
    harness.worker.preload_pending = true;

    harness
        .worker
        .finish_login(SiteKind::Attendance, None)
        .expect("收尾应成功");

    let queued: Vec<String> = harness.worker.pending_data.iter().map(Job::label).collect();
    assert_eq!(
        queued,
        vec!["课表", "考勤流水", "思源学堂", "作业"],
        "登入结束后应补做预载：{queued:?}"
    );
    assert!(!harness.worker.preload_pending, "补做后应清除待办");
}

/// 介面已經送出的同頁請求與預載合併，不會各查一次。
///
/// 解鎖時介面會送出一份當前頁面的載入，而預載又會排入四個頁面；兩者若各自
/// 成隊，同一個頁面會被查詢兩次，第二次的結果會把使用者已經移動過的選取
/// 重設回第一項。
#[test]
fn preload_merges_the_request_the_interface_already_queued() {
    let mut harness = harness(|_request: &HttpRequest| panic!("两站已登录时预载不应发出请求"));
    harness.login_both_sites();
    // 模擬介面在預載之前送出的當前頁面載入（還在通道裡）。
    harness.send_job(Job::LoadSchedule { force: false });

    harness.dispatch(Job::Preload).expect("预载应成功");

    let queued: Vec<String> = harness.worker.pending_data.iter().map(Job::label).collect();
    assert_eq!(
        queued,
        vec!["课表", "考勤流水", "思源学堂", "作业"],
        "同一个页面不得排入两笔：{queued:?}"
    );
    // 那一筆必須已經收進同一份佇列：留在通道裡就會在預載之後再執行一次。
    assert!(
        harness.worker.jobs.try_recv().is_err(),
        "通道不应残留同一个页面的请求"
    );
}

/// 介面送出的強制刷新不會被預載降級為非強制。
#[test]
fn preload_does_not_downgrade_a_forced_refresh() {
    let mut harness = harness(|_request: &HttpRequest| panic!("两站已登录时预载不应发出请求"));
    harness.login_both_sites();
    harness.send_job(Job::LoadCourses { force: true });

    harness.dispatch(Job::Preload).expect("预载应成功");

    let forced = harness
        .worker
        .pending_data
        .iter()
        .filter(|job| matches!(job, Job::LoadCourses { force: true }))
        .count();
    assert_eq!(forced, 1, "强制刷新应原样保留");
}

/// 使用者取消登入時，待補做的預載一併放棄：不該由背景擅自重新登入。
#[test]
fn cancelling_a_login_abandons_the_pending_preload() {
    let mut harness = harness(|_request: &HttpRequest| panic!("本测试不应发出请求"));
    harness.worker.preload_pending = true;

    harness.dispatch(Job::CancelLogin).expect("取消应成功");

    assert!(
        !harness.worker.preload_pending,
        "使用者取消后不得由背景重新发起预载"
    );
    assert!(
        harness.worker.pending_data.is_empty(),
        "取消后不应排入任何载入任务"
    );
}

/// 預載失敗要指出真正失敗的站點。
///
/// 修復前 `login_site_of` 對 `Job::Preload` 一律回 `None`，介面因此把失敗
/// 一律當成考勤——思源學堂登入失敗也會顯示成考勤。
#[test]
fn preload_failure_points_at_the_site_that_failed() {
    let mut harness = harness(|_request: &HttpRequest| panic!("本测试不应发出请求"));

    harness.worker.login_site = Some(SiteKind::Lms);
    assert_eq!(
        harness.worker.login_site_of(&Job::Preload),
        Some(SiteKind::Lms),
        "预载失败应归属于当时正在登入的站点"
    );

    harness.worker.login_site = Some(SiteKind::Attendance);
    assert_eq!(
        harness.worker.login_site_of(&Job::Preload),
        Some(SiteKind::Attendance),
        "登入考勤阶段失败时应归属于考勤"
    );
}

/// 登入收尾要續跑「兩個來源」的等待任務，不能只跑其中一個。
///
/// 修復前寫成 `retry.or(self.retry.take())`：`Option::or` 的參數是值傳遞，
/// `take()` 一定會執行，但當 `retry` 已是 `Some` 時，取出的那個任務就被
/// 靜默丟棄（頁面停在「載入中」，也沒有任何失敗事件）。
#[test]
fn finishing_a_login_resumes_tasks_from_both_sources() {
    let mut harness = harness(|_request: &HttpRequest| panic!("本测试不应发出请求"));
    harness.login_both_sites();
    // `self.retry`：等待重登的資料任務（此處以會排入載入的控制任務代替，
    // 以免測試真的發出請求）。
    harness.worker.retry = Some(Job::SetScheduleWeek { week: 3 });

    harness
        .worker
        .finish_login(
            SiteKind::Attendance,
            Some(Job::SetHomeworkTerm {
                term: "2026-2027-1".to_owned(),
            }),
        )
        .expect("收尾应成功");

    assert!(harness.worker.retry.is_none(), "等待中的任務不应被遗留");
    let mut queued: Vec<String> = harness.worker.pending_data.iter().map(Job::label).collect();
    queued.sort();
    assert_eq!(
        queued,
        vec!["作业", "课表"],
        "兩邊的等待任務都應續跑：{queued:?}"
    );
}

/// 尚未建立會話（例如會話在重建失敗後被停用）時預載直接報錯，不排入任務。
#[test]
fn preload_without_a_session_reports_an_error() {
    let mut harness = harness(|_request: &HttpRequest| panic!("会话未建立时不应发出请求"));
    harness.worker.session = None;

    harness.dispatch(Job::Preload).expect_err("未解锁时应报错");

    assert!(
        harness.worker.pending_data.is_empty(),
        "没有会话时不应排入任何载入任务"
    );
}

/// 考勤登入需要簡訊驗證的假站點：用來製造「登入正在進行」的狀態。
///
/// `logins` 累計登入入口被請求的次數，用來觀察登入是否被重新啟動。
fn mfa_login_responses(
    logins: Arc<AtomicUsize>,
) -> impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static {
    move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == attendance::LOGIN_URL {
            logins.fetch_add(1, Ordering::SeqCst);
            return Ok(HttpResponse::new(
                200,
                ATTENDANCE_POST,
                login_page_with_mfa(),
            ));
        }
        if url == ATTENDANCE_POST {
            return Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY));
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
        panic!("未预期的请求：{url}");
    }
}

/// 登入進行中收到資料任務失敗：把任務排回待執行，不重啟登入。
///
/// 重啟登入會覆寫 `flow`，丟掉目前流程等待續跑的任務（例如使用者正在輸入的
/// 簡訊驗證碼），也會讓 `retry` 槽被佔用；正確做法是等這次登入結束後再重跑
/// 該任務——排入待執行的任務在登入完成前不會被取出。
#[test]
fn a_data_failure_during_a_login_is_parked_instead_of_restarting_it() {
    let logins = Arc::new(AtomicUsize::new(0));
    let mut harness = harness(mfa_login_responses(Arc::clone(&logins)));
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("登录应停在短信验证");
    assert!(harness.worker.flow.is_some(), "测试前提：登入正在进行");

    let generation = harness.worker.generation;
    harness.worker.report_data_failure(
        SiteKind::Attendance,
        Job::LoadFlow { page: 1 },
        generation,
        AppError::SessionExpired,
    );

    assert_eq!(
        logins.load(Ordering::SeqCst),
        1,
        "不得重新启动（覆盖）进行中的登入"
    );
    assert!(harness.worker.retry.is_none(), "不得占用待重试槽");
    assert_eq!(
        harness.worker.pending_data.len(),
        1,
        "任务应排回待执行，等登入结束后重跑"
    );
}

/// 離線時自動重登連開始都做不到：不得留下「正在登入」的進度畫面，頁面也要收斂。
///
/// 修復前：工作者先發 `LoginProgress`（介面顯示「正在登录考勤系统…」），之後的
/// 失敗只回報在原頁面上，覆蓋層因此永遠留在進度畫面——而進度畫面只接受 `q`，
/// 使用者連 `r` 都無法刷新，只能重啟程式。
#[test]
fn offline_relogin_failure_settles_page_and_reports_login_failure() {
    let mut harness = harness(|_request: &HttpRequest| {
        Err(AppError::network_kind(
            NetworkKind::Dns,
            "域名解析失败".to_owned(),
        ))
    });

    // 考勤站點尚未登入：資料任務先回報工作階段失效，再嘗試自動重登。
    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("数据任务失败不应冒泡为任务错误");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::SessionExpired {
                site: SiteKind::Attendance
            }
        )),
        "应先回报工作阶段失效：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginProgress(_))),
        "登录流程连开始都做不到时，不得显示「正在登录」：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Schedule,
                ..
            }
        )),
        "原页面必须收敛为失败，而不是停在加载中：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Login,
                ..
            }
        )),
        "单纯的连接失败不应弹出登录框：{events:?}"
    );
    assert!(
        harness.worker.retry.is_none(),
        "重登失败后不应保留待重试任务"
    );
}

/// 自動重登已進到會顯示進度的階段才失敗時，覆蓋層必須被收斂成失敗畫面。
///
/// 介面端由 `tui::event_test` 的
/// `data_failure_settles_page_and_stuck_login_progress` 覆蓋；這裡確認工作者在
/// 這種情況下確實會送出「原頁面失敗」事件（也就是介面收斂的依據）。
#[test]
fn relogin_progress_is_followed_by_a_page_failure_when_the_login_breaks() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
        // 登入入口可取回，但公鑰取不到：登入流程已進到會顯示進度的階段才失敗。
        if url == rsa::PUBLIC_KEY_URL {
            return Err(AppError::network_kind(
                NetworkKind::Connect,
                "连接失败".to_owned(),
            ));
        }
        if url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
        }
        Err(AppError::network_kind(
            NetworkKind::Connect,
            "连接失败".to_owned(),
        ))
    });

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("数据任务失败不应冒泡为任务错误");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LoginProgress(_))),
        "登录流程确实开始时应显示进度：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Schedule,
                ..
            }
        )),
        "进度之后必须跟着原页面的失败事件，界面才能收敛覆盖层：{events:?}"
    );
    assert!(harness.worker.retry.is_none());
}

/// 登入步驟失敗後不得留下一個「沒有人會再推進它」的流程。
///
/// 取不到驗證碼圖片（`show_captcha` 失敗）就是這種情形：錯誤從 `drive` 冒出
/// 之後若還留著 `flow`，工作者主迴圈（`Worker::run`）會一直延後資料任務——
/// 使用者按 `r` 送出的任務只會被合併進待執行佇列，頁面看起來像卡在「載入中」。
#[test]
fn a_failed_login_step_does_not_leave_the_flow_behind() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
        if url == crate::auth::captcha::CAPTCHA_URL {
            // 取不到驗證碼圖片：非連線層錯誤，不會自動重試。
            return Ok(HttpResponse::new(404, url.to_owned(), b"".as_slice()));
        }
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
        }
        // 帳密被拒；失敗次數會跨驅動器累積（第三次之後要求圖片驗證碼）。
        Ok(HttpResponse::new(
            401,
            ATTENDANCE_POST,
            "<html></html>".to_owned(),
        ))
    });

    // 前三次：帳密被拒，流程正常結束。
    for _ in 0..3 {
        harness
            .dispatch(Job::RetryLogin {
                site: SiteKind::Attendance,
            })
            .expect("帐密被拒应正常回报");
        assert!(harness.worker.flow.is_none(), "凭据被拒后不应留下流程");
    }

    // 第四次：已達驗證碼門檻，但圖片取不到 → 登入流程無法繼續。
    harness.worker.captcha_path = Some(std::path::PathBuf::from("/nonexistent/captcha.png"));
    let err = harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect_err("取不到验证码图片时登录应当失败");
    assert!(matches!(err, AppError::Http { status: 404 }), "{err:?}");
    assert!(
        harness.worker.flow.is_none(),
        "失败的登入步骤必须作废流程，否则资料任务会被无限延后"
    );
    assert!(
        harness.worker.captcha_path.is_none(),
        "作废流程时应一并清掉验证码暂存"
    );
}

/// 互動登入步驟「硬失敗」（不是驗證碼填錯）時同樣要作廢流程。
///
/// 這裡刻意讓待存憑證為 `None`（一般的手動重登）：這種情況下
/// `discard_pending_vault` 會直接返回，不會順手清掉流程，必須由控制任務的
/// 善後負責；否則 `flow` 會一直留著，資料任務永遠排不到。
#[test]
fn a_hard_login_step_failure_drops_the_flow_without_a_pending_switch() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
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
                ATTENDANCE_POST,
                login_page_with_mfa(),
            ));
        }
        if url.contains("/mfa/detect") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "state": "s-1", "need": true }
            })));
        }
        if url.contains("/securephone/send") || url.contains("/initByType/securephone") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "securePhone": "138****1234", "gid": "g-1" }
            })));
        }
        // 核驗端點壞掉：不是「驗證碼填錯」，不可重試也不可保留流程。
        Ok(HttpResponse::new(500, url.to_owned(), b"".as_slice()))
    });

    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("登录应停在短信验证");
    assert!(harness.saw(|event| matches!(event, Event::LoginNeedsMfa { .. })));
    harness
        .dispatch(Job::SendMfaCode)
        .expect("发送验证码应当成功");
    assert!(
        harness.worker.pending_vault.is_none(),
        "测试前提：没有待存凭证"
    );

    let err = harness
        .dispatch(Job::VerifyMfaCode(Secret::from("123456")))
        .expect_err("核验接口坏掉时应回报错误");
    assert!(!matches!(err, AppError::VerificationRetry(_)), "{err:?}");
    assert!(
        harness.worker.flow.is_none(),
        "硬失败后必须作废流程，否则资料任务会被无限延后"
    );
}

/// 只有「送簡訊驗證碼」失敗不算流程結束：驅動器仍可用，使用者再按一次即可重送。
#[test]
fn only_a_sms_send_failure_keeps_the_login_flow_alive() {
    for job in [
        Job::RetryLogin {
            site: SiteKind::Attendance,
        },
        Job::RetryWithAccount {
            site: SiteKind::Attendance,
            passphrase: Secret::from("secret123"),
            credentials: Credentials::new("3120000002", "new-password"),
        },
        Job::ChangeAccount {
            passphrase: Secret::from("secret123"),
            credentials: Credentials::new("3120000002", "new-password"),
        },
        Job::SubmitCaptcha(Secret::from("abcd")),
        Job::VerifyMfaCode(Secret::from("123456")),
    ] {
        assert!(login_step_breaks_the_flow(&job), "应作废流程：{job:?}");
    }
    assert!(
        !login_step_breaks_the_flow(&Job::SendMfaCode),
        "送码失败只是那一次发送失败，驱动仍可用"
    );
    assert!(!login_step_breaks_the_flow(&Job::LoadSchedule {
        force: false
    }));
}

/// 取消登入：丟棄流程、待存憑證與待重試任務。
///
/// 登入互動期間資料任務一律延後，若不取消，使用者關閉登入覆蓋層後按 `r`
/// 送出的任務會永遠排在佇列裡（介面看起來像「r 沒反應」）。
#[test]
fn cancel_login_drops_pending_login_state() {
    // 建立一個登入流程（假客戶端回傳學校網域的登入頁回應）。
    let client: Arc<dyn HttpClient> =
        Arc::new(FakeClient::with_responder(|request: &HttpRequest| {
            Ok(HttpResponse::new(200, request.url.clone(), "<html></html>"))
        }));
    let driver =
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登录驱动器");

    let mut harness = harness(|_request: &HttpRequest| panic!("取消登录不应触发网络请求"));
    harness.worker.flow = Some(LoginFlow {
        site: SiteKind::Attendance,
        driver: Box::new(driver),
        retry: Some(Job::LoadSchedule { force: false }),
    });
    harness.worker.retry = Some(Job::LoadSchedule { force: false });
    // 模擬「使用者重新輸入密碼後嘗試登入」的當下狀態。
    harness.worker.credentials = Some(Credentials::new("3120000001", "typed-password"));
    harness.worker.pending_vault = Some(PendingVault {
        passphrase: Secret::from("secret123"),
        credentials: Credentials::new("3120000001", "typed-password"),
        previous: Some(Credentials::new("3120000001", "old-password")),
    });

    harness
        .dispatch(Job::CancelLogin)
        .expect("取消登录应当成功");

    assert!(harness.worker.flow.is_none(), "应丢弃登录流程");
    assert!(harness.worker.pending_vault.is_none(), "应丢弃待存凭证");
    assert!(harness.worker.retry.is_none(), "应丢弃待重试任务");
    assert_eq!(
        harness.worker.credentials,
        Some(Credentials::new("3120000001", "old-password")),
        "取消后内存中的凭证应还原为保险库保存的旧凭证"
    );
    assert_eq!(
        harness
            .worker
            .session
            .as_ref()
            .and_then(|session| session.credentials().cloned()),
        Some(Credentials::new("3120000001", "old-password")),
        "工作阶段的凭证也应还原"
    );
    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LoginCancelled)),
        "应回报取消完成，界面才能清除等待状态"
    );
    assert!(
        events.iter().any(|event| matches!(event, Event::Notice(_))),
        "应提示已取消登录"
    );
}

#[test]
fn cancel_login_settles_the_waiting_page() {
    // 等待重登的頁面在取消後不會再被重試：必須收到 LoadingCancelled 收斂，
    // 否則會永遠停在「載入中」。
    let client: Arc<dyn HttpClient> =
        Arc::new(FakeClient::with_responder(|request: &HttpRequest| {
            Ok(HttpResponse::new(200, request.url.clone(), "<html></html>"))
        }));
    let driver =
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登录驱动器");

    let mut harness = harness(|_request: &HttpRequest| panic!("取消登录不应触发网络请求"));
    harness.worker.flow = Some(LoginFlow {
        site: SiteKind::Attendance,
        driver: Box::new(driver),
        retry: None,
    });
    harness.worker.retry = Some(Job::LoadHomework { force: false });

    harness
        .dispatch(Job::CancelLogin)
        .expect("取消登录应当成功");

    assert!(
        harness.saw(|event| matches!(
            event,
            Event::LoadingCancelled {
                target: FailedTarget::Homework
            }
        )),
        "取消登录应收敛等待重试的页面"
    );
}

/// 切換訪問模式時取消進行中的登入流程。
///
/// 流程裡的驅動器配著舊的後端與路線；續用它完成登入會把舊客戶端的 cookie
/// 與新的訪問方式湊在一起（`SessionManager::set_access_policy` 已作廢登入
/// 步驟，這裡確認介面上的覆蓋層也會被收斂，而不是留在等待輸入的畫面）。
#[test]
fn changing_access_policy_cancels_an_in_flight_login() {
    let client: Arc<dyn HttpClient> =
        Arc::new(FakeClient::with_responder(|request: &HttpRequest| {
            Ok(HttpResponse::new(200, request.url.clone(), "<html></html>"))
        }));
    let driver =
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登录驱动器");

    let mut harness = harness(|_request: &HttpRequest| panic!("取消登录不应触发网络请求"));
    harness.worker.flow = Some(LoginFlow {
        site: SiteKind::Attendance,
        driver: Box::new(driver),
        retry: None,
    });

    harness
        .dispatch(Job::SetAccessPolicy(AccessPolicy::WebVpn))
        .expect("切换访问模式应当成功");

    assert!(harness.worker.flow.is_none(), "应丢弃配着旧后端的登录流程");
    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LoginCancelled)),
        "应回报取消完成，界面才能关闭登录覆盖层：{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AccessPolicyUpdated(AccessPolicy::WebVpn))),
        "仍要回报访问方式已更新：{events:?}"
    );
}

// ── 資料任務合併（強制刷新優先、同鍵至多一筆）──────────

#[test]
fn merge_upgrades_queued_non_forced_job_in_place() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合并不应触发网络请求"));
    harness
        .worker
        .pending_data
        .push_back(Job::LoadSchedule { force: false });
    harness
        .worker
        .pending_data
        .push_back(Job::LoadCourses { force: false });

    harness
        .worker
        .merge_data_job(Job::LoadCourses { force: true }, None);

    let queued: Vec<&Job> = harness.worker.pending_data.iter().collect();
    assert_eq!(queued.len(), 2, "同键任务应原位升级而不是追加");
    assert!(
        matches!(queued[0], Job::LoadSchedule { .. }),
        "无关任务位置不变"
    );
    assert!(
        matches!(queued[1], Job::LoadCourses { force: true }),
        "非强制任务应原位升级为强制"
    );
}

#[test]
fn merge_never_downgrades_forced_job() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合并不应触发网络请求"));
    harness
        .worker
        .pending_data
        .push_back(Job::LoadHomework { force: true });

    harness
        .worker
        .merge_data_job(Job::LoadHomework { force: false }, None);
    harness
        .worker
        .merge_data_job(Job::LoadHomework { force: true }, None);

    assert_eq!(harness.worker.pending_data.len(), 1, "不得重复追加");
    assert!(
        harness.worker.pending_data[0].is_forced(),
        "已排队的强制任务不得被降级"
    );
}

#[test]
fn merge_drops_duplicates_of_unforced_jobs() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合并不应触发网络请求"));
    harness
        .worker
        .pending_data
        .push_back(Job::LoadSchedule { force: false });
    harness
        .worker
        .pending_data
        .push_back(Job::LoadFlow { page: 1 });

    // 同鍵重複：丟棄；不同頁碼是不同資源鍵，允許並存。
    harness
        .worker
        .merge_data_job(Job::LoadSchedule { force: false }, None);
    harness
        .worker
        .merge_data_job(Job::LoadFlow { page: 1 }, None);
    harness
        .worker
        .merge_data_job(Job::LoadFlow { page: 2 }, None);

    let keys: Vec<Option<DataKey>> = harness
        .worker
        .pending_data
        .iter()
        .map(|job| job.data_key())
        .collect();
    assert_eq!(
        keys,
        vec![
            Some(DataKey::Schedule),
            Some(DataKey::Flow(1)),
            Some(DataKey::Flow(2)),
        ]
    );
}

#[test]
fn merge_keeps_one_forced_job_behind_a_running_unforced_job() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合并不应触发网络请求"));
    let running = Job::LoadCourses { force: false };

    harness
        .worker
        .merge_data_job(Job::LoadCourses { force: true }, Some(&running));
    // 重複的強制請求不得再排入第二筆。
    harness
        .worker
        .merge_data_job(Job::LoadCourses { force: true }, Some(&running));

    assert_eq!(harness.worker.pending_data.len(), 1, "只保留一笔强制任务");
    assert!(harness.worker.pending_data[0].is_forced());
}

#[test]
fn merge_drops_same_key_requests_while_a_forced_job_runs() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合并不应触发网络请求"));
    let running = Job::LoadHomework { force: true };

    harness
        .worker
        .merge_data_job(Job::LoadHomework { force: true }, Some(&running));
    harness
        .worker
        .merge_data_job(Job::LoadHomework { force: false }, Some(&running));

    assert!(
        harness.worker.pending_data.is_empty(),
        "执行中的强制任务已涵盖同键请求"
    );
}

#[test]
fn set_homework_term_leaves_exactly_one_forced_reload() {
    let mut harness = harness(|_request: &HttpRequest| panic!("设置学期不应触发网络请求"));
    harness
        .worker
        .pending_data
        .push_back(Job::LoadHomework { force: false });

    harness
        .dispatch(Job::SetHomeworkTerm {
            term: "2026-2027-1".to_owned(),
        })
        .expect("记住学期");

    let queued: Vec<&Job> = harness.worker.pending_data.iter().collect();
    assert_eq!(queued.len(), 1, "非强制作业任务应被原位升级");
    assert!(queued[0].is_forced(), "重载必须是强制刷新");
    assert_eq!(
        harness.worker.config.homework_term.as_deref(),
        Some("2026-2027-1")
    );
    assert!(harness.saw(|event| matches!(
        event,
        Event::Notice(text) if text.contains("已记住学期")
    )));

    // 已有强制任务时不重复追加。
    harness
        .dispatch(Job::SetHomeworkTerm {
            term: "2026-2027-1".to_owned(),
        })
        .expect("记住学期");
    assert_eq!(harness.worker.pending_data.len(), 1, "不得重复追加");
}

#[test]
fn unlock_does_not_start_login() {
    let mut harness = harness(|_request: &HttpRequest| panic!("解锁不应触发任何网络请求"));

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .expect("解锁应当成功");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::VaultReady)),
        "应回报凭证已就绪"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginProgress(_))),
        "解锁不再预登录任何站点"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::SessionsCleared {
                account_changed: true
            }
        )),
        "解锁后应回报会话已重置（并标记页面资料失效）"
    );
}

#[test]
fn set_access_policy_saves_without_login_and_keeps_old_value_on_failure() {
    let mut harness = harness(|_request: &HttpRequest| panic!("保存访问模式不应触发网络请求"));

    harness
        .dispatch(Job::SetAccessPolicy(AccessPolicy::WebVpn))
        .expect("保存应当成功");
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::SessionsCleared {
                account_changed: false
            }
        )),
        "切换访问模式后应回报会话已重置（页面资料仍有效）"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AccessPolicyUpdated(AccessPolicy::WebVpn))),
        "应回报访问策略已更新"
    );
    let saved = std::fs::read_to_string(harness.config_path()).expect("读取配置文件");
    assert!(saved.contains("webvpn"), "设置应写入磁盘：{saved}");

    // 将存档路径指向目录，迫使写入失败。
    harness.worker.config.save_path = Some(harness._dir.path().to_path_buf());
    let result = harness.dispatch(Job::SetAccessPolicy(AccessPolicy::Direct));
    assert!(result.is_err(), "写入失败时应报告错误");
    assert_eq!(
        harness.worker.config.access_policy,
        AccessPolicy::WebVpn,
        "写入失败时应保留旧值"
    );
    assert!(
        !harness.saw(|event| matches!(event, Event::AccessPolicyUpdated(_))),
        "失败时不得回报已更新"
    );
}

#[test]
fn accept_agreement_saves_version_and_emits_event() {
    let mut harness = harness(|_request: &HttpRequest| panic!("同意协议不应触发网络请求"));

    harness
        .dispatch(Job::AcceptAgreement)
        .expect("同意协议应当成功");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AgreementAccepted)),
        "应回报协议已同意"
    );
    assert_eq!(
        harness.worker.config.privacy_version.as_deref(),
        Some(crate::privacy::VERSION),
        "内存中的设置应记录已同意版本"
    );
    let saved = std::fs::read_to_string(harness.config_path()).expect("读取配置文件");
    assert!(
        saved.contains(&format!(
            "\"privacy_version\": \"{}\"",
            crate::privacy::VERSION
        )),
        "已同意版本应写入磁盘：{saved}"
    );
}

#[test]
fn accept_agreement_save_failure_keeps_previous_version() {
    let mut harness = harness(|_request: &HttpRequest| panic!("同意协议不应触发网络请求"));
    harness.worker.config.privacy_version = Some("1.0".to_owned());

    // 将存档路径指向目录，迫使写入失败。
    harness.worker.config.save_path = Some(harness._dir.path().to_path_buf());
    let result = harness.dispatch(Job::AcceptAgreement);
    assert!(result.is_err(), "写入失败时应报告错误");
    assert_eq!(
        harness.worker.config.privacy_version.as_deref(),
        Some("1.0"),
        "写入失败时应保留旧版本记录"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Agreement,
                ..
            }
        )),
        "失败应定位到协议画面"
    );
    assert!(
        !harness.saw(|event| matches!(event, Event::AgreementAccepted)),
        "失败时不得回报已同意"
    );
}

// ── 作業流程 ─────────────────────────────────────────

/// 作業流程用的最小思源學堂假站點。
struct FakeHomeworkSite {
    seen: Arc<Mutex<Vec<String>>>,
    courses: serde_json::Value,
    activities: Vec<(&'static str, serde_json::Value)>,
    details: Vec<(&'static str, serde_json::Value)>,
    /// 第一次提交記錄查詢是否回傳登入頁（驗證會話失效傳播）。
    expire_first_submission: bool,
    submissions: AtomicUsize,
    /// 考勤當前學期（學年, 學期名）；`None` 表示考勤未登入且不應被查詢。
    attendance_term: Option<(&'static str, &'static str)>,
}

impl FakeHomeworkSite {
    /// 已請求的 URL 清單。
    fn urls(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }

    fn handle(&self, request: &HttpRequest) -> AppResult<HttpResponse> {
        let url = request.url.clone();
        self.seen.lock().expect("lock").push(url.clone());

        if url.ends_with("/api/my-courses") {
            return Ok(json(self.courses.clone()));
        }
        if url.ends_with("/user/index") {
            return Ok(HttpResponse::new(
                200,
                "https://lms.xjtu.edu.cn/user/index",
                USER_PAGE.as_bytes(),
            ));
        }
        if url == lms::LOGIN_URL {
            return Ok(HttpResponse::new(200, LMS_POST, login_page()));
        }
        if url == LMS_POST {
            return Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY));
        }
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url.ends_with("/timetable/semesters") {
            let Some((year, name)) = self.attendance_term else {
                panic!("考勤未登录时不应查询学期：{url}");
            };
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": [{
                    "semesterId": "s-1",
                    "academicYear": year,
                    "semesterName": name,
                    "startDate": "2026-09-07",
                }]
            })));
        }
        if url.contains("/player-url") {
            return Ok(json(serde_json::json!({
                "url": "https://lms.xjtu.edu.cn/lesson/player?token=abc"
            })));
        }
        for (course_id, payload) in &self.activities {
            if url.ends_with(&format!("/courses/{course_id}/activities")) {
                return Ok(json(payload.clone()));
            }
        }
        for (activity_id, payload) in &self.details {
            if url.ends_with(&format!("/api/activities/{activity_id}")) {
                return Ok(json(payload.clone()));
            }
        }
        if url.contains("/submission_list") {
            let attempt = self.submissions.fetch_add(1, Ordering::SeqCst);
            if self.expire_first_submission && attempt == 0 {
                return Ok(HttpResponse::new(
                    200,
                    "https://login.xjtu.edu.cn/cas/login?service=lms",
                    login_page().as_bytes(),
                ));
            }
            return Ok(json(serde_json::json!({ "list": [] })));
        }
        panic!("未预期的请求：{url}");
    }
}

/// 思源學堂首頁（含使用者 ID，供個人提交查詢使用）。
///
/// 以真實頁面的 JavaScript 物件語法（未加引號的鍵、`None`）撰寫，
/// 驗證整條提交查詢鏈路都依賴寬容解析。
const USER_PAGE: &str = r#"<html><script>
    var globalData = { user: { id: 42, name: "张三", dept: None, role: "Student", }, dept: { id: 3 }, locale: "zh-CN" };
</script></html>"#;

/// 取出所有作業更新事件。
fn homework_updates(harness: &mut Harness) -> Vec<HomeworkUpdate> {
    harness
        .drain_events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Homework(update) => Some(update),
            _ => None,
        })
        .collect()
}

/// 取出說明中的純文字（測試斷言用；是否含圖片、附件另有斷言）。
fn description_text(description: &Option<lms::ActivityContent>) -> Option<&str> {
    description
        .as_ref()
        .and_then(|content| content.text.as_deref())
}

/// 取出說明中的附件名稱（測試斷言用）。
fn attachment_names(description: &Option<lms::ActivityContent>) -> Vec<String> {
    description
        .as_ref()
        .map(|content| {
            content
                .attachments
                .iter()
                .map(|upload| upload.display_name().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn attendance_term_propagates_session_expiry_instead_of_falling_back() {
    // 考勤已登入，但學期端點回傳登入頁（會話過期）：錯誤應向上傳播，
    // 而非靜默回傳 None，讓作業清單悄悄退回記憶中的學期。
    let mut harness = harness(|request: &HttpRequest| {
        if request.url.ends_with("/timetable/semesters") {
            Ok(HttpResponse::new(
                200,
                "https://login.xjtu.edu.cn/cas/login?service=attendance",
                login_page().as_bytes(),
            ))
        } else {
            panic!("未预期的请求：{}", request.url)
        }
    });
    harness.login_both_sites();

    let err = harness
        .worker
        .attendance_term()
        .expect_err("会话失效应向上传播");
    assert!(err.needs_relogin(), "应触发重新登录，实际：{err}");
}

#[test]
fn attendance_term_is_none_when_not_logged_in() {
    // 未登入考勤时不應發出任何請求，也不應出錯。
    let mut harness = harness(|request: &HttpRequest| panic!("未应发起请求：{}", request.url));
    let term = harness.worker.attendance_term().expect("未登录不应出错");
    assert!(term.is_none(), "未登录时不应有学期");
}

#[test]
fn parse_date_reports_category_without_echoing_server_value() {
    let err = super::data::parse_date("<html>2026/09/07</html>").expect_err("应拒绝非 YYYY-MM-DD");
    let message = err.to_string();
    assert!(!message.contains("2026/09/07"), "不得夹带原始值：{message}");
    assert!(!message.contains("<html>"), "不得夹带原始值：{message}");
}

#[test]
fn homework_load_ends_the_timing_span() {
    // 每次全新的作業載入都會開始一次計時；載入結束（含無課程的早退）時
    // 必須收尾，否則下一次載入會把兩次之間的時間算進去。
    let mut harness = harness(fake_flow(0));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    assert!(harness.worker.timing.is_idle(), "载入结束后计时应已收尾");
    let events = harness.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Notice(text) if text.starts_with("计时"))),
        "默认（未设 OHMYXJTU_TIMING）不得输出计时报告：{events:?}"
    );
}

#[test]
fn finish_timing_emits_a_report_only_when_enabled() {
    let mut harness = harness(|request: &HttpRequest| panic!("不应发起请求：{}", request.url));
    // 沒有進行中的計時：即使收尾也不輸出任何訊息。
    harness.worker.finish_timing();
    assert!(harness.drain_events().is_empty(), "未计时不应有事件");

    harness.worker.timing.begin(true);
    harness.worker.finish_timing();
    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(text) if text.starts_with("计时 总"))),
        "启用后应输出计时报告：{events:?}"
    );
}

#[test]
fn homework_prefetches_details_without_repeating_or_cross_wiring_them() {
    // 五門課程各一項作業：詳情與活動同批預取。每項活動的詳情必須恰好查一次
    //（預取與就地補抓都跑就會是兩次），且各自的說明不得互相錯掛。
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "课程A", "semester": { "code": "2026-1" } },
            { "id": "2", "name": "课程B", "semester": { "code": "2026-1" } },
            { "id": "3", "name": "课程C", "semester": { "code": "2026-1" } },
            { "id": "4", "name": "课程D", "semester": { "code": "2026-1" } },
            { "id": "5", "name": "课程E", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![
            (
                "1",
                serde_json::json!({ "activities": [{ "id": "11", "type": "homework",
                    "title": "作业A", "end_time": "2099-12-31 23:59:59" }] }),
            ),
            (
                "2",
                serde_json::json!({ "activities": [{ "id": "12", "type": "homework",
                    "title": "作业B", "end_time": "2099-12-31 23:59:59" }] }),
            ),
            (
                "3",
                serde_json::json!({ "activities": [{ "id": "13", "type": "homework",
                    "title": "作业C", "end_time": "2099-12-31 23:59:59" }] }),
            ),
            (
                "4",
                serde_json::json!({ "activities": [{ "id": "14", "type": "homework",
                    "title": "作业D", "end_time": "2099-12-31 23:59:59" }] }),
            ),
            (
                "5",
                serde_json::json!({ "activities": [{ "id": "15", "type": "homework",
                    "title": "作业E", "end_time": "2099-12-31 23:59:59" }] }),
            ),
        ],
        details: (1..=5)
            .map(|index| {
                let id: &'static str = match index {
                    1 => "11",
                    2 => "12",
                    3 => "13",
                    4 => "14",
                    _ => "15",
                };
                let title: &'static str = match index {
                    1 => "作业A",
                    2 => "作业B",
                    3 => "作业C",
                    4 => "作业D",
                    _ => "作业E",
                };
                let description: &'static str = match index {
                    1 => "说明A",
                    2 => "说明B",
                    3 => "说明C",
                    4 => "说明D",
                    _ => "说明E",
                };
                (
                    id,
                    serde_json::json!({ "id": id, "type": "homework", "title": title,
                        "end_time": "2099-12-31 23:59:59", "submit_by_group": false,
                        "user_submit_count": 0,
                        "data": { "description": format!("<p>{description}</p>") } }),
                )
            })
            .collect(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: true })
        .expect("作业加载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    let descriptions: Vec<&str> = last
        .items
        .iter()
        .map(|item| {
            item.description
                .as_ref()
                .and_then(|content| content.text.as_deref())
                .unwrap_or("<none>")
        })
        .collect();
    assert_eq!(
        descriptions,
        ["说明A", "说明B", "说明C", "说明D", "说明E"],
        "每项作业应各自带自己的说明"
    );

    let seen = site.urls();
    for activity_id in ["11", "12", "13", "14", "15"] {
        let count = seen
            .iter()
            .filter(|url| url.ends_with(&format!("/api/activities/{activity_id}")))
            .count();
        assert_eq!(
            count, 1,
            "活动 {activity_id} 的详情应恰好查询一次：{seen:?}"
        );
    }
}

#[test]
fn homework_filters_to_current_term_and_streams_progress() {
    // 截止時間取遠未來：本測試需要「尚未截止」的作業（`待提交`），
    // 固定的近日日期會隨時鐘走過而變成「逾期」。
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            { "id": "2", "name": "操作系统", "semester": { "code": "2026-1" } },
            { "id": "9", "name": "历史课程", "semester": { "code": "2025-2" } },
        ]}),
        activities: vec![
            (
                "1",
                serde_json::json!({ "activities": [
                    { "id": "11", "type": "homework", "title": "作业A",
                      "end_time": "2099-12-31 23:59:59" },
                    { "id": "12", "type": "material", "title": "课件" },
                ]}),
            ),
            ("2", serde_json::json!({ "activities": [] })),
        ],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "end_time": "2099-12-31 23:59:59",
                "submit_by_group": true, "group_id": "7" }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    let updates = homework_updates(&mut harness);
    assert!(updates.len() >= 2, "应有部分结果与最终结果：{updates:#?}");
    let last = updates.last().expect("最终更新");
    assert!(last.progress.is_none(), "最终更新不应带进度：{last:#?}");
    assert!(
        updates[..updates.len() - 1]
            .iter()
            .any(|update| update.progress.is_some()),
        "加载期间应回报进度"
    );
    assert_eq!(last.term_label.as_deref(), Some("2026-2027 学年 第 1 学期"));
    assert_eq!(last.courses_included, 2);
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].state, HomeworkState::Pending);
    assert_eq!(
        last.items[0].submit_by_group,
        Some(true),
        "小组判定应取自活动详情"
    );
    assert_eq!(
        last.items[0].description, None,
        "详情没有说明时不得凭空产生"
    );

    let seen = site.urls();
    assert!(
        !seen.iter().any(|url| url.contains("/courses/9/activities")),
        "历史学期的课程不应被查询：{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|url| url.contains("/groups/7/submission_list")),
        "小组作业应走小组提交记录：{seen:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|url| url.ends_with("/api/my-courses"))
            .count(),
        1
    );
}

#[test]
fn homework_fetches_activity_lists_in_batches_without_repeating_requests() {
    // 五門課程（超過一批的並行上限）：每門課程的活動仍恰好查一次，結果依
    // 課程順序彙總。預取只是把同一批的請求同時送出，不會重複查詢既有課程。
    let courses: Vec<serde_json::Value> = ["A", "B", "C", "D", "E"]
        .iter()
        .enumerate()
        .map(|(index, suffix)| {
            serde_json::json!({
                "id": (index + 1).to_string(),
                "name": format!("课程{suffix}"),
                "semester": { "code": "2026-1" },
            })
        })
        .collect();
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": courses }),
        activities: vec![
            (
                "1",
                serde_json::json!({ "activities": [
                    { "id": "11", "type": "homework", "title": "作业A",
                      "end_time": "2099-12-31 23:59:59" },
                ]}),
            ),
            (
                "2",
                serde_json::json!({ "activities": [
                    { "id": "12", "type": "homework", "title": "作业B",
                      "end_time": "2099-12-31 23:59:59" },
                ]}),
            ),
            (
                "3",
                serde_json::json!({ "activities": [
                    { "id": "13", "type": "homework", "title": "作业C",
                      "end_time": "2099-12-31 23:59:59" },
                ]}),
            ),
            (
                "4",
                serde_json::json!({ "activities": [
                    { "id": "14", "type": "homework", "title": "作业D",
                      "end_time": "2099-12-31 23:59:59" },
                ]}),
            ),
            (
                "5",
                serde_json::json!({ "activities": [
                    { "id": "15", "type": "homework", "title": "作业E",
                      "end_time": "2099-12-31 23:59:59" },
                ]}),
            ),
        ],
        details: ["11", "12", "13", "14", "15"]
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let suffix = ["A", "B", "C", "D", "E"][index];
                (
                    *id,
                    serde_json::json!({ "id": id, "type": "homework",
                        "title": format!("作业{suffix}"),
                        "end_time": "2099-12-31 23:59:59",
                        "submit_by_group": false, "user_submit_count": 0 }),
                )
            })
            .collect(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    let last = homework_updates(&mut harness).pop().expect("最终更新");
    let titles: Vec<&str> = last.items.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(
        titles,
        ["作业A", "作业B", "作业C", "作业D", "作业E"],
        "结果应依课程顺序彙總"
    );

    let seen = site.urls();
    for course in 1..=5 {
        let requests = seen
            .iter()
            .filter(|url| url.ends_with(&format!("/courses/{course}/activities")))
            .count();
        assert_eq!(
            requests, 1,
            "第 {course} 门课程的活动应恰好查询一次：{seen:?}"
        );
    }
}

#[test]
fn homework_uses_remembered_term_without_touching_attendance() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "9", "name": "历史课程", "semester": { "code": "2025-2" } },
        ]}),
        activities: vec![(
            "9",
            serde_json::json!({ "activities": [
                { "id": "91", "type": "homework", "title": "旧作业" },
            ]}),
        )],
        details: vec![(
            "91",
            serde_json::json!({ "id": "91", "type": "homework", "title": "旧作业",
                "submit_by_group": false }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2025-2026-2".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.courses_included, 1);
    assert_eq!(last.term_label.as_deref(), Some("2025-2026 学年 第 2 学期"));
    assert_eq!(last.items.len(), 1);
    assert!(
        !site
            .urls()
            .iter()
            .any(|url| url.contains("timetable/semesters")),
        "考勤未登录时不得查询其学期"
    );
}

#[test]
fn homework_needs_term_until_the_user_chooses_one() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            { "id": "9", "name": "历史课程", "semester": { "code": "2025-2" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "submit_by_group": true, "group_id": "7" }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("应要求选择学期");
    let events = harness.drain_events();
    let options = events
        .iter()
        .find_map(|event| match event {
            Event::HomeworkNeedsTerm { options, .. } => Some(options.clone()),
            _ => None,
        })
        .expect("应要求选择学期");
    assert_eq!(
        options,
        vec![
            TermCode::parse("2026-2027-1").expect("学期"),
            TermCode::parse("2025-2026-2").expect("学期"),
        ]
    );

    // 選擇本學期：設定被記住，且自動排入一次強制重載。
    harness
        .dispatch(Job::SetHomeworkTerm {
            term: "2026-2027-1".to_owned(),
        })
        .expect("记住学期");
    assert_eq!(
        harness.worker.config.homework_term.as_deref(),
        Some("2026-2027-1")
    );
    let queued = harness
        .worker
        .pending_data
        .pop_front()
        .expect("应排入重载任务");
    harness.dispatch(queued).expect("重载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.term_label.as_deref(), Some("2026-2027 学年 第 1 学期"));
    assert!(
        !site
            .urls()
            .iter()
            .any(|url| url.contains("/courses/9/activities")),
        "未选中的历史学期不应被查询"
    );
}

#[test]
fn homework_session_expiry_relogs_in_and_retries() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "submit_by_group": true, "group_id": "7" }),
        )],
        expire_first_submission: true,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("重登后应当成功");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::SessionExpired {
                site: SiteKind::Lms
            }
        )),
        "会话失效时应回报对应站点"
    );
    let updates: Vec<HomeworkUpdate> = events
        .into_iter()
        .filter_map(|event| match event {
            Event::Homework(update) => Some(update),
            _ => None,
        })
        .collect();
    let last = updates.last().expect("最终更新");
    assert_eq!(last.items.len(), 1, "重试后应得到最终结果");
    assert_eq!(last.items[0].state, HomeworkState::Pending);

    let submissions = site
        .urls()
        .iter()
        .filter(|url| url.contains("/submission_list"))
        .count();
    assert_eq!(submissions, 2, "会话失效后应重登并重试一次");
    assert!(
        harness.worker.retry.is_none(),
        "重试完成后不应保留待重试任务"
    );
}

#[test]
fn homework_reuses_cache_until_forced() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "submit_by_group": true, "group_id": "7" }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    let count = |site: &FakeHomeworkSite, needle: &str| {
        site.urls()
            .iter()
            .filter(|url| url.ends_with(needle))
            .count()
    };

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("首次加载");
    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("再次加载");
    assert_eq!(count(&site, "/api/my-courses"), 1, "第二次应命中课程缓存");
    assert_eq!(
        count(&site, "/courses/1/activities"),
        1,
        "第二次应命中活动缓存"
    );

    harness
        .dispatch(Job::LoadHomework { force: true })
        .expect("强制刷新");
    assert_eq!(count(&site, "/api/my-courses"), 2, "强制刷新应重新查询");
    assert_eq!(count(&site, "/courses/1/activities"), 2);
}

#[test]
fn homework_reports_user_page_failure_once_without_repeated_requests() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        tracker.lock().expect("lock").push(url.clone());
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/user/index") {
            // 使用者首頁不含可解析的 globalData（例如改版或登入頁）。
            return Ok(HttpResponse::new(
                200,
                "https://lms.xjtu.edu.cn/user/index",
                "<html><body>无法解析的页面</body></html>",
            ));
        }
        if url.ends_with("/courses/1/activities") {
            return Ok(json(serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A" },
                { "id": "12", "type": "homework", "title": "作业B" },
            ]})));
        }
        if url.ends_with("/api/activities/11") {
            return Ok(json(serde_json::json!({
                "id": "11", "type": "homework", "title": "作业A", "submit_by_group": false
            })));
        }
        if url.ends_with("/api/activities/12") {
            return Ok(json(serde_json::json!({
                "id": "12", "type": "homework", "title": "作业B", "submit_by_group": false
            })));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.items.len(), 2);
    assert!(
        last.items
            .iter()
            .all(|item| item.state == HomeworkState::Unknown),
        "用户信息无法解析时不得判成任一确定状态：{:#?}",
        last.items
    );
    assert_eq!(
        last.issues.len(),
        1,
        "共同故障只应汇总一次：{:#?}",
        last.issues
    );
    assert_eq!(last.issues[0].count, 2);
    assert!(
        last.issues[0].reason.contains("用户信息解析失败"),
        "原因应指向用户信息解析阶段：{}",
        last.issues[0].reason
    );

    let user_page_requests = seen
        .lock()
        .expect("lock")
        .iter()
        .filter(|url| url.ends_with("/user/index"))
        .count();
    assert_eq!(
        user_page_requests, 1,
        "解析失败应负缓存，不得逐项重取 /user/index"
    );
}

#[test]
fn homework_uses_detail_submit_count_without_submission_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        tracker.lock().expect("lock").push(url.clone());
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/courses/1/activities") {
            return Ok(json(serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "已交作业" },
                { "id": "12", "type": "homework", "title": "未交作业" },
            ]})));
        }
        if url.ends_with("/api/activities/11") {
            return Ok(json(serde_json::json!({
                "id": "11", "type": "homework", "title": "已交作业",
                "submit_by_group": false, "user_submit_count": 2
            })));
        }
        if url.ends_with("/api/activities/12") {
            return Ok(json(serde_json::json!({
                "id": "12", "type": "homework", "title": "未交作业",
                "submit_by_group": false, "user_submit_count": 0
            })));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    let by_title = |title: &str| {
        last.items
            .iter()
            .find(|item| item.title == title)
            .unwrap_or_else(|| panic!("找不到作业 {title}"))
    };
    assert_eq!(by_title("已交作业").state, HomeworkState::Completed);
    assert_eq!(by_title("未交作业").state, HomeworkState::Pending);
    assert!(last.issues.is_empty());

    let url_list = seen.lock().expect("lock").clone();
    assert!(
        !url_list.iter().any(|url| url.contains("/submission_list")),
        "详情提供 user_submit_count 时不应再查提交列表：{url_list:?}"
    );
    assert!(
        !url_list.iter().any(|url| url.ends_with("/user/index")),
        "不应为已确定的状态查询用户信息：{url_list:?}"
    );
}

#[test]
fn group_homework_without_group_id_stays_unknown_with_reason() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        tracker.lock().expect("lock").push(url.clone());
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/courses/1/activities") {
            return Ok(json(serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "小组作业" },
            ]})));
        }
        if url.ends_with("/api/activities/11") {
            // 小组作业但缺少 group_id：无法拼接提交地址。
            return Ok(json(serde_json::json!({
                "id": "11", "type": "homework", "title": "小组作业",
                "submit_by_group": true
            })));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].state, HomeworkState::Unknown);
    let note = last.items[0].note.as_deref().unwrap_or_default();
    assert!(note.contains("group_id"), "原因应指出缺少 group_id：{note}");
    assert_eq!(last.issues.len(), 1);
    assert!(last.issues[0].reason.contains("group_id"));
    assert!(
        !seen
            .lock()
            .expect("lock")
            .iter()
            .any(|url| url.contains("/submission_list")),
        "缺少 group_id 时不得请求提交列表"
    );
}

#[test]
fn homework_without_submit_by_group_stays_unknown_without_queries() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        tracker.lock().expect("lock").push(url.clone());
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/courses/1/activities") {
            return Ok(json(serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "缺单位作业" },
            ]})));
        }
        if url.ends_with("/api/activities/11") {
            // 详情缺少 submit_by_group：无法判定个人或小组，不得猜测。
            return Ok(json(serde_json::json!({
                "id": "11", "type": "homework", "title": "缺单位作业"
            })));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].state, HomeworkState::Unknown);
    assert_eq!(last.items[0].submit_by_group, None);
    let note = last.items[0].note.as_deref().unwrap_or_default();
    assert!(
        note.contains("submit_by_group"),
        "原因应指出缺少提交单位：{note}"
    );
    assert_eq!(last.issues.len(), 1, "共同故障只汇总一次");
    assert!(last.issues[0].reason.contains("submit_by_group"));
    let seen = seen.lock().expect("lock");
    assert!(
        !seen.iter().any(|url| url.contains("/submission_list")),
        "缺少提交单位时不得请求提交列表：{seen:?}"
    );
    assert!(
        !seen.iter().any(|url| url.ends_with("/user/index")),
        "缺少提交单位时不得查询用户信息：{seen:?}"
    );
}

#[test]
fn schedule_after_semester_end_is_empty_with_notice_and_no_queries() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let today = chrono::Local::now().date_naive();
    let start = (today - chrono::Duration::days(100)).to_string();
    let end = (today - chrono::Duration::days(1)).to_string();
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        tracker.lock().expect("lock").push(url.clone());
        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        panic!("学期结束后不应再查询：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("课表加载应当成功");

    let schedule = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .expect("应发出课表事件");
    assert!(schedule.lessons.is_empty(), "学期结束后不得显示旧课程");
    let notice = schedule.notice.as_deref().unwrap_or_default();
    assert!(notice.contains("已结束"), "应提示学期已结束：{notice}");
    assert_eq!(schedule.semester, "2026-2027-1");
    let seen = seen.lock().expect("lock");
    assert!(
        !seen.iter().any(|url| url.contains("/timetable/weekly")),
        "学期结束后不得查询课表：{seen:?}"
    );
    assert!(
        !seen.iter().any(|url| url.contains("attendance-records")),
        "学期结束后不得查询考勤记录：{seen:?}"
    );
}

#[test]
fn schedule_before_semester_start_is_empty_with_notice() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let today = chrono::Local::now().date_naive();
    let start = (today + chrono::Duration::days(3)).to_string();
    let end = (today + chrono::Duration::days(100)).to_string();
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        tracker.lock().expect("lock").push(url.clone());
        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        panic!("学期尚未开始时不应再查询：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("课表加载应当成功");

    let schedule = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .expect("应发出课表事件");
    assert!(schedule.lessons.is_empty(), "学期开始前不得显示未来课程");
    let notice = schedule.notice.as_deref().unwrap_or_default();
    assert!(notice.contains("尚未开始"), "应提示尚未开始：{notice}");
    let seen = seen.lock().expect("lock");
    assert!(
        !seen.iter().any(|url| url.contains("/timetable/weekly")),
        "学期开始前不得查询课表：{seen:?}"
    );
    assert!(
        !seen.iter().any(|url| url.contains("attendance-records")),
        "学期开始前不得查询考勤记录：{seen:?}"
    );
}

/// 產生「今天正好有一堂課」的學期測資：學期開始日為週一（貼近真實校曆），
/// 課程排在今天的星期，`weeks` 為今天的週次。
///
/// 以推導取代固定值：固定「開始日＝今天減 14 天＋課程排週一」只在今天恰為
/// 週一時語意自洽，其餘日子並未真正驗證星期與週次的對齊。
/// 回傳（學期開始日期字串、課程的 `dayOfWeek`）。
fn semester_fixture_meeting_today(today: chrono::NaiveDate, weeks: u32) -> (String, u32) {
    use chrono::Datelike as _;

    let monday = today - chrono::Duration::days(i64::from(today.weekday().num_days_from_monday()));
    let start = monday - chrono::Duration::days(i64::from(weeks - 1) * 7);
    (
        start.to_string(),
        today.weekday().num_days_from_monday() + 1,
    )
}

#[test]
fn schedule_in_session_matches_attendance_without_notice() {
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let end = (today + chrono::Duration::days(90)).to_string();
    let attendance_date = today.to_string();
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        if url.contains("/timetable/weekly") {
            return Ok(json(serde_json::json!({ "code": 0, "data": { "courses": [{
                "courseName": "线性代数",
                "teacherName": "张老师",
                "classroomName": "主楼A101",
                "dayOfWeek": day_of_week,
                "startSection": 1,
                "endSection": 2,
                "weekRanges": "1-30",
            }]}})));
        }
        if url.contains("attendance-records") {
            return Ok(json(serde_json::json!({ "code": 0, "data": {
                "rows": [{
                    "resultId": 1,
                    "startSection": 1,
                    "endSection": 2,
                    "courseWeek": 3,
                    "classroomName": "主楼A101",
                    "teacherName": "张老师",
                    "attendanceStatus": "NORMAL",
                    "attendanceDate": attendance_date.clone(),
                }],
                "total": 1,
            }})));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("课表加载应当成功");

    let schedule = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .expect("应发出课表事件");
    assert!(schedule.notice.is_none(), "学期内不应有提示");
    assert_eq!(schedule.week, 3);
    assert_eq!(schedule.lessons.len(), 1);
    assert_eq!(schedule.lessons[0].date, today);
    assert_eq!(
        schedule.lessons[0].attendance,
        LessonAttendance::Recorded(AttendanceStatus::Normal)
    );
}

/// 讀不出來的考勤記錄與課程只跳過該筆，並在課表頁提示筆數。
///
/// 修復前是整批嚴格解析：一筆 `resultId` 型別異常的記錄或一門缺 `dayOfWeek`
/// 的課程都會讓整個課表頁失敗，連帶所有課程都沒有考勤狀態。
#[test]
fn schedule_reports_skipped_records_and_courses() {
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let end = (today + chrono::Duration::days(90)).to_string();
    let attendance_date = today.to_string();
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        if url.contains("/timetable/weekly") {
            return Ok(json(serde_json::json!({ "code": 0, "data": { "courses": [
                {
                    "courseName": "线性代数",
                    "teacherName": "张老师",
                    "classroomName": "主楼A101",
                    "dayOfWeek": day_of_week,
                    "startSection": 1,
                    "endSection": 2,
                    "weekRanges": "1-30",
                },
                // 缺 `dayOfWeek`：无法比对，跳過這門課。
                {"courseName": "缺星期", "startSection": 3, "endSection": 4}
            ]}})));
        }
        if url.contains("attendance-records") {
            return Ok(json(serde_json::json!({ "code": 0, "data": {
                "rows": [
                    {
                        "resultId": 1,
                        "startSection": 1,
                        "endSection": 2,
                        "courseWeek": 3,
                        "classroomName": "主楼A101",
                        "teacherName": "张老师",
                        "attendanceStatus": "NORMAL",
                        "attendanceDate": attendance_date.clone(),
                    },
                    // 缺 `attendanceStatus`：无法比对，跳過這筆。
                    {"resultId": 2, "startSection": 1, "endSection": 2}
                ],
                "total": 2,
            }})));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("单笔异常不得让课表加载失败");

    let schedule = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .expect("应发出课表事件");
    assert_eq!(schedule.skipped, 1, "读不出来的课程数应计入提示");
    let notice = schedule.notice.as_deref().unwrap_or_default();
    assert!(
        notice.contains("已跳过 1 条无法解析的考勤记录"),
        "应提示被跳过的记录：{notice}"
    );
    assert_eq!(schedule.lessons.len(), 1, "能解析的课程仍应显示");
    assert_eq!(
        schedule.lessons[0].attendance,
        LessonAttendance::Recorded(AttendanceStatus::Normal),
        "能解析的记录仍应正常比对"
    );
}

/// 第 23 週（超出假設的 22 個教學週、仍在學期內）不得回退顯示第 22 週
/// 的舊課程：舊碼會夾取週次，讓已結課課程以錯位日期重新出現。
#[test]
fn schedule_does_not_fall_back_to_week_22_after_teaching_weeks() {
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 23);
    let end = (today + chrono::Duration::days(30)).to_string();
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        if url.contains("/timetable/weekly") {
            return Ok(json(serde_json::json!({ "code": 0, "data": { "courses": [
                {
                    "courseName": "已结课课程",
                    "teacherName": "张老师",
                    "classroomName": "主楼A101",
                    "dayOfWeek": day_of_week,
                    "startSection": 1,
                    "endSection": 2,
                    "weekRanges": "1-22",
                },
                {
                    "courseName": "贯穿课程",
                    "teacherName": "李老师",
                    "classroomName": "主楼B202",
                    "dayOfWeek": day_of_week,
                    "startSection": 3,
                    "endSection": 4,
                    "weekRanges": "1-30",
                },
            ]}})));
        }
        if url.contains("attendance-records") {
            return Ok(json(
                serde_json::json!({ "code": 0, "data": { "rows": [], "total": 0 } }),
            ));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("课表加载应当成功");

    let schedule = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .expect("应发出课表事件");
    assert_eq!(
        schedule.week, 23,
        "第 23 周必须以真实周次呈现（不再夹取为 22）"
    );
    assert!(schedule.notice.is_none(), "学期内不应有提示");
    let names: Vec<&str> = schedule
        .lessons
        .iter()
        .map(|lesson| lesson.course_name.as_str())
        .collect();
    assert!(
        !names.contains(&"已结课课程"),
        "第 22 周后不得回退显示已结课课程：{names:?}"
    );
    assert!(
        names.contains(&"贯穿课程"),
        "跨越第 23 周的课程仍应显示：{names:?}"
    );
}

/// 同日課程的節次必須以數值排序：`sections` 是顯示字串，字典序會讓
/// 「11-12」排到「3-4」之前；跨日仍以日期為先。
#[test]
fn schedule_sorts_lessons_by_numeric_sections() {
    let today = chrono::Local::now().date_naive();
    let start = (today - chrono::Duration::days(14)).to_string();
    let end = (today + chrono::Duration::days(90)).to_string();
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.clone();
        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        if url.contains("/timetable/weekly") {
            return Ok(json(serde_json::json!({ "code": 0, "data": { "courses": [
                {
                    "courseName": "晚课十一",
                    "teacherName": "张老师",
                    "classroomName": "主楼A101",
                    "dayOfWeek": 1,
                    "startSection": 11,
                    "endSection": 12,
                    "weekRanges": "1-30",
                },
                {
                    "courseName": "早课一",
                    "teacherName": "李老师",
                    "classroomName": "主楼A102",
                    "dayOfWeek": 1,
                    "startSection": 1,
                    "endSection": 2,
                    "weekRanges": "1-30",
                },
                {
                    "courseName": "下午课",
                    "teacherName": "王老师",
                    "classroomName": "主楼A103",
                    "dayOfWeek": 1,
                    "startSection": 3,
                    "endSection": 4,
                    "weekRanges": "1-30",
                },
                {
                    "courseName": "晚课十",
                    "teacherName": "赵老师",
                    "classroomName": "主楼A104",
                    "dayOfWeek": 2,
                    "startSection": 10,
                    "endSection": 11,
                    "weekRanges": "1-30",
                },
                {
                    "courseName": "早课二",
                    "teacherName": "钱老师",
                    "classroomName": "主楼A105",
                    "dayOfWeek": 2,
                    "startSection": 2,
                    "endSection": 3,
                    "weekRanges": "1-30",
                },
            ]}})));
        }
        if url.contains("attendance-records") {
            return Ok(json(
                serde_json::json!({ "code": 0, "data": { "rows": [], "total": 0 } }),
            ));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("课表加载应当成功");

    let schedule = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .expect("应发出课表事件");

    let sections: Vec<&str> = schedule
        .lessons
        .iter()
        .map(|lesson| lesson.sections.as_str())
        .collect();
    assert_eq!(
        sections,
        ["1-2", "3-4", "11-12", "2-3", "10-11"],
        "同日节次应按数值排序，跨日以日期为先后"
    );
    for lesson in &schedule.lessons {
        assert_eq!(
            lesson.sections,
            format!("{}-{}", lesson.start_section, lesson.end_section),
            "显示字符串应与数值节次一致"
        );
    }
}

/// 取出最近一次課表事件（其餘事件忽略）。
fn schedule_event(harness: &mut Harness) -> crate::model::ScheduleData {
    let events = harness.drain_events();
    // 失敗時帶上實際收到的事件，方便診斷（例如載入失敗的錯誤訊息）。
    let kinds: Vec<String> = events
        .iter()
        .map(|event| match event {
            Event::Schedule(_) => "schedule".to_owned(),
            Event::Failed { message, .. } => format!("failed: {message}"),
            Event::SessionExpired { .. } => "session-expired".to_owned(),
            Event::LoginProgress(_) => "login-progress".to_owned(),
            Event::LoadingCancelled { .. } => "loading-cancelled".to_owned(),
            Event::Notice(message) => format!("notice: {message}"),
            _ => "other".to_owned(),
        })
        .collect();
    events
        .into_iter()
        .find_map(|event| match event {
            Event::Schedule(data) => Some(*data),
            _ => None,
        })
        .unwrap_or_else(|| panic!("应发出课表事件（实际事件：{kinds:?}）"))
}

/// 執行排隊中的資料任務（模擬 [`Worker::run`] 取出佇列中的任務）。
///
/// `SetScheduleWeek` 是控制任務：它只記住週次並排入一次載入，真正的載入
/// 由工作執行緒的佇列取出後執行；同步的測試必須自行模擬這一步。
fn run_queued(harness: &mut Harness) {
    let queued: Vec<Job> = harness.worker.pending_data.drain(..).collect();
    assert!(!queued.is_empty(), "应有排队的载入任务");
    for job in queued {
        harness.dispatch(job).expect("排队的载入应当成功");
    }
}

/// 課表測試用的假考勤站點：一個學期、一門課（週次 `1-30`）與空的考勤記錄。
fn schedule_site(
    seen: Arc<Mutex<Vec<String>>>,
    start: String,
    end: String,
    day_of_week: u32,
) -> impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static {
    schedule_site_with_weeks(seen, start, end, day_of_week, "1-30")
}

/// 同上，但可指定課程的 `weekRanges`（用來驗證週次上限的來源）。
///
/// 記錄每個請求；考勤記錄請求額外帶上查詢的日期範圍（`startDate~endDate`），
/// 用來驗證切週時查的是以學期開始日錨定的那一週。
fn schedule_site_with_weeks(
    seen: Arc<Mutex<Vec<String>>>,
    start: String,
    end: String,
    day_of_week: u32,
    week_ranges: &'static str,
) -> impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static {
    move |request: &HttpRequest| {
        let url = request.url.clone();
        let mut label = url.clone();
        if url.contains("attendance-records")
            && let Some(Body::Json(body)) = request.body.as_ref()
        {
            label = format!(
                "{url} {}~{}",
                body["data"]["startDate"].as_str().unwrap_or_default(),
                body["data"]["endDate"].as_str().unwrap_or_default()
            );
        }
        seen.lock().expect("lock").push(label);

        if url.ends_with("/timetable/semesters") {
            return Ok(json(serde_json::json!({ "code": 0, "data": [{
                "semesterId": "s-1",
                "academicYear": "2026-2027",
                "semesterName": "第一学期",
                "startDate": start.clone(),
                "endDate": end.clone(),
            }]})));
        }
        if url.contains("/timetable/weekly") {
            return Ok(json(serde_json::json!({ "code": 0, "data": { "courses": [{
                "courseName": "线性代数",
                "teacherName": "张老师",
                "classroomName": "主楼A101",
                "dayOfWeek": day_of_week,
                "startSection": 1,
                "endSection": 2,
                "weekRanges": week_ranges,
            }]}})));
        }
        if url.contains("attendance-records") {
            return Ok(json(
                serde_json::json!({ "code": 0, "data": { "rows": [], "total": 0 } }),
            ));
        }
        panic!("未预期的请求：{url}");
    }
}

/// 切換週次重用整學期課表快取：只重查該週的考勤記錄，日期以學期開始日錨定。
#[test]
fn schedule_week_switch_reuses_the_semester_cache() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    // 學期開始日為本週週一往回兩週：今天是第 3 週。
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let start_date = chrono::NaiveDate::parse_from_str(&start, "%Y-%m-%d").expect("学期开始日");
    let end = (today + chrono::Duration::days(90)).to_string();
    let mut harness = harness(schedule_site(Arc::clone(&seen), start, end, day_of_week));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("首次载入应当成功");
    let first = schedule_event(&mut harness);
    assert_eq!(first.week, 3, "默认显示当前周");
    assert_eq!(first.total_weeks, 30, "总周数取课程声明的最大周次");
    seen.lock().expect("lock").clear();

    harness
        .dispatch(Job::SetScheduleWeek { week: 5 })
        .expect("切换周次应当成功");
    run_queued(&mut harness);
    let switched = schedule_event(&mut harness);
    assert_eq!(switched.week, 5);
    assert_eq!(switched.total_weeks, 30);
    assert_eq!(switched.lessons.len(), 1, "第 5 周仍有课程");
    let monday = start_date + chrono::Duration::days(28);
    assert_eq!(
        switched.lessons[0].date,
        monday + chrono::Duration::days(i64::from(day_of_week - 1)),
        "第 5 周的课程日期应以学期开始日锚定"
    );

    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1, "切周只需重查该周考勤：{seen:?}");
    let sunday = monday + chrono::Duration::days(6);
    assert!(
        seen[0].contains(&format!("{monday}~{sunday}")),
        "第 5 周的考勤范围应以学期开始日锚定：{seen:?}"
    );
}

/// 尚未載入過課表就切週：補查學期、整學期課表與該週考勤。
#[test]
fn schedule_week_switch_before_first_load_fetches_everything() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let end = (today + chrono::Duration::days(90)).to_string();
    let mut harness = harness(schedule_site(Arc::clone(&seen), start, end, day_of_week));
    harness.login_both_sites();

    harness
        .dispatch(Job::SetScheduleWeek { week: 2 })
        .expect("切换周次应当成功");
    run_queued(&mut harness);

    let schedule = schedule_event(&mut harness);
    assert_eq!(schedule.week, 2, "未载入过也能直接切到指定周");
    assert_eq!(schedule.total_weeks, 30);
    let seen = seen.lock().expect("lock");
    assert!(
        seen.iter().any(|url| url.ends_with("/timetable/semesters")),
        "应补查学期：{seen:?}"
    );
    assert!(
        seen.iter().any(|url| url.contains("/timetable/weekly")),
        "应补查整学期课表：{seen:?}"
    );
    assert!(
        seen.iter().any(|url| url.contains("attendance-records")),
        "应补查该周考勤：{seen:?}"
    );
}

/// 連續切週：佇列中至多一筆載入，只查最後選定的那一週。
#[test]
fn consecutive_week_switches_only_load_the_last_week() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let start_date = chrono::NaiveDate::parse_from_str(&start, "%Y-%m-%d").expect("学期开始日");
    let end = (today + chrono::Duration::days(90)).to_string();
    let mut harness = harness(schedule_site(Arc::clone(&seen), start, end, day_of_week));
    harness.login_both_sites();
    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("首次载入应当成功");
    let _ = schedule_event(&mut harness);
    seen.lock().expect("lock").clear();

    harness
        .dispatch(Job::SetScheduleWeek { week: 4 })
        .expect("切换周次应当成功");
    harness
        .dispatch(Job::SetScheduleWeek { week: 6 })
        .expect("再次切换应当成功");
    assert_eq!(harness.worker.pending_data.len(), 1, "同键触发至多保留一笔");

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("补跑排队的载入应当成功");
    let schedule = schedule_event(&mut harness);
    assert_eq!(schedule.week, 6, "只执行最后选定的周次");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 1, "中间周次不应发起查询：{seen:?}");
    let monday = start_date + chrono::Duration::days(35);
    let sunday = monday + chrono::Duration::days(6);
    assert!(
        seen[0].contains(&format!("{monday}~{sunday}")),
        "只应查询最后选定那一周的考勤：{seen:?}"
    );
}

/// `r` 強制刷新保持目前瀏覽的週次，並重新查詢學期與課表（繞過快取）。
#[test]
fn forced_refresh_keeps_the_selected_week() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let end = (today + chrono::Duration::days(90)).to_string();
    let mut harness = harness(schedule_site(Arc::clone(&seen), start, end, day_of_week));
    harness.login_both_sites();
    harness
        .dispatch(Job::SetScheduleWeek { week: 5 })
        .expect("切换周次应当成功");
    run_queued(&mut harness);
    let _ = schedule_event(&mut harness);
    seen.lock().expect("lock").clear();

    harness
        .dispatch(Job::LoadSchedule { force: true })
        .expect("强制刷新应当成功");
    let schedule = schedule_event(&mut harness);
    assert_eq!(schedule.week, 5, "刷新不应跳回当前周");
    let seen = seen.lock().expect("lock");
    assert!(
        seen.iter().any(|url| url.ends_with("/timetable/semesters")),
        "强制刷新应重查学期：{seen:?}"
    );
    assert!(
        seen.iter().any(|url| url.contains("/timetable/weekly")),
        "强制刷新应重查整学期课表：{seen:?}"
    );
    assert!(
        seen.iter().any(|url| url.contains("attendance-records")),
        "强制刷新应重查该周考勤：{seen:?}"
    );
}

/// 學期已結束時明確切週：補查該週課表與考勤（預設載入仍不查詢）。
#[test]
fn explicit_week_after_semester_end_loads_that_week() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    let start = (today - chrono::Duration::days(90)).to_string();
    let end = (today - chrono::Duration::days(3)).to_string();
    let mut harness = harness(schedule_site(Arc::clone(&seen), start, end, 1));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("默认载入应当成功");
    let ended = schedule_event(&mut harness);
    assert!(ended.lessons.is_empty(), "学期结束后默认不显示旧课程");
    assert!(
        ended
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("已结束"),
        "默认载入应提示学期已结束：{:?}",
        ended.notice
    );

    harness
        .dispatch(Job::SetScheduleWeek { week: 3 })
        .expect("切换周次应当成功");
    run_queued(&mut harness);
    let schedule = schedule_event(&mut harness);
    assert_eq!(schedule.week, 3, "明确切周应加载该周");
    assert!(schedule.notice.is_none(), "明确切周不再提示学期已结束");
    assert_eq!(schedule.lessons.len(), 1, "补查的课表应有第 3 周的课程");
    assert_eq!(schedule.total_weeks, 30);
}

/// 同一週次重複切換不重複查詢。
#[test]
fn set_schedule_week_same_value_is_a_no_op() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    let end = (today + chrono::Duration::days(90)).to_string();
    let mut harness = harness(schedule_site(Arc::clone(&seen), start, end, day_of_week));
    harness.login_both_sites();
    harness
        .dispatch(Job::SetScheduleWeek { week: 5 })
        .expect("切换周次应当成功");
    run_queued(&mut harness);
    let _ = schedule_event(&mut harness);
    seen.lock().expect("lock").clear();

    harness
        .dispatch(Job::SetScheduleWeek { week: 5 })
        .expect("同值切换应当成功");
    assert!(
        harness.worker.pending_data.is_empty(),
        "同值切换不应排队任何载入"
    );
    assert!(
        seen.lock().expect("lock").is_empty(),
        "同值切换不应发出请求"
    );
}

/// 學期結束日涵蓋考試週與假期（考勤入口實測可到第 23 週）時，週次上限仍以
/// 「課表最晚有課的週次」為準——參考實作的考勤來源就是這樣算的（教務系統的
/// 「總周次」欄位考勤 API 並未提供）。
#[test]
fn week_bound_follows_the_last_course_week_not_the_semester_end() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    let (start, day_of_week) = semester_fixture_meeting_today(today, 3);
    // 結束日遠在課表之後：從第 3 週再往後 130 天約為第 21 週。
    let end = (today + chrono::Duration::days(130)).to_string();
    let mut harness = harness(schedule_site_with_weeks(
        Arc::clone(&seen),
        start,
        end,
        day_of_week,
        "1-19",
    ));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("课表加载应当成功");

    let schedule = schedule_event(&mut harness);
    assert_eq!(schedule.week, 3);
    assert_eq!(
        schedule.total_weeks, 19,
        "上限应取课表最晚有课的周次，而不是学期结束日推算出的周次"
    );
    assert_eq!(schedule.lessons.len(), 1, "第 3 周仍有课程");
}

/// 考試週（教學週已結束）往回翻週時，週次上限不得跟著縮小：否則介面的
/// `[`／`]` 會以上限為邊界，使用者再也翻不回本週（`r` 保留選定週次）。
#[test]
fn week_bound_does_not_shrink_when_paging_back_from_an_exam_week() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let today = chrono::Local::now().date_naive();
    // 課程只排到第 19 週，今天是第 21 週（考試週）。
    let (start, day_of_week) = semester_fixture_meeting_today(today, 21);
    let end = (today + chrono::Duration::days(30)).to_string();
    let mut harness = harness(schedule_site_with_weeks(
        Arc::clone(&seen),
        start,
        end,
        day_of_week,
        "1-19",
    ));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadSchedule { force: false })
        .expect("首次载入应当成功");
    let first = schedule_event(&mut harness);
    assert_eq!(first.week, 21, "今天是第 21 周");
    assert_eq!(first.total_weeks, 21, "上限至少涵盖本周");

    // 往回翻到最後一堂教學週：上限必須維持不變。
    harness
        .dispatch(Job::SetScheduleWeek { week: 19 })
        .expect("切换周次应当成功");
    run_queued(&mut harness);
    let back = schedule_event(&mut harness);
    assert_eq!(back.week, 19);
    assert_eq!(back.total_weeks, 21, "往回翻周不应让上限缩小");
    assert_eq!(back.lessons.len(), 1, "第 19 周仍有课程");

    // 上限未變，因此可以再翻回本週。
    harness
        .dispatch(Job::SetScheduleWeek { week: 21 })
        .expect("切回本周应当成功");
    run_queued(&mut harness);
    let again = schedule_event(&mut harness);
    assert_eq!(again.week, 21);
    assert_eq!(again.total_weeks, 21);
}

#[test]
fn activity_detail_for_material_skips_submission_request() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: vec![(
            "77",
            serde_json::json!({ "id": "77", "type": "material", "title": "课件",
                "end_time": "2026-10-01 12:00:00",
                "data": { "description": "   ", "content": "<div>课程介绍</div>" } }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadActivityDetail {
            activity_id: "77".to_owned(),
            force: false,
        })
        .expect("加载详情");

    let seen = site.urls();
    assert!(
        seen.iter().any(|url| url.ends_with("/api/activities/77")),
        "应请求活动详情：{seen:?}"
    );
    assert!(
        !seen.iter().any(|url| url.contains("/submission_list")),
        "非作业不得查询提交记录：{seen:?}"
    );
    // `saw` 會取出事件，因此一次取完再逐項斷言。
    let detail = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::ActivityDetail(detail) => Some(detail),
            _ => None,
        })
        .expect("应回报活动详情");
    assert_eq!(detail.kind, lms::ActivityKind::Material);
    assert!(detail.submissions.is_none(), "非作业不得带提交状态");
    assert_eq!(
        description_text(&detail.description),
        Some("课程介绍"),
        "页面型活动的正文应取自 data.content（description 为空）"
    );
}

/// 活動詳情的 `force` 要真的略過快取（介面在詳情層按 `r` 時送出）。
///
/// 修復前 `Job::LoadActivityDetail` 沒有 `force` 欄位、`load_activity_detail`
/// 也硬寫 `false`，因此詳情層的 `r` 只會命中快取、畫面不會更新。
#[test]
fn forced_activity_detail_skips_the_cache() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: vec![(
            "77",
            serde_json::json!({ "id": "77", "type": "material", "title": "课件",
                "data": { "content": "<div>课程介绍</div>" } }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    let detail_requests = || {
        site.urls()
            .into_iter()
            .filter(|url| url.ends_with("/api/activities/77"))
            .count()
    };
    let load = |harness: &mut Harness, force: bool| {
        harness
            .dispatch(Job::LoadActivityDetail {
                activity_id: "77".to_owned(),
                force,
            })
            .expect("加载活动详情");
        let _ = harness.drain_events();
    };

    load(&mut harness, false);
    assert_eq!(detail_requests(), 1, "第一次应查询详情");

    load(&mut harness, false);
    assert_eq!(detail_requests(), 1, "非强制载入应命中详情快取");

    load(&mut harness, true);
    assert_eq!(detail_requests(), 2, "强制刷新必须重新查询详情");
}

#[test]
fn activity_detail_for_homework_queries_submission_list() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "第 3 次作业",
                "end_time": "2026-10-01 23:59:59", "submit_by_group": false,
                "user_submit_count": 0,
                "data": { "description": "<p>第一章习题</p>" } }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    harness
        .dispatch(Job::LoadActivityDetail {
            activity_id: "11".to_owned(),
            force: false,
        })
        .expect("加载详情");

    let seen = site.urls();
    assert!(
        seen.iter()
            .any(|url| url.contains("/students/42/submission_list")),
        "作业详情应查询个人提交记录：{seen:?}"
    );
    // `saw` 會取出事件，因此一次取完再逐項斷言。
    let detail = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::ActivityDetail(detail) => Some(detail),
            _ => None,
        })
        .expect("应回报活动详情");
    assert_eq!(detail.kind, lms::ActivityKind::Homework);
    assert!(detail.submissions.is_some(), "作业详情应带提交记录");
    assert_eq!(
        description_text(&detail.description),
        Some("第一章习题"),
        "作业说明应转为纯文字带进详情"
    );
}

/// 作業說明來自已抓取的活動詳情：冷啟動與快取命中都必須帶出，且不增加請求。
#[test]
fn homework_carries_activity_description_without_extra_requests() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A",
                  "end_time": "2099-12-31 23:59:59" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "end_time": "2099-12-31 23:59:59",
                "submit_by_group": false, "user_submit_count": 0,
                "data": { "description": "<p>第一章习题</p><p>交到邮箱</p>", "content": "" },
                "uploads": [{ "id": 1, "name": "题目.pdf", "size": 2048 }] }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");
    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(
        description_text(&last.items[0].description),
        Some("第一章习题\n交到邮箱"),
        "作业说明应为去除标签的纯文字"
    );
    assert_eq!(
        attachment_names(&last.items[0].description),
        ["题目.pdf"],
        "附件应随详情一起带出"
    );

    // 第二次载入命中快取：说明仍须带出，且不得重新查询课程、活动或详情
    // （考勤学期查询与作业快取无关，不在此限）。
    let api_calls = |site: &FakeHomeworkSite| {
        site.urls()
            .iter()
            .filter(|url| url.contains("/api/"))
            .count()
    };
    let before = api_calls(&site);
    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("第二次加载应当成功");
    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("第二次最终更新");
    assert_eq!(
        description_text(&last.items[0].description),
        Some("第一章习题\n交到邮箱"),
        "缓存命中时说明不得遗失"
    );
    assert_eq!(
        attachment_names(&last.items[0].description),
        ["题目.pdf"],
        "缓存命中时附件不得遗失"
    );
    assert_eq!(api_calls(&site), before, "缓存命中不应重新查询 LMS 资料");
}

/// 整份说明只有一张图片：不得当成「没有说明」，要保留「含图片」的标记。
#[test]
fn homework_marks_image_only_description() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "图片作业",
                  "end_time": "2099-12-31 23:59:59" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "图片作业",
                "end_time": "2099-12-31 23:59:59",
                "submit_by_group": false, "user_submit_count": 0,
                "data": { "description": "<p><img src=\"/a.png\"></p>" } }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");
    let updates = homework_updates(&mut harness);
    let description = updates.last().expect("最终更新").items[0]
        .description
        .clone()
        .expect("纯图片说明仍应保留正文（供界面标注）");
    assert_eq!(description.text, None, "图片没有可见文字");
    assert!(description.has_media, "应标记含图片");
}

/// 說明欄位型別異常時只損失該段說明，不得連帶把提交狀態打成「待核实」。
///
/// 回歸：詳情子物件内部欄位型別不符曾讓整份活動詳情解析失敗，作業因此全部退回
/// 「待核实」（即使伺服器已給出提交次數）。
#[test]
fn malformed_description_does_not_break_submission_status() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A",
                  "end_time": "2099-12-31 23:59:59" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "end_time": "2099-12-31 23:59:59",
                "submit_by_group": false, "user_submit_count": 0,
                "data": { "description": 12345 } }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");
    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.items.len(), 1);
    // 型別異常的說明不顯示內容，但要留下原因讓介面提示（不得靜默成「沒有說明」）。
    let description = last.items[0]
        .description
        .as_ref()
        .expect("类型异常时应保留原因");
    assert_eq!(description.text, None, "类型异常的说明不应显示内容");
    assert_eq!(description.issue, Some(BODY_FIELD_TYPE_NOTE));
    assert_eq!(
        last.items[0].state,
        HomeworkState::Pending,
        "提交状态不得因说明字段类型异常而退回「待核实」"
    );
    assert!(
        last.issues.is_empty(),
        "不应产生待核实汇总：{:?}",
        last.issues
    );
}

/// `data` 不是物件（型別假設與實際回應不符）時：提交狀態照常，說明帶出原因。
///
/// 這是實網驗收的診斷路徑——若學校把正文改成字串（或改了欄位結構），作業清單仍
/// 必須正確，而詳情面板會說明「未取得正文」，不必靠猜。
#[test]
fn non_object_activity_data_keeps_the_status_and_reports_the_reason() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A",
                  "end_time": "2099-12-31 23:59:59" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "end_time": "2099-12-31 23:59:59",
                "submit_by_group": false, "user_submit_count": 0,
                "data": "<p>整份是字符串</p>" }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("作业加载应当成功");
    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.items.len(), 1);
    assert_eq!(
        last.items[0].state,
        HomeworkState::Pending,
        "提交状态不得因正文字段型别异常而退回「待核实」"
    );
    let description = last.items[0]
        .description
        .as_ref()
        .expect("应保留读取失败的原因");
    assert_eq!(description.text, None);
    assert_eq!(description.issue, Some(BODY_NOT_OBJECT_NOTE));
}

#[test]
fn opening_lesson_activity_uses_server_player_url() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    harness
        .dispatch(Job::OpenActivity {
            activity_id: "23".to_owned(),
            course_id: None,
            kind: lms::ActivityKind::Lesson,
        })
        .expect("打开活动");

    let seen = site.urls();
    assert!(
        seen.iter()
            .any(|url| url.contains("/api/lessons/23/player-url")),
        "课程内容应查询播放器接口：{seen:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/lesson/player?token=abc"
        )),
        "应回报服务器提供的播放地址"
    );
}

#[test]
fn opening_homework_activity_opens_course_homework_page() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    harness
        .dispatch(Job::OpenActivity {
            activity_id: "11".to_owned(),
            course_id: Some("42".to_owned()),
            kind: lms::ActivityKind::Homework,
        })
        .expect("打开活动");

    let seen = site.urls();
    assert!(
        !seen.iter().any(|url| url.contains("/player-url")),
        "作业不得查询播放器接口：{seen:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/course/42/homework"
        )),
        "作业应开启所属课程的作业列表（且不得附带 hash 片段）"
    );
}

#[test]
fn opening_homework_activity_without_usable_course_falls_back_to_home() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    // 缺少課程識別碼。
    harness
        .dispatch(Job::OpenActivity {
            activity_id: "11".to_owned(),
            course_id: None,
            kind: lms::ActivityKind::Homework,
        })
        .expect("打开活动");
    // 識別碼含 URL unsafe 字元：不得拼接進網址。
    harness
        .dispatch(Job::OpenActivity {
            activity_id: "12".to_owned(),
            course_id: Some("4 2/../x".to_owned()),
            kind: lms::ActivityKind::Homework,
        })
        .expect("打开活动");

    let events = harness.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::Notice(message) if message.contains("无法确定作业所属课程")
            ))
            .count(),
        2,
        "两种无效识别码都应告知用户：{events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::OpenUrl(url) if url.contains("/course/")
        )),
        "无效识别码不得拼出课程网址：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn"
        )),
        "应回退到思源学堂首页"
    );
}

#[test]
fn opening_lesson_in_webvpn_mode_rewrites_url() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness
        .worker
        .session
        .as_mut()
        .expect("会话已建立")
        .mark_logged_in(SiteKind::Lms, AccessMode::WebVpn, Vec::new());

    harness
        .dispatch(Job::OpenActivity {
            activity_id: "23".to_owned(),
            course_id: None,
            kind: lms::ActivityKind::Lesson,
        })
        .expect("打开活动");

    assert!(
        harness.saw(|event| matches!(
            event,
            Event::OpenUrl(url) if url.starts_with("https://webvpn.xjtu.edu.cn/")
        )),
        "WebVPN 模式下应回报改写后的网址"
    );
}

#[test]
fn opening_lesson_without_player_url_falls_back_to_home() {
    let mut harness = harness(|request| {
        let url = request.url.clone();
        if url.contains("/player-url") {
            return Ok(HttpResponse::new(500, url.as_str(), b"oops".to_vec()));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_lms_only();

    harness
        .dispatch(Job::OpenActivity {
            activity_id: "23".to_owned(),
            course_id: None,
            kind: lms::ActivityKind::Lesson,
        })
        .expect("打开活动");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(message) if message.contains("无法获取播放地址")
        )),
        "取不到播放地址时应告知用户"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn"
        )),
        "应回退到思源学堂首页"
    );
}

/// 伺服器回傳的播放網址不在校內網域時一律拒絕開啟：它帶有存取 token。
#[test]
fn opening_lesson_with_an_external_player_url_falls_back_to_home() {
    let mut harness = harness(|request| {
        let url = request.url.clone();
        if url.contains("/player-url") {
            return Ok(json(serde_json::json!({
                "url": "https://example.com/player?token=secret-token"
            })));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_lms_only();

    harness
        .dispatch(Job::OpenActivity {
            activity_id: "23".to_owned(),
            course_id: None,
            kind: lms::ActivityKind::Lesson,
        })
        .expect("打开活动");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(message)
                if message.contains("无法获取播放地址") && message.contains("example.com")
        )),
        "应说明被拒绝的主机：{events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::OpenUrl(url) if url.contains("example.com")
        )),
        "不得开启非校内网址：{events:?}"
    );
    assert!(
        events
            .iter()
            .all(|event| !format!("{event:?}").contains("secret-token")),
        "任何事件都不得带出存取 token：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn"
        )),
        "应回退到思源学堂首页"
    );
}

#[test]
fn only_open_activity_is_interactive() {
    let open = Job::OpenActivity {
        activity_id: "1".to_owned(),
        course_id: None,
        kind: lms::ActivityKind::Homework,
    };
    assert!(open.is_interactive(), "开启活动是互动式数据任务");
    assert!(
        !open.is_control(),
        "互动式任务维持数据任务语意（去重与统一重新登录重试）"
    );

    for job in [
        Job::LoadSchedule { force: false },
        Job::LoadHomework { force: false },
        Job::LoadFlow { page: 1 },
        Job::LoadCourses { force: false },
        Job::LoadActivities {
            course_id: "1".to_owned(),
            force: false,
        },
        Job::LoadActivityDetail {
            activity_id: "1".to_owned(),
            force: false,
        },
        Job::SetHomeworkTerm {
            term: "2026-2027-1".to_owned(),
        },
        Job::CancelLogin,
        Job::Shutdown,
    ] {
        assert!(!job.is_interactive(), "{job:?} 不是互动式任务");
    }
}

/// 只有發送簡訊驗證碼不適合自動重送：重送可能讓使用者收到兩條簡訊。
#[test]
fn only_the_sms_send_is_not_replayable() {
    assert!(!Job::SendMfaCode.is_replayable(), "发送短信有可见副作用");

    for job in [
        Job::RefreshCaptcha,
        Job::SubmitCaptcha("1234".into()),
        Job::VerifyMfaCode("123456".into()),
        Job::RetryLogin {
            site: SiteKind::Attendance,
        },
        Job::LoadSchedule { force: false },
        Job::OpenActivity {
            activity_id: "1".to_owned(),
            course_id: None,
            kind: lms::ActivityKind::Homework,
        },
    ] {
        assert!(job.is_replayable(), "{job:?} 可以原樣重送");
    }
}

/// 送碼端點連線失敗時直接回報，不自動重送（免得發出兩條簡訊）。
#[test]
fn connection_errors_do_not_replay_the_sms_send() {
    let sends = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sends);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
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
                ATTENDANCE_POST,
                login_page_with_mfa(),
            ));
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
        if url.contains("/securephone/send") {
            counter.fetch_add(1, Ordering::SeqCst);
            return Err(AppError::network_kind(
                NetworkKind::Timeout,
                "请求超时".to_owned(),
            ));
        }
        panic!("未预期的请求：{url}");
    });
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("登录应停在短信验证");

    harness
        .dispatch(Job::SendMfaCode)
        .expect_err("发送失败应直接回报");

    assert_eq!(
        sends.load(Ordering::SeqCst),
        1,
        "不得自动重送（使用者可能因此收到两条短信）"
    );
    let events = harness.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Notice(message) if message.contains("正在重试"))),
        "不应有自动重试提示：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Login,
                ..
            }
        )),
        "应直接回报失败：{events:?}"
    );
}

#[test]
fn flush_interactive_runs_the_queued_open_and_keeps_other_jobs() {
    let mut harness = harness(|request: &HttpRequest| -> AppResult<HttpResponse> {
        panic!("开启作业网页不需任何请求：{}", request.url);
    });
    harness.login_lms_only();
    harness
        .worker
        .pending_data
        .push_back(Job::LoadSchedule { force: false });
    harness.worker.pending_data.push_back(Job::OpenActivity {
        activity_id: "5".to_owned(),
        course_id: Some("9".to_owned()),
        kind: lms::ActivityKind::Homework,
    });

    assert!(
        !harness.worker.flush_interactive(),
        "没有登录流程时不应暂停"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/course/9/homework"
        )),
        "排队中的互动式任务应立即执行"
    );
    assert_eq!(
        harness.worker.pending_data.len(),
        1,
        "非互动式任务不符条件，不得被执行"
    );
    assert!(matches!(
        harness.worker.pending_data[0],
        Job::LoadSchedule { .. }
    ));
}

// ── 互動式任務執行期間的控制任務插隊窗口 ──────────────

/// 把任務送進工作通道（通道注入端在建立 harness 後才會就緒）。
fn inject(slot: &Arc<Mutex<Option<Sender<Job>>>>, job: Job) {
    let sender = slot.lock().expect("lock").clone().expect("注入端已就绪");
    sender.send(job).expect("注入任务");
}

/// 「互動式任務執行期間插入控制任務」的共用假站點。
///
/// 兩門課程（活動皆為空）與完整的思源學堂、考勤同步登入流程：課程 1 的
/// 活動查詢把 `o`（課程內容）送進工作通道（下一輪排空時合併），思源學堂
/// 帳密提交（`LMS_POST`）成功那一刻把 `injected` 送進通道——它會在重新
/// 登入成功後、內層重試排空通道時被處理（本組測試要覆蓋的窗口）。
fn harness_with_switch_during_open(injected: Job) -> Harness {
    let slot: Arc<Mutex<Option<Sender<Job>>>> = Arc::new(Mutex::new(None));
    let injector = Arc::clone(&slot);
    let player_calls = AtomicUsize::new(0);
    let harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == lms::LOGIN_URL {
            return Ok(HttpResponse::new(200, LMS_POST, login_page()));
        }
        if url == LMS_POST {
            // 重新登入提交成功的當下注入控制任務。
            inject(&injector, injected.clone());
            return Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY));
        }
        if url == LMS_HOME || url.ends_with("/user/index") {
            return Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY));
        }
        if url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
        }
        if url == ATTENDANCE_POST {
            return Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY));
        }
        if url == ATTENDANCE_EXCHANGE {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "tokenValue": "token-1" }
            })));
        }
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
                { "id": "2", "name": "操作系统", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.ends_with("/courses/1/activities") {
            // 載入進行中按下 `o`。
            inject(
                &injector,
                Job::OpenActivity {
                    activity_id: "7".to_owned(),
                    course_id: Some("1".to_owned()),
                    kind: lms::ActivityKind::Lesson,
                },
            );
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.ends_with("/courses/2/activities") {
            // 活動查詢以一批（並行上限）為單位送出：第二門課程與第一門同時
            // 發出，因此即使在開啟任務期間取消，這個請求仍可能已經送出。
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        if url.contains("/player-url") {
            // 第一次查詢回報登入態失效（觸發同步重新登入）；其後恢復正常。
            if player_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(HttpResponse::new(
                    200,
                    "https://login.xjtu.edu.cn/cas/login?service=lms",
                    login_page(),
                ));
            }
            return Ok(json(serde_json::json!({
                "url": "https://lms.xjtu.edu.cn/lesson/player?token=abc"
            })));
        }
        panic!("未预期的请求：{url}");
    });
    *slot.lock().expect("lock") = Some(harness._jobs.clone());
    harness
}

/// 互動式任務執行期間同步完成的重新登入，若在內層處理了換帳號指令，
/// 外層不得再以舊帳號的資料繼續載入並回填畫面。
#[test]
fn account_switch_during_interactive_open_does_not_backfill_stale_homework() {
    let mut harness = harness_with_switch_during_open(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness.worker.run_homework_job(false);

    let events = harness.drain_events();
    let switched = events
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::SessionsCleared {
                    account_changed: true
                }
            )
        })
        .expect("换帐号应在开启任务执行期间发生");
    let stale: Vec<&Event> = events[switched..]
        .iter()
        .filter(|event| matches!(event, Event::Homework(_)))
        .collect();
    assert!(
        stale.is_empty(),
        "换帐号后不得回填旧帐号的作业数据：{stale:?}"
    );
    assert!(
        harness.worker.pending_data.is_empty(),
        "被取消的加载不得重新排队"
    );
}

/// 執行互動式任務期間的學期切換，外層不得再以舊學期的結果回填。
#[test]
fn term_switch_during_interactive_open_does_not_backfill_stale_homework() {
    let mut harness = harness_with_switch_during_open(Job::SetHomeworkTerm {
        term: "2025-2026-2".to_owned(),
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness.worker.run_homework_job(false);

    let events = harness.drain_events();
    let switched = events
        .iter()
        .position(|event| matches!(event, Event::CoursesTerm(Some(_))))
        .expect("切换学期应在开启任务执行期间发生");
    let stale: Vec<&Event> = events[switched..]
        .iter()
        .filter(|event| matches!(event, Event::Homework(_)))
        .collect();
    assert!(stale.is_empty(), "切换学期后不得回填旧学期结果：{stale:?}");
    assert!(
        matches!(
            harness.worker.pending_data.front(),
            Some(Job::LoadHomework { force: true })
        ),
        "切换学期排入的强制重载必须保留：{:?}",
        harness.worker.pending_data
    );
}

/// 開啟操作獨立取得自動重登額度：長載入已用掉自己的額度時，開啟仍應嘗試
/// 重新登入，而不是直接被判定為「自動重新登入後仍然失敗」。
#[test]
fn interactive_open_gets_its_own_relogin_budget() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });
    let system = Arc::clone(&site);
    let player_calls = AtomicUsize::new(0);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url.contains("/player-url") {
            // 第一次回報登入態失效；重新登入後的重試恢復正常。
            if player_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(HttpResponse::new(
                    200,
                    "https://login.xjtu.edu.cn/cas/login?service=lms",
                    login_page(),
                ));
            }
            return Ok(json(serde_json::json!({
                "url": "https://lms.xjtu.edu.cn/lesson/player?token=abc"
            })));
        }
        system.handle(request)
    });
    harness.login_lms_only();
    // 模擬作業載入與此開啟任務先前各用掉一次自動重登額度：額度按任務鍵
    // 獨立保存，新的開啟操作應重新取得自己的額度。
    assert!(
        harness
            .worker
            .relogin
            .try_consume(&DataKey::Homework)
            .is_some(),
        "前置：额度应可用"
    );
    assert!(
        harness
            .worker
            .relogin
            .try_consume(&DataKey::OpenActivity("7".to_owned()))
            .is_some(),
        "前置：额度应可用"
    );
    harness.worker.pending_data.push_back(Job::OpenActivity {
        activity_id: "7".to_owned(),
        course_id: None,
        kind: lms::ActivityKind::Lesson,
    });

    assert!(
        !harness.worker.flush_interactive(),
        "登录同步完成后不应留下进行中的流程"
    );

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/lesson/player?token=abc"
        )),
        "开启操作应独立取得额度并在重登后重试成功：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Failed { message, .. } if message.contains("自动重新登录")
        )),
        "不得误报自动重登失败：{events:?}"
    );
}

/// 開啟操作的自動重登仍有上限：重登後再失效即停止自動重試。
#[test]
fn interactive_open_relogin_stays_bounded() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });
    let system = Arc::clone(&site);
    let logins = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&logins);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == lms::LOGIN_URL {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        if url.contains("/player-url") {
            // 永遠回報登入態失效（包含重新登入後的重試）。
            return Ok(HttpResponse::new(
                200,
                "https://login.xjtu.edu.cn/cas/login?service=lms",
                login_page(),
            ));
        }
        system.handle(request)
    });
    harness.login_lms_only();
    harness.worker.pending_data.push_back(Job::OpenActivity {
        activity_id: "7".to_owned(),
        course_id: None,
        kind: lms::ActivityKind::Lesson,
    });

    assert!(!harness.worker.flush_interactive());

    let events = harness.drain_events();
    assert_eq!(logins.load(Ordering::SeqCst), 1, "开启操作至多自动重登一次");
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                message,
                target: FailedTarget::ActivityOpen,
                ..
            } if message.contains("自动重新登录")
        )),
        "重登后仍失效应回报自动重登失败：{events:?}"
    );
    assert!(harness.worker.retry.is_none(), "放弃后不得保留待重试任务");
}

/// 反向情境：開啟操作先重登成功後，作業載入的第一次失效仍應取得自己的
/// 自動重登額度（兩者的額度必須互相獨立，不得被對方消耗）。
#[test]
fn interactive_open_relogin_does_not_consume_the_homework_budget() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            { "id": "2", "name": "操作系统", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![
            ("1", serde_json::json!({ "activities": [] })),
            ("2", serde_json::json!({ "activities": [] })),
        ],
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });
    let system = Arc::clone(&site);
    let logins = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&logins);
    let player_calls = AtomicUsize::new(0);
    let activity_calls = AtomicUsize::new(0);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == lms::LOGIN_URL {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        if url.contains("/player-url") {
            // 第一次回報登入態失效；重新登入後恢復正常。
            if player_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(HttpResponse::new(
                    200,
                    "https://login.xjtu.edu.cn/cas/login?service=lms",
                    login_page(),
                ));
            }
            return Ok(json(serde_json::json!({
                "url": "https://lms.xjtu.edu.cn/lesson/player?token=abc"
            })));
        }
        if url.ends_with("/courses/1/activities") {
            // 載入的第一次查詢在開啟操作重登之後才失效：載入仍應有自己的額度。
            if activity_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(HttpResponse::new(
                    200,
                    "https://login.xjtu.edu.cn/cas/login?service=lms",
                    login_page(),
                ));
            }
        }
        system.handle(request)
    });
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());
    // 載入進行中按下 `o`：開啟任務排在步進邊界被立即執行。
    harness.worker.pending_data.push_back(Job::OpenActivity {
        activity_id: "7".to_owned(),
        course_id: Some("1".to_owned()),
        kind: lms::ActivityKind::Lesson,
    });

    harness.worker.run_homework_job(false);

    let events = harness.drain_events();
    assert_eq!(
        logins.load(Ordering::SeqCst),
        2,
        "开启与加载应各自取得一次自动重登：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/lesson/player?token=abc"
        )),
        "开启操作应在重登后完成：{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Homework(update) if update.progress.is_none())),
        "作业加载应在重登后完成，而不是被误判为自动重登失败：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Failed { message, .. } if message.contains("自动重新登录")
        )),
        "不得误报自动重登失败：{events:?}"
    );
}

/// 單課程單作業的假站點（詳情直接提供提交次數，不查提交列表）。
fn cached_homework_site() -> Arc<FakeHomeworkSite> {
    Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![(
            "1",
            serde_json::json!({ "activities": [
                { "id": "11", "type": "homework", "title": "作业A",
                  "end_time": "2026-10-01 23:59:59" },
            ]}),
        )],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "end_time": "2026-10-01 23:59:59", "submit_by_group": false,
                "user_submit_count": 2 }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    })
}

#[test]
fn homework_second_load_reuses_detail_and_summary_caches() {
    let site = cached_homework_site();
    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("首次加载");
    let first = site.urls().len();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("第二次加载");

    let second = site.urls();
    assert_eq!(second.len(), first, "第二次加载应全面命中缓存：{second:?}");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.requests, 0, "命中缓存时不应再送请求");
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.items[0].state, HomeworkState::Completed);
}

#[test]
fn homework_forced_refresh_refetches_details() {
    let site = cached_homework_site();
    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: true })
        .expect("首次加载");
    harness
        .dispatch(Job::LoadHomework { force: true })
        .expect("强制刷新");

    let seen = site.urls();
    assert_eq!(
        seen.iter()
            .filter(|url| url.ends_with("/api/activities/11"))
            .count(),
        2,
        "强制刷新应重新取得活动详情：{seen:?}"
    );

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert!(last.requests >= 3, "强制刷新应重取课程／活动／详情");
}

#[test]
fn homework_reports_requests_and_elapsed() {
    let site = cached_homework_site();
    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());

    let before = site.urls().len();
    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("加载");
    let after = site.urls().len();

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.requests, after - before, "统计请求数应与实际相符");
    assert!(
        last.elapsed <= Duration::from_secs(60),
        "耗时统计应为合理值"
    );
}

#[test]
fn homework_continues_after_single_course_activity_failure() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            { "id": "2", "name": "操作系统", "semester": { "code": "2026-1" } },
        ]}),
        activities: vec![
            // 第一門課的活動列表格式錯誤（非認證、非連線層）：應只略過該課程。
            ("1", serde_json::json!({ "activities": "oops" })),
            (
                "2",
                serde_json::json!({ "activities": [
                    { "id": "21", "type": "homework", "title": "作业B" },
                ]}),
            ),
        ],
        details: vec![(
            "21",
            serde_json::json!({ "id": "21", "type": "homework", "title": "作业B",
                "submit_by_group": false }),
        )],
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("单门课程失败不应终结整批");

    let events = harness.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. })),
        "可恢复的课程失败不得报成页面失败：{events:#?}"
    );
    let last = events
        .iter()
        .filter_map(|event| match event {
            Event::Homework(update) => Some(update),
            _ => None,
        })
        .next_back()
        .expect("应有最终更新");
    assert!(last.progress.is_none(), "应抵达终态");
    assert_eq!(last.courses_failed, 1, "应记录略过的课程数");
    assert_eq!(last.items.len(), 1, "其余课程的作业仍应汇总");
    assert_eq!(last.items[0].title, "作业B");
}

#[test]
fn courses_event_carries_current_term_hint() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
        ]}),
        activities: Vec::new(),
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: None,
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_lms_only();

    // 無考勤學期、也無記憶學期：無法判定，不得猜測。
    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("加载课程");
    let first = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Courses(data) => Some(data),
            _ => None,
        })
        .expect("应有课程事件");
    assert_eq!(first.current_term, None, "无来源时不应猜测学期");
    assert_eq!(first.courses.len(), 1);

    // 使用者記住的學期可作為提示（零額外請求）。
    harness.worker.config.homework_term = Some("2026-2027-1".to_owned());
    harness
        .dispatch(Job::LoadCourses { force: true })
        .expect("加载课程");
    let second = harness
        .drain_events()
        .into_iter()
        .find_map(|event| match event {
            Event::Courses(data) => Some(data),
            _ => None,
        })
        .expect("应有课程事件");
    assert_eq!(
        second.current_term,
        TermCode::parse("2026-2027-1"),
        "应复用用户记忆的学期"
    );
}

#[test]
fn clears_captcha_file_after_successful_login() {
    let mut harness = harness(fake_flow(0));
    let path = harness._dir.path().join("captcha.png");
    std::fs::write(&path, b"png").expect("写入验证码文件");
    harness.worker.captcha_path = Some(path.clone());

    harness
        .worker
        .finish_login(SiteKind::Attendance, None)
        .expect("完成登录");

    assert!(!path.exists(), "登录成功后应删除验证码文件");
    assert!(harness.worker.captcha_path.is_none(), "应清除路径记录");
}

#[test]
fn clears_captcha_file_when_credentials_are_rejected() {
    let mut harness = harness(fake_flow(0));
    let path = harness._dir.path().join("captcha.png");
    std::fs::write(&path, b"png").expect("写入验证码文件");
    harness.worker.captcha_path = Some(path.clone());

    harness
        .worker
        .handle_reply(LoginReply::Fail {
            message: "登录失败：用户名或密码错误".to_owned(),
        })
        .expect("处理失败回复");

    assert!(!path.exists(), "凭证被拒后应删除验证码文件");
    assert!(harness.worker.captcha_path.is_none(), "应清除路径记录");
}

#[test]
fn clears_captcha_file_when_login_job_fails() {
    let mut harness = harness(fake_flow(0));
    let path = harness._dir.path().join("captcha.png");
    std::fs::write(&path, b"png").expect("写入验证码文件");
    harness.worker.captcha_path = Some(path.clone());

    // 沒有進行中的登入流程：提交驗證碼會立即失敗（屬登入類任務）。
    harness
        .worker
        .handle_control(Job::SubmitCaptcha("1234".into()));

    assert!(!path.exists(), "登录任务失败后应删除验证码文件");
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Login,
                ..
            }
        )),
        "应回报登录失败事件：{events:?}"
    );
}

/// 憑證檔權限過寬（例如由他處複製進來而帶有 0644）時應收緊並告知使用者。
#[cfg(unix)]
#[test]
fn unlock_tightens_loose_vault_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let mut harness = harness(|_request: &HttpRequest| panic!("解锁不应触发任何网络请求"));
    harness.seed_vault("secret123", &Credentials::new("3120000001", "old-password"));

    let path = harness.vault.path().to_path_buf();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("放宽权限");
    assert_eq!(mode(&path), 0o644);

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .expect("解锁应当成功");

    assert!(
        harness.saw(|event| matches!(
            event,
            Event::Notice(message) if message.contains("权限")
        )),
        "应提示凭证文件权限过宽"
    );
    assert_eq!(mode(&path), 0o600, "权限应收紧为 0600");
}

/// 帳號切換失败後必須回到「乾淨的舊帳號會話」。
///
/// 只還原帳密不夠：新帳號在登入過程中可能已在伺服器端留下登入態（cookie），
/// 不重建後端的話，之後任何一次登入都會被判定為「已登入」，畫面就會出現
/// 新帳號的資料。
#[test]
fn rollback_after_a_failed_switch_rebuilds_the_session() {
    let built = Arc::new(AtomicUsize::new(0));
    let harness_built = Arc::clone(&built);
    let mut harness = Harness::rebuildable(
        |request: &HttpRequest| match request.url.as_str() {
            rsa::PUBLIC_KEY_URL => Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            )),
            attendance::LOGIN_URL => Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page())),
            // CAS 登入成功（在伺服器端建立新帳號的登入態）……
            ATTENDANCE_POST => Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY)),
            // ……但業務 token 交換失敗。
            ATTENDANCE_EXCHANGE => Ok(HttpResponse::new(
                500,
                ATTENDANCE_EXCHANGE,
                "<html>维护</html>",
            )),
            other => panic!("未预期的请求：{other}"),
        },
        harness_built,
    );
    harness.seed_vault("secret123", &Credentials::new("3120000001", "old-password"));
    assert_eq!(built.load(Ordering::SeqCst), 2, "建立时两个后端各建一次");

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });

    assert!(result.is_err(), "收尾失败的换账号必须失败");
    assert_eq!(
        built.load(Ordering::SeqCst),
        6,
        "换账号重建一次、恢复旧账号又重建一次：新账号的 cookie 不得沿用"
    );
    assert!(harness.worker.flow.is_none(), "进行中的登录流程应一并作废");
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "内存中的凭证应还原为旧账号"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(
        stored.username, "3120000001",
        "保险库不得被未验证的凭证覆盖"
    );
}

/// 「重新輸入帳密」必須沿用原本失敗的站點。
#[test]
fn retry_with_account_targets_the_site_that_failed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tracker = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        tracker.lock().expect("lock").push(url.to_owned());
        match url {
            rsa::PUBLIC_KEY_URL => Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            )),
            lms::LOGIN_URL => Ok(HttpResponse::new(200, LMS_POST, login_page())),
            LMS_POST => Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY)),
            LMS_HOME => Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY)),
            other => panic!("思源学堂重试不应连到其他站点：{other}"),
        }
    });

    harness
        .dispatch(Job::RetryWithAccount {
            site: SiteKind::Lms,
            credentials: Credentials::new("3120000002", "new-password"),
            passphrase: "secret123".into(),
        })
        .expect("思源学堂的重新登录应当成功");

    let urls = seen.lock().expect("lock").clone();
    assert!(
        urls.iter().any(|url| url == LMS_POST),
        "登录必须送到思源学堂：{urls:?}"
    );
    assert!(
        !urls.iter().any(|url| url.contains("bk-kq.xjtu.edu.cn")),
        "不得因为重试而改走考勤系统：{urls:?}"
    );
    assert_eq!(
        harness.worker.login_site,
        Some(SiteKind::Lms),
        "重试站点应记为思源学堂"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000002", "验证成功后应写回新凭证");
}

/// 上一次切換尚未結束（例如驗證碼填錯後又改輸另一組帳密）時，失敗的還原目標
/// 必須是保險庫中真正保存的憑證，而不是記憶體中那組未驗證的新帳號。
#[test]
fn rollback_uses_the_vault_credentials_not_an_unverified_pending_account() {
    let mut harness = harness(|request: &HttpRequest| {
        // 重新輸入帳密後立刻斷網：登入流程在第一個請求就失敗。
        Err(AppError::network_kind(
            NetworkKind::Connect,
            format!("连接失败：{}", request.url),
        ))
    });
    // 保險庫中保存的是 A；上一次切換（A → B）因驗證碼填錯而仍在進行中。
    harness.seed_vault("secret123", &Credentials::new("3120000001", "old-password"));
    harness.worker.credentials = Some(Credentials::new("3120000002", "pending-password"));
    harness.worker.pending_vault = Some(PendingVault {
        passphrase: Secret::from("secret123"),
        credentials: Credentials::new("3120000002", "pending-password"),
        previous: Some(Credentials::new("3120000001", "old-password")),
    });

    let err = harness
        .dispatch(Job::RetryWithAccount {
            site: SiteKind::Attendance,
            credentials: Credentials::new("3120000003", "third-password"),
            passphrase: "secret123".into(),
        })
        .expect_err("离线时重新登录必定失败");

    assert!(
        err.to_string().contains("连接失败"),
        "应回报连接失败：{err}"
    );
    assert!(
        harness.worker.pending_vault.is_none(),
        "失败后应丢弃整条待存凭证链"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "应还原为保险库中的 A，而不是上一次未验证的 B"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "保险库内容不得被更动");
}

/// 設定表單重複輸入**同一帳號**的錯誤密碼時，失敗計數必須累積。
///
/// 修復前每次嘗試都無條件清零，`failN` 恆為 0，伺服器要求的圖片驗證碼
/// 永遠不會出現。
#[test]
fn change_account_keeps_failure_count_for_the_same_account() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
        }
        if let Some(fail_n) = request.form_field("failN") {
            recorder.lock().expect("lock").push(fail_n.to_owned());
            // 帳密被拒。
            return Ok(HttpResponse::new(401, ATTENDANCE_POST, "<html></html>"));
        }
        // 驗證碼圖片端點（第四次嘗試才會走到）刻意失敗：不需要寫入真實資料目錄。
        Err(AppError::network_kind(
            NetworkKind::Connect,
            "连接失败".to_owned(),
        ))
    });

    let attempt = |harness: &mut Harness| {
        harness.dispatch(Job::ChangeAccount {
            passphrase: "secret123".into(),
            // 與目前生效的憑證同帳號。
            credentials: Credentials::new("3120000001", "wrong-password"),
        })
    };

    for _ in 0..3 {
        attempt(&mut harness).expect("账密被拒属于预期结果");
    }

    assert_eq!(
        seen.lock().expect("lock").as_slice(),
        ["0", "1", "2"],
        "同账号重复失败必须累积（修复前每次都被清零）"
    );
    assert_eq!(
        harness
            .worker
            .login_failures
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![3],
        "失败计数必须保留"
    );

    // 第四次：已達門檻，不再提交帳密，改為要求圖片驗證碼。
    assert!(attempt(&mut harness).is_err(), "达到门槛后不应再提交账密");
    assert_eq!(
        seen.lock().expect("lock").len(),
        3,
        "第四次尝试不得再送出账密"
    );
}

/// 換帳號先經過簡訊驗證、再於收尾（換取業務 token）失敗：必須回復舊帳號。
///
/// 修復前 `handle_control` 只對 ChangeAccount／RetryWithAccount 的錯誤善後，
/// 互動驗證步驟冒出的錯誤會被漏掉：B 的帳密與待存憑證留在記憶體裡，之後
/// 一次成功的登入就會把它寫進保險庫。
#[test]
fn failed_switch_after_mfa_rolls_back_the_new_account() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
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
                ATTENDANCE_POST,
                login_page_with_mfa(),
            ));
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
        if url.contains("/securephone/send") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "securePhone": "138****1234", "gid": "g-1" }
            })));
        }
        if url.contains("/securephone/valid") {
            return Ok(json(
                serde_json::json!({ "code": 0, "data": { "status": 2 } }),
            ));
        }
        if url == ATTENDANCE_POST {
            return Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY));
        }
        if url == ATTENDANCE_EXCHANGE {
            // 收尾階段的終止錯誤。
            return Ok(HttpResponse::new(
                500,
                ATTENDANCE_EXCHANGE,
                "<html>维护</html>",
            ));
        }
        if url == lms::LOGIN_URL {
            return Ok(HttpResponse::new(200, LMS_POST, login_page()));
        }
        if url == LMS_POST || url == LMS_HOME {
            return Ok(HttpResponse::new(200, LMS_HOME, TARGET_BODY));
        }
        panic!("未预期的请求：{url}");
    });

    harness
        .dispatch(Job::ChangeAccount {
            passphrase: "secret123".into(),
            credentials: Credentials::new("3120000002", "new-password"),
        })
        .expect("换账号应停在短信验证");
    assert!(
        harness.saw(|event| matches!(event, Event::LoginNeedsMfa { .. })),
        "换账号应进入短信验证"
    );
    harness
        .dispatch(Job::SendMfaCode)
        .expect("发送验证码应当成功");

    let result = harness.dispatch(Job::VerifyMfaCode(Secret::from("123456")));
    assert!(result.is_err(), "收尾失败必须回报错误");

    assert!(
        harness.worker.pending_vault.is_none(),
        "互动验证之后才失败的切换同样必须丢弃待存凭证"
    );
    assert!(harness.worker.flow.is_none(), "失败的登录流程应一并作废");
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "内存中的凭证应还原为旧账号"
    );

    // 之後思源學堂登入成功：不得把失敗切換的憑證寫進保險庫。
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Lms,
        })
        .expect("思源学堂登录应当成功");
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "保险库不得写入 B 的凭证");
    assert_eq!(stored.password, "old-password");
}

/// 簡訊驗證碼填錯是可重試的：不得作廢整次帳號切換。
#[test]
fn wrong_mfa_code_keeps_the_pending_switch() {
    // 第一次提交的驗證碼是錯的，之後接受——用來驗證重輸即可續用同一次登入。
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
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
                ATTENDANCE_POST,
                login_page_with_mfa(),
            ));
        }
        if url.contains("/mfa/detect") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "state": "s-1", "need": true }
            })));
        }
        if url.contains("/initByType/securephone") || url.contains("/securephone/send") {
            return Ok(json(serde_json::json!({
                "code": 0,
                "data": { "securePhone": "138****1234", "gid": "g-1" }
            })));
        }
        if url.contains("/securephone/valid") {
            // 第一次驗證碼錯誤（狀態非 2），重輸後即通過。
            let status = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                1
            } else {
                2
            };
            return Ok(json(
                serde_json::json!({ "code": 0, "data": { "status": status } }),
            ));
        }
        if url == ATTENDANCE_EXCHANGE {
            return Ok(json(
                serde_json::json!({ "code": 0, "data": { "tokenValue": "token-1" } }),
            ));
        }
        if request.method == Method::Post {
            // 帳密表單提交成功（CAS 回跳）。
            return Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY));
        }
        if url == ATTENDANCE_TARGET {
            return Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY));
        }
        panic!("未预期的请求：{url}");
    });

    harness
        .dispatch(Job::ChangeAccount {
            passphrase: "secret123".into(),
            credentials: Credentials::new("3120000002", "new-password"),
        })
        .expect("换账号应停在短信验证");
    harness
        .dispatch(Job::SendMfaCode)
        .expect("发送验证码应当成功");

    let err = harness
        .dispatch(Job::VerifyMfaCode(Secret::from("000000")))
        .expect_err("验证码错误应回报错误");
    assert!(err.to_string().contains("短信验证码不正确"), "{err}");
    assert!(
        harness.worker.pending_vault.is_some(),
        "验证码填错时应保留待存凭证，让用户重输后继续同一次切换"
    );
    assert!(
        harness.worker.flow.is_some(),
        "登录流程应保留以便重输验证码"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000002".to_owned()),
        "重输验证码仍应以新账号进行"
    );
    // 介面必須收到「可重輸」的訊號（而不是一般失敗畫面），否則輸入框會被蓋掉。
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::VerificationRetry { site: SiteKind::Attendance, message }
                if message.contains("短信验证码不正确")
        )),
        "应回报可重试的验证错误：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. })),
        "验证码填错不应弹出一般失败：{events:?}"
    );

    // 重輸正確的驗證碼即可續用同一次切換（同一個驅動器、同一個 gid）。
    let resumed = harness.dispatch(Job::VerifyMfaCode(Secret::from("123456")));
    assert!(resumed.is_ok(), "正确的验证码应能继续登录：{resumed:?}");
}

/// 圖片驗證碼填錯同樣是可重試的：不得作廢整次帳號切換。
#[test]
fn captcha_mistake_keeps_the_pending_switch() {
    // 直接以 `handle_reply` 驗證善後規則：不經網路，也不會寫入驗證碼圖片。
    let mut harness = harness(|_request: &HttpRequest| panic!("本测试不应发出请求"));

    let client: Arc<dyn HttpClient> =
        Arc::new(FakeClient::with_responder(|request: &HttpRequest| {
            if request.url == rsa::PUBLIC_KEY_URL {
                return Ok(HttpResponse::new(
                    200,
                    rsa::PUBLIC_KEY_URL,
                    public_key_pem(),
                ));
            }
            if request.url == attendance::LOGIN_URL {
                return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
            }
            // 帳密／驗證碼被拒。
            Ok(HttpResponse::new(401, ATTENDANCE_POST, "<html></html>"))
        }));
    let mut driver =
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登录驱动器");
    // 已達門檻：直接進入驗證碼流程（不需先失敗三次）。
    driver.set_fail_count(3);
    assert_eq!(
        driver
            .start(
                &Credentials::new("3120000002", "new-password"),
                AccountType::Undergraduate
            )
            .expect("启动登录"),
        LoginReply::NeedCaptcha
    );
    let reply = driver.submit_captcha("bad-code").expect("提交验证码");
    assert!(matches!(reply, LoginReply::Fail { .. }), "{reply:?}");
    assert!(
        driver.last_attempt_submitted_captcha(),
        "测试前提：这次提交带了验证码"
    );

    // 模擬「換帳號進行中」的狀態。
    harness.worker.credentials = Some(Credentials::new("3120000002", "new-password"));
    harness.worker.pending_vault = Some(PendingVault {
        passphrase: Secret::from("secret123"),
        credentials: Credentials::new("3120000002", "new-password"),
        previous: Some(Credentials::new("3120000001", "old-password")),
    });
    harness.worker.flow = Some(LoginFlow {
        site: SiteKind::Attendance,
        driver: Box::new(driver),
        retry: None,
    });

    harness
        .worker
        .handle_reply(reply)
        .expect("处理登录回复不应出错");

    assert!(
        harness.worker.pending_vault.is_some(),
        "验证码填错时应保留待存凭证，让用户重输后继续同一次切换"
    );
    assert!(
        harness.worker.flow.is_some(),
        "登录流程应保留以便重输验证码"
    );
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::VerificationRetry { site: SiteKind::Attendance, message }
                if !message.is_empty()
        )),
        "验证码填错应回报可重试事件（让界面留在输入画面）：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginFailed { .. } | Event::Failed { .. })),
        "验证码填错不应弹出一般失败画面：{events:?}"
    );
}

/// 驗證碼填錯後「工作者 → 介面」的完整串接：輸入畫面必須留著並顯示錯誤。
#[test]
fn wrong_captcha_keeps_the_captcha_input_screen() {
    // 白箱：以假驅動器把流程帶到「已送出錯誤驗證碼」，不必寫入真實的驗證碼圖片。
    let mut harness = harness(|_request: &HttpRequest| panic!("本测试不应发出请求"));

    let client: Arc<dyn HttpClient> =
        Arc::new(FakeClient::with_responder(|request: &HttpRequest| {
            if request.url == rsa::PUBLIC_KEY_URL {
                return Ok(HttpResponse::new(
                    200,
                    rsa::PUBLIC_KEY_URL,
                    public_key_pem(),
                ));
            }
            if request.url == attendance::LOGIN_URL {
                return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
            }
            // 帳密／驗證碼被拒；驗證碼圖片端點也失敗（不會寫入使用者的資料目錄）。
            Ok(HttpResponse::new(401, ATTENDANCE_POST, "<html></html>"))
        }));
    let mut driver =
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登录驱动器");
    driver.set_fail_count(3);
    assert_eq!(
        driver
            .start(
                &Credentials::new("3120000002", "new-password"),
                AccountType::Undergraduate
            )
            .expect("启动登录"),
        LoginReply::NeedCaptcha
    );
    let reply = driver.submit_captcha("bad-code").expect("提交验证码");
    assert!(matches!(reply, LoginReply::Fail { .. }), "{reply:?}");

    // 目前的驗證碼圖片（換新圖失敗時必須保留給使用者重輸）。
    let dir = TempDir::new().expect("建立暂存目录");
    let image = dir.path().join("captcha.png");
    std::fs::write(&image, b"png-bytes").expect("写入测试图片");
    harness.worker.captcha_path = Some(image.clone());
    harness.worker.credentials = Some(Credentials::new("3120000002", "new-password"));
    harness.worker.pending_vault = Some(PendingVault {
        passphrase: Secret::from("secret123"),
        credentials: Credentials::new("3120000002", "new-password"),
        previous: Some(Credentials::new("3120000001", "old-password")),
    });
    harness.worker.flow = Some(LoginFlow {
        site: SiteKind::Attendance,
        driver: Box::new(driver),
        retry: None,
    });

    // 介面端此刻正停在驗證碼輸入畫面。
    let mut app = App::new(AccessPolicy::Direct);
    app.login = Some(Box::new(LoginScreen::Captcha {
        path: image.clone(),
        input: InputLine::with_value("bad-code"),
        error: None,
    }));

    harness
        .worker
        .handle_reply(reply)
        .expect("处理登录回复不应出错");
    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::VerificationRetry { .. })),
        "应回报可重试的验证错误：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginFailed { .. })),
        "不得发出一般登录失败（会把输入画面换掉）：{events:?}"
    );

    // 依序套用工作者發出的事件：畫面必須仍是驗證碼輸入框。
    for event in events {
        crate::tui::apply_event_for_test(&mut app, event);
    }
    match app.login.as_deref() {
        Some(LoginScreen::Captcha { input, error, path }) => {
            assert!(input.is_empty(), "重输前应清空验证码：{:?}", input.value());
            assert!(
                error.as_deref().is_some_and(|text| !text.is_empty()),
                "输入画面应就地显示错误：{error:?}"
            );
            assert_eq!(path, &image, "换不到新图时应沿用旧图");
        }
        other => panic!("应留在验证码输入画面，实际为 {other:?}"),
    }
    assert!(image.exists(), "换新图失败时不得删掉用户正在看的验证码图片");
    assert!(
        harness.worker.flow.is_some() && harness.worker.pending_vault.is_some(),
        "同一次账号切换必须保留，让重输的验证码沿用同一个登录流程"
    );
}

/// 回復舊帳號時若無法建立乾淨的會話，必須停用會話而不是繼續用新帳號的 cookie。
#[test]
fn rollback_without_a_clean_session_disables_the_session() {
    let calls = Arc::new(AtomicUsize::new(0));
    // 第 0／1 次是建立時；第 2／3 次是換帳號的重建；第 4 次起（回復舊帳號
    // 的重建）一律失敗。
    let mut harness = Harness::with_broken_factory(
        |request: &HttpRequest| match request.url.as_str() {
            rsa::PUBLIC_KEY_URL => Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            )),
            attendance::LOGIN_URL => Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page())),
            ATTENDANCE_POST => Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY)),
            ATTENDANCE_EXCHANGE => Ok(HttpResponse::new(
                500,
                ATTENDANCE_EXCHANGE,
                "<html>维护</html>",
            )),
            // 會話若沒被停用，後續任何請求都會走到這裡（測試即失敗）。
            other => panic!("会话停用后不得再发出请求：{other}"),
        },
        Arc::clone(&calls),
        4,
    );
    harness.seed_vault("secret123", &Credentials::new("3120000001", "old-password"));

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });
    assert!(result.is_err(), "收尾失败的换账号必须失败");

    assert!(
        harness.worker.session.is_none(),
        "无法建立干净的会话时必须停用会话（不得继续带新账号的 cookie）"
    );
    assert!(
        harness.saw(|event| matches!(event, Event::SessionDisabled(_))),
        "应回报会话已停用，让界面回到解锁画面"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "凭证仍应还原为旧账号"
    );

    // 後續資料任務不得發出任何請求（responder 會 panic）。
    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("数据任务失败不应冒泡为任务错误");
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Courses,
                ..
            }
        )),
        "数据任务应回报失败（会话尚未建立）"
    );
}

/// 憑證檔權限（僅 Unix 有權限位）。
#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("读取权限")
        .permissions()
        .mode()
        & 0o777
}

// ── 帳號切換與憑證安全（換帳號必須重新驗證，失敗不得寫回）──

/// 離線的連線層錯誤。
fn offline() -> AppError {
    AppError::network_kind(NetworkKind::Dns, "域名解析失败".to_owned())
}

/// 換帳號離線失敗：丟棄待存憑證並還原記憶體中的舊憑證。
#[test]
fn offline_account_switch_discards_pending_credentials() {
    let mut harness = harness(|_request: &HttpRequest| Err(offline()));

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });

    assert!(result.is_err(), "离线的换账号操作应当失败");
    assert!(
        harness.worker.pending_vault.is_none(),
        "失败后不得留下待存凭证，否则之后任何一次登录成功都会把它写回"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "内存中的凭证应还原为旧账号"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(
        stored.username, "3120000001",
        "保险库不得被未验证的凭据覆盖"
    );
    assert_eq!(stored.password, "old-password");
}

/// 換帳號失敗後改口令：稍後登入成功不得把口令改回舊的（迴歸）。
#[test]
fn failed_account_switch_does_not_revert_a_later_passphrase_change() {
    let flow = fake_flow(0);
    let offline_flag = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&offline_flag);
    let mut harness = harness(move |request: &HttpRequest| {
        if flag.load(Ordering::SeqCst) {
            return Err(offline());
        }
        flow(request)
    });

    // 1) 換帳號斷網 → 失敗。
    assert!(
        harness
            .dispatch(Job::ChangeAccount {
                passphrase: "secret123".into(),
                credentials: Credentials::new("3120000002", "new-password"),
            })
            .is_err()
    );

    // 2) 使用者修改加密口令。
    harness
        .dispatch(Job::ChangePassphrase {
            old: "secret123".into(),
            new: "new-secret456".into(),
        })
        .expect("修改口令应当成功");

    // 3) 網路恢復後登入成功。
    offline_flag.store(false, Ordering::SeqCst);
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Attendance,
        })
        .expect("登录应当成功");

    assert!(
        harness.vault.load("new-secret456").is_ok(),
        "新口令必须仍然有效"
    );
    assert!(
        harness.vault.load("secret123").is_err(),
        "旧口令不得复活（待存凭据不得用旧口令覆写保险库）"
    );
}

/// 「重新輸入帳密」即使站點仍在登入狀態，也必須實際向伺服器提交帳密。
#[test]
fn retry_with_account_always_verifies_over_the_network() {
    let password_posts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&password_posts);
    let flow = fake_flow(0);
    let mut harness = harness(move |request: &HttpRequest| {
        if request.form_field("password").is_some() {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        flow(request)
    });
    // 站點都還「已登入」：修復前這條路徑會直接進入成功分支並寫回未驗證的憑證。
    harness.login_both_sites();

    harness
        .dispatch(Job::RetryWithAccount {
            site: SiteKind::Attendance,
            credentials: Credentials::new("3120000002", "new-password"),
            passphrase: "secret123".into(),
        })
        .expect("重新输入的帐密应可登录");

    assert_eq!(
        password_posts.load(Ordering::SeqCst),
        1,
        "即使站点仍在登录状态，也必须实际提交帐密做验证"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000002", "验证成功后应写回新凭据");
}

/// 伺服器端仍有登入態時，換帳號不得被判定為成功（未提交帳密即完成）。
#[test]
fn account_switch_is_refused_when_login_skips_credentials() {
    let mut harness = harness(|request: &HttpRequest| match request.url.as_str() {
        // 登入入口直接回目標頁：等同伺服器端仍保有舊帳號的登入態。
        attendance::LOGIN_URL => Ok(HttpResponse::new(200, ATTENDANCE_TARGET, TARGET_BODY)),
        ATTENDANCE_EXCHANGE => Ok(json(
            serde_json::json!({ "code": 0, "data": { "tokenValue": "token-1" } }),
        )),
        other => panic!("未预期的请求：{other}"),
    });

    let err = harness
        .dispatch(Job::ChangeAccount {
            passphrase: "secret123".into(),
            credentials: Credentials::new("3120000002", "new-password"),
        })
        .expect_err("未提交账密的「登录」不得视为验证成功");

    assert!(
        err.to_string().contains("无法验证新账号"),
        "信息应说明无法验证新账号：{err}"
    );
    assert!(harness.worker.pending_vault.is_none(), "应丢弃待存凭证");
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "内存中的凭证应还原"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "未验证的凭证不得写回");
    assert_eq!(stored.password, "old-password");
}

// ── 失敗站點歸屬與失敗計數 ───────────────────────────

/// 作業載入時「查考勤學期」失敗必須歸給考勤系統，而非思源學堂。
#[test]
fn homework_load_attributes_attendance_term_failure_to_attendance() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.contains("/timetable/semesters") {
            // 考勤登入態已失效：站點層據此回報 SessionExpired。
            return Ok(HttpResponse::new(
                200,
                "https://login.xjtu.edu.cn/cas/login?service=attendance",
                login_page().as_bytes(),
            ));
        }
        if url.starts_with(attendance::LOGIN_URL) {
            // 重新登入也失敗，讓流程收斂（本測試只關心歸因）。
            return Err(AppError::network_kind(
                NetworkKind::Connect,
                "连接失败".to_owned(),
            ));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("数据任务失败不应冒泡为任务错误");

    assert!(
        harness.saw(|event| matches!(
            event,
            Event::SessionExpired {
                site: SiteKind::Attendance
            }
        )),
        "考勤学期查询失败必须归给考勤系统，重登才会打到对的站点"
    );
}

/// 思源學堂登入失敗必須回報思源學堂（重試才不會打到考勤系統）。
#[test]
fn login_failure_reports_the_site_that_failed() {
    let mut harness = harness(|request: &HttpRequest| match request.url.as_str() {
        rsa::PUBLIC_KEY_URL => Ok(HttpResponse::new(
            200,
            rsa::PUBLIC_KEY_URL,
            public_key_pem(),
        )),
        lms::LOGIN_URL => Ok(HttpResponse::new(200, LMS_POST, login_page())),
        // 帳密被拒。
        LMS_POST => Ok(HttpResponse::new(401, LMS_POST, "<html></html>")),
        other => panic!("未预期的请求：{other}"),
    });

    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("数据任务失败不应冒泡为任务错误");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::LoginFailed {
                site: SiteKind::Lms,
                ..
            }
        )),
        "登录失败事件必须指出思源学堂：{events:?}"
    );
    assert_eq!(
        harness.worker.login_site,
        Some(SiteKind::Lms),
        "重试站点应记为思源学堂"
    );
}

/// 登入失敗的計數必須跨驅動器重建保存，否則伺服器要求的圖片驗證碼永遠不會出現。
///
/// 驗證碼圖片端點刻意讓伺服器回錯：本測試不需要真的寫入使用者的資料目錄。
#[test]
fn login_failure_count_survives_driver_rebuilds() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let mut harness = harness(move |request: &HttpRequest| {
        let url = request.url.as_str();
        if url == rsa::PUBLIC_KEY_URL {
            return Ok(HttpResponse::new(
                200,
                rsa::PUBLIC_KEY_URL,
                public_key_pem(),
            ));
        }
        if url == attendance::LOGIN_URL {
            return Ok(HttpResponse::new(200, ATTENDANCE_POST, login_page()));
        }
        if let Some(fail_n) = request.form_field("failN") {
            recorder.lock().expect("lock").push(fail_n.to_owned());
            // 帳密被拒。
            return Ok(HttpResponse::new(401, ATTENDANCE_POST, "<html></html>"));
        }
        Err(AppError::network_kind(
            NetworkKind::Connect,
            "连接失败".to_owned(),
        ))
    });

    for _ in 0..3 {
        harness
            .dispatch(Job::RetryLogin {
                site: SiteKind::Attendance,
            })
            .expect("账密被拒属于预期结果");
    }

    assert_eq!(
        seen.lock().expect("lock").as_slice(),
        ["0", "1", "2"],
        "每次提交的 failN 应累积（修复前恒为 0）"
    );
    assert_eq!(
        harness
            .worker
            .login_failures
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![3],
        "失败次数必须跨驱动器保存"
    );

    // 第四次：已達門檻，不再提交帳密，改為要求圖片驗證碼（圖片端點在此失敗）。
    assert!(
        harness
            .dispatch(Job::RetryLogin {
                site: SiteKind::Attendance,
            })
            .is_err(),
        "达到门槛后不应再提交账密"
    );
    assert_eq!(
        seen.lock().expect("lock").len(),
        3,
        "第四次尝试不得再送出账密"
    );

    // 取消登入彈窗不得清除計數：伺服器端的門檻是跨嘗試累計的。
    harness
        .dispatch(Job::CancelLogin)
        .expect("取消登录不应失败");
    assert_eq!(
        harness
            .worker
            .login_failures
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![3],
        "取消弹窗不得清除失败计数"
    );
}

/// 登入流程已經結束、但等待重登的任務還留著時，關閉登入提示仍必須收斂該頁。
///
/// 帳密被拒的路徑會把 `flow` 與 `pending_vault` 都清掉（`discard_pending_vault`
/// 對 `pending_vault == None` 直接返回），但 `self.retry` 仍握著原任務。
/// 舊寫法在這種情況下直接早退，頁面就永遠停在「載入中」。
#[test]
fn cancel_login_settles_a_page_when_the_flow_already_ended() {
    let mut harness = harness(|_request: &HttpRequest| panic!("取消登录不应触发网络请求"));
    assert!(harness.worker.flow.is_none());
    assert!(harness.worker.pending_vault.is_none());
    harness.worker.retry = Some(Job::LoadSchedule { force: false });

    harness
        .dispatch(Job::CancelLogin)
        .expect("取消登录应当成功");

    assert!(harness.worker.retry.is_none(), "应丢弃待重试任务");
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::LoadingCancelled {
                target: FailedTarget::Schedule
            }
        )),
        "必须收敛等待重试的页面，否则它会永远停在「加载中」"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LoginCancelled)),
        "即使已经没有进行中的登录，也必须回报取消完成，界面才能清除等待状态"
    );
    assert!(
        !events.iter().any(|event| matches!(event, Event::Notice(_))),
        "没有进行中的登录时不应覆盖界面既有的提示"
    );
}

/// 使用者按 `s` 選定的學期優先於考勤的當前學期（否則選了也看不到）。
#[test]
fn chosen_term_overrides_the_attendance_term_within_the_session() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [
            { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            { "id": "9", "name": "历史课程", "semester": { "code": "2025-2" } },
        ]}),
        activities: vec![
            ("1", serde_json::json!({ "activities": [] })),
            ("9", serde_json::json!({ "activities": [] })),
        ],
        details: Vec::new(),
        expire_first_submission: false,
        submissions: AtomicUsize::new(0),
        attendance_term: Some(("2026-2027", "第一学期")),
    });

    let system = Arc::clone(&site);
    let mut harness = harness(move |request| system.handle(request));
    harness.login_both_sites();

    // 考勤說本學期是 2026-2027-1；使用者明確選擇上一個學期。
    harness
        .dispatch(Job::SetHomeworkTerm {
            term: "2025-2026-2".to_owned(),
        })
        .expect("记住学期");
    assert_eq!(
        harness.worker.chosen_term,
        TermCode::parse("2025-2026-2"),
        "本次选择应记入工作阶段"
    );
    let queued = harness
        .worker
        .pending_data
        .pop_front()
        .expect("应排入重载任务");
    harness.dispatch(queued).expect("重载应当成功");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.term_label.as_deref(), Some("2025-2026 学年 第 2 学期"));
    assert_eq!(
        last.term_source,
        Some(TermSource::Chosen),
        "来源应为本次选择，而不是考勤"
    );
    assert!(
        !site
            .urls()
            .iter()
            .any(|url| url.contains("/courses/1/activities")),
        "未选中的学期课程不应被查询"
    );
    assert!(
        site.urls()
            .iter()
            .any(|url| url.contains("/courses/9/activities")),
        "应查询所选学期的课程"
    );
}

/// 考勤暫時不可達（逾時）時，仍以可用學期完成作業查詢並提示故障。
#[test]
fn homework_continues_with_the_remembered_term_when_attendance_times_out() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "9", "name": "历史课程", "semester": { "code": "2025-2" } },
            ]})));
        }
        if url.contains("/timetable/semesters") {
            // 考勤逾時：不得因此拖垮本來可以查詢的思源學堂。
            return Err(AppError::network_kind(
                NetworkKind::Timeout,
                "请求超时".to_owned(),
            ));
        }
        if url.ends_with("/courses/9/activities") {
            return Ok(json(serde_json::json!({ "activities": [
                { "id": "91", "type": "homework", "title": "旧作业",
                  "end_time": "2026-10-01 23:59:59" },
            ]})));
        }
        if url.ends_with("/api/activities/91") {
            return Ok(json(serde_json::json!({
                "id": "91", "type": "homework", "title": "旧作业",
                "end_time": "2026-10-01 23:59:59",
                "submit_by_group": false, "user_submit_count": 0,
            })));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();
    harness.worker.config.homework_term = Some("2025-2026-2".to_owned());

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("考勤超时不应让作业加载失败");

    // `saw` 會取出事件，因此一次收齊後再斷言。
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(text) if text.contains("考勤系统暂时不可用")
        )),
        "应提示考勤故障与学期来源"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. })),
        "不得回报为失败"
    );
    let updates: Vec<HomeworkUpdate> = events
        .into_iter()
        .filter_map(|event| match event {
            Event::Homework(update) => Some(update),
            _ => None,
        })
        .collect();
    let last = updates.last().expect("最终更新");
    assert_eq!(last.term_label.as_deref(), Some("2025-2026 学年 第 2 学期"));
    assert_eq!(last.courses_included, 1, "所选学期的课程仍应纳入");
    assert_eq!(last.items.len(), 1, "作业仍应加载");
}

/// 考勤逾時且完全沒有可用學期時，退回學期選擇器而不是整批失敗。
#[test]
fn homework_asks_for_a_term_when_attendance_times_out_without_a_fallback() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "1", "name": "编译原理", "semester": { "code": "2026-1" } },
            ]})));
        }
        if url.contains("/timetable/semesters") {
            return Err(AppError::network_kind(
                NetworkKind::Timeout,
                "请求超时".to_owned(),
            ));
        }
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("应要求选择学期，而不是整批失败");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::HomeworkNeedsTerm { .. })),
        "没有可用学期时应显示选择器"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. })),
        "不得回报为失败"
    );
}

/// 已明確選定學期時，作業查詢不再受考勤的登入狀態影響。
///
/// 舊寫法會在讀取 `chosen_term` 之前就查考勤：考勤工作階段過期（401）、
/// 後續的登入入口又逾時時，即使 LMS 正常且使用者已選定學期，作業仍會失敗。
#[test]
fn chosen_term_loads_homework_without_consulting_attendance() {
    let mut harness = harness(|request: &HttpRequest| {
        let url = request.url.as_str();
        if url.ends_with("/api/my-courses") {
            return Ok(json(serde_json::json!({ "courses": [
                { "id": "9", "name": "历史课程", "semester": { "code": "2025-2" } },
            ]})));
        }
        if url.ends_with("/courses/9/activities") {
            return Ok(json(serde_json::json!({ "activities": [] })));
        }
        // 明確選擇學期後連考勤的學期端點都不應碰。
        panic!("未预期的请求：{url}");
    });
    harness.login_both_sites();
    harness.worker.chosen_term = TermCode::parse("2025-2026-2");

    harness
        .dispatch(Job::LoadHomework { force: false })
        .expect("明确选择的学期不应受考勤影响");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(
        last.term_source,
        Some(TermSource::Chosen),
        "来源应为本次选择"
    );
    assert_eq!(last.term_label.as_deref(), Some("2025-2026 学年 第 2 学期"));
    assert_eq!(last.courses_included, 1);
}

/// 選定學期後必須通知介面更新課程分區所用的學期提示。
#[test]
fn set_homework_term_notifies_the_course_partition_hint() {
    let mut harness = harness(|_request: &HttpRequest| panic!("选学期不应触发网络请求"));

    harness
        .dispatch(Job::SetHomeworkTerm {
            term: "2025-2026-2".to_owned(),
        })
        .expect("记住学期");

    let expected = TermCode::parse("2025-2026-2").expect("学期");
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::CoursesTerm(Some(term)) if *term == expected
        )),
        "应通知界面更新课程分区的学期提示"
    );
}

/// 學期選擇寫入失敗時要還原記憶體中的「記住的學期」。
///
/// 否則記憶體與檔案不一致：課程分區與學期來源説明會採用一個沒有真的保存
/// 下來的值，重啟後設定又回到舊學期，畫面上顯示的卻不是。
#[test]
fn set_homework_term_keeps_the_previous_term_when_saving_fails() {
    let mut harness = harness(|_request: &HttpRequest| panic!("选学期不应触发网络请求"));
    harness.worker.config.homework_term = Some("2025-2026-2".to_owned());

    // 将存档路径指向目录，迫使写入失败。
    harness.worker.config.save_path = Some(harness._dir.path().to_path_buf());
    let result = harness.dispatch(Job::SetHomeworkTerm {
        term: "2026-2027-1".to_owned(),
    });

    assert!(result.is_err(), "写入失败时应报告错误");
    assert_eq!(
        harness.worker.config.homework_term.as_deref(),
        Some("2025-2026-2"),
        "写入失败时应保留原学期"
    );
    assert!(
        harness.worker.chosen_term.is_none(),
        "写入失败时不得记住新选择的学期"
    );
}

/// 帳號驗證成功但憑證寫入失敗：必須發出保存失敗事件（介面據此解除表單的
/// 「處理中」），且原有憑證不得被覆蓋。
#[cfg(unix)]
#[test]
fn credential_save_failure_reports_and_keeps_the_old_vault() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    let mut harness = harness(fake_flow(0));
    // 讓資料目錄唯讀：`load` 仍能讀到既有憑證，但 `store` 建立暫存檔會失敗。
    let dir = harness._dir.path().to_path_buf();
    let original = fs::metadata(&dir).expect("读取目录权限").permissions();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).expect("设目录唯读");

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });

    // 還原權限，讓暫存目錄能被清理。
    fs::set_permissions(&dir, original).expect("还原目录权限");

    assert!(result.is_ok(), "只有保存失败，任务本身不应失败");
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::CredentialSaveFailed(message) if message.contains("凭据保存失败")
        )),
        "应发出凭证保存失败事件：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AccountUpdated)),
        "保存失败不得回报账号已更新：{events:?}"
    );
    // 順序：登入成功（主要結果）在前、保存失敗（附帶副作用）在後；否則介面
    // 的「登录成功」會蓋掉保存失敗的提醒。
    let login_at = events
        .iter()
        .position(|event| matches!(event, Event::LoginSucceeded { .. }))
        .expect("应先回报登录成功");
    let save_at = events
        .iter()
        .position(|event| matches!(event, Event::CredentialSaveFailed(_)))
        .expect("应回报保存失败");
    assert!(login_at < save_at, "保存失败必须是最后通知：{events:?}");
    // 保險庫仍是舊憑證（寫入失敗未覆蓋）。
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001");
    assert_eq!(stored.password, "old-password");
}

/// 重試額度：按任務鍵各自計算——同一任務耗盡後不得再消耗，重置只影響該
/// 任務；其他任務的額度互不影響。
#[test]
fn attempt_budget_is_kept_per_task() {
    let mut budget = AttemptBudgets::new(MAX_LOGIN_ATTEMPTS);
    let open = DataKey::OpenActivity("7".to_owned());
    let homework = DataKey::Homework;

    assert_eq!(
        budget.try_consume(&open),
        Some(2),
        "首次消耗回報即將進行的嘗試序號"
    );
    assert_eq!(
        budget.try_consume(&open),
        None,
        "同一任务的额度用尽后不得再消耗"
    );
    assert!(
        budget.try_consume(&homework).is_some(),
        "其他任务的额度互不影响（不得被对方的消耗拖累）"
    );
    budget.reset(&open);
    assert!(budget.try_consume(&open).is_some(), "重置后应重新取得额度");
    budget.clear();
    assert!(
        budget.try_consume(&homework).is_some(),
        "清空后所有任务重新取得额度"
    );
}

/// 連線重試的額度上限與自動重登不同（3 次嘗試），序號遞增到上限為止。
#[test]
fn connection_retry_budget_counts_up_to_its_own_limit() {
    let mut budget = AttemptBudgets::new(MAX_ATTEMPTS);
    let key = DataKey::Courses;

    assert_eq!(budget.try_consume(&key), Some(2), "首次重試是第 2 次嘗試");
    assert_eq!(budget.try_consume(&key), Some(3), "再下一次是第 3 次嘗試");
    assert_eq!(budget.try_consume(&key), None, "達到上限後不得再消耗");
}

// ── 自訂義任務（任務服務） ──────────────────────────────

/// 測試用自訂義任務。
fn todo_task(content: &str) -> Task {
    Task {
        id: 0,
        content: content.to_owned(),
        description: None,
        tag: None,
        deadline: None,
        priority: Priority::Low,
        completed: false,
    }
}

/// 事件中最後一次回報的任務快照。
fn last_tasks(events: &[Event]) -> Option<Vec<Task>> {
    events.iter().rev().find_map(|event| match event {
        Event::Tasks(tasks) => Some(tasks.clone()),
        _ => None,
    })
}

#[test]
fn unlocking_loads_the_encrypted_task_file() {
    let mut harness = harness(fake_flow(0));
    // 先以同一口令準備一份任務檔（模擬上一次使用留下的內容）。
    {
        let mut store = TaskStore::at(harness.tasks_path());
        store.init("secret123").unwrap();
        store.add(todo_task("写实验报告")).unwrap();
        store.add(todo_task("复习")).unwrap();
    }

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .expect("解锁");

    // 任務服務是非同步的：解鎖（送出 `InitTasks`）之後稍等一下才會有快照。
    let events = harness.wait_until_unlocked();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::VaultReady)),
        "解锁应回报凭证已就绪"
    );
    let tasks = last_tasks(&events).expect("解锁后应回报任务快照");
    assert_eq!(tasks.len(), 2, "任务文件应在解锁时载入");
}

/// 設定檔損毀重建的提示只能在解鎖之後送出。
///
/// 啟動當下的畫面是協議閱讀門或解鎖表單，兩者都不繪製底欄訊息；進到主畫面
/// 時 `apply_vault_ready` 又會把訊息覆寫成「凭证已就绪」。修復前提示是在
/// `Worker::run` 開頭發的，等於永遠看不到（而 `PRIVACY.md` 明文承諾會提示）。
#[test]
fn config_rebuild_notice_is_emitted_after_unlock() {
    let mut harness = harness(fake_flow(0));
    harness.worker.config.rebuilt = true;

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .expect("解锁");
    let events = harness.wait_until_unlocked();

    let ready = events
        .iter()
        .position(|event| matches!(event, Event::VaultReady))
        .expect("解锁应回报凭证已就绪");
    let notice = events
        .iter()
        .position(|event| {
            matches!(event, Event::Notice(message) if message.contains("配置文件已损坏并重建"))
        })
        .expect("重建提示应在解锁后送达");
    assert!(notice > ready, "提示必须晚于 VaultReady 才有底栏可显示");
    assert!(!harness.worker.config.rebuilt, "提示只发一次");
}

/// 沒有損毀重建時不應出現提示。
#[test]
fn config_rebuild_notice_is_absent_without_a_rebuild() {
    let mut harness = harness(fake_flow(0));

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .expect("解锁");
    let events = harness.wait_until_unlocked();

    assert!(
        !events.iter().any(|event| {
            matches!(event, Event::Notice(message) if message.contains("配置文件已损坏并重建"))
        }),
        "未发生重建时不应出现提示"
    );
}

#[test]
fn task_operations_report_snapshots_and_notices() {
    let mut harness = harness(fake_flow(0));
    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .unwrap();
    harness.wait_until_unlocked();

    let events = harness.dispatch_task(Job::AddTask {
        task: todo_task("甲"),
    });
    let tasks = last_tasks(&events).expect("新增后应回报快照");
    assert_eq!(tasks.len(), 1);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(message) if message == "已添加任务")),
        "新增应附带提示"
    );

    let id = tasks[0].id;
    let tasks = last_tasks(&harness.dispatch_task(Job::SetTaskDone { id, done: true })).unwrap();
    assert!(tasks[0].completed, "标记完成后快照应反映状态");

    let mut edited = tasks[0].clone();
    edited.content = "甲（改）".to_owned();
    edited.priority = Priority::High;
    let tasks = last_tasks(&harness.dispatch_task(Job::UpdateTask { id, task: edited })).unwrap();
    assert_eq!(tasks[0].content, "甲（改）");
    assert_eq!(tasks[0].priority, Priority::High);

    let tasks = last_tasks(&harness.dispatch_task(Job::SetTasksDone {
        ids: vec![id],
        done: false,
    }))
    .unwrap();
    assert!(!tasks[0].completed);

    let events = harness.dispatch_task(Job::DeleteTasks { ids: vec![id] });
    assert!(last_tasks(&events).unwrap().is_empty());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(message) if message == "已删除 1 个任务")),
        "批次删除应附带数量"
    );

    // 找不到任務時回報失敗（服務自己回報，不經過工作者）。
    let events = harness.dispatch_task(Job::DeleteTask { id });
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Tasks,
                ..
            }
        )),
        "任务失败应落在任务落点上：{events:?}"
    );
}

#[test]
fn deleting_completed_tasks_reports_the_count() {
    let mut harness = harness(fake_flow(0));
    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .unwrap();
    harness.dispatch_task(Job::AddTask {
        task: todo_task("甲"),
    });
    let events = harness.dispatch_task(Job::AddTask {
        task: todo_task("乙"),
    });
    let ids: Vec<u64> = last_tasks(&events)
        .unwrap()
        .iter()
        .map(|task| task.id)
        .collect();
    harness.dispatch_task(Job::SetTasksDone { ids, done: true });

    let events = harness.dispatch_task(Job::DeleteCompletedTasks);
    assert!(last_tasks(&events).unwrap().is_empty());
    assert!(
        events.iter().any(
            |event| matches!(event, Event::Notice(message) if message == "已删除 2 个已完成任务")
        ),
        "应回报删除数量"
    );

    // 沒有已完成任務時只提示，不報錯。
    let events = harness.dispatch_task(Job::DeleteCompletedTasks);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Notice(message) if message == "没有已完成的任务"))
    );
}

#[test]
fn changing_the_passphrase_reencrypts_the_task_file() {
    let mut harness = harness(fake_flow(0));
    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .unwrap();
    harness.dispatch_task(Job::AddTask {
        task: todo_task("换口令前的任务"),
    });
    harness
        .dispatch(Job::ChangePassphrase {
            old: "secret123".into(),
            new: "new-passphrase-1".into(),
        })
        .expect("修改口令");

    // 換口令會在寫入保險庫之前先同步等待任務檔重新加密（兩個檔案必須同口令）。
    let mut reloaded = TaskStore::at(harness.tasks_path());
    reloaded.init("new-passphrase-1").unwrap();
    assert_eq!(
        reloaded.tasks().len(),
        1,
        "换口令后任务文件应以新口令重新加密"
    );
    harness
        .vault
        .load("new-passphrase-1")
        .expect("凭证也应以新口令解开");
}

#[test]
fn changing_the_passphrase_is_refused_when_the_task_file_is_unreadable() {
    let mut harness = harness(fake_flow(0));
    // 先放一個無法解讀的任務檔：解鎖後任務存儲會被標記為不可用。
    std::fs::write(harness.tasks_path(), b"not an envelope at all").expect("写入损坏的任务文件");
    let before = std::fs::read(harness.tasks_path()).expect("读取原档");

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .expect("任务文件不可用不应阻断解锁");
    let events = harness.wait_until_unlocked();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Notice(message) if message.contains("任务文件无法读取")
        )),
        "解锁应提示任务文件无法读取：{events:?}"
    );

    // 換口令必須被拒絕：不得以記憶體中的空任務覆寫原檔。
    let err = harness
        .dispatch(Job::ChangePassphrase {
            old: "secret123".into(),
            new: "new-passphrase-1".into(),
        })
        .expect_err("任务文件不可用时换口令应失败");
    assert!(
        err.to_string().contains("任务文件无法读取"),
        "错误应说明任务文件无法读取：{err}"
    );
    assert_eq!(
        std::fs::read(harness.tasks_path()).expect("原档仍在"),
        before,
        "被拒绝的换口令不得改动原文件"
    );
    // 換口令在寫入保險庫之前就失敗：憑證口令必須維持不變。
    harness.vault.load("secret123").expect("旧口令仍有效");
    assert!(
        harness.vault.load("new-passphrase-1").is_err(),
        "新口令不得生效"
    );
}

#[test]
fn task_save_failure_reports_the_tasks_target() {
    let mut harness = harness(fake_flow(0));
    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".into(),
        })
        .unwrap();
    harness.wait_until_unlocked();
    // 以同名目錄佔位，讓寫入必定失敗（寫入失敗時記憶體狀態必須回滾）。
    std::fs::create_dir(harness.tasks_path()).unwrap();

    let events = harness.dispatch_task(Job::AddTask {
        task: todo_task("写不进去"),
    });
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Tasks,
                ..
            }
        )),
        "保存失败应回报任务落点：{events:?}"
    );
}

#[test]
fn interface_sender_reaches_the_task_service() {
    // 整合驗證：正式啟動路徑（工作者＋任務服務）下，介面送出的任務操作確實
    // 由任務服務處理——而不是躺在工作者的佇列裡等網路。
    let dir = TempDir::new().expect("建立暂存目录");
    let path = dir.path().join("credentials.vault");
    let vault = Vault::at(path);
    vault
        .store("secret123", &Credentials::new("3120000001", "pw-12345"))
        .expect("写入测试凭据");
    let config = Config {
        access_policy: AccessPolicy::Direct,
        save_path: Some(dir.path().join("config.json")),
        ..Config::default()
    };

    let (jobs, events) =
        crate::task::worker::spawn_with_tasks(config, vault, dir.path().join("tasks.vault"))
            .expect("启动工作者与任务服务");

    jobs.send(Job::Unlock {
        passphrase: "secret123".into(),
    })
    .expect("送出解锁");

    // 等到 `VaultReady`：介面在這之後才會操作任務。
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut unlocked = false;
    while !unlocked && Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(200)) {
            unlocked = matches!(event, Event::VaultReady);
        }
    }
    assert!(unlocked, "应回报凭证已就绪");

    jobs.send(Job::AddTask {
        task: todo_task("通过介面新增"),
    })
    .expect("送出新增任务");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut snapshot: Option<Vec<Task>> = None;
    while snapshot.is_none() && Instant::now() < deadline {
        if let Ok(Event::Tasks(tasks)) = events.recv_timeout(Duration::from_millis(200))
            && !tasks.is_empty()
        {
            snapshot = Some(tasks);
        }
    }
    assert_eq!(
        snapshot.expect("任务服务应回报快照").len(),
        1,
        "任务应由任务服务写入并回报"
    );

    // 任務檔確實落在指定的路徑（沒有動到使用者的資料目錄）。
    let mut store = TaskStore::at(dir.path().join("tasks.vault"));
    store.init("secret123").expect("以同一口令载入");
    assert_eq!(store.tasks().len(), 1);
}
