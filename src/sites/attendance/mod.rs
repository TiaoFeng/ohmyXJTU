//! 本科考勤系統客戶端。
//!
//! 端點與欄位對應參考實作 `ref/attendance/attendance.py`：統一認證完成後需以
//! 回跳網址的 `loginRequestId` 與 `ticket` 向 `/sa/auth/cas/exchange` 換取
//! 業務 token，後續請求都必須附帶 `X-Business-Token`。

pub mod models;

pub use models::{AttendanceStatus, FlowPage, FlowRecord, Semester, TimetableCourse, WaterRecord};

use std::collections::HashMap;

use chrono::NaiveDate;
use serde_json::{Value, json};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::http::HttpRequest;
use crate::session::{PostLogin, SessionManager, SiteAdapter, SiteKind, SiteLogin, SitePolicy};
use crate::sites::{deserialize_value, unwrap_envelope};

/// 本科生考勤系統網域。
pub const DOMAIN: &str = "bk-kq.xjtu.edu.cn";
/// 站點入口（會跳轉至統一認證）。
pub const LOGIN_URL: &str = "https://bk-kq.xjtu.edu.cn/sa/auth/cas/login/student-pc";
/// 業務 token 交換端點。
const EXCHANGE_URL: &str = "https://bk-kq.xjtu.edu.cn/sa/auth/cas/exchange";

/// 存取策略：考勤系統僅校內可直連，校外需經 WebVPN。
pub const POLICY: SitePolicy = SitePolicy {
    login_url: LOGIN_URL,
    supports_webvpn: true,
    use_webvpn_when_off_campus: true,
};

/// 考勤記錄的分頁大小。
const RECORDS_PAGE_SIZE: u32 = 100;
/// 自動翻頁的上限，避免異常回應造成無窮迴圈。
const MAX_PAGES: u32 = 20;

/// 站點擴充點。
#[derive(Debug, Default, Clone, Copy)]
pub struct AttendanceSite;

impl SiteAdapter for AttendanceSite {
    fn kind(&self) -> SiteKind {
        SiteKind::Attendance
    }

    fn policy(&self) -> SitePolicy {
        POLICY
    }

    fn post_login(&self, context: &PostLogin<'_>) -> AppResult<SiteLogin> {
        let response = context
            .final_response()
            .ok_or_else(|| AppError::protocol("考勤系统登录未返回最终跳转，无法换取业务令牌"))?;
        let url = Url::parse(&response.final_url)
            .map_err(|err| AppError::protocol(format!("登录回跳地址无法解析：{err}")))?;
        let params: HashMap<String, String> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let login_request_id = params
            .get("loginRequestId")
            .ok_or_else(|| AppError::protocol("登录回跳地址缺少 loginRequestId"))?;
        let ticket = params
            .get("ticket")
            .ok_or_else(|| AppError::protocol("登录回跳地址缺少 ticket"))?;

        let response = context.post_json(
            EXCHANGE_URL,
            json!({ "loginRequestId": login_request_id, "ticket": ticket }),
        )?;
        let data: Value = unwrap_envelope(&response, "换取考勤业务令牌")?;
        let token = data
            .get("tokenValue")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::protocol("业务令牌响应缺少 tokenValue 字段"))?;

        Ok(SiteLogin {
            headers: vec![("X-Business-Token".to_owned(), token.to_owned())],
            user_id: None,
        })
    }
}

/// 考勤系統 API。
pub struct AttendanceApi<'a> {
    session: &'a mut SessionManager,
}

impl<'a> AttendanceApi<'a> {
    /// 建立 API 物件。
    pub fn new(session: &'a mut SessionManager) -> Self {
        Self { session }
    }

    /// 學期清單（伺服器回傳的第一筆為當前學期）。
    pub fn semesters(&mut self) -> AppResult<Vec<Semester>> {
        let request = HttpRequest::get(format!(
            "https://{DOMAIN}/sa/student/service/timetable/semesters"
        ));
        let data = self.data(request, "查询学期列表")?;
        deserialize_value(data, "查询学期列表")
    }

    /// 當前學期。
    pub fn current_semester(&mut self) -> AppResult<Semester> {
        self.semesters()?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::protocol("考勤系统未返回任何学期"))
    }

    /// 整學期課表。
    pub fn weekly_courses(&mut self, semester_id: &str) -> AppResult<Vec<TimetableCourse>> {
        let mut url = Url::parse(&format!(
            "https://{DOMAIN}/sa/student/service/timetable/weekly"
        ))
        .map_err(|err| AppError::protocol(format!("课表地址无法解析：{err}")))?;
        url.query_pairs_mut().append_pair("semesterId", semester_id);

        let data = self.data(HttpRequest::get(url.to_string()), "查询课表")?;
        let courses = data
            .get("courses")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        deserialize_value(courses, "查询课表")
    }

    /// 分頁查詢考勤流水。
    pub fn flow_page(&mut self, page: u32, page_size: u32) -> AppResult<FlowPage> {
        let request = HttpRequest::post_json(
            format!("https://{DOMAIN}/sa/student/pc/attendance-streams/page"),
            json!({ "pageNum": page, "pageSize": page_size, "data": {} }),
        );
        let data = self.data(request, "查询考勤流水")?;
        let records = deserialize_value(
            data.get("rows")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
            "查询考勤流水",
        )?;
        Ok(FlowPage {
            records,
            total: data.get("total").and_then(Value::as_u64).unwrap_or(0),
            page,
            page_size,
        })
    }

    /// 查詢指定期間的課程考勤記錄（自動翻頁）。
    pub fn records_between(
        &mut self,
        start: NaiveDate,
        end: NaiveDate,
    ) -> AppResult<Vec<WaterRecord>> {
        let mut records = Vec::new();
        let mut page = 1;
        loop {
            let request = HttpRequest::post_json(
                format!("https://{DOMAIN}/sa/student/pc/attendance-records/page"),
                json!({
                    "pageNum": page,
                    "pageSize": RECORDS_PAGE_SIZE,
                    "data": { "startDate": start.to_string(), "endDate": end.to_string() },
                }),
            );
            let data = self.data(request, "查询课程考勤记录")?;
            let rows: Vec<WaterRecord> = deserialize_value(
                data.get("rows")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
                "查询课程考勤记录",
            )?;
            let total = data.get("total").and_then(Value::as_u64).unwrap_or(0);
            records.extend(rows);

            if records.len() as u64 >= total || page >= MAX_PAGES {
                break;
            }
            page += 1;
        }
        Ok(records)
    }

    fn data(&mut self, request: HttpRequest, context: &str) -> AppResult<Value> {
        let response = self.session.send(SiteKind::Attendance, request)?;
        unwrap_envelope(&response, context)
    }
}

#[cfg(test)]
#[path = "tests/attendance_test.rs"]
mod attendance_test;
