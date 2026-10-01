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
use crate::task::protocol::{CoursesData, Event};

use super::{Worker, is_recoverable};

/// 考勤流水分頁大小。
const FLOW_PAGE_SIZE: u32 = 20;

impl Worker {
    /// 課表（本週）與本週考勤。
    pub(super) fn load_schedule(&mut self) -> AppResult<ScheduleData> {
        let semester = {
            let session = self.session_mut()?;
            AttendanceApi::new(session).current_semester()?
        };
        let today = Local::now().date_naive();
        let semester_start = parse_date(&semester.start_date)?;
        // `end_date` 為選填：缺失或無法解析時不啟用「已結束」判斷（不讓整個
        // 頁面因此失敗），但可解析時一定以它為準。
        let semester_end = semester
            .end_date
            .as_deref()
            .and_then(|raw| NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok());
        let term = semester.term_name();
        let label = semester.display_label();
        // 記錄本會話得知的學期，供思源學堂課程分區使用（不重複查詢考勤）。
        self.known_term = term.as_deref().and_then(TermCode::parse);

        // 學期外（已結束／尚未開始）不查課表與考勤記錄，只顯示空狀態與提示：
        // 夾取週次會讓學期結束後仍顯示最後一週的舊課程，而考勤只涵蓋當前
        // 日曆週，結果必然全部「待核实」——不得如此呈現。
        if let Some(end) = semester_end.filter(|end| today > *end) {
            return Ok(off_session_schedule(
                &label,
                schedule::clamp_week(
                    schedule::week_number(semester_start, today),
                    term.as_deref().unwrap_or(""),
                ),
                format!("本学期已结束（{end}）"),
            ));
        }
        if today < semester_start {
            return Ok(off_session_schedule(
                &label,
                1,
                format!("本学期尚未开始（{semester_start}）"),
            ));
        }

        let courses = {
            let session = self.session_mut()?;
            AttendanceApi::new(session).weekly_courses(&semester.semester_id)?
        };
        let week = schedule::week_number(semester_start, today);

        let (monday, sunday) = schedule::week_window(today);
        let RecordBatch { records, truncated } = {
            let session = self.session_mut()?;
            AttendanceApi::new(session).records_between(monday, sunday)?
        };

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
                attendance: attendance_match::display_state(status, date, today),
            });
        }
        lessons.sort_by(|left, right| {
            (left.date, left.sections.clone()).cmp(&(right.date, right.sections.clone()))
        });

        let notice = truncated.then(|| {
            format!(
                "考勤记录超过分页上限，仅比对前 {} 条，部分课程可能显示「待核实」",
                records.len()
            )
        });

        Ok(ScheduleData {
            semester: label,
            week,
            lessons,
            skipped,
            notice,
        })
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
    ) -> AppResult<ActivityDetailView> {
        let activity = self.lms_activity_detail(activity_id, false)?;
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.activity_from(activity)?;
        let kind = detail.activity.kind();

        Ok(ActivityDetailView {
            id: detail.activity.id.clone(),
            title: detail.activity.display_title(),
            kind,
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
fn off_session_schedule(semester: &str, week: u32, notice: String) -> ScheduleData {
    ScheduleData {
        semester: semester.to_owned(),
        week,
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
