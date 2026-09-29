//! 背景工作執行緒測試：憑證重輸、待重試任務與保險庫寫入時機。
//!
//! 以假 HTTP 客戶端離線組出「登入頁 → 公鑰 → 提交帳密 → 業務收尾」的完整流程，
//! 驗證重新輸入的憑證只在登入成功後才寫入保險庫，且登入失敗不會遺失待重試的任務。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ::rsa::pkcs8::EncodePublicKey as _;
use ::rsa::{RsaPrivateKey, RsaPublicKey};
use tempfile::TempDir;

use crate::auth::rsa;
use crate::domain::homework::HomeworkState;
use crate::domain::semester::TermCode;
use crate::http::fake::{FakeClient, html, json};
use crate::http::{HttpClient, HttpRequest, HttpResponse};
use crate::session::AccessMode;
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
    /// 保持任務通道開啟（資料任務會檢查通道是否斷開）。
    _jobs: Sender<Job>,
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
            save_path: Some(dir.path().join("config.json")),
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
                pending_data: VecDeque::new(),
                generation: 0,
                cache: LmsCache::default(),
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
        match self.worker.dispatch(job) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.worker.emit(Event::Failed {
                    what,
                    message: err.to_string(),
                    target,
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
        harness.saw(|event| matches!(event, Event::LoginSucceeded { .. })),
        "第二次重试应当登录成功"
    );
}

#[test]
fn pending_data_job_survives_failed_relogin_and_resumes_afterwards() {
    let mut harness = harness(fake_flow(1));
    // 模擬「載入作業時登入態失效」：任務已排入待重試。
    harness.worker.retry = Some(Job::LoadHomework { force: true });

    let failed = harness.dispatch(Job::RetryLogin);
    assert!(failed.is_err(), "公钥失败时重新登录应当失败");
    assert!(
        matches!(harness.worker.retry, Some(Job::LoadHomework { .. })),
        "登录失败不得丢失待重试的任务"
    );

    // 第二次重試：考勤登入成功後應自動續跑作業載入（思源學堂課程為空）。
    harness.dispatch(Job::RetryLogin).expect("重试应当成功");
    assert!(
        harness.saw(|event| matches!(event, Event::Homework(update) if update.items.is_empty())),
        "待重试的作业任务应在登录成功后自动续跑"
    );
    assert!(harness.worker.retry.is_none(), "任务续跑后不应继续保留");
}

#[test]
fn unlock_does_not_start_login() {
    let mut harness = harness(|_request: &HttpRequest| panic!("解锁不应触发任何网络请求"));

    harness
        .dispatch(Job::Unlock {
            passphrase: "secret123".to_owned(),
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
        events
            .iter()
            .any(|event| matches!(event, Event::SessionsCleared)),
        "解锁后应回报会话已重置"
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
        events
            .iter()
            .any(|event| matches!(event, Event::SessionsCleared)),
        "切换访问模式后应回报会话已重置"
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
