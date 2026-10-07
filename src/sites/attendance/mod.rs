//! 本科考勤系統客戶端。
//!
//! 端點與欄位對應參考實作 `ref/attendance/attendance.py`：統一認證完成後需以
//! 回跳網址的 `loginRequestId` 與 `ticket` 向 `/sa/auth/cas/exchange` 換取
//! 業務 token，後續請求都必須附帶 `X-Business-Token`。

pub mod models;

pub use models::{
    AttendanceStatus, FlowPage, FlowRecord, RecordBatch, Semester, TimetableCourse, WaterRecord,
};

use std::collections::HashMap;

use chrono::NaiveDate;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::http::HttpRequest;
use crate::session::{PostLogin, SessionManager, SiteAdapter, SiteKind, SiteLogin, SitePolicy};
use crate::sites::{deserialize_value, parse_lenient, unwrap_envelope};

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

    /// 整學期課表（附帶被跳過的課程數）。
    ///
    /// 逐項寬容解析：單一門課的欄位型別異常只該讓那門課從課表消失（並回報
    /// 筆數），不該讓整個課表頁失敗——`deserialize_value` 會讓整批一起失敗。
    pub fn weekly_courses(
        &mut self,
        semester_id: &str,
    ) -> AppResult<(Vec<TimetableCourse>, usize)> {
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
        parse_lenient(courses, "查询课表")
    }

    /// 分頁查詢考勤流水。
    pub fn flow_page(&mut self, page: u32, page_size: u32) -> AppResult<FlowPage> {
        let request = HttpRequest::post_json(
            format!("https://{DOMAIN}/sa/student/pc/attendance-streams/page"),
            json!({ "pageNum": page, "pageSize": page_size, "data": {} }),
        );
        let data = self.data(request, "查询考勤流水")?;
        // `total` 必須是整數：缺欄位或型別不符時明確報錯，避免靜默歸零而
        // 只取到第一頁的資料（流水被悄悄截斷）。
        let total = required_total(&data, "查询考勤流水")?;
        let (records, skipped) = required_rows(&data, total, "查询考勤流水")?;
        Ok(FlowPage {
            records,
            total,
            page,
            page_size,
            skipped,
        })
    }

    /// 查詢指定期間的課程考勤記錄（自動翻頁）。
    ///
    /// 到達分頁上限時明確標記 `truncated`：缺失的記錄會讓課程顯示
    /// 「待核实」，使用者必須知道資料不完整，不得静默截断。
    pub fn records_between(&mut self, start: NaiveDate, end: NaiveDate) -> AppResult<RecordBatch> {
        let mut records = Vec::new();
        let mut skipped = 0;
        // 伺服器回報的列數（含無法解析者）：分頁的「這一頁有沒有進展」必須以
        // 它為準——若只看成功解析的筆數，被跳過的記錄會讓迴圈誤以為沒有進展。
        let mut fetched = 0_u64;
        let mut page = 1;
        let truncated = loop {
            let request = HttpRequest::post_json(
                format!("https://{DOMAIN}/sa/student/pc/attendance-records/page"),
                json!({
                    "pageNum": page,
                    "pageSize": RECORDS_PAGE_SIZE,
                    "data": { "startDate": start.to_string(), "endDate": end.to_string() },
                }),
            );
            let data = self.data(request, "查询课程考勤记录")?;
            let total = required_total(&data, "查询课程考勤记录")?;
            let (rows, skipped_rows) = required_rows(&data, total, "查询课程考勤记录")?;
            let before = fetched;
            fetched += (rows.len() + skipped_rows) as u64;
            skipped += skipped_rows;
            records.extend(rows);

            if fetched >= total {
                break false;
            }
            // `total` 說還有記錄、這一頁卻一列都沒有：再翻頁只會重複同樣的
            // 請求（最多 MAX_PAGES 次）。標記為不完整並停止，不假裝已取完。
            if fetched == before {
                break true;
            }
            if page >= MAX_PAGES {
                break true;
            }
            page += 1;
        };
        Ok(RecordBatch {
            records,
            truncated,
            skipped,
        })
    }

    fn data(&mut self, request: HttpRequest, context: &str) -> AppResult<Value> {
        let response = self.session.send(SiteKind::Attendance, request)?;
        unwrap_envelope(&response, context)
    }
}

/// 取出分頁回應的記錄陣列（逐項寬容解析），回傳（記錄, 被跳過的筆數）。
///
/// `total` 為 0 時缺 `rows` 是正常的（真的沒有記錄）；`total` 大於 0 卻沒有
/// 可用的 `rows` 陣列，代表回應與 `total` 自相矛盾——當成空陣列會讓分頁一路
/// 查到上限，最後以「超過分頁上限」回報且沒有任何記錄，把協定問題誤報成
/// 資料問題。
///
/// 個別項目解析失敗時只跳過該項（與思源學堂的 [`parse_lenient`] 一致）：整批
/// 嚴格解析會讓**一筆**型別異常的記錄毀掉整頁——課表頁直接顯示失敗，連帶所有
/// 課程都沒有考勤狀態。跳過的記錄讓對應課程顯示「待核实」（缺失記錄不得推斷為
/// 正常），並由呼叫端把筆數回報給使用者。
fn required_rows<T: DeserializeOwned>(
    data: &Value,
    total: u64,
    context: &str,
) -> AppResult<(Vec<T>, usize)> {
    let rows = data.get("rows").cloned().unwrap_or(Value::Null);
    if matches!(rows, Value::Array(_)) {
        return parse_lenient(rows, context);
    }
    if total == 0 {
        return Ok((Vec::new(), 0));
    }
    Err(AppError::protocol(format!(
        "{context} 响应缺少记录数组（total 为 {total}）"
    )))
}

/// 取出必填的整數 `total`；缺欄位或型別不符時回報協定錯誤。
///
/// 分頁若把無法辨識的 `total` 當成 0，會在取完第一頁後就停止，使用者不會
/// 察覺記錄被悄悄截斷；明確報錯才能讓問題浮現。
fn required_total(data: &Value, context: &str) -> AppResult<u64> {
    data.get("total")
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::protocol(format!("{context} 响应缺少整数 total 字段")))
}

#[cfg(test)]
#[path = "tests/attendance_test.rs"]
mod attendance_test;
