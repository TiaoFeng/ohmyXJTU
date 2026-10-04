//! 課程資料快取：思源學堂（`LmsCache`）與課表（`ScheduleCache`）。
//!
//! `LmsCache` 只負責儲存（記憶體、有效期五分鐘）；`impl Worker` 的 `lms_*`
//! 方法是快取的前門：有效期內直接重用，`force`（使用者按 `r`）一律略過快取
//! 重新查詢。帳號或訪問模式變更時由調度核心呼叫 `LmsCache::clear`。
//!
//! `ScheduleCache` 保存整學期課表（合併後的時段與學期資訊）：課表端點一次
//! 回傳整學期課程，本週只是客戶端過濾的結果，因此切換週次只需重查該週的
//! 考勤記錄。它沒有有效期——使用者按 `r` 或帳號／訪問模式變更時重建
//!（`Worker::load_schedule`）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::NaiveDate;

use crate::domain::schedule::CourseSlot;
use crate::error::{AppError, AppResult};
use crate::http::HttpRequest;
use crate::session::SiteKind;
use crate::sites::lms::{LmsActivity, LmsApi, LmsCourse, SubmissionSummary};

use super::Worker;
use super::timing::Phase;

/// 思源學堂課程／活動快取的有效時間。
const LMS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// 單門課程的活動查詢結果（活動清單＋被跳過的項目數）。
pub(super) type ActivitiesOutcome = AppResult<(Vec<LmsActivity>, usize)>;

/// 單項活動詳情的查詢結果。
pub(super) type DetailOutcome = AppResult<LmsActivity>;

/// 課表快取：整學期課程（合併後）與學期資訊。
///
/// 保存整學期課程後，切換週次只需重查該週的考勤記錄；`force`（使用者按
/// `r`）或帳號／訪問模式變更時重建（`Worker::load_schedule`）。
#[derive(Clone)]
pub(super) struct ScheduleCache {
    /// 學期顯示標籤（例如 `2026-2027-1`）。
    pub(super) label: String,
    /// 學期開始日（週次計算的錨點）。
    pub(super) start: NaiveDate,
    /// 學期結束日（可解析時）。
    pub(super) end: Option<NaiveDate>,
    /// 學期代碼（`YYYY-YYYY+1-T`；無法識別時為 `None`）。
    pub(super) term: Option<String>,
    /// 合併後的課程時段。
    pub(super) slots: Vec<CourseSlot>,
    /// 因週次格式問題被跳過的課程筆數。
    pub(super) skipped: usize,
    /// 課程聲明的最大週次。
    pub(super) max_week: Option<u32>,
}

/// 思源學堂課程／活動快取（記憶體、有效期五分鐘）。
#[derive(Default)]
pub(super) struct LmsCache {
    courses: Option<(Vec<LmsCourse>, usize, Instant)>,
    activities: HashMap<String, (Vec<LmsActivity>, usize, Instant)>,
    details: HashMap<String, (LmsActivity, Instant)>,
    summaries: HashMap<String, (SubmissionSummary, Instant)>,
}

impl LmsCache {
    /// 清空快取（帳號或訪問模式變更時呼叫）。
    pub(super) fn clear(&mut self) {
        self.courses = None;
        self.activities.clear();
        self.details.clear();
        self.summaries.clear();
    }

    /// 取課程快取（有效期內且非強制刷新時）。
    fn courses(&self, force: bool) -> Option<(Vec<LmsCourse>, usize)> {
        if force {
            return None;
        }
        let (courses, skipped, at) = self.courses.as_ref()?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| (courses.clone(), *skipped))
    }

    /// 寫入課程快取。
    fn store_courses(&mut self, courses: &[LmsCourse], skipped: usize) {
        self.courses = Some((courses.to_vec(), skipped, Instant::now()));
    }

    /// 取活動快取（有效期內且非強制刷新時）。
    fn activities(&self, course_id: &str, force: bool) -> Option<(Vec<LmsActivity>, usize)> {
        if force {
            return None;
        }
        let (activities, skipped, at) = self.activities.get(course_id)?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| (activities.clone(), *skipped))
    }

    /// 寫入活動快取。
    fn store_activities(&mut self, course_id: &str, activities: &[LmsActivity], skipped: usize) {
        self.activities.insert(
            course_id.to_owned(),
            (activities.to_vec(), skipped, Instant::now()),
        );
    }

    /// 取活動詳情快取（有效期內且非強制刷新時）。
    fn detail(&self, activity_id: &str, force: bool) -> Option<LmsActivity> {
        if force {
            return None;
        }
        let (detail, at) = self.details.get(activity_id)?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| detail.clone())
    }

    /// 寫入活動詳情快取。
    fn store_detail(&mut self, activity_id: &str, detail: &LmsActivity) {
        self.details
            .insert(activity_id.to_owned(), (detail.clone(), Instant::now()));
    }

    /// 取提交摘要快取（有效期內且非強制刷新時）。
    fn summary(&self, activity_id: &str, force: bool) -> Option<SubmissionSummary> {
        if force {
            return None;
        }
        let (summary, at) = self.summaries.get(activity_id)?;
        (at.elapsed() < LMS_CACHE_TTL).then(|| summary.clone())
    }

    /// 寫入提交摘要快取。
    fn store_summary(&mut self, activity_id: &str, summary: &SubmissionSummary) {
        self.summaries
            .insert(activity_id.to_owned(), (summary.clone(), Instant::now()));
    }
}

impl Worker {
    /// 課程清單（含被跳過的項目數）；有效期內重用快取。
    pub(super) fn lms_courses(&mut self, force: bool) -> AppResult<(Vec<LmsCourse>, usize)> {
        if let Some(cached) = self.cache.courses(force) {
            self.timing.note_hit();
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (courses, skipped) = api.my_courses()?;
        self.cache.store_courses(&courses, skipped);
        Ok((courses, skipped))
    }

    /// 課程活動（含被跳過的項目數）；有效期內重用快取。
    pub(super) fn lms_activities(
        &mut self,
        course_id: &str,
        force: bool,
    ) -> AppResult<(Vec<LmsActivity>, usize)> {
        if let Some(cached) = self.cache.activities(course_id, force) {
            self.timing.note_hit();
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (activities, skipped) = api.course_activities(course_id)?;
        self.cache.store_activities(course_id, &activities, skipped);
        Ok((activities, skipped))
    }

    /// 一次取回多門課程的活動（併發；有效期內重用快取）。
    ///
    /// 回傳與 `course_ids` **同序**的結果。外層 `Err` 代表整批無法進行
    ///（例如登入態失效，交由呼叫端走統一重登）；內層 `Err` 只代表該門課程
    /// 失敗，呼叫端可以只略過那一門。
    ///
    /// 快取命中的課程不發請求；其餘以一批併發送出，因此這一輪只會佔用工作
    /// 執行緒一次往返的時間，而不是逐門累加。
    pub(super) fn lms_activities_batch(
        &mut self,
        course_ids: &[String],
        force: bool,
    ) -> AppResult<Vec<ActivitiesOutcome>> {
        let mut slots: Vec<Option<ActivitiesOutcome>> =
            (0..course_ids.len()).map(|_| None).collect();
        let mut pending: Vec<(usize, HttpRequest)> = Vec::new();

        for (index, course_id) in course_ids.iter().enumerate() {
            if let Some(cached) = self.cache.activities(course_id, force) {
                self.timing.note_hit();
                slots[index] = Some(Ok(cached));
                continue;
            }
            match LmsApi::course_activities_request(course_id) {
                Ok(request) => pending.push((index, request)),
                // 識別碼不合法：只影響這一門課程，不是整批失敗。
                Err(err) => slots[index] = Some(Err(err)),
            }
        }

        if !pending.is_empty() {
            let requests = pending.iter().map(|(_, request)| request.clone()).collect();
            let responses = self.session_mut()?.send_batch(SiteKind::Lms, requests)?;
            for ((index, _), response) in pending.iter().zip(responses) {
                let parsed =
                    response.and_then(|response| LmsApi::parse_course_activities(&response));
                if let Ok((activities, skipped)) = parsed.as_ref() {
                    self.cache
                        .store_activities(&course_ids[*index], activities, *skipped);
                }
                slots[*index] = Some(parsed);
            }
        }

        Ok(slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| Err(AppError::protocol("课程活动的查询结果不完整"))))
            .collect())
    }

    /// 活動詳情（記憶體內快取先行）。
    pub(super) fn lms_activity_detail(
        &mut self,
        activity_id: &str,
        force: bool,
    ) -> AppResult<LmsActivity> {
        if let Some(cached) = self.cache.detail(activity_id, force) {
            self.timing.note_hit();
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.fetch_activity_detail(activity_id)?;
        self.cache.store_detail(activity_id, &detail);
        Ok(detail)
    }

    /// 一次取回多項活動的詳情（併發；有效期內重用快取）。
    ///
    /// 回傳與 `activity_ids` **同序**的結果。外層 `Err` 代表整批無法進行
    ///（例如登入態失效，交由呼叫端走統一重登）；內層 `Err` 只代表該項失敗。
    pub(super) fn lms_detail_batch(
        &mut self,
        activity_ids: &[String],
        force: bool,
    ) -> AppResult<Vec<DetailOutcome>> {
        let mut slots: Vec<Option<DetailOutcome>> = (0..activity_ids.len()).map(|_| None).collect();
        let mut pending: Vec<(usize, HttpRequest)> = Vec::new();

        for (index, activity_id) in activity_ids.iter().enumerate() {
            if let Some(cached) = self.cache.detail(activity_id, force) {
                self.timing.note_hit();
                slots[index] = Some(Ok(cached));
                continue;
            }
            match LmsApi::activity_detail_request(activity_id) {
                Ok(request) => pending.push((index, request)),
                // 識別碼不合法：只影響這一項，不是整批失敗。
                Err(err) => slots[index] = Some(Err(err)),
            }
        }

        if !pending.is_empty() {
            let requests = pending.iter().map(|(_, request)| request.clone()).collect();
            let responses = self.session_mut()?.send_batch(SiteKind::Lms, requests)?;
            for ((index, _), response) in pending.iter().zip(responses) {
                let parsed = response.and_then(|response| LmsApi::parse_activity_detail(&response));
                if let Ok(detail) = parsed.as_ref() {
                    self.cache.store_detail(&activity_ids[*index], detail);
                }
                slots[*index] = Some(parsed);
            }
        }

        Ok(slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| Err(AppError::protocol("活动详情的查询结果不完整"))))
            .collect())
    }

    /// 作業提交摘要（快取先行；詳情已由呼叫端取得）。
    pub(super) fn lms_submission_summary_from_detail(
        &mut self,
        activity_id: &str,
        detail: &LmsActivity,
        force: bool,
    ) -> AppResult<SubmissionSummary> {
        if let Some(cached) = self.cache.summary(activity_id, force) {
            self.timing.note_hit();
            return Ok(cached);
        }
        let started = self.timing.mark();
        let summary = self.fetch_submission_summary(detail);
        self.timing.record(Phase::Submission, started, 1);
        let summary = summary?;
        self.cache.store_summary(activity_id, &summary);
        Ok(summary)
    }

    /// 向伺服器查詢提交摘要（詳情已取得）。
    fn fetch_submission_summary(&mut self, detail: &LmsActivity) -> AppResult<SubmissionSummary> {
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        api.submission_summary_for(detail)
    }
}
