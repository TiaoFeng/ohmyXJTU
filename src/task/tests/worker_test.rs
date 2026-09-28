//! 背景工作執行緒測試：憑證重輸、待重試任務與保險庫寫入時機。
//!
//! 以假 HTTP 客戶端離線組出「登入頁 → 公鑰 → 提交帳密 → 業務收尾」的完整流程，
//! 驗證重新輸入的憑證只在登入成功後才寫入保險庫，且登入失敗不會遺失待重試的任務。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use ::rsa::pkcs8::EncodePublicKey as _;
use ::rsa::{RsaPrivateKey, RsaPublicKey};
use tempfile::TempDir;

use crate::auth::rsa;
use crate::http::fake::{FakeClient, html, json};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::sites::{attendance, lms};

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

/// 統一認證登入頁（含 `execution`，且關閉 MFA）。
fn login_page() -> String {
    r#"<html><head><script>
    var globalConfig = eval('(' + "{\"mfaEnabled\":false}" + ')');
    </script></head><body>
    <input type="hidden" name="execution" value="e1s1" />
    </body></html>"#
        .to_owned()
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

/// 測試用工作執行緒：以假客戶端取代真實網路與資料目錄。
struct Harness {
    worker: Worker,
    events: Receiver<Event>,
    vault: Vault,
    /// 持有暫存目錄，離開作用域時自動刪除。
    _dir: TempDir,
}

impl Harness {
    fn new(
        responder: impl Fn(&HttpRequest) -> AppResult<HttpResponse> + Send + Sync + 'static,
    ) -> Self {
        let dir = TempDir::new().expect("建立暂存目录");
        let vault = Vault::at(dir.path().join("credentials.vault"));

        let config = Config {
            access_policy: AccessPolicy::Direct,
            ..Config::default()
        };

        let client = Arc::new(FakeClient::with_responder(responder));
        let direct: Arc<dyn HttpClient> = client.clone();
        let webvpn: Arc<dyn HttpClient> = client;

        let credentials = Credentials::new("3120000001", "old-password");
        let mut session = SessionManager::with_clients(&config, direct, webvpn);
        session.register(Box::new(AttendanceSite));
        session.register(Box::new(LmsSite));
        session.set_credentials(credentials.clone());

        let (_jobs, jobs) = channel();
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
            },
            events: event_rx,
            vault,
            _dir: dir,
        }
    }

    /// 預先寫入舊憑證。
    fn seed_vault(&self, passphrase: &str, credentials: &Credentials) {
        self.vault
            .store(passphrase, credentials)
            .expect("写入测试凭据");
    }

    /// 執行任務（錯誤處理比照 [`Worker::run`]）。
    fn dispatch(&mut self, job: Job) -> AppResult<()> {
        let what = job.label();
        match self.worker.dispatch(job) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.worker.emit(Event::Failed {
                    what,
                    message: err.to_string(),
                });
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
        credentials: Credentials::new("3120000002", "new-password"),
        passphrase: "wrong-passphrase".to_owned(),
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
fn saves_credentials_only_after_login_succeeds() {
    let mut harness = harness(fake_flow(0));

    harness
        .dispatch(Job::RetryWithAccount {
            credentials: Credentials::new("3120000002", "new-password"),
            passphrase: "secret123".to_owned(),
        })
        .expect("登录应当成功");

    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.username, "3120000002");
    assert_eq!(stored.password, "new-password");

    let events = harness.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::LoginSucceeded)),
        "应当回报登录成功"
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
            credentials: Credentials::new("3120000002", "wrong-password"),
            passphrase: "secret123".to_owned(),
        })
        .expect("登录被拒属于预期结果，不应是任务错误");

    assert!(
        harness.saw(|event| matches!(event, Event::LoginFailed(_))),
        "应当回报登录失败"
    );
    let stored = harness.vault.load("secret123").expect("读取凭据");
    assert_eq!(stored.password, "old-password", "失败不得覆盖旧凭据");
}

#[test]
fn public_key_failure_is_recoverable_by_retrying() {
    let mut harness = harness(fake_flow(1));

    let first = harness.dispatch(Job::RetryLogin);
    assert!(
        matches!(first, Err(AppError::Protocol(_))),
        "公钥不是 PEM 时应报告协议错误，实际：{first:?}"
    );
    assert!(
        harness.saw(|event| matches!(event, Event::Failed { .. })),
        "应当回报失败事件"
    );

    harness.dispatch(Job::RetryLogin).expect("重试应当成功");
    assert!(
        harness.saw(|event| matches!(event, Event::LoginSucceeded)),
        "第二次重试应当登录成功"
    );
}

#[test]
fn pending_data_job_survives_failed_relogin_and_resumes_afterwards() {
    let mut harness = harness(fake_flow(1));
    // 模擬「載入作業時登入態失效」：任務已排入待重試。
    harness.worker.retry = Some(Job::LoadHomework);

    let failed = harness.dispatch(Job::RetryLogin);
    assert!(failed.is_err(), "公钥失败时重新登录应当失败");
    assert!(
        matches!(harness.worker.retry, Some(Job::LoadHomework)),
        "登录失败不得丢失待重试的任务"
    );

    // 第二次重試：考勤登入成功後應自動續跑作業載入（思源學堂課程為空）。
    harness.dispatch(Job::RetryLogin).expect("重试应当成功");
    assert!(
        harness.saw(|event| matches!(event, Event::Homework(items) if items.is_empty())),
        "待重试的作业任务应在登录成功后自动续跑"
    );
    assert!(harness.worker.retry.is_none(), "任务续跑后不应继续保留");
}
