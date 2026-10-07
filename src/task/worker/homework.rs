//! 作業載入：步進狀態機與進度回報。
//!
//! `HomeworkRunner` 一次推進一格（課程 → 活動 → 提交摘要），每一步之間由
//! 調度核心插入控制任務；每項作業完成後節流地回報部分結果。學期判定
//!（明確選擇 → 考勤當前學期 → 記憶 → 選擇器）與課程過濾也在此。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::Local;

use crate::domain::homework::{self, HomeworkInput};
use crate::domain::semester::{self, TermCode, TermResolution, TermSource};
use crate::error::{AppError, AppResult};
use crate::session::SiteKind;
use crate::sites::lms::{ActivityKind, LmsActivity, LmsCourse, submission_failure_note};
use crate::task::protocol::{Event, FailedTarget, HomeworkIssue, HomeworkUpdate, Job};

use super::cache::{ActivitiesOutcome, DetailOutcome};
use super::timing::Phase;
use super::{SiteFailure, Worker, is_recoverable};

/// 作業載入進度事件的節流間隔。
///
/// 每完成這麼多項作業（或每門課程結束）才送出一次完整快照，避免大型學期
/// 逐項重算彙總造成 O(N²) 成本與無界的事件佇列。
const PROGRESS_EMIT_INTERVAL: usize = 10;

/// 每次併發抓取的課程數。
///
/// 取傳輸層的併發上限：一批就是一個往返波次。批次之間仍會回到步進迴圈
///（排空控制任務），因此這個值也決定了「控制任務最多會被擋多久」。
const ACTIVITY_BATCH: usize = crate::http::batch::MAX_CONCURRENT_REQUESTS;

/// 每次補齊作業詳情的數量上限（同樣是一個往返波次）。
///
/// 詳情的總數隨作業數量成長，**不能**一次全部送出：那會讓「一門課含多項
/// 作業」時的等待隨波次累加，期間無法處理控制任務，也無法輸出首批結果。
const DETAIL_BATCH: usize = crate::http::batch::MAX_CONCURRENT_REQUESTS;

/// 作業載入的步進階段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeworkStage {
    /// 載入目前課程的作業活動。
    Activities,
    /// 載入目前活動的提交摘要。
    Submission,
    /// 切換到下一門課程。
    AdvanceCourse,
    /// 全部完成。
    Done,
}

/// 單次作業載入的計量基準：開始時間與請求計數。
///
/// 兩者在載入開始時取得、僅供完成訊息統計使用，合併為一個值避免四處傳遞。
#[derive(Debug, Clone, Copy)]
struct LoadMeter {
    /// 載入開始時間。
    started: Instant,
    /// 載入開始前已送出的請求數。
    requests_baseline: usize,
}

/// 作業載入的步進狀態。
struct HomeworkRunner {
    /// 目標學期。
    term: TermCode,
    /// 學期判定來源。
    term_source: TermSource,
    /// 納入查詢的課程。
    courses: Vec<LmsCourse>,
    /// 缺少學期資訊而未納入的課程數。
    skipped_terms: usize,
    /// 可選學期（供介面顯示選擇器）。
    term_options: Vec<TermCode>,
    /// 下一門課程的索引。
    course_index: usize,
    /// 目前課程的作業活動。
    activities: Vec<LmsActivity>,
    /// 已併發預取、等待取用的課程活動（鍵為課程索引；已過濾為作業）。
    prefetched: HashMap<usize, ActivitiesOutcome>,
    /// 已併發預取、等待取用的活動詳情（鍵為活動識別碼）。
    prefetched_details: HashMap<String, DetailOutcome>,
    /// 佇列中尚未預取詳情的作業識別碼（依處理順序），每步最多補一批。
    pending_details: VecDeque<String>,
    /// 下一個活動的索引。
    activity_index: usize,
    /// 已彙總的輸入。
    inputs: Vec<HomeworkInput>,
    /// 活動列表查詢失敗而略過的課程數。
    failed_courses: usize,
    /// 距離上次發出進度事件以來完成的作業數（用於節流）。
    since_emit: usize,
    /// 目前階段。
    stage: HomeworkStage,
    /// 是否略過快取。
    force: bool,
    /// 本次載入開始時間（統計用）。
    started: Instant,
    /// 本次載入開始前的請求計數（統計用）。
    requests_baseline: usize,
}

impl HomeworkRunner {
    /// 建立載入狀態：計數與索引歸零，初始階段依課程數決定。
    fn new(
        term: TermCode,
        term_source: TermSource,
        courses: Vec<LmsCourse>,
        skipped_terms: usize,
        term_options: Vec<TermCode>,
        force: bool,
        meter: LoadMeter,
    ) -> Self {
        let stage = if courses.is_empty() {
            HomeworkStage::Done
        } else {
            HomeworkStage::Activities
        };
        Self {
            term,
            term_source,
            courses,
            skipped_terms,
            term_options,
            course_index: 0,
            activities: Vec::new(),
            prefetched: HashMap::new(),
            prefetched_details: HashMap::new(),
            pending_details: VecDeque::new(),
            activity_index: 0,
            inputs: Vec::new(),
            failed_courses: 0,
            since_emit: 0,
            stage,
            force,
            started: meter.started,
            requests_baseline: meter.requests_baseline,
        }
    }

    /// 目前課程。
    fn current_course(&self) -> Option<&LmsCourse> {
        self.courses.get(self.course_index)
    }

    /// 目前的載入更新（完成時 `progress` 為 `None`）。
    fn update(&self, elapsed: Duration, requests: usize) -> HomeworkUpdate {
        let now = Local::now().fixed_offset();
        let progress = if matches!(self.stage, HomeworkStage::Done) {
            None
        } else {
            Some((
                self.course_index.min(self.courses.len()),
                self.courses.len(),
            ))
        };
        HomeworkUpdate {
            term_label: Some(self.term.label()),
            term_source: Some(self.term_source),
            courses_included: self.courses.len(),
            courses_skipped: self.skipped_terms,
            term_options: self.term_options.clone(),
            items: homework::aggregate(&self.inputs, now),
            issues: homework_issues(&self.inputs),
            courses_failed: self.failed_courses,
            progress,
            elapsed,
            requests,
        }
    }
}

/// 只保留作業類型的活動（其餘類型在作業頁沒有用途）。
fn homework_only(activities: Vec<LmsActivity>) -> Vec<LmsActivity> {
    activities
        .into_iter()
        .filter(|activity| activity.kind() == ActivityKind::Homework)
        .collect()
}

/// 彙總「待核实」作業的共同原因（同一原因只列一次，依項數遞減再按文字排序）。
fn homework_issues(inputs: &[HomeworkInput]) -> Vec<HomeworkIssue> {
    let mut grouped: Vec<HomeworkIssue> = Vec::new();
    for input in inputs
        .iter()
        .filter(|input| input.submission_count.is_none())
    {
        let reason = input
            .note
            .clone()
            .unwrap_or_else(|| "无法确认提交状态".to_owned());
        match grouped.iter_mut().find(|issue| issue.reason == reason) {
            Some(issue) => issue.count += 1,
            None => grouped.push(HomeworkIssue { reason, count: 1 }),
        }
    }
    grouped.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    grouped
}

impl Worker {
    /// 記住使用者選擇的學期，並立即重新載入作業。
    pub(super) fn set_homework_term(&mut self, term: &str) -> AppResult<()> {
        let term = TermCode::parse(term)
            .ok_or_else(|| AppError::protocol(format!("学期格式无法识别：{term}")))?;
        // 寫入失敗時保留原設定：否則記憶體中的「記住的學期」會與檔案不一致，
        // 課程分區與學期來源説明都會採用一個沒有真的保存下來的值（作法與
        // `set_access_policy`／`accept_agreement` 的 `previous` 回滾一致）。
        let previous = self.config.homework_term.clone();
        self.config.homework_term = Some(term.to_string());
        if let Err(err) = self.config.save() {
            self.config.homework_term = previous;
            return Err(err);
        }
        // 本次明確選擇：接下來的載入以它為準，不再被考勤的當前學期蓋過。
        self.chosen_term = Some(term);
        // 課程清單的分區以「當前學期」為準：學期改了要同步給介面，否則作業
        // 已切到所選學期，回到思源學堂仍按舊學期分區（要手動刷新才會更新）。
        self.emit(Event::CoursesTerm(Some(term)));
        self.emit(Event::Notice(format!("已记住学期 {}", term.label())));
        // 學期已變更：使進行中的作業載入失效（它基於舊學期），並確保佇列中
        // 恰有一筆強制重載（忽略執行中任務），切換才會立即生效。
        self.homework_epoch += 1;
        self.merge_data_job(Job::LoadHomework { force: true }, None);
        Ok(())
    }

    /// 作業載入（步進執行：每門課程、每項作業之間先處理控制任務）。
    pub(super) fn run_homework_job(&mut self, force: bool) {
        let generation = self.generation;
        let epoch = self.homework_epoch;
        let job = Job::LoadHomework { force };

        let mut runner = match self.begin_homework(force) {
            Ok(Some(runner)) => runner,
            // 沒有課程或已改為等待選擇學期：本次載入到此為止。
            Ok(None) => {
                self.finish_timing();
                return;
            }
            Err(failure) => {
                self.report_data_failure(failure.site, job, generation, failure.err);
                return;
            }
        };

        loop {
            if !self.drain_channel(&job) {
                // 收到結束指令：不再輸出報告。
                self.timing.abandon();
                return;
            }
            if self.homework_cancelled(generation, epoch) {
                self.timing.abandon();
                return;
            }
            // 互動式資料任務（按 `o` 開啟網頁）不應等待整輪載入：在下一個
            // 步進邊界立即執行。執行期間可能同步完成重新登入——登入成功後的
            // 內層重試會排空通道，可能處理換帳號／換學期指令（代際或學期在
            // 這裡被改動）；返回後必須重新驗證，否則會以舊帳號或舊學期的
            // 資料繼續載入並回填畫面。
            let login_pending = self.flush_interactive();
            if self.homework_cancelled(generation, epoch) {
                self.timing.abandon();
                return;
            }
            if login_pending {
                // 觸發了互動式登入：暫停本輪載入並重新排隊——登入完成（或
                // 取消）後會再跑一次，已取得的資料多在快取中。計時不結束，
                // 登入時間才會計入同一次使用者請求。
                self.merge_data_job(job, None);
                return;
            }
            if matches!(runner.stage, HomeworkStage::Done) {
                self.emit_homework(&runner);
                self.finish_timing();
                return;
            }
            if let Err(err) = self.homework_step(&mut runner) {
                // 步進階段只會向思源學堂查詢。
                self.report_data_failure(SiteKind::Lms, job, generation, err);
                return;
            }
        }
    }

    /// 本輪作業載入是否已失效（換帳號、切換模式、學期切換或收到結束指令）。
    ///
    /// 換帳號／切換模式與學期切換會一併回報事件收斂介面（結束指令除外：
    /// 程式正在退出，介面不需要收斂事件）；回傳 `true` 代表呼叫端必須停止
    /// 本輪，且不得重新排隊。
    fn homework_cancelled(&mut self, generation: u64, epoch: u64) -> bool {
        if self.shutdown {
            return true;
        }
        if generation != self.generation {
            self.emit(Event::Notice(
                "账号或访问模式已变更，已取消进行中的作业加载".to_owned(),
            ));
            self.emit(Event::LoadingCancelled {
                target: FailedTarget::Homework,
            });
            return true;
        }
        if epoch != self.homework_epoch {
            // 學期已切換：本輪基於舊學期，停止並讓已排入的強制重載接手，
            // 避免舊學期的進度與完成結果繼續回填畫面。
            self.emit(Event::LoadingCancelled {
                target: FailedTarget::Homework,
            });
            return true;
        }
        false
    }

    /// 準備作業載入：載入課程、判定學期、過濾課程並回報首批進度。
    ///
    /// 回傳 `None` 代表本次載入不需（也無法）進入步進階段：沒有課程、已改為
    /// 等待使用者選擇學期，或已回報失敗。
    fn begin_homework(&mut self, force: bool) -> Result<Option<HomeworkRunner>, SiteFailure> {
        let meter = LoadMeter {
            started: Instant::now(),
            requests_baseline: self.request_count(),
        };

        let (courses, skipped_data) = {
            let started = self.timing.mark();
            let courses = self.lms_courses(force);
            self.timing.record(Phase::Courses, started, 1);
            courses?
        };
        if skipped_data > 0 {
            self.emit(Event::Warning(format!(
                "已跳过 {skipped_data} 项无法解析的思源学堂数据"
            )));
        }
        // 沒有任何課程時無從（也無需）判定學期：直接回報空結果。
        if courses.is_empty() {
            self.emit_empty_homework(meter);
            return Ok(None);
        }

        let Some((term, term_source)) = ({
            let started = self.timing.mark();
            let resolved = self.homework_term(&courses);
            self.timing.record(Phase::Term, started, 1);
            resolved?
        }) else {
            return Ok(None);
        };
        let term_options = semester::term_options(&courses, term);
        let (included, skipped_terms) = semester::courses_for_term(courses, term);

        let runner = HomeworkRunner::new(
            term,
            term_source,
            included,
            skipped_terms,
            term_options,
            force,
            meter,
        );
        self.emit_homework(&runner);
        Ok(Some(runner))
    }

    /// 判定要載入的學期；`Ok(None)` 代表已發出學期選擇事件，等待使用者決定。
    fn homework_term(
        &mut self,
        courses: &[LmsCourse],
    ) -> Result<Option<(TermCode, TermSource)>, SiteFailure> {
        let chosen = self.chosen_term;
        let remembered = self
            .config
            .homework_term
            .as_deref()
            .and_then(TermCode::parse);
        let today = Local::now().date_naive();

        // 使用者已明確選定學期時，不必（也不應）再查考勤：他指定的學期就是
        // 答案，考勤的登入狀態（過期、逾時）不該讓作業查詢跟著失敗。
        //
        // 其餘情況才向考勤取權威學期。考勤的失敗必須標成考勤站點：否則會用
        // `Job::LoadHomework` 推得思源學堂，重登之後還是會失敗，錯誤訊息也
        // 指向錯的站點；但只有「登入態已失效」值得中斷整批作業查詢——重登
        // 之後就能取回權威的學期，單純的連線層錯誤則降級為「考勤不可用」，
        // 免得考勤的暫時故障連帶拖垮本來可用的思源學堂。
        let (attendance_term, attendance_error) = if chosen.is_some() {
            (None, None)
        } else {
            match self.attendance_term() {
                Ok(term) => (term, None),
                Err(err) if err.needs_relogin() => return Err(SiteFailure::attendance(err)),
                Err(err) => (None, Some(err)),
            }
        };

        let (term, term_source) =
            match semester::resolve_term(chosen, attendance_term, remembered, today) {
                TermResolution::Resolved { term, source } => (term, source),
                TermResolution::NeedsChoice { suggestion } => {
                    self.emit(Event::HomeworkNeedsTerm {
                        options: semester::course_terms(courses),
                        suggestion,
                        reason: "无法自动判定当前学期：考勤系统不可用，且没有选择或记住的学期。"
                            .to_owned(),
                    });
                    return Ok(None);
                }
            };
        // 考勤故障但仍在其他來源下繼續：明確告知使用者學期是從何而來的，
        // 否則他會以為看到的就是考勤認定的本學期。
        if let Some(err) = attendance_error {
            self.emit(Event::Warning(format!(
                "考勤系统暂时不可用（{err}），本学期改用{}判定",
                term_source.label()
            )));
        }
        Ok(Some((term, term_source)))
    }

    /// 沒有課程可查時的空結果（不進入步進階段）。
    fn emit_empty_homework(&self, meter: LoadMeter) {
        self.emit(Event::Homework(HomeworkUpdate {
            term_label: None,
            term_source: None,
            courses_included: 0,
            courses_skipped: 0,
            term_options: Vec::new(),
            items: Vec::new(),
            issues: Vec::new(),
            courses_failed: 0,
            progress: None,
            elapsed: meter.started.elapsed(),
            requests: self.request_count().saturating_sub(meter.requests_baseline),
        }));
    }

    /// 推進一格作業載入。
    fn homework_step(&mut self, runner: &mut HomeworkRunner) -> AppResult<()> {
        match runner.stage {
            HomeworkStage::Activities => {
                runner
                    .current_course()
                    .ok_or_else(|| AppError::protocol("课程索引越界"))?;
                match self.course_activities_step(runner)? {
                    Ok((activities, skipped)) => {
                        if skipped > 0 {
                            self.emit(Event::Warning(format!(
                                "已跳过 {skipped} 项无法解析的思源学堂数据"
                            )));
                        }
                        // 預取時已過濾為作業類型。
                        runner.activities = activities;
                        runner.activity_index = 0;
                        runner.stage = if runner.activities.is_empty() {
                            HomeworkStage::AdvanceCourse
                        } else {
                            HomeworkStage::Submission
                        };
                    }
                    // 登入態失效與連線層錯誤向上傳播（重登、路由回退或最終失敗）。
                    Err(err) if !is_recoverable(&err) => return Err(err),
                    // 單門課程的活動列表失敗：略過該課程，其餘課程照常載入，
                    // 並在彙總中以數量提示（不將整批標為失敗）。
                    Err(_) => {
                        runner.failed_courses += 1;
                        runner.stage = HomeworkStage::AdvanceCourse;
                    }
                }
            }
            HomeworkStage::Submission => {
                let activity = runner
                    .activities
                    .get(runner.activity_index)
                    .cloned()
                    .ok_or_else(|| AppError::protocol("活动索引越界"))?;
                let course = runner
                    .current_course()
                    .cloned()
                    .ok_or_else(|| AppError::protocol("课程索引越界"))?;
                // 詳情多已在同一批課程預取時一併抓回；尚未補齊時只補一個波次。
                if !runner.prefetched_details.contains_key(&activity.id) {
                    self.prefetch_details(runner)?;
                }
                let detail = match runner.prefetched_details.remove(&activity.id) {
                    Some(detail) => detail,
                    None => {
                        // 防禦：預取遺漏（例如活動識別碼在批次後才出現）時
                        // 就地補抓一筆，不讓作業無故變成「待核实」。
                        let started = self.timing.mark();
                        let detail = self.lms_activity_detail(&activity.id, runner.force);
                        self.timing.record(Phase::Detail, started, 1);
                        detail
                    }
                };
                let input = self.homework_input(&course, &activity, detail, runner.force)?;
                runner.inputs.push(input);
                self.timing.note_first_item();

                runner.activity_index += 1;
                runner.since_emit += 1;
                let finished_course = runner.activity_index >= runner.activities.len();
                if finished_course {
                    // 整門課程結束：由 `AdvanceCourse` 統一發出一次進度事件。
                    runner.stage = HomeworkStage::AdvanceCourse;
                } else if runner.since_emit >= PROGRESS_EMIT_INTERVAL {
                    // 課程尚未結束但已累積足夠作業：節流地補一次進度。
                    runner.since_emit = 0;
                    self.emit_homework(runner);
                }
            }
            HomeworkStage::AdvanceCourse => {
                runner.course_index += 1;
                runner.activities.clear();
                runner.activity_index = 0;
                runner.since_emit = 0;
                runner.stage = if runner.course_index >= runner.courses.len() {
                    HomeworkStage::Done
                } else {
                    HomeworkStage::Activities
                };
                self.emit_homework(runner);
            }
            HomeworkStage::Done => {}
        }
        Ok(())
    }

    /// 取得目前課程的活動：優先取用預取結果，否則併發抓取後續數門課程。
    ///
    /// 一次抓取 [`ACTIVITY_BATCH`] 門課程並放進 [`HomeworkRunner::prefetched`]，
    /// 之後每一步直接取用；同一批課程的**作業詳情**也在這裡一併預取（放進
    /// [`HomeworkRunner::prefetched_details`]），提交階段只負責算提交摘要。
    /// 步進的節奏（每一步之間排空控制任務）因此不變，但序列往返的波次少了
    /// 好幾倍；批次內單門課程失敗仍只影響那一門。
    fn course_activities_step(
        &mut self,
        runner: &mut HomeworkRunner,
    ) -> AppResult<ActivitiesOutcome> {
        if let Some(result) = runner.prefetched.remove(&runner.course_index) {
            return Ok(result);
        }
        let start = runner.course_index;
        let course_ids: Vec<String> = runner
            .courses
            .iter()
            .skip(start)
            .take(ACTIVITY_BATCH)
            .map(|course| course.id.clone())
            .collect();

        let started = self.timing.mark();
        let results = self.lms_activities_batch(&course_ids, runner.force)?;
        self.timing
            .record(Phase::Activities, started, results.len());

        // 預取時就只留作業，並把整批課程的作業收進同一個待抓佇列：每批詳情
        // 才湊得滿一個波次，不會因為「每門課只有一兩項作業」而一批只送一個
        // 請求（詳情不可只在單門課程內批次）。
        let results: Vec<ActivitiesOutcome> = results
            .into_iter()
            .map(|result| result.map(|(activities, skipped)| (homework_only(activities), skipped)))
            .collect();
        for result in &results {
            let Ok((activities, _)) = result else {
                continue;
            };
            // 診斷用：統計列表本身就帶了哪些提交欄位。若幾乎都有值，就能
            // 靠列表判定提交狀態、只在開啟說明時才抓詳情。
            for activity in activities {
                self.timing.note_list_item(
                    activity.submit_by_group.is_some(),
                    activity.user_submit_count.is_some(),
                    activity.group_id.is_some(),
                );
            }
        }

        // 詳情只先排入待抓佇列、送出一個波次；其餘由後續步進補齊。一次把整批
        // 課程的詳情全部送出去，會讓「一門課含多項作業」時的等待隨波次累加，
        // 期間（也是首批結果出來之前）無法處理控制任務。
        runner.pending_details.extend(
            results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .flat_map(|(activities, _)| activities.iter().map(|activity| activity.id.clone())),
        );
        self.prefetch_details(runner)?;

        let mut results = results.into_iter();
        let current = results
            .next()
            .unwrap_or_else(|| Err(AppError::protocol("课程活动批次为空")));
        for (offset, result) in results.enumerate() {
            runner.prefetched.insert(start + offset + 1, result);
        }
        Ok(current)
    }

    /// 補齊尚未預取的作業詳情：一次最多送出一個波次。
    ///
    /// 每次呼叫只佔用工作執行緒一次往返的時間，之後就回到步進迴圈排空控制
    /// 任務並檢查取消狀態；剩下的詳情由後續步進繼續補齊。取過的識別碼會
    /// 離開佇列，因此不會重複請求。
    fn prefetch_details(&mut self, runner: &mut HomeworkRunner) -> AppResult<()> {
        let count = runner.pending_details.len().min(DETAIL_BATCH);
        if count == 0 {
            return Ok(());
        }
        let ids: Vec<String> = runner.pending_details.iter().take(count).cloned().collect();
        let started = self.timing.mark();
        let details = self.lms_detail_batch(&ids, runner.force)?;
        self.timing.record(Phase::Detail, started, details.len());
        for _ in 0..count {
            runner.pending_details.pop_front();
        }
        for (activity_id, detail) in ids.into_iter().zip(details) {
            runner.prefetched_details.insert(activity_id, detail);
        }
        Ok(())
    }

    /// 取得單一作業的提交摘要（詳情已由批次預取提供）。
    fn homework_input(
        &mut self,
        course: &LmsCourse,
        activity: &LmsActivity,
        detail: DetailOutcome,
        force: bool,
    ) -> AppResult<HomeworkInput> {
        let mut input = HomeworkInput {
            course_id: course.id.clone(),
            course_name: course.name.clone(),
            activity_id: activity.id.clone(),
            title: activity.display_title(),
            end_time: activity.end_time.clone(),
            // 簡要列表常缺少此欄位：先原樣保留，登入成功後由提交摘要覆寫；
            // 摘要失敗時維持 `None`（無法確認），不得預設為個人作業。
            submit_by_group: activity.submit_by_group,
            description: None,
            submission_count: None,
            note: None,
        };

        let detail = match detail {
            Ok(detail) => detail,
            // 登入態失效與連線層錯誤向上傳播；其他單項失敗保留「待核实」。
            Err(err) if !is_recoverable(&err) => return Err(err),
            Err(err) => {
                input.note = Some(submission_failure_note(&err));
                return Ok(input);
            }
        };

        match self.lms_submission_summary_from_detail(&activity.id, &detail, force) {
            Ok(summary) => {
                input.submit_by_group = summary.submit_by_group;
                input.submission_count = summary.count;
                input.note = summary.note;
                input.description = summary.description;
            }
            // 登入態失效與連線層錯誤向上傳播；其他單項失敗保留「待核实」。
            Err(err) if !is_recoverable(&err) => return Err(err),
            Err(err) => input.note = Some(submission_failure_note(&err)),
        }
        Ok(input)
    }

    /// 發送一次作業載入更新（附耗時與本次載入已送出的請求數）。
    fn emit_homework(&mut self, runner: &HomeworkRunner) {
        if !runner.inputs.is_empty() {
            self.timing.note_first_shown();
        }
        let update = runner.update(
            runner.started.elapsed(),
            self.request_count()
                .saturating_sub(runner.requests_baseline),
        );
        self.emit(Event::Homework(update));
    }
}
