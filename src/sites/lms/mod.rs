//! 思源學堂客戶端。
//!
//! 端點對應參考實作 `ref/lms/lms.py`：課程清單為 `POST /api/my-courses`，
//! 課程活動為 `GET /api/courses/{id}/activities`，活動詳情為
//! `GET /api/activities/{id}`（作業會附帶提交記錄）。

mod js_object;
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

/// 課程作業列表頁的前端網址（`o` 開啟作業用）。
///
/// 路由形態 `/course/<課程識別碼>/homework` 由外部維護中的開源工具核實
/// （其前端路由表為 `/course/<識別碼>/homework#/`）。這裡**刻意不帶 `#`
/// 片段**：同一工具的實測記錄指出，帶 hash 的啟動網址會讓 SPA 卡死，
/// 因此只使用不含 hash 的路徑。
///
/// 課程識別碼僅接受 URL 安全字元；無法安全拼接時回傳 `None`，由呼叫端
/// 決定替代目標。
pub fn course_homework_url(course_id: &str) -> Option<String> {
    let course_id = safe_identifier(course_id)?;
    Some(format!("{BASE_URL}/course/{course_id}/homework"))
}

/// 伺服器提供的識別碼是否可安全拼接進 API／前端路徑。
///
/// 只接受 URL 安全字元（ASCII 英數、`-`、`_`）：`?`、`#`、`/` 等字元會改變
/// 實際請求目標。主機固定為思源學堂，因此這是縱深防禦與一致性檢查。
fn safe_identifier(value: &str) -> Option<&str> {
    let value = value.trim();
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    safe.then_some(value)
}

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
    /// 無法取得提交記錄的原因（階段化；查詢成功或非作業時為 `None`）。
    pub note: Option<String>,
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
    ///
    /// 使用者資訊解析失敗時會記入站點狀態（負快取），避免逐項作業重複請求
    /// `/user/index`；重新登入或成功解析時自動清除。
    pub fn user_id(&mut self) -> AppResult<String> {
        if let Some(user_id) = self.session.site_user_id(SiteKind::Lms) {
            return Ok(user_id.to_owned());
        }
        if let Some(reason) = self.session.site_user_id_error(SiteKind::Lms) {
            return Err(AppError::protocol(reason.to_owned()));
        }

        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::get(format!("{BASE_URL}/user/index")),
        )?;
        match user_id_from_page(&response.text()) {
            Some(user_id) => {
                self.session
                    .set_site_user_id(SiteKind::Lms, user_id.clone());
                Ok(user_id)
            }
            None => {
                let reason = "思源学堂用户信息解析失败（/user/index 无法解析 globalData.user）";
                self.session
                    .set_site_user_id_error(SiteKind::Lms, reason.to_owned());
                Err(AppError::protocol(reason))
            }
        }
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
        let activity = self.fetch_activity_detail(activity_id)?;
        self.activity_from(activity)
    }

    /// 以既有詳情組出活動詳情（作業會一併抓取提交記錄）。
    pub fn activity_from(&mut self, activity: LmsActivity) -> AppResult<ActivityDetail> {
        let mut note = None;
        let submissions = if activity.kind() == ActivityKind::Homework {
            // 詳情中的 submit_by_group 是權威的小組判定（簡要列表常缺少此欄位）。
            let submit_by_group = activity.submit_by_group.unwrap_or(false);
            match self.submissions(&activity.id, submit_by_group, activity.group_id.as_deref()) {
                Ok(list) => Some(list),
                // 登入態失效必須向上傳播，交由統一重登流程處理。
                Err(err) if err.needs_relogin() => return Err(err),
                // 其他錯誤保持「待核实」，不可誤判為未提交。
                Err(err) => {
                    note = Some(submission_failure_note(&err));
                    None
                }
            }
        } else {
            None
        };

        Ok(ActivityDetail {
            activity,
            submissions,
            note,
        })
    }

    /// 作業提交摘要：先取活動詳情確定小組，再依需要查詢提交記錄。
    ///
    /// 個人作業優先採用詳情中的 `user_submit_count`（「当前用户提交次数」，
    /// 有值時可省一次提交列表請求；語意待實網脫敏樣本核實，若語意有出入
    /// 只需調整此處）；小組作業一律以詳情確認的 `group_id` 查詢小組提交
    /// 記錄，缺 `group_id` 時保持「待核实」並說明原因。
    pub fn submission_summary(&mut self, activity_id: &str) -> AppResult<SubmissionSummary> {
        let detail = self.fetch_activity_detail(activity_id)?;
        self.submission_summary_for(&detail)
    }

    /// 以既有詳情計算提交摘要（個人作業可省一次提交列表請求）。
    pub fn submission_summary_for(&mut self, detail: &LmsActivity) -> AppResult<SubmissionSummary> {
        let submit_by_group = detail.submit_by_group.unwrap_or(false);

        if !submit_by_group && let Some(count) = detail.user_submit_count {
            return Ok(SubmissionSummary {
                submit_by_group,
                // 伺服器值為 u64：在 32 位元目標上 `as usize` 會截斷並可能誤判為
                // 「未提交」；超出範圍時視為無法確認（`None`）。
                count: usize::try_from(count).ok(),
                note: None,
            });
        }

        match self.submissions(&detail.id, submit_by_group, detail.group_id.as_deref()) {
            Ok(list) => Ok(SubmissionSummary {
                submit_by_group,
                count: Some(list.effective_count()),
                note: None,
            }),
            // 登入態失效必須向上傳播，交由統一重登流程處理。
            Err(err) if err.needs_relogin() => Err(err),
            // 其他錯誤保持「待核实」，並保留階段化原因。
            Err(err) => Ok(SubmissionSummary {
                submit_by_group,
                count: None,
                note: Some(submission_failure_note(&err)),
            }),
        }
    }

    /// 課程內容（lesson／直播）的播放器網址。
    ///
    /// 由伺服器回傳（附帶存取 token），不自行拼接前端路徑；
    /// 來源為參考實作的 `_get_lesson_player_url`。
    pub fn lesson_player_url(&mut self, activity_id: &str) -> AppResult<String> {
        let activity_id = safe_identifier(activity_id)
            .ok_or_else(|| AppError::protocol("活动识别码不符合预期格式"))?;
        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::get(format!(
                "{BASE_URL}/api/lessons/{activity_id}/player-url?from_page=course"
            )),
        )?;
        let value: Value = parse_json(&response, "查询播放地址")?;
        value
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| AppError::protocol("播放器接口未返回网址"))
    }

    /// 取得活動詳情（不含提交記錄）；供需要自行快取的呼叫端使用。
    pub fn fetch_activity_detail(&mut self, activity_id: &str) -> AppResult<LmsActivity> {
        let activity_id = safe_identifier(activity_id)
            .ok_or_else(|| AppError::protocol("活动识别码不符合预期格式"))?;
        let response = self.session.send(
            SiteKind::Lms,
            HttpRequest::get(format!("{BASE_URL}/api/activities/{activity_id}")),
        )?;
        parse_json(&response, "查询活动详情")
    }

    /// 查詢個人或小組的提交記錄。
    pub fn submissions(
        &mut self,
        activity_id: &str,
        submit_by_group: bool,
        group_id: Option<&str>,
    ) -> AppResult<LmsSubmissionList> {
        let activity_id = safe_identifier(activity_id)
            .ok_or_else(|| AppError::protocol("活动识别码不符合预期格式"))?;
        let url = if submit_by_group {
            let group_id = group_id
                .and_then(|value| safe_identifier(value))
                .ok_or_else(|| AppError::protocol("小组作业缺少有效 group_id，无法查询提交记录"))?;
            format!("{BASE_URL}/api/activities/{activity_id}/groups/{group_id}/submission_list")
        } else {
            let user_id = self.user_id()?;
            let user_id = safe_identifier(&user_id)
                .ok_or_else(|| AppError::protocol("思源学堂用户识别码不符合预期格式"))?;
            format!("{BASE_URL}/api/activities/{activity_id}/students/{user_id}/submission_list")
        };

        let response = self.session.send(SiteKind::Lms, HttpRequest::get(url))?;
        parse_json(&response, "查询作业提交记录")
    }
}

/// 「待核实」項目的原因文案（階段化、脫敏；供作業頁彙總顯示）。
pub fn submission_failure_note(err: &AppError) -> String {
    format!("无法确认提交状态：{err}")
}

/// 從 `/user/index` 頁面的 `globalData.user` 取出使用者 ID。
///
/// 真實頁面的 `globalData` 是 JavaScript 物件語法（鍵名可不加引號、
/// 以 `None` 表示空值、允許尾逗號）：先以寬容解析器讀取整個 `globalData`，
/// 失敗時再按參考實作的語義（`dept` 為邊界）單獨擷取 `user` 子物件。
fn user_id_from_page(html: &str) -> Option<String> {
    if let Some(global) = js_object::find_named_value(html, "globalData")
        && let Some(user) = global.get("user")
        && let Some(user_id) = user_id_from_value(user)
    {
        return Some(user_id);
    }
    let user = js_object::parse_js_object(html, "user", "dept")?;
    user_id_from_value(&user)
}

/// `user` 子物件中的 `id`（數字或字串皆可；空字串視為無效）。
fn user_id_from_value(user: &Value) -> Option<String> {
    match user.get("id") {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
#[path = "tests/lms_test.rs"]
mod lms_test;
