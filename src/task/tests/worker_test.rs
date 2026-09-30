//! 背景工作執行緒測試：憑證重輸、待重試任務與保險庫寫入時機。
//!
//! 以假 HTTP 客戶端離線組出「登入頁 → 公鑰 → 提交帳密 → 業務收尾」的完整流程，
//! 驗證重新輸入的憑證只在登入成功後才寫入保險庫，且登入失敗不會遺失待重試的任務。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ::rsa::pkcs8::EncodePublicKey as _;
use ::rsa::{RsaPrivateKey, RsaPublicKey};
use tempfile::TempDir;

use crate::auth::rsa;
use crate::domain::homework::HomeworkState;
use crate::domain::semester::TermCode;
use crate::error::NetworkKind;
use crate::http::fake::{FakeClient, html, json};
use crate::http::{HttpClient, HttpRequest, HttpResponse, Method};
use crate::session::AccessMode;
use crate::sites::{attendance, lms};
use crate::tui::app::{App, LoginScreen};
use crate::tui::text::InputLine;

use super::*;

/// 考勤站點的登入頁位址（同時是帳密表單的提交位址）。
const ATTENDANCE_POST: &str = "https://login.xjtu.edu.cn/cas/login?service=attendance";
/// 考勤站點的登入回跳位址（帶 `loginRequestId` 與 `ticket`）。
const ATTENDANCE_TARGET: &str =
    "https://bk-kq.xjtu.edu.cn/sa/auth/cas/student-pc?loginRequestId=req-1&ticket=ticket-1";
/// 考勤站點的業務 token 交換端點。
const ATTENDANCE_EXCHANGE: &str = "https://bk-kq.xjtu.edu.cn/sa/auth/cas/exchange";
/// 思源學堂的登入頁位址（同時是帳密表單的提交位址）。
const LMS_POST: &str = "https://login.xjtu.edu.cn/cas/login?service=lms";
/// 思源學堂首頁位址。
const LMS_HOME: &str = "https://lms.xjtu.edu.cn/user/index";
/// 思源學堂課程清單端點。
const LMS_COURSES: &str = "https://lms.xjtu.edu.cn/api/my-courses";
/// 登入成功後回傳的目標網頁。
const TARGET_BODY: &str =
    "<html><head><title>思源学堂</title></head><body>globalData</body></html>";

/// 測試用公鑰 PEM（2048 位元金鑰產生較慢，整個測試二進位檔共用一份）。
fn public_key_pem() -> &'static str {
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        let mut rng = chacha20poly1305::aead::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("生成测试密钥");
        RsaPublicKey::from(&private)
            .to_public_key_pem(::rsa::pkcs8::LineEnding::LF)
            .expect("导出公钥 PEM")
    })
}

/// 統一認證登入頁（含 `execution`）；`mfa_enabled` 控制是否要求簡訊驗證。
///
/// 注意：原始碼中的 `\"` 會原樣出現在頁面文字裡，測試裡要改 `mfaEnabled`
/// 必須比對整段（見下方兩個包裝函式）。
fn login_page_with(mfa_enabled: bool) -> String {
    format!(
        r#"<html><head><script>
    var globalConfig = eval('(' + "{{\"mfaEnabled\":{mfa_enabled}}}" + ')');
    </script></head><body>
    <input type="hidden" name="execution" value="e1s1" />
    </body></html>"#
    )
}

/// 統一認證登入頁（不需要簡訊驗證）。
fn login_page() -> String {
    login_page_with(false)
}

/// 統一認證登入頁（需要簡訊驗證）。
fn login_page_with_mfa() -> String {
    login_page_with(true)
}

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
                relogin_attempts: 0,
                cache: LmsCache::default(),
                known_term: None,
                shutdown: false,
            },
            events: event_rx,
            vault,
            _jobs: job_tx,
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

    /// 執行任務（錯誤處理比照 [`Worker::run`]）。
    fn dispatch(&mut self, job: Job) -> AppResult<()> {
        let what = job.label();
        let target = failed_target_of(&job);
        let site = self.worker.login_site_of(&job);
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
        "Debug 不得洩漏口令：{debug}"
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
        "换账号应清除旧账号的资料与快取"
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
        .dispatch(Job::LoadSchedule)
        .expect("資料任務失敗不應冒泡為任務錯誤");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::SessionExpired {
                site: SiteKind::Attendance
            }
        )),
        "應先回報工作階段失效：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginProgress(_))),
        "登入流程連開始都做不到時，不得顯示「正在登入」：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Schedule,
                ..
            }
        )),
        "原頁面必須收斂為失敗，而不是停在載入中：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Login,
                ..
            }
        )),
        "單純的連線失敗不應彈出登入框：{events:?}"
    );
    assert!(
        harness.worker.retry.is_none(),
        "重登失敗後不應保留待重試任務"
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
        .dispatch(Job::LoadSchedule)
        .expect("資料任務失敗不應冒泡為任務錯誤");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LoginProgress(_))),
        "登入流程確實開始時應顯示進度：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Schedule,
                ..
            }
        )),
        "進度之後必須跟著原頁面的失敗事件，介面才能收斂覆蓋層：{events:?}"
    );
    assert!(harness.worker.retry.is_none());
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
        retry: Some(Job::LoadSchedule),
    });
    harness.worker.retry = Some(Job::LoadSchedule);
    // 模擬「使用者重新輸入密碼後嘗試登入」的當下狀態。
    harness.worker.credentials = Some(Credentials::new("3120000001", "typed-password"));
    harness.worker.pending_vault = Some(PendingVault {
        passphrase: Secret::from("secret123"),
        credentials: Credentials::new("3120000001", "typed-password"),
        previous: Some(Credentials::new("3120000001", "old-password")),
    });

    harness
        .dispatch(Job::CancelLogin)
        .expect("取消登入应当成功");

    assert!(harness.worker.flow.is_none(), "應丟棄登入流程");
    assert!(harness.worker.pending_vault.is_none(), "應丟棄待存憑證");
    assert!(harness.worker.retry.is_none(), "應丟棄待重試任務");
    assert_eq!(
        harness.worker.credentials,
        Some(Credentials::new("3120000001", "old-password")),
        "取消後記憶體中的憑證應還原為保險庫保存的舊憑證"
    );
    assert_eq!(
        harness
            .worker
            .session
            .as_ref()
            .and_then(|session| session.credentials().cloned()),
        Some(Credentials::new("3120000001", "old-password")),
        "工作階段的憑證也應還原"
    );
    assert!(
        harness.saw(|event| matches!(event, Event::Notice(_))),
        "應提示已取消登入"
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

// ── 資料任務合併（強制刷新優先、同鍵至多一筆）──────────

#[test]
fn merge_upgrades_queued_non_forced_job_in_place() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合併不应触发网络请求"));
    harness.worker.pending_data.push_back(Job::LoadSchedule);
    harness
        .worker
        .pending_data
        .push_back(Job::LoadCourses { force: false });

    harness
        .worker
        .merge_data_job(Job::LoadCourses { force: true }, None);

    let queued: Vec<&Job> = harness.worker.pending_data.iter().collect();
    assert_eq!(queued.len(), 2, "同键任务应原位升级而不是追加");
    assert!(matches!(queued[0], Job::LoadSchedule), "无关任务位置不变");
    assert!(
        matches!(queued[1], Job::LoadCourses { force: true }),
        "非强制任务应原位升级为强制"
    );
}

#[test]
fn merge_never_downgrades_forced_job() {
    let mut harness = harness(|_request: &HttpRequest| panic!("合併不应触发网络请求"));
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
    let mut harness = harness(|_request: &HttpRequest| panic!("合併不应触发网络请求"));
    harness.worker.pending_data.push_back(Job::LoadSchedule);
    harness
        .worker
        .pending_data
        .push_back(Job::LoadFlow { page: 1 });

    // 同鍵重複：丟棄；不同頁碼是不同資源鍵，允許並存。
    harness.worker.merge_data_job(Job::LoadSchedule, None);
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
    let mut harness = harness(|_request: &HttpRequest| panic!("合併不应触发网络请求"));
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
    let mut harness = harness(|_request: &HttpRequest| panic!("合併不应触发网络请求"));
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
    assert!(saved.contains("webvpn"), "设定应写入磁盘：{saved}");

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
        "内存中的设定应记录已同意版本"
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
                panic!("考勤未登入时不应查询学期：{url}");
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
    let err = super::parse_date("<html>2026/09/07</html>").expect_err("应拒绝非 YYYY-MM-DD");
    let message = err.to_string();
    assert!(!message.contains("2026/09/07"), "不得夹带原始值：{message}");
    assert!(!message.contains("<html>"), "不得夹带原始值：{message}");
}

#[test]
fn homework_filters_to_current_term_and_streams_progress() {
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
                      "end_time": "2026-10-01 23:59:59" },
                    { "id": "12", "type": "material", "title": "课件" },
                ]}),
            ),
            ("2", serde_json::json!({ "activities": [] })),
        ],
        details: vec![(
            "11",
            serde_json::json!({ "id": "11", "type": "homework", "title": "作业A",
                "end_time": "2026-10-01 23:59:59",
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
    assert!(last.items[0].submit_by_group, "小组判定应取自活动详情");

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
        "考勤未登入时不得查询其学期"
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
    assert_eq!(count(&site, "/api/my-courses"), 1, "第二次应命中课程快取");
    assert_eq!(
        count(&site, "/courses/1/activities"),
        1,
        "第二次应命中活动快取"
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
fn activity_detail_for_material_skips_submission_request() {
    let site = Arc::new(FakeHomeworkSite {
        seen: Arc::new(Mutex::new(Vec::new())),
        courses: serde_json::json!({ "courses": [] }),
        activities: Vec::new(),
        details: vec![(
            "77",
            serde_json::json!({ "id": "77", "type": "material", "title": "课件",
                "end_time": "2026-10-01 12:00:00" }),
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
        })
        .expect("載入詳情");

    let seen = site.urls();
    assert!(
        seen.iter().any(|url| url.ends_with("/api/activities/77")),
        "應請求活動詳情：{seen:?}"
    );
    assert!(
        !seen.iter().any(|url| url.contains("/submission_list")),
        "非作業不得查詢提交記錄：{seen:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::ActivityDetail(detail)
                if detail.kind == lms::ActivityKind::Material && detail.submissions.is_none()
        )),
        "應回報資料類型的詳情且不帶提交狀態"
    );
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
                "user_submit_count": 0 }),
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
        })
        .expect("載入詳情");

    let seen = site.urls();
    assert!(
        seen.iter()
            .any(|url| url.contains("/students/42/submission_list")),
        "作業詳情應查詢個人提交記錄：{seen:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::ActivityDetail(detail)
                if detail.kind == lms::ActivityKind::Homework && detail.submissions.is_some()
        )),
        "作業詳情應帶提交記錄"
    );
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
        "課程內容應查詢播放器接口：{seen:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/lesson/player?token=abc"
        )),
        "應回報伺服器提供的播放地址"
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
        "作業不得查詢播放器接口：{seen:?}"
    );
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn/course/42/homework"
        )),
        "作業應開啟所屬課程的作業列表（且不得附帶 hash 片段）"
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
        "兩種無效識別碼都應告知使用者：{events:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::OpenUrl(url) if url.contains("/course/")
        )),
        "無效識別碼不得拼出課程網址：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn"
        )),
        "應回退到思源學堂首頁"
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
        "WebVPN 模式下應回報改寫後的網址"
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
        "取不到播放地址时应告知使用者"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::OpenUrl(url) if url == "https://lms.xjtu.edu.cn"
        )),
        "應回退到思源學堂首頁"
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
    assert_eq!(second.len(), first, "第二次載入應全面命中快取：{second:?}");

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert_eq!(last.requests, 0, "命中快取時不應再送請求");
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
        "強制刷新應重新取得活動詳情：{seen:?}"
    );

    let updates = homework_updates(&mut harness);
    let last = updates.last().expect("最终更新");
    assert!(last.requests >= 3, "強制刷新應重取課程／活動／詳情");
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
    assert_eq!(last.requests, after - before, "統計請求數應與實際相符");
    assert!(
        last.elapsed <= Duration::from_secs(60),
        "耗時統計應為合理值"
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
        "应复用使用者记忆的学期"
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

    assert!(!path.exists(), "登入成功後應刪除驗證碼檔案");
    assert!(harness.worker.captcha_path.is_none(), "應清除路徑記錄");
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

    assert!(!path.exists(), "憑證被拒後應刪除驗證碼檔案");
    assert!(harness.worker.captcha_path.is_none(), "應清除路徑記錄");
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

    assert!(!path.exists(), "登入任務失敗後應刪除驗證碼檔案");
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Login,
                ..
            }
        )),
        "应回报登入失败事件：{events:?}"
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
    assert_eq!(built.load(Ordering::SeqCst), 2, "建立時兩個後端各建一次");

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });

    assert!(result.is_err(), "收尾失敗的換帳號必須失敗");
    assert_eq!(
        built.load(Ordering::SeqCst),
        6,
        "換帳號重建一次、回復舊帳號又重建一次：新帳號的 cookie 不得沿用"
    );
    assert!(harness.worker.flow.is_none(), "進行中的登入流程應一併作廢");
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "記憶體中的憑證應還原為舊帳號"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(
        stored.username, "3120000001",
        "保險庫不得被未驗證的憑證覆蓋"
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
            other => panic!("思源学堂重試不應连到其他站点：{other}"),
        }
    });

    harness
        .dispatch(Job::RetryWithAccount {
            site: SiteKind::Lms,
            credentials: Credentials::new("3120000002", "new-password"),
            passphrase: "secret123".into(),
        })
        .expect("思源学堂的重新登入应当成功");

    let urls = seen.lock().expect("lock").clone();
    assert!(
        urls.iter().any(|url| url == LMS_POST),
        "登入必須送到思源学堂：{urls:?}"
    );
    assert!(
        !urls.iter().any(|url| url.contains("bk-kq.xjtu.edu.cn")),
        "不得因為重試而改走考勤系统：{urls:?}"
    );
    assert_eq!(
        harness.worker.login_site,
        Some(SiteKind::Lms),
        "重試站點應記為思源学堂"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000002", "驗證成功後應寫回新憑證");
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
        .expect_err("離線時重新登入必定失敗");

    assert!(
        err.to_string().contains("连接失败"),
        "應回報連線失敗：{err}"
    );
    assert!(
        harness.worker.pending_vault.is_none(),
        "失敗後應丟棄整條待存憑證鏈"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "應還原為保險庫中的 A，而不是上一次未驗證的 B"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "保險庫內容不得被更動");
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
        attempt(&mut harness).expect("帳密被拒屬於預期結果");
    }

    assert_eq!(
        seen.lock().expect("lock").as_slice(),
        ["0", "1", "2"],
        "同帳號重複失敗必須累積（修復前每次都被清零）"
    );
    assert_eq!(
        harness
            .worker
            .login_failures
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![3],
        "失敗計數必須保留"
    );

    // 第四次：已達門檻，不再提交帳密，改為要求圖片驗證碼。
    assert!(attempt(&mut harness).is_err(), "達到門檻後不應再提交帳密");
    assert_eq!(
        seen.lock().expect("lock").len(),
        3,
        "第四次嘗試不得再送出帳密"
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
        .expect("換帳號應停在簡訊驗證");
    assert!(
        harness.saw(|event| matches!(event, Event::LoginNeedsMfa { .. })),
        "換帳號應進入簡訊驗證"
    );
    harness
        .dispatch(Job::SendMfaCode)
        .expect("發送驗證碼应当成功");

    let result = harness.dispatch(Job::VerifyMfaCode(Secret::from("123456")));
    assert!(result.is_err(), "收尾失敗必須回報錯誤");

    assert!(
        harness.worker.pending_vault.is_none(),
        "互動驗證之後才失敗的切換同樣必須丟棄待存憑證"
    );
    assert!(harness.worker.flow.is_none(), "失敗的登入流程應一併作廢");
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "記憶體中的憑證應還原為舊帳號"
    );

    // 之後思源學堂登入成功：不得把失敗切換的憑證寫進保險庫。
    harness
        .dispatch(Job::RetryLogin {
            site: SiteKind::Lms,
        })
        .expect("思源学堂登入应当成功");
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "保險庫不得寫入 B 的憑證");
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
        .expect("換帳號應停在簡訊驗證");
    harness
        .dispatch(Job::SendMfaCode)
        .expect("發送驗證碼应当成功");

    let err = harness
        .dispatch(Job::VerifyMfaCode(Secret::from("000000")))
        .expect_err("驗證碼錯誤應回報錯誤");
    assert!(err.to_string().contains("短信验证码不正确"), "{err}");
    assert!(
        harness.worker.pending_vault.is_some(),
        "驗證碼填錯時應保留待存憑證，讓使用者重輸後繼續同一次切換"
    );
    assert!(
        harness.worker.flow.is_some(),
        "登入流程應保留以便重輸驗證碼"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000002".to_owned()),
        "重輸驗證碼仍應以新帳號進行"
    );
    // 介面必須收到「可重輸」的訊號（而不是一般失敗畫面），否則輸入框會被蓋掉。
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::VerificationRetry { site: SiteKind::Attendance, message }
                if message.contains("短信验证码不正确")
        )),
        "應回報可重試的驗證錯誤：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Failed { .. })),
        "驗證碼填錯不應彈出一般失敗：{events:?}"
    );

    // 重輸正確的驗證碼即可續用同一次切換（同一個驅動器、同一個 gid）。
    let resumed = harness.dispatch(Job::VerifyMfaCode(Secret::from("123456")));
    assert!(resumed.is_ok(), "正確的驗證碼應能繼續登入：{resumed:?}");
}

/// 圖片驗證碼填錯同樣是可重試的：不得作廢整次帳號切換。
#[test]
fn captcha_mistake_keeps_the_pending_switch() {
    // 直接以 `handle_reply` 驗證善後規則：不經網路，也不會寫入驗證碼圖片。
    let mut harness = harness(|_request: &HttpRequest| panic!("本測試不應發出請求"));

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
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登入驅動器");
    // 已達門檻：直接進入驗證碼流程（不需先失敗三次）。
    driver.set_fail_count(3);
    assert_eq!(
        driver
            .start(
                &Credentials::new("3120000002", "new-password"),
                AccountType::Undergraduate
            )
            .expect("啟動登入"),
        LoginReply::NeedCaptcha
    );
    let reply = driver.submit_captcha("bad-code").expect("提交驗證碼");
    assert!(matches!(reply, LoginReply::Fail { .. }), "{reply:?}");
    assert!(
        driver.last_attempt_submitted_captcha(),
        "測試前提：這次提交帶了驗證碼"
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
        .expect("處理登入回覆不應出錯");

    assert!(
        harness.worker.pending_vault.is_some(),
        "驗證碼填錯時應保留待存憑證，讓使用者重輸後繼續同一次切換"
    );
    assert!(
        harness.worker.flow.is_some(),
        "登入流程應保留以便重輸驗證碼"
    );
    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::VerificationRetry { site: SiteKind::Attendance, message }
                if !message.is_empty()
        )),
        "驗證碼填錯應回報可重試事件（讓介面留在輸入畫面）：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginFailed { .. } | Event::Failed { .. })),
        "驗證碼填錯不應彈出一般失敗畫面：{events:?}"
    );
}

/// 驗證碼填錯後「工作者 → 介面」的完整串接：輸入畫面必須留著並顯示錯誤。
#[test]
fn wrong_captcha_keeps_the_captcha_input_screen() {
    // 白箱：以假驅動器把流程帶到「已送出錯誤驗證碼」，不必寫入真實的驗證碼圖片。
    let mut harness = harness(|_request: &HttpRequest| panic!("本測試不應發出請求"));

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
        LoginDriver::new(client, attendance::LOGIN_URL, &"0".repeat(32)).expect("建立登入驅動器");
    driver.set_fail_count(3);
    assert_eq!(
        driver
            .start(
                &Credentials::new("3120000002", "new-password"),
                AccountType::Undergraduate
            )
            .expect("啟動登入"),
        LoginReply::NeedCaptcha
    );
    let reply = driver.submit_captcha("bad-code").expect("提交驗證碼");
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
        .expect("處理登入回覆不應出錯");
    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::VerificationRetry { .. })),
        "應回報可重試的驗證錯誤：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::LoginFailed { .. })),
        "不得發出一般登入失敗（會把輸入畫面換掉）：{events:?}"
    );

    // 依序套用工作者發出的事件：畫面必須仍是驗證碼輸入框。
    for event in events {
        crate::tui::apply_event_for_test(&mut app, event);
    }
    match app.login.as_deref() {
        Some(LoginScreen::Captcha { input, error, path }) => {
            assert!(input.is_empty(), "重輸前應清空驗證碼：{:?}", input.value());
            assert!(
                error.as_deref().is_some_and(|text| !text.is_empty()),
                "輸入畫面應就地顯示錯誤：{error:?}"
            );
            assert_eq!(path, &image, "換不到新圖時應沿用舊圖");
        }
        other => panic!("應留在驗證碼輸入畫面，實際為 {other:?}"),
    }
    assert!(
        image.exists(),
        "換新圖失敗時不得刪掉使用者正在看的驗證碼圖片"
    );
    assert!(
        harness.worker.flow.is_some() && harness.worker.pending_vault.is_some(),
        "同一次帳號切換必須保留，讓重輸的驗證碼沿用同一個登入流程"
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
            other => panic!("會話停用後不得再發出請求：{other}"),
        },
        Arc::clone(&calls),
        4,
    );
    harness.seed_vault("secret123", &Credentials::new("3120000001", "old-password"));

    let result = harness.dispatch(Job::ChangeAccount {
        passphrase: "secret123".into(),
        credentials: Credentials::new("3120000002", "new-password"),
    });
    assert!(result.is_err(), "收尾失敗的換帳號必須失敗");

    assert!(
        harness.worker.session.is_none(),
        "無法建立乾淨的會話時必須停用會話（不得繼續帶新帳號的 cookie）"
    );
    assert!(
        harness.saw(|event| matches!(event, Event::SessionDisabled(_))),
        "應回報會話已停用，讓介面回到解鎖畫面"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "憑證仍應還原為舊帳號"
    );

    // 後續資料任務不得發出任何請求（responder 會 panic）。
    harness
        .dispatch(Job::LoadCourses { force: false })
        .expect("資料任務失敗不應冒泡為任務錯誤");
    assert!(
        harness.saw(|event| matches!(
            event,
            Event::Failed {
                target: FailedTarget::Courses,
                ..
            }
        )),
        "資料任務應回報失敗（會話尚未建立）"
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
        "失敗後不得留下待存憑證，否則之後任何一次登入成功都會把它寫回"
    );
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "記憶體中的憑證應還原為舊帳號"
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
        .expect_err("未提交帳密的「登入」不得視為驗證成功");

    assert!(
        err.to_string().contains("无法验证新账号"),
        "訊息應說明無法驗證新帳號：{err}"
    );
    assert!(harness.worker.pending_vault.is_none(), "應丟棄待存憑證");
    assert_eq!(
        harness
            .worker
            .credentials
            .as_ref()
            .map(|credentials| credentials.username.clone()),
        Some("3120000001".to_owned()),
        "記憶體中的憑證應還原"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000001", "未驗證的憑證不得寫回");
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
        .expect("資料任務失敗不應冒泡為任務錯誤");

    assert!(
        harness.saw(|event| matches!(
            event,
            Event::SessionExpired {
                site: SiteKind::Attendance
            }
        )),
        "考勤學期查詢失敗必須歸給考勤系統，重登才會打到對的站點"
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
        .expect("資料任務失敗不應冒泡為任務錯誤");

    let events = harness.drain_events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::LoginFailed {
                site: SiteKind::Lms,
                ..
            }
        )),
        "登入失敗事件必須指出思源學堂：{events:?}"
    );
    assert_eq!(
        harness.worker.login_site,
        Some(SiteKind::Lms),
        "重試站點應記為思源學堂"
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
            .expect("帳密被拒屬於預期結果");
    }

    assert_eq!(
        seen.lock().expect("lock").as_slice(),
        ["0", "1", "2"],
        "每次提交的 failN 應累積（修復前恆為 0）"
    );
    assert_eq!(
        harness
            .worker
            .login_failures
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![3],
        "失敗次數必須跨驅動器保存"
    );

    // 第四次：已達門檻，不再提交帳密，改為要求圖片驗證碼（圖片端點在此失敗）。
    assert!(
        harness
            .dispatch(Job::RetryLogin {
                site: SiteKind::Attendance,
            })
            .is_err(),
        "達到門檻後不應再提交帳密"
    );
    assert_eq!(
        seen.lock().expect("lock").len(),
        3,
        "第四次嘗試不得再送出帳密"
    );

    // 取消登入彈窗不得清除計數：伺服器端的門檻是跨嘗試累計的。
    harness
        .dispatch(Job::CancelLogin)
        .expect("取消登入不應失敗");
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
