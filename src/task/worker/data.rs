//! 校內資料查詢：課表、考勤流水與思源學堂瀏覽。
//!
//! 這裡的每個 `load_*` 是 `super::worker` 調度核心單步執行的實際工作：向站點
//! API 取資料並轉成介面模型。學期判定所需的考勤查詢（`Worker::attendance_term`）
//! 也在這裡，供作業載入共用。

use chrono::{Local, NaiveDate};

use crate::domain::semester::TermCode;
use crate::domain::{attendance_match, schedule};
use crate::error::{AppError, AppResult};
use crate::model::{ActivityDetailView, FlowData, LessonEntry, ScheduleData};
use crate::session::SiteKind;
use crate::sites::attendance::{AttendanceApi, RecordBatch};
use crate::sites::lms::{self, ActivityKind, LmsActivity, LmsApi};
use crate::task::protocol::{CoursesData, Event, Job};

use super::cache::ScheduleCache;
use super::{Worker, is_recoverable};

/// 考勤流水分頁大小。
const FLOW_PAGE_SIZE: u32 = 20;

impl Worker {
    /// 課表（指定週次；未指定時為當前週）與該週考勤。
    ///
    /// 課表端點一次回傳整學期課程（`weekRanges`），本週只是客戶端過濾的結果；
    /// 因此整學期課程保存在 [`ScheduleCache`]，切換週次只需重查該週的考勤記錄。
    /// `force`（使用者按 `r`）或快取不存在時重新查詢學期與課表。
    ///
    /// 學期已結束／尚未開始時，**自動載入**（未指定週次）不查課表與考勤記錄，
    /// 只顯示空狀態與提示；使用者明確切到某一週時才補查該週
    ///（見 [`Worker::set_schedule_week`]）。
    pub(super) fn load_schedule(&mut self, force: bool) -> AppResult<ScheduleData> {
        let today = Local::now().date_naive();
        // 「自動」＝跟隨當前週（使用者尚未切週）：學期外的早退只適用於它。
        let auto = self.schedule_week.is_none();

        if force || self.schedule_cache.is_none() {
            let semester = {
                let session = self.session_mut()?;
                AttendanceApi::new(session).current_semester()?
            };
            let start = parse_date(&semester.start_date)?;
            // `end_date` 為選填：缺失或無法解析時不啟用「已結束」判斷（不讓整個
            // 頁面因此失敗），但可解析時一定以它為準。
            let end = semester
                .end_date
                .as_deref()
                .and_then(|raw| NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok());
            let term = semester.term_name();
            let label = semester.display_label();
            // 記錄本會話得知的學期，供思源學堂課程分區使用（不重複查詢考勤）。
            self.known_term = term.as_deref().and_then(TermCode::parse);

            // 學期外（已結束／尚未開始）不查課表與考勤記錄，只顯示空狀態與
            // 提示：夾取週次會讓學期結束後仍顯示最後一週的舊課程，而考勤只
            // 涵蓋當前日曆週，結果必然全部「待核实」——不得如此呈現。
            //（使用者明確切週時不在此列：他就是要看那一週。）
            if auto {
                let term_name = term.as_deref().unwrap_or("");
                if let Some(end_date) = end.filter(|end| today > *end) {
                    return Ok(off_session_schedule(
                        &label,
                        schedule::clamp_week(schedule::week_number(start, today), term_name),
                        schedule::semester_length(term_name),
                        format!("本学期已结束（{end_date}）"),
                    ));
                }
                if today < start {
                    return Ok(off_session_schedule(
                        &label,
                        1,
                        schedule::semester_length(term_name),
                        format!("本学期尚未开始（{start}）"),
                    ));
                }
            }

            let courses = {
                let session = self.session_mut()?;
                AttendanceApi::new(session).weekly_courses(&semester.semester_id)?
            };
            let slots = schedule::merge_courses(&courses);
            let skipped = courses
                .iter()
                .filter(|course| schedule::parse_weeks(&course.week_ranges).is_empty())
                .count();
            let max_week = slots
                .iter()
                .flat_map(|slot| slot.weeks.iter().copied())
                .max();
            self.schedule_cache = Some(ScheduleCache {
                label,
                start,
                slots,
                skipped,
                max_week,
            });
        }

        let cache = self
            .schedule_cache
            .clone()
            .ok_or_else(|| AppError::config("课表缓存尚未建立"))?;
        // 上限的「至少涵蓋」對象是**今天**的週次，不是使用者選定的週次：否則
        // 往回翻週會讓上限一起變小，考試週往回翻就再也回不到本週。
        let today_week = schedule::week_number(cache.start, today);
        let week = self.schedule_week.unwrap_or(today_week);
        let total = schedule::total_weeks(cache.max_week, today_week);
        let Some((monday, sunday)) = schedule::week_bounds(cache.start, week) else {
            return Err(AppError::protocol("周次超出可表示的日期范围"));
        };

        let RecordBatch { records, truncated } = {
            let session = self.session_mut()?;
            AttendanceApi::new(session).records_between(monday, sunday)?
        };

        let mut lessons: Vec<LessonEntry> = Vec::new();
        for slot in cache.slots.iter().filter(|slot| slot.is_in_week(week)) {
            let Some(date) = slot.date_in_week(cache.start, week) else {
                continue;
            };
            let status = attendance_match::status_for(slot, date, &records);
            let weeks = slot.weeks_label();
            lessons.push(LessonEntry {
                date,
                sections: format!("{}-{}", slot.start_section, slot.end_section),
                start_section: slot.start_section,
                end_section: slot.end_section,
                course_name: slot.course_name.clone(),
                classroom: slot.classroom.clone().unwrap_or_default(),
                teacher: slot.teacher.clone().unwrap_or_default(),
                weeks,
                attendance: attendance_match::display_state(status, date, today),
            });
        }
        // 同日課程按節次先後排序：以數值比較（`sections` 是顯示字串，字典序
        // 會讓「11-12」排到「3-4」之前）；`sort_by_key` 穩定，同鍵維持
        // `merge_courses` 的課名順序。
        lessons.sort_by_key(|lesson| (lesson.date, lesson.start_section, lesson.end_section));

        let notice = truncated.then(|| {
            format!(
                "考勤记录超过分页上限，仅比对前 {} 条，部分课程可能显示「待核实」",
                records.len()
            )
        });

        Ok(ScheduleData {
            semester: cache.label,
            week,
            total_weeks: total,
            lessons,
            skipped: cache.skipped,
            notice,
        })
    }

    /// 記住使用者選擇的週次並重新載入課表（`[`／`]`）。
    ///
    /// 週次保存在工作者狀態（如同作業的學期選擇）：任務本身只是觸發，因此
    /// 佇列中至多保留一筆載入，連續切週只會執行最後一週（新值在該筆執行時
    /// 才被讀取）。
    ///
    /// 進行中的載入**不會**被作廢：單步任務只在執行前排空控制任務，因此切週
    /// 指令要等該筆載入回報後才生效。使用者在載入途中又按了 `[`／`]` 時，介面
    /// 會先收到前一週（真實資料）再收到最後選定的一週；最終狀態必為最後選定
    /// 的週次。
    pub(super) fn set_schedule_week(&mut self, week: u32) -> AppResult<()> {
        if self.schedule_week == Some(week) {
            return Ok(());
        }
        self.schedule_week = Some(week);
        self.merge_data_job(Job::LoadSchedule { force: false }, None);
        Ok(())
    }

    /// 嘗試由考勤系統取得當前學期；未登入時回傳 `Ok(None)`（不觸發登入）。
    ///
    /// 已登入但遇到登入態失效或連線層錯誤時向上傳播（交由呼叫端走統一重登），
    /// 不再靜默降級為「沒有考勤學期」——否則作業清單會悄悄退回記憶中的舊學期，
    /// 使用者看不到登入已過期。
    pub(super) fn attendance_term(&mut self) -> AppResult<Option<TermCode>> {
        let term = {
            let Some(session) = self.session.as_mut() else {
                return Ok(None);
            };
            if !session.is_logged_in(SiteKind::Attendance) {
                return Ok(None);
            }
            let mut api = AttendanceApi::new(session);
            match api.current_semester() {
                // 名稱無法識別或格式不符：可恢復，交由後續來源決定。
                Ok(semester) => {
                    match semester.term_name().and_then(|name| TermCode::parse(&name)) {
                        Some(term) => term,
                        None => return Ok(None),
                    }
                }
                Err(err) if is_recoverable(&err) => return Ok(None),
                Err(err) => return Err(err),
            }
        };
        // 記住本會話得知的學期，供思源學堂課程分區使用（不重複請求）。
        self.known_term = Some(term);
        Ok(Some(term))
    }

    /// 考勤流水（一頁）。
    pub(super) fn load_flow(&mut self, page: u32) -> AppResult<FlowData> {
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

    /// 思源學堂課程清單（附當前學期提示）。
    pub(super) fn load_courses(&mut self, force: bool) -> AppResult<CoursesData> {
        let (courses, _) = self.lms_courses(force)?;
        Ok(CoursesData {
            courses,
            current_term: self.current_term_hint(),
        })
    }

    /// 目前已能確定的本學期（不觸發任何網路請求）：
    /// 使用者本次的明確選擇 → 本會話曾查得的考勤學期 → 設定檔記住的學期。
    fn current_term_hint(&self) -> Option<TermCode> {
        self.chosen_term.or(self.known_term).or_else(|| {
            self.config
                .homework_term
                .as_deref()
                .and_then(TermCode::parse)
        })
    }

    /// 課程活動列表。
    pub(super) fn load_activities(
        &mut self,
        course_id: &str,
        force: bool,
    ) -> AppResult<Vec<LmsActivity>> {
        let (activities, _) = self.lms_activities(course_id, force)?;
        Ok(activities)
    }

    /// 活動詳情（供介面顯示）。
    pub(super) fn load_activity_detail(
        &mut self,
        activity_id: &str,
        force: bool,
    ) -> AppResult<ActivityDetailView> {
        let activity = self.lms_activity_detail(activity_id, force)?;
        // 正文先轉純文字：`activity_from` 會取走活動，且轉換只需做一次。
        let description = activity.body();
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.activity_from(activity)?;
        let kind = detail.activity.kind();

        Ok(ActivityDetailView {
            id: detail.activity.id.clone(),
            title: detail.activity.display_title(),
            kind,
            description,
            end_time: detail.activity.end_time,
            submit_by_group: detail.activity.submit_by_group,
            submissions: detail.submissions.map(|list| list.list),
            note: detail.note,
        })
    }

    /// 解析「開啟活動網頁」的目標網址（WebVPN 模式自動改址）。
    ///
    /// - 課程內容與直播：優先使用伺服器回傳的播放器網址。
    /// - 作業：開啟所屬課程的作業列表（前端路由；缺少課程識別碼時回退首頁）。
    /// - 資料與其他類型：思源學堂首頁（前端路由未經驗證，不拼接自造路徑）。
    pub(super) fn open_activity_url(
        &mut self,
        activity_id: &str,
        course_id: Option<&str>,
        kind: ActivityKind,
    ) -> AppResult<String> {
        let mut url = lms::LOGIN_URL.to_owned();
        match kind {
            ActivityKind::Lesson | ActivityKind::LectureLive => {
                match self.lesson_player_url(activity_id) {
                    Ok(player_url) => url = player_url,
                    Err(err) if err.needs_relogin() => return Err(err),
                    Err(err) => {
                        self.emit(Event::Notice(format!(
                            "无法获取播放地址，已改为打开思源学堂首页：{err}"
                        )));
                    }
                }
            }
            ActivityKind::Homework => match course_id.and_then(lms::course_homework_url) {
                Some(homework_url) => url = homework_url,
                None => self.emit(Event::Notice(
                    "无法确定作业所属课程，已改为打开思源学堂首页".to_owned(),
                )),
            },
            ActivityKind::Material | ActivityKind::Unknown => {}
        }
        self.session_mut()?.rewrite_url(SiteKind::Lms, &url)
    }

    /// 課程內容的播放器網址（由伺服器回傳，附帶存取 token）。
    fn lesson_player_url(&mut self, activity_id: &str) -> AppResult<String> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        api.lesson_player_url(activity_id)
    }
}

/// 學期外的空課表（沒有本週課程，只有提示原因）。
fn off_session_schedule(
    semester: &str,
    week: u32,
    total_weeks: u32,
    notice: String,
) -> ScheduleData {
    ScheduleData {
        semester: semester.to_owned(),
        week,
        total_weeks,
        lessons: Vec::new(),
        skipped: 0,
        notice: Some(notice),
    }
}

/// 考勤學期的開始日期（`YYYY-MM-DD`）。
pub(super) fn parse_date(value: &str) -> AppResult<NaiveDate> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| AppError::protocol("学期开始日期格式无法识别（应为 YYYY-MM-DD）"))
}
