//! 背景工作執行緒。
//!
//! 所有網路請求都在這裡執行，介面只透過 [`Job`] 下指令、透過 [`Event`] 收結果。
//! 登入流程由 [`LoginDriver`] 驅動：需要驗證碼或簡訊驗證時回報事件，
//! 使用者輸入後再繼續；登入態失效時會自動重新登入並重試原本的任務。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use chrono::{Local, NaiveDate};
use zeroize::Zeroizing;

use crate::auth::{AccountType, LoginDriver, LoginReply};
use crate::config::{AccessPolicy, Config};
use crate::credentials::{Credentials, Vault};
use crate::domain::homework::{HomeworkInput, HomeworkItem};
use crate::domain::{attendance_match, homework, schedule};
use crate::error::{AppError, AppResult};
use crate::session::{LoginStage, SessionManager, SiteKind};
use crate::sites::attendance::{AttendanceApi, AttendanceSite};
use crate::sites::lms::{ActivityKind, LmsActivity, LmsApi, LmsCourse, LmsSite};
use crate::tui::app::{ActivityDetailView, FlowData, LessonEntry, ScheduleData};

/// 考勤流水分頁大小。
const FLOW_PAGE_SIZE: u32 = 20;

/// 介面送到背景的任務。
#[derive(Debug, Clone)]
pub enum Job {
    /// 首次建立保險庫並登入。
    CreateVault {
        /// 使用者設定的加密口令。
        passphrase: String,
        /// 帳號密碼。
        credentials: Credentials,
    },
    /// 解鎖保險庫。
    Unlock {
        /// 加密口令。
        passphrase: String,
    },
    /// 提交圖片驗證碼。
    SubmitCaptcha(String),
    /// 重新取得驗證碼圖片。
    RefreshCaptcha,
    /// 發送簡訊驗證碼。
    SendMfaCode,
    /// 提交簡訊驗證碼。
    VerifyMfaCode(String),
    /// 重新開始登入（登入失敗後重試）。
    RetryLogin,
    /// 以重新輸入的帳號密碼重試登入，成功後才寫入保險庫。
    RetryWithAccount {
        /// 使用者重新輸入的帳號密碼。
        credentials: Credentials,
        /// 加密口令（用於登入成功後更新保險庫）。
        passphrase: String,
    },
    /// 載入課表（含本週考勤）。
    LoadSchedule,
    /// 載入待處理作業。
    LoadHomework,
    /// 載入考勤流水。
    LoadFlow {
        /// 頁碼。
        page: u32,
    },
    /// 載入思源學堂課程。
    LoadCourses,
    /// 載入課程活動。
    LoadActivities {
        /// 課程識別碼。
        course_id: String,
    },
    /// 載入活動詳情。
    LoadActivityDetail {
        /// 活動識別碼。
        activity_id: String,
    },
    /// 修改帳號。
    ChangeAccount {
        /// 原加密口令。
        passphrase: String,
        /// 新帳號密碼。
        credentials: Credentials,
    },
    /// 修改加密口令。
    ChangePassphrase {
        /// 原口令。
        old: String,
        /// 新口令。
        new: String,
    },
    /// 切換訪問策略。
    SetAccessPolicy(AccessPolicy),
    /// 結束工作執行緒。
    Shutdown,
}

impl Job {
    /// 任務說明（用於錯誤訊息）。
    pub fn label(&self) -> String {
        match self {
            Self::CreateVault { .. } | Self::Unlock { .. } => "账户设置".to_owned(),
            Self::SubmitCaptcha(_) | Self::RefreshCaptcha => "验证码".to_owned(),
            Self::SendMfaCode | Self::VerifyMfaCode(_) => "短信验证".to_owned(),
            Self::RetryLogin => "登录".to_owned(),
            Self::RetryWithAccount { .. } => "账户设置".to_owned(),
            Self::LoadSchedule => "课表".to_owned(),
            Self::LoadHomework => "作业".to_owned(),
            Self::LoadFlow { .. } => "考勤流水".to_owned(),
            Self::LoadCourses | Self::LoadActivities { .. } | Self::LoadActivityDetail { .. } => {
                "思源学堂".to_owned()
            }
            Self::ChangeAccount { .. } | Self::ChangePassphrase { .. } => "账户设置".to_owned(),
            Self::SetAccessPolicy(_) => "访问模式".to_owned(),
            Self::Shutdown => String::new(),
        }
    }
}

/// 背景回報的事件。
#[derive(Debug)]
pub enum Event {
    /// 保險庫已建立或解鎖，可以開始登入。
    VaultReady,
    /// 登入進度。
    LoginProgress(String),
    /// 需要圖片驗證碼（附圖檔路徑）。
    LoginNeedsCaptcha(PathBuf),
    /// 需要簡訊驗證碼。
    LoginNeedsMfa {
        /// 綁定手機號（中間遮蔽）。
        phone: Option<String>,
        /// 是否已發送驗證碼。
        sent: bool,
    },
    /// 登入失敗。
    LoginFailed(String),
    /// 登入成功。
    LoginSucceeded,
    /// 課表資料。
    Schedule(Box<ScheduleData>),
    /// 作業資料。
    Homework(Vec<HomeworkItem>),
    /// 考勤流水資料。
    Flow(Box<FlowData>),
    /// 課程列表。
    Courses(Vec<LmsCourse>),
    /// 課程活動列表。
    Activities(Vec<LmsActivity>),
    /// 活動詳情。
    ActivityDetail(Box<ActivityDetailView>),
    /// 帳號已更新。
    AccountUpdated,
    /// 加密口令已更新。
    PassphraseUpdated,
    /// 訪問策略已更新。
    AccessPolicyUpdated(AccessPolicy),
    /// 提示訊息（例如有資料因格式問題被跳過）。
    Notice(String),
    /// 任務失敗。
    Failed {
        /// 任務名稱。
        what: String,
        /// 錯誤訊息。
        message: String,
    },
}

/// 進行中的登入流程。
struct LoginFlow {
    site: SiteKind,
    driver: Box<LoginDriver>,
    retry: Option<Job>,
}

/// 等待登入成功後才寫入保險庫的憑證。
struct PendingVault {
    /// 加密口令（寫入後隨即丟棄）。
    passphrase: Zeroizing<String>,
    /// 使用者重新輸入的帳號密碼。
    credentials: Credentials,
}

/// 背景工作執行緒。
struct Worker {
    jobs: Receiver<Job>,
    events: Sender<Event>,
    vault: Vault,
    config: Config,
    session: Option<SessionManager>,
    credentials: Option<Credentials>,
    flow: Option<LoginFlow>,
    retry: Option<Job>,
    pending_vault: Option<PendingVault>,
}

/// 啟動背景工作執行緒，回傳（任務送出端, 事件接收端）。
pub fn spawn(config: Config, vault: Vault) -> AppResult<(Sender<Job>, Receiver<Event>)> {
    let (job_tx, job_rx) = channel();
    let (event_tx, event_rx) = channel();

    let mut worker = Worker {
        jobs: job_rx,
        events: event_tx,
        vault,
        config,
        session: None,
        credentials: None,
        flow: None,
        retry: None,
        pending_vault: None,
    };

    thread::Builder::new()
        .name("ohmyXJTU-worker".to_owned())
        .spawn(move || worker.run())
        .map_err(|err| AppError::config(format!("无法启动后台任务线程：{err}")))?;

    Ok((job_tx, event_rx))
}

impl Worker {
    fn run(&mut self) {
        while let Ok(job) = self.jobs.recv() {
            let shutdown = matches!(job, Job::Shutdown);
            let what = job.label();
            match self.dispatch(job) {
                Ok(()) => {}
                Err(err) => self.emit(Event::Failed {
                    what: if what.is_empty() {
                        "操作".to_owned()
                    } else {
                        what
                    },
                    message: err.to_string(),
                }),
            }
            if shutdown {
                return;
            }
        }
    }

    fn dispatch(&mut self, job: Job) -> AppResult<()> {
        match job {
            Job::CreateVault {
                passphrase,
                credentials,
            } => self.create_vault(&passphrase, credentials),
            Job::Unlock { passphrase } => self.unlock(&passphrase),
            Job::SubmitCaptcha(code) => self.submit_captcha(&code),
            Job::RefreshCaptcha => self.refresh_captcha(),
            Job::SendMfaCode => self.send_mfa_code(),
            Job::VerifyMfaCode(code) => self.verify_mfa_code(&code),
            Job::RetryLogin => self.retry_login(),
            Job::RetryWithAccount {
                credentials,
                passphrase,
            } => self.retry_with_account(&passphrase, credentials),
            Job::ChangeAccount {
                passphrase,
                credentials,
            } => self.change_account(&passphrase, credentials),
            Job::ChangePassphrase { old, new } => self.change_passphrase(&old, &new),
            Job::SetAccessPolicy(policy) => self.set_access_policy(policy),
            Job::Shutdown => Ok(()),
            data_job => self.load(data_job),
        }
    }

    // ── 憑證 ─────────────────────────────────────────────

    fn create_vault(&mut self, passphrase: &str, credentials: Credentials) -> AppResult<()> {
        self.vault.store(passphrase, &credentials)?;
        self.start_session(credentials)?;
        self.emit(Event::VaultReady);
        self.begin_login(SiteKind::Attendance, None)
    }

    fn unlock(&mut self, passphrase: &str) -> AppResult<()> {
        let credentials = self.vault.load(passphrase)?;
        self.start_session(credentials)?;
        self.emit(Event::VaultReady);
        self.begin_login(SiteKind::Attendance, None)
    }

    fn change_account(&mut self, passphrase: &str, credentials: Credentials) -> AppResult<()> {
        // 先以原口令解密，驗證口令正確（失敗會回報 [`AppError::WrongPassphrase`]）。
        self.vault.load(passphrase)?;
        self.vault.store(passphrase, &credentials)?;
        self.start_session(credentials)?;
        self.emit(Event::AccountUpdated);
        self.emit(Event::VaultReady);
        self.begin_login(SiteKind::Attendance, None)
    }

    fn change_passphrase(&mut self, old: &str, new: &str) -> AppResult<()> {
        self.vault.change_passphrase(old, new)?;
        self.emit(Event::PassphraseUpdated);
        Ok(())
    }

    fn start_session(&mut self, credentials: Credentials) -> AppResult<()> {
        let mut session = SessionManager::new(&self.config)?;
        session.register(Box::new(AttendanceSite));
        session.register(Box::new(LmsSite));
        session.set_credentials(credentials.clone());
        self.session = Some(session);
        self.credentials = Some(credentials);
        self.flow = None;
        self.retry = None;
        Ok(())
    }

    fn set_access_policy(&mut self, policy: AccessPolicy) -> AppResult<()> {
        self.config.access_policy = policy;
        self.config.save()?;
        if let Some(session) = self.session.as_mut() {
            session.set_access_policy(policy);
        }
        self.emit(Event::AccessPolicyUpdated(policy));
        self.begin_login(SiteKind::Attendance, None)
    }

    // ── 登入 ─────────────────────────────────────────────

    fn begin_login(&mut self, site: SiteKind, retry: Option<Job>) -> AppResult<()> {
        let credentials = self
            .credentials
            .clone()
            .ok_or_else(|| AppError::config("尚未解锁凭证"))?;
        self.emit(Event::LoginProgress(format!("正在登录{site}…")));
        // 重新開始登入時丟棄上一個（多半已失敗的）流程。
        self.flow = None;

        let stage = self.session_mut()?.next_login_step(site)?;
        self.drive(stage, site, credentials, retry)
    }

    fn drive(
        &mut self,
        stage: LoginStage,
        site: SiteKind,
        credentials: Credentials,
        retry: Option<Job>,
    ) -> AppResult<()> {
        match stage {
            LoginStage::Done => self.finish_login(site, retry),
            LoginStage::Drive(mut driver) => {
                let reply = driver.start(&credentials, AccountType::Undergraduate)?;
                self.flow = Some(LoginFlow {
                    site,
                    driver,
                    retry,
                });
                self.handle_reply(reply)
            }
        }
    }

    fn handle_reply(&mut self, reply: LoginReply) -> AppResult<()> {
        match reply {
            LoginReply::Success => self.complete_flow(),
            LoginReply::Fail { message } => {
                self.flow = None;
                // 憑證被拒：丟棄待存憑證，不覆蓋保險庫中的舊憑證。
                self.pending_vault = None;
                self.emit(Event::LoginFailed(message));
                Ok(())
            }
            LoginReply::NeedCaptcha => {
                let path = self.driver()?.fetch_captcha()?;
                self.emit(Event::LoginNeedsCaptcha(path));
                Ok(())
            }
            LoginReply::NeedMfa => {
                let phone = self.driver_mut()?.mfa_phone().ok();
                self.emit(Event::LoginNeedsMfa { phone, sent: false });
                Ok(())
            }
            LoginReply::NeedAccountChoice(_) => {
                // 本科身份由驅動器自動選擇；若選擇失敗會回報錯誤。
                let reply = self.driver_mut()?.resume()?;
                self.handle_reply(reply)
            }
        }
    }

    fn complete_flow(&mut self) -> AppResult<()> {
        let Some(flow) = self.flow.take() else {
            return Ok(());
        };
        let stage = self
            .session_mut()?
            .complete_login_step(flow.site, &flow.driver)?;
        let credentials = self
            .credentials
            .clone()
            .ok_or_else(|| AppError::config("尚未解锁凭证"))?;
        self.drive(stage, flow.site, credentials, flow.retry)
    }

    fn finish_login(&mut self, _site: SiteKind, retry: Option<Job>) -> AppResult<()> {
        // 登入成功後才更新保險庫，失敗的憑證不會覆蓋舊憑證。
        self.commit_pending_vault();
        self.emit(Event::LoginSucceeded);
        let job = retry.or(self.retry.take());
        match job {
            Some(job) => self.dispatch(job),
            None => Ok(()),
        }
    }

    /// 把等待中的憑證寫入保險庫；寫入失敗不影響已完成的登入。
    fn commit_pending_vault(&mut self) {
        let Some(pending) = self.pending_vault.take() else {
            return;
        };

        match self.vault.store(&pending.passphrase, &pending.credentials) {
            Ok(()) => self.emit(Event::AccountUpdated),
            Err(err) => self.emit(Event::Notice(format!("登录成功，但凭据保存失败：{err}"))),
        }
    }

    fn submit_captcha(&mut self, code: &str) -> AppResult<()> {
        let reply = self.driver_mut()?.submit_captcha(code)?;
        self.handle_reply(reply)
    }

    fn refresh_captcha(&mut self) -> AppResult<()> {
        let path = self.driver()?.fetch_captcha()?;
        self.emit(Event::LoginNeedsCaptcha(path));
        Ok(())
    }

    fn send_mfa_code(&mut self) -> AppResult<()> {
        let phone = self.driver_mut()?.send_mfa_code()?;
        self.emit(Event::LoginNeedsMfa {
            phone: Some(phone),
            sent: true,
        });
        Ok(())
    }

    fn verify_mfa_code(&mut self, code: &str) -> AppResult<()> {
        self.driver_mut()?.verify_mfa_code(code)?;
        let reply = self.driver_mut()?.resume()?;
        self.handle_reply(reply)
    }

    fn retry_login(&mut self) -> AppResult<()> {
        self.begin_login(SiteKind::Attendance, None)
    }

    /// 以使用者重新輸入的憑證重試登入；先驗證口令，登入成功後才寫入保險庫。
    fn retry_with_account(&mut self, passphrase: &str, credentials: Credentials) -> AppResult<()> {
        // 口令錯誤時回報 [`AppError::WrongPassphrase`]，舊憑證不受影響。
        self.vault.load(passphrase)?;

        self.session_mut()?.set_credentials(credentials.clone());
        self.credentials = Some(credentials.clone());
        self.pending_vault = Some(PendingVault {
            passphrase: Zeroizing::new(passphrase.to_owned()),
            credentials,
        });
        self.begin_login(SiteKind::Attendance, None)
    }

    fn driver(&self) -> AppResult<&LoginDriver> {
        self.flow
            .as_ref()
            .map(|flow| flow.driver.as_ref())
            .ok_or_else(|| AppError::config("当前没有进行中的登录流程"))
    }

    fn driver_mut(&mut self) -> AppResult<&mut LoginDriver> {
        self.flow
            .as_mut()
            .map(|flow| flow.driver.as_mut())
            .ok_or_else(|| AppError::config("当前没有进行中的登录流程"))
    }

    // ── 資料載入 ─────────────────────────────────────────

    fn load(&mut self, job: Job) -> AppResult<()> {
        let site = site_of(&job);
        let result = match &job {
            Job::LoadSchedule => self
                .load_schedule()
                .map(|data| Event::Schedule(Box::new(data))),
            Job::LoadHomework => self.load_homework().map(Event::Homework),
            Job::LoadFlow { page } => self
                .load_flow(*page)
                .map(|data| Event::Flow(Box::new(data))),
            Job::LoadCourses => self.load_courses().map(Event::Courses),
            Job::LoadActivities { course_id } => {
                self.load_activities(course_id).map(Event::Activities)
            }
            Job::LoadActivityDetail { activity_id } => self
                .load_activity_detail(activity_id)
                .map(|view| Event::ActivityDetail(Box::new(view))),
            _ => return Ok(()),
        };

        match result {
            Ok(event) => {
                self.emit(event);
                Ok(())
            }
            Err(err) if err.needs_relogin() => {
                let Some(site) = site else {
                    return Err(err);
                };
                // 登入態失效：記下任務，重新登入後自動重試。登入本身失敗時任務仍
                // 留在 `self.retry`，使用者重試成功後會自動續跑。
                self.retry = Some(job);
                self.begin_login(site, None)
            }
            Err(err) => Err(err),
        }
    }

    fn load_schedule(&mut self) -> AppResult<ScheduleData> {
        let session = self.session_mut()?;
        let mut api = AttendanceApi::new(session);

        let semester = api.current_semester()?;
        let courses = api.weekly_courses(&semester.semester_id)?;
        let today = Local::now().date_naive();
        let semester_start = parse_date(&semester.start_date)?;
        let term = semester.term_name();
        let week = schedule::clamp_week(schedule::week_number(semester_start, today), &term);

        let (monday, sunday) = schedule::week_window(today);
        let records = api.records_between(monday, sunday)?;

        let skipped = courses
            .iter()
            .filter(|course| schedule::parse_weeks(&course.week_ranges).is_empty())
            .count();

        let mut lessons: Vec<LessonEntry> = Vec::new();
        for slot in schedule::merge_courses(&courses)
            .into_iter()
            .filter(|slot| slot.is_in_week(week))
        {
            let Some(date) = slot.date_in_week(semester_start, week) else {
                continue;
            };
            let status = attendance_match::status_for(&slot, date, &records);
            let weeks = slot.weeks_label();
            lessons.push(LessonEntry {
                date,
                sections: format!("{}-{}", slot.start_section, slot.end_section),
                course_name: slot.course_name.clone(),
                classroom: slot.classroom.clone().unwrap_or_default(),
                teacher: slot.teacher.clone().unwrap_or_default(),
                weeks,
                status,
                label: attendance_match::display_label(status, date, today),
            });
        }
        lessons.sort_by(|left, right| {
            (left.date, left.sections.clone()).cmp(&(right.date, right.sections.clone()))
        });

        Ok(ScheduleData {
            semester: term,
            week,
            lessons,
            skipped,
        })
    }

    fn load_homework(&mut self) -> AppResult<Vec<HomeworkItem>> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);

        let (courses, mut skipped) = api.my_courses()?;
        let mut inputs: Vec<HomeworkInput> = Vec::new();

        for course in &courses {
            let (activities, skipped_activities) = api.course_activities(&course.id)?;
            skipped += skipped_activities;

            for activity in activities
                .iter()
                .filter(|activity| activity.kind() == ActivityKind::Homework)
            {
                // 取不到提交記錄時記為「無法確認」，不可誤判為未提交。
                let submission_count = api
                    .submissions(
                        &activity.id,
                        activity.submit_by_group.unwrap_or(false),
                        activity.group_id.as_deref(),
                    )
                    .ok()
                    .map(|list| list.count());

                inputs.push(HomeworkInput {
                    course_id: course.id.clone(),
                    course_name: course.name.clone(),
                    activity_id: activity.id.clone(),
                    title: activity.display_title(),
                    end_time: activity.end_time.clone(),
                    submit_by_group: activity.submit_by_group.unwrap_or(false),
                    submission_count,
                });
            }
        }

        let now = Local::now().fixed_offset();
        let mut items = homework::aggregate(&inputs, now);
        if skipped > 0 {
            // 有項目因格式問題被跳過時提示使用者，但不影響已彙總的結果。
            self.emit(Event::Notice(format!(
                "已跳过 {skipped} 项无法解析的思源学堂数据"
            )));
        }
        items.shrink_to_fit();
        Ok(items)
    }

    fn load_flow(&mut self, page: u32) -> AppResult<FlowData> {
        let session = self.session_mut()?;
        let mut api = AttendanceApi::new(session);
        let page_data = api.flow_page(page.max(1), FLOW_PAGE_SIZE)?;

        Ok(FlowData {
            total: page_data.total,
            total_pages: page_data.total_pages(),
            page: page_data.page,
            records: page_data.records,
        })
    }

    fn load_courses(&mut self) -> AppResult<Vec<LmsCourse>> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (courses, _) = api.my_courses()?;
        Ok(courses)
    }

    fn load_activities(&mut self, course_id: &str) -> AppResult<Vec<LmsActivity>> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (activities, _) = api.course_activities(course_id)?;
        Ok(activities)
    }

    fn load_activity_detail(&mut self, activity_id: &str) -> AppResult<ActivityDetailView> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.activity(activity_id)?;

        let (submissions, note) = match detail.submissions {
            Some(list) => (Some(list.list), None),
            None => (None, Some("无法确认提交状态（未取到提交记录）".to_owned())),
        };

        Ok(ActivityDetailView {
            title: detail.activity.display_title(),
            kind: detail.activity.kind().label().to_owned(),
            end_time: detail.activity.end_time,
            submit_by_group: detail.activity.submit_by_group.unwrap_or(false),
            submissions,
            note,
        })
    }

    // ── 工具 ─────────────────────────────────────────────

    fn session_mut(&mut self) -> AppResult<&mut SessionManager> {
        self.session
            .as_mut()
            .ok_or_else(|| AppError::config("会话尚未建立，请先解锁凭证"))
    }

    fn emit(&self, event: Event) {
        // 介面若已結束，事件自然被丟棄。
        let _ = self.events.send(event);
    }
}

/// 任務所屬站點。
fn site_of(job: &Job) -> Option<SiteKind> {
    match job {
        Job::LoadSchedule | Job::LoadFlow { .. } => Some(SiteKind::Attendance),
        Job::LoadHomework
        | Job::LoadCourses
        | Job::LoadActivities { .. }
        | Job::LoadActivityDetail { .. } => Some(SiteKind::Lms),
        _ => None,
    }
}

fn parse_date(value: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|err| AppError::protocol(format!("学期开始日期无法解析（{value}）：{err}")))
}

#[cfg(test)]
#[path = "tests/worker_test.rs"]
mod worker_test;
