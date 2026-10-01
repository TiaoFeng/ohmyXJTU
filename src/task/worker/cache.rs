//! 思源學堂課程／活動快取。
//!
//! `LmsCache` 只負責儲存（記憶體、有效期五分鐘）；`impl Worker` 的 `lms_*`
//! 方法是快取的前門：有效期內直接重用，`force`（使用者按 `r`）一律略過快取
//! 重新查詢。帳號或訪問模式變更時由調度核心呼叫 `LmsCache::clear`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::error::AppResult;
use crate::sites::lms::{LmsActivity, LmsApi, LmsCourse, SubmissionSummary};

use super::Worker;

/// 思源學堂課程／活動快取的有效時間。
const LMS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

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
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let (activities, skipped) = api.course_activities(course_id)?;
        self.cache.store_activities(course_id, &activities, skipped);
        Ok((activities, skipped))
    }

    /// 活動詳情（記憶體內快取先行）。
    pub(super) fn lms_activity_detail(
        &mut self,
        activity_id: &str,
        force: bool,
    ) -> AppResult<LmsActivity> {
        if let Some(cached) = self.cache.detail(activity_id, force) {
            return Ok(cached);
        }
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let detail = api.fetch_activity_detail(activity_id)?;
        self.cache.store_detail(activity_id, &detail);
        Ok(detail)
    }

    /// 作業提交摘要（快取先行；詳情先行確定小組）。
    pub(super) fn lms_submission_summary(
        &mut self,
        activity_id: &str,
        force: bool,
    ) -> AppResult<SubmissionSummary> {
        if let Some(cached) = self.cache.summary(activity_id, force) {
            return Ok(cached);
        }
        let detail = self.lms_activity_detail(activity_id, force)?;
        let session = self.session_mut()?;
        let mut api = LmsApi::new(session);
        let summary = api.submission_summary_for(&detail)?;
        self.cache.store_summary(activity_id, &summary);
        Ok(summary)
    }
}
