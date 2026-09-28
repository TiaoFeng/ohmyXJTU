//! 思源學堂客戶端。
//!
//! 端點對應參考實作 `ref/lms/lms.py`：課程清單為 `POST /api/my-courses`，
//! 課程活動為 `GET /api/courses/{id}/activities`，活動詳情為
//! `GET /api/activities/{id}`（作業會附帶提交記錄）。

pub mod models;

pub use models::{ActivityKind, LmsActivity, LmsCourse, LmsSubmission, LmsSubmissionList};

use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::http::HttpRequest;
use crate::session::{PostLogin, SessionManager, SiteAdapter, SiteKind, SiteLogin, SitePolicy};
use crate::sites::{parse_json, parse_lenient};

/// 思源學堂網址。
pub const BASE_URL: &str = "https://lms.xjtu.edu.cn";
/// 站點入口。
pub const LOGIN_URL: &str = "https://lms.xjtu.edu.cn";

/// 存取策略：思源學堂為雲端服務，校外可直接連線，不必多繞一層 WebVPN。
pub const POLICY: SitePolicy = SitePolicy {
    login_url: LOGIN_URL,
    supports_webvpn: true,
    use_webvpn_when_off_campus: false,
};

/// 站點擴充點。
#[derive(Debug, Default, Clone, Copy)]
pub struct LmsSite;

impl SiteAdapter for LmsSite {
    fn kind(&self) -> SiteKind {
        SiteKind::Lms
    }

    fn policy(&self) -> SitePolicy {
        POLICY
    }

    fn post_login(&self, context: &PostLogin<'_>) -> AppResult<SiteLogin> {
        let response = context.get(&format!("{BASE_URL}/user/index"))?;
        Ok(SiteLogin {
            headers: Vec::new(),
            user_id: user_id_from_page(&response.text()),
        })
    }
}

/// 活動詳情與其提交記錄。
#[derive(Debug, Clone)]
pub struct ActivityDetail {
    /// 活動本身。
    pub activity: LmsActivity,
    /// 提交記錄；`None` 代表無法確認（顯示為「待核实」）。
    pub submissions: Option<LmsSubmissionList>,
}

/// 作業提交摘要。
#[derive(Debug, Clone)]
pub struct SubmissionSummary {
    /// 是否以小組為單位提交（以活動詳情為準）。
    pub submit_by_group: bool,
    /// 有效提交數；`None` 代表無法確認。
    pub count: Option<usize>,
    /// 無法確認的原因。
    pub note: Option<String>,
}

/// 思源學堂 API。
pub struct LmsApi<'a> {
    session: &'a mut SessionManager,
}

impl<'a> LmsApi<'a> {
    /// 建立 API 物件。
    pub fn new(session: &'a mut SessionManager) -> Self {
        Self { session }
    }

    /// 目前登入者的使用者 ID。
    pub fn user_id(&mut self) -> AppResult<String> {
        if let Some(user_id) = self.session.site_user_id(SiteKind::Lms) {
            return Ok(user_id.to_owned());
        }

        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::get(format!("{BASE_URL}/user/index")),
        )?;
        let user_id = user_id_from_page(&response.text())
            .ok_or_else(|| AppError::protocol("无法从思源学堂首页解析用户 ID"))?;
        self.session
            .set_site_user_id(SiteKind::Lms, user_id.clone());
        Ok(user_id)
    }

    /// 我的課程（附帶被跳過的項目數）。
    pub fn my_courses(&mut self) -> AppResult<(Vec<LmsCourse>, usize)> {
        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::post(format!("{BASE_URL}/api/my-courses")),
        )?;
        let value: Value = parse_json(&response, "查询我的课程")?;
        let courses = value
            .get("courses")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        parse_lenient(courses, "查询我的课程")
    }

    /// 課程活動列表（附帶被跳過的項目數）。
    pub fn course_activities(&mut self, course_id: &str) -> AppResult<(Vec<LmsActivity>, usize)> {
        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::get(format!("{BASE_URL}/api/courses/{course_id}/activities")),
        )?;
        let value: Value = parse_json(&response, "查询课程活动")?;
        let activities = value
            .get("activities")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        parse_lenient(activities, "查询课程活动")
    }

    /// 活動詳情；作業會一併抓取提交記錄。
    pub fn activity(&mut self, activity_id: &str) -> AppResult<ActivityDetail> {
        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::get(format!("{BASE_URL}/api/activities/{activity_id}")),
        )?;
        let activity: LmsActivity = parse_json(&response, "查询活动详情")?;

        let submissions = if activity.kind() == ActivityKind::Homework {
            // 詳情中的 submit_by_group 是權威的小組判定（簡要列表常缺少此欄位）。
            let submit_by_group = activity.submit_by_group.unwrap_or(false);
            match self.submissions(activity_id, submit_by_group, activity.group_id.as_deref()) {
                Ok(list) => Some(list),
                // 登入態失效必須向上傳播，交由統一重登流程處理。
                Err(err) if err.needs_relogin() => return Err(err),
                // 其他錯誤保持「待核实」，不可誤判為未提交。
                Err(_) => None,
            }
        } else {
            None
        };

        Ok(ActivityDetail {
            activity,
            submissions,
        })
    }

    /// 作業提交摘要：先取活動詳情確定小組，再抓提交記錄。
    pub fn submission_summary(&mut self, activity_id: &str) -> AppResult<SubmissionSummary> {
        let detail = self.activity(activity_id)?;
        let submit_by_group = detail.activity.submit_by_group.unwrap_or(false);
        Ok(match detail.submissions {
            Some(list) => SubmissionSummary {
                submit_by_group,
                count: Some(list.effective_count()),
                note: None,
            },
            None => SubmissionSummary {
                submit_by_group,
                count: None,
                note: Some("未取到提交记录，无法确认提交状态".to_owned()),
            },
        })
    }

    /// 查詢個人或小組的提交記錄。
    pub fn submissions(
        &mut self,
        activity_id: &str,
        submit_by_group: bool,
        group_id: Option<&str>,
    ) -> AppResult<LmsSubmissionList> {
        let url = if submit_by_group {
            let group_id = group_id
                .filter(|value| !value.is_empty())
                .ok_or_else(|| AppError::protocol("小组作业缺少 group_id，无法查询提交记录"))?;
            format!("{BASE_URL}/api/activities/{activity_id}/groups/{group_id}/submission_list")
        } else {
            let user_id = self.user_id()?;
            format!("{BASE_URL}/api/activities/{activity_id}/students/{user_id}/submission_list")
        };

        let response = self.session.send(SiteKind::Lms, HttpRequest::get(url))?;
        parse_json(&response, "查询作业提交记录")
    }
}

/// 從 `/user/index` 的 `globalData.user` 取出使用者 ID。
fn user_id_from_page(html: &str) -> Option<String> {
    let config = extract_js_object(html, "globalData")?;
    match config.pointer("/user/id") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// 取出 `key = {…}` 形式的 JavaScript 物件（以字串與轉義感知的花括號配對）。
fn extract_js_object(html: &str, key: &str) -> Option<Value> {
    let start = html.find(key)?;
    let rest = &html[start + key.len()..];
    let open = rest.find('{')?;
    let object = &rest[open..];
    let end = matching_brace(object)?;
    serde_json::from_str(&object[..=end]).ok()
}

/// 回傳與第一個 `{` 配對的 `}` 之位元組索引。
fn matching_brace(text: &str) -> Option<usize> {
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[path = "tests/lms_test.rs"]
mod lms_test;
