//! 考勤系統資料模型與外殼解析測試（使用脫敏的固定回應）。

use super::*;
use crate::http::HttpResponse;

fn json_response(body: &str) -> HttpResponse {
    HttpResponse::new(200, "https://bk-kq.xjtu.edu.cn/sa", body.as_bytes())
}

#[test]
fn parses_semesters_and_term_names() {
    let response = json_response(
        r#"{"code":0,"message":"ok","data":[
            {"semesterId":123,"academicYear":"2026-2027","semesterName":"第一学期",
             "startDate":"2026-09-07","endDate":"2027-01-17"},
            {"semesterId":"122","academicYear":"2025-2026","semesterName":"第三学期",
             "startDate":"2026-06-29"}
        ]}"#,
    );

    let semesters: Vec<Semester> =
        crate::sites::unwrap_envelope(&response, "查询学期列表").unwrap();
    assert_eq!(semesters.len(), 2);
    // 數值型別的識別碼也要能解析。
    assert_eq!(semesters[0].semester_id, "123");
    assert_eq!(semesters[0].term_name().as_deref(), Some("2026-2027-1"));
    assert_eq!(semesters[1].term_name().as_deref(), Some("2025-2026-3"));
    assert_eq!(semesters[0].display_label(), "2026-2027-1");
}

#[test]
fn parses_timetable_courses_with_mixed_types() {
    let response = json_response(
        r#"{"code":0,"data":{"courses":[
            {"courseName":"高等数学","teacherName":"张老师","classroomName":"主楼A101",
             "dayOfWeek":1,"startSection":"1","endSection":2,"weekRanges":"1-4,6-16"}
        ]}}"#,
    );

    let data: serde_json::Value = crate::sites::unwrap_envelope(&response, "查询课表").unwrap();
    let courses: Vec<TimetableCourse> =
        crate::sites::deserialize_value(data.get("courses").cloned().unwrap(), "查询课表").unwrap();
    assert_eq!(courses.len(), 1);
    assert_eq!(courses[0].course_name, "高等数学");
    assert_eq!(courses[0].day_of_week, 1);
    assert_eq!(courses[0].start_section, 1);
    assert_eq!(courses[0].end_section, 2);
}

#[test]
fn parses_water_records_and_statuses() {
    let response = json_response(
        r#"{"code":0,"data":{"rows":[
            {"resultId":9001,"startSection":1,"endSection":2,"courseWeek":2,
             "classroomName":"主楼A101","teacherName":"张老师",
             "attendanceStatus":"ABSENT","attendanceDate":"2026-09-14","semesterId":123},
            {"resultId":"9002","startSection":3,"endSection":4,"courseWeek":2,
             "attendanceStatus":"WHATEVER","attendanceDate":"2026-09-15"}
        ],"total":2}}"#,
    );

    let data: serde_json::Value =
        crate::sites::unwrap_envelope(&response, "查询课程考勤记录").unwrap();
    let rows: Vec<WaterRecord> =
        crate::sites::deserialize_value(data.get("rows").cloned().unwrap(), "查询课程考勤记录")
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].result_id, "9001");
    assert_eq!(rows[0].status(), AttendanceStatus::Absent);
    assert_eq!(rows[1].status(), AttendanceStatus::Unknown);
}

#[test]
fn reports_business_errors() {
    let response = json_response(r#"{"code":401,"message":"登录已过期","data":null}"#);
    let err =
        crate::sites::unwrap_envelope::<serde_json::Value>(&response, "查询课表").unwrap_err();
    assert!(
        matches!(err, AppError::Server { code: 401, .. }),
        "实际错误：{err}"
    );
}

#[test]
fn required_total_rejects_missing_or_non_integer_totals() {
    // 缺 `total`／字串／null：明確協定錯誤，不得靜默歸零（否則只取第一頁、
    // 流水被悄悄截斷）。
    for body in [
        r#"{"rows":[]}"#,
        r#"{"rows":[],"total":"3"}"#,
        r#"{"rows":[],"total":null}"#,
    ] {
        let data: serde_json::Value = serde_json::from_str(body).expect("解析 JSON");
        let err = required_total(&data, "查询考勤流水").expect_err("应拒绝无效 total");
        assert!(matches!(err, AppError::Protocol(_)), "应为协定错误：{err}");
    }

    let data: serde_json::Value = serde_json::from_str(r#"{"total":5}"#).expect("解析 JSON");
    assert_eq!(
        required_total(&data, "查询考勤流水").expect("整数 total"),
        5
    );
}

#[test]
fn computes_total_pages_for_flow_page() {
    let page = FlowPage {
        records: Vec::new(),
        total: 21,
        page: 1,
        page_size: 10,
        skipped: 0,
    };
    assert_eq!(page.total_pages(), 3);

    let empty = FlowPage {
        records: Vec::new(),
        total: 0,
        page: 1,
        page_size: 10,
        skipped: 0,
    };
    assert_eq!(empty.total_pages(), 1);
}

/// 極端的 `total` 與每頁筆數：不得除以零，也不得以 `as u32` 靜默截斷。
#[test]
fn clamps_total_pages_for_hostile_totals() {
    let zero_size = FlowPage {
        records: Vec::new(),
        total: 100,
        page: 1,
        page_size: 0,
        skipped: 0,
    };
    assert_eq!(zero_size.total_pages(), 1);

    // `total` 遠超 `u32` 可表示範圍時以上限表示，不截斷成看似合理的錯誤頁數。
    let huge = FlowPage {
        records: Vec::new(),
        total: u64::MAX,
        page: 1,
        page_size: 20,
        skipped: 0,
    };
    assert_eq!(huge.total_pages(), u32::MAX);

    // 可正常表示的範圍不受影響。
    let large = FlowPage {
        records: Vec::new(),
        total: 1_000_000,
        page: 1,
        page_size: 20,
        skipped: 0,
    };
    assert_eq!(large.total_pages(), 50_000);
}

#[test]
fn unknown_semester_names_do_not_produce_a_zero_ordinal() {
    let semester = Semester {
        semester_id: "9".to_owned(),
        academic_year: "2026-2027".to_owned(),
        semester_name: "夏季学期".to_owned(),
        start_date: "2026-07-01".to_owned(),
        end_date: None,
    };
    assert_eq!(semester.term_name(), None, "不得臆造 2026-2027-0");
    assert_eq!(semester.display_label(), "夏季学期", "显示时回退原始名称");

    let blank = Semester {
        semester_name: "   ".to_owned(),
        ..semester
    };
    assert_eq!(blank.term_name(), None);
    assert_eq!(blank.display_label(), "未知学期");
}

/// 達到分頁上限且仍有記錄未取完時，必須標記截斷（不得靜默傳回部分資料）。
#[test]
fn records_between_marks_truncation_at_the_page_cap() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::{FakeClient, json};
    use crate::session::{AccessMode, SessionManager, SiteKind};

    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let client = Arc::new(FakeClient::with_responder(move |request: &HttpRequest| {
        assert!(
            request.url.contains("attendance-records"),
            "不应有其他请求：{}",
            request.url
        );
        counter.fetch_add(1, Ordering::SeqCst);
        let rows: Vec<serde_json::Value> = (0..100)
            .map(|index| {
                serde_json::json!({
                    "resultId": index,
                    "startSection": 1,
                    "endSection": 2,
                    "courseWeek": 1,
                    "attendanceStatus": "NORMAL",
                    "attendanceDate": "2026-09-01",
                })
            })
            .collect();
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": { "rows": rows, "total": 2500 },
        })))
    }));
    let direct: Arc<dyn HttpClient> = client.clone();
    let webvpn: Arc<dyn HttpClient> = client;
    let config = Config {
        access_policy: AccessPolicy::Direct,
        ..Config::default()
    };
    let mut session = SessionManager::with_clients(&config, direct, webvpn);
    session.register(Box::new(AttendanceSite));
    session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    let mut api = AttendanceApi::new(&mut session);

    let start = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("日期");
    let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 7).expect("日期");
    let batch = api.records_between(start, end).expect("分页查询应成功");
    assert!(batch.truncated, "达到分页上限时应标记截断");
    assert_eq!(batch.records.len(), 2000, "至多 20 页 × 100 笔");
    assert_eq!(requests.load(Ordering::SeqCst), 20, "恰应请求 20 页");
}

/// `total` 說有記錄、回應卻沒有 `rows` 陣列：不得當成空頁一路翻到上限。
///
/// 當成空陣列會白跑 20 頁請求、最後以「超過分頁上限」回報且沒有任何記錄，把
/// 協定問題誤報成資料問題；`total` 為 0 時缺 `rows` 則是正常的。
#[test]
fn missing_rows_array_follows_total() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::{FakeClient, json};
    use crate::session::{AccessMode, SessionManager, SiteKind};

    let start = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("日期");
    let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 7).expect("日期");

    // 同一個假客戶端：課程考勤記錄說有 250 筆、流水說沒有記錄。
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let client = Arc::new(FakeClient::with_responder(move |request: &HttpRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        let total = if request.url.contains("attendance-records") {
            250
        } else {
            0
        };
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": { "total": total },
        })))
    }));
    let direct: Arc<dyn HttpClient> = client.clone();
    let webvpn: Arc<dyn HttpClient> = client;
    let config = Config {
        access_policy: AccessPolicy::Direct,
        ..Config::default()
    };
    let mut session = SessionManager::with_clients(&config, direct, webvpn);
    session.register(Box::new(AttendanceSite));
    session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    let mut api = AttendanceApi::new(&mut session);

    let err = api
        .records_between(start, end)
        .expect_err("total 与回应矛盾时应回报协议错误");
    assert!(matches!(err, AppError::Protocol(_)), "{err:?}");
    assert!(err.to_string().contains("记录数组"), "{err}");
    assert_eq!(requests.load(Ordering::SeqCst), 1, "不应重试或继续翻页");

    // total 為 0 時缺 rows 是正常回應：空結果、不標截斷、只查一次。
    let page = api.flow_page(1, 100).expect("total 为 0 时应视为空页");
    assert!(page.records.is_empty());
    assert_eq!(page.total, 0);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

/// 流水回應說還有記錄、這一頁卻是空的：標記為不完整並停止，不重複翻頁。
#[test]
fn empty_page_stops_the_pagination_early() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::{FakeClient, json};
    use crate::session::{AccessMode, SessionManager, SiteKind};

    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let client = Arc::new(FakeClient::with_responder(move |_request: &HttpRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        // total 一直說還有 250 筆，但每一頁都回空陣列。
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": { "rows": [], "total": 250 },
        })))
    }));
    let direct: Arc<dyn HttpClient> = client.clone();
    let webvpn: Arc<dyn HttpClient> = client;
    let config = Config {
        access_policy: AccessPolicy::Direct,
        ..Config::default()
    };
    let mut session = SessionManager::with_clients(&config, direct, webvpn);
    session.register(Box::new(AttendanceSite));
    session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    let mut api = AttendanceApi::new(&mut session);

    let start = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("日期");
    let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 7).expect("日期");
    let batch = api.records_between(start, end).expect("查询本身应成功");

    assert!(batch.records.is_empty());
    assert!(batch.truncated, "回应不完整时必须标记截断");
    assert_eq!(requests.load(Ordering::SeqCst), 1, "空页不应重复请求");
}

/// 流水旗標 `effective` 寬容讀取：字串形式與 `isEffective` 別名都要接受。
#[test]
fn parses_the_effective_flag_leniently() {
    let value = serde_json::json!([
        {"id": 1, "effective": true},
        {"id": 2, "effective": "1"},
        {"id": 3, "effective": "false"},
        {"id": 4, "isEffective": "yes"},
        {"id": 5},
        {"id": 6, "effective": {"unexpected": true}},
    ]);

    let records: Vec<FlowRecord> =
        crate::sites::deserialize_value(value, "查询考勤流水").expect("应可解析");
    assert_eq!(records.len(), 6, "单笔型别异常不得让整页失败");
    assert!(records[0].effective, "true 视为有效");
    assert!(records[1].effective, "\"1\" 视为有效");
    assert!(!records[2].effective, "\"false\" 视为无效");
    assert!(records[3].effective, "isEffective 别名应被接受");
    assert!(!records[4].effective, "缺字段保守视为无效");
    assert!(!records[5].effective, "无法解读的型别视为无效");
}

/// 只用來建立一個已登入的考勤工作階段與假客戶端。
fn attendance_api_with(
    responder: impl Fn(&HttpRequest) -> crate::error::AppResult<crate::http::HttpResponse>
    + Send
    + Sync
    + 'static,
) -> (
    SessionManager,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::FakeClient;
    use crate::session::{AccessMode, SessionManager, SiteKind};

    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let client = Arc::new(FakeClient::with_responder(move |request: &HttpRequest| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        responder(request)
    }));
    let direct: Arc<dyn HttpClient> = client.clone();
    let webvpn: Arc<dyn HttpClient> = client;
    let config = Config {
        access_policy: AccessPolicy::Direct,
        ..Config::default()
    };
    let mut session = SessionManager::with_clients(&config, direct, webvpn);
    session.register(Box::new(AttendanceSite));
    session.mark_logged_in(SiteKind::Attendance, AccessMode::Direct, Vec::new());
    (session, requests)
}

/// 課程考勤記錄逐項寬容：讀不出來的記錄只跳過該筆，不得讓整頁失敗。
///
/// 修復前是整批嚴格解析（`deserialize_value`），而 `resultId` 原本還是必填的
/// 嚴格欄位：任何一筆型別異常都會讓課表頁直接顯示失敗，連帶所有課程都沒有考勤
/// 狀態。`resultId` 與 `courseWeek` 目前不參與比對，讀不出來也不該丟棄記錄。
#[test]
fn records_tolerate_loosely_typed_and_unused_fields() {
    use crate::http::fake::json;

    let (mut session, requests) = attendance_api_with(|_request| {
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "total": 4,
                "rows": [
                    // `resultId` 是物件、`courseWeek` 缺失：仍保留這筆記錄。
                    {
                        "resultId": {"unexpected": true},
                        "startSection": 1,
                        "endSection": 2,
                        "attendanceStatus": "NORMAL",
                        "attendanceDate": "2026-09-01"
                    },
                    // 參與比對的欄位是字串形式：接受。
                    {
                        "resultId": "9001",
                        "startSection": "3",
                        "endSection": "4",
                        "courseWeek": "5",
                        "attendanceStatus": "LATE",
                        "attendanceDate": "2026-09-02"
                    },
                    // 缺 `attendanceStatus`：無法比對，跳過。
                    {
                        "resultId": 9002,
                        "startSection": 1,
                        "endSection": 2,
                        "attendanceDate": "2026-09-03"
                    },
                    // 根本不是物件：跳過。
                    "not an object"
                ]
            }
        })))
    });
    let mut api = AttendanceApi::new(&mut session);

    let start = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("日期");
    let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 7).expect("日期");
    let batch = api
        .records_between(start, end)
        .expect("单笔异常不得让整页失败");

    assert_eq!(batch.records.len(), 2, "只有能用于比对的记录被保留");
    assert_eq!(batch.skipped, 2, "无法解析的笔数必须保留");
    assert!(!batch.truncated, "取满 total 就不该标记截断");
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "跳过的记录不应造成额外翻页"
    );
    assert_eq!(batch.records[0].result_id, "", "读不出来的识别码视为空");
    assert_eq!(batch.records[0].course_week, 0, "读不出来的周次视为 0");
    assert_eq!(batch.records[1].result_id, "9001");
    assert_eq!(batch.records[1].start_section, 3);
    assert_eq!(batch.records[1].course_week, 5);
}

/// 流水逐項寬容：讀不出來的流水只跳過該筆，並回報筆數。
#[test]
fn flow_page_tolerates_loosely_typed_and_unused_fields() {
    use crate::http::fake::json;

    let (mut session, requests) = attendance_api_with(|_request| {
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "total": 3,
                "rows": [
                    {"id": 1, "effective": true, "classroomName": "主楼A101"},
                    // `id` 是物件：本程式不以識別碼判斷任何事，仍保留這筆。
                    {"id": {"unexpected": true}, "effective": "yes"},
                    "not an object"
                ]
            }
        })))
    });
    let mut api = AttendanceApi::new(&mut session);

    let page = api.flow_page(1, 100).expect("单笔异常不得让整页失败");
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.skipped, 1);
    assert_eq!(page.total, 3);
    assert_eq!(page.records[0].id, "1");
    assert_eq!(page.records[1].id, "", "读不出来的识别码视为空");
    assert!(page.records[1].effective, "isEffective 之外的字段不受影响");
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// 考勤日期接受 `-` 與 `/` 分隔，解析時一律正規化為 `YYYY-MM-DD`。
///
/// 比對以日期字串全等為鍵：分隔符一變（`2026/10/07`）就會全數失配，每一堂已過
/// 的課都變成「待核实」。無法解讀的日期則由 `parse_lenient` 跳過並計數。
#[test]
fn normalizes_attendance_dates_and_skips_unreadable_ones() {
    use crate::http::fake::json;

    let (mut session, _requests) = attendance_api_with(|_request| {
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "total": 3,
                "rows": [
                    {
                        "resultId": 1,
                        "startSection": 1,
                        "endSection": 2,
                        "attendanceStatus": "NORMAL",
                        "attendanceDate": "2026/09/01"
                    },
                    {
                        "resultId": 2,
                        "startSection": 1,
                        "endSection": 2,
                        "attendanceStatus": "NORMAL",
                        "attendanceDate": " 2026-09-02 "
                    },
                    {
                        "resultId": 3,
                        "startSection": 1,
                        "endSection": 2,
                        "attendanceStatus": "NORMAL",
                        "attendanceDate": "下周一"
                    }
                ]
            }
        })))
    });
    let mut api = AttendanceApi::new(&mut session);

    let start = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).expect("日期");
    let end = chrono::NaiveDate::from_ymd_opt(2026, 9, 7).expect("日期");
    let batch = api.records_between(start, end).expect("查询应成功");

    assert_eq!(batch.records.len(), 2);
    assert_eq!(batch.skipped, 1, "无法解读的日期应被跳过并计数");
    assert_eq!(
        batch.records[0].attendance_date, "2026-09-01",
        "斜杠应被正规化"
    );
    assert_eq!(
        batch.records[1].attendance_date, "2026-09-02",
        "前后空白应被去除"
    );
}

/// 課表逐項寬容：讀不出來的課程只跳過該門，不得讓整份課表失敗。
#[test]
fn weekly_courses_skips_unparsable_items() {
    use crate::http::fake::json;

    let (mut session, _requests) = attendance_api_with(|_request| {
        Ok(json(serde_json::json!({
            "code": 0,
            "message": "ok",
            "data": {
                "courses": [
                    {
                        "courseName": "编译原理",
                        "teacherName": "李老师",
                        "classroomName": "主楼A101",
                        "dayOfWeek": 1,
                        "startSection": 1,
                        "endSection": 2,
                        "weekRanges": "1-16"
                    },
                    // 缺 `dayOfWeek`（參與比對）：跳過這門課。
                    {"courseName": "缺星期", "startSection": 3, "endSection": 4},
                    {"courseName": "上课时间不是数字", "dayOfWeek": "x", "startSection": 3, "endSection": 4}
                ]
            }
        })))
    });
    let mut api = AttendanceApi::new(&mut session);

    let (courses, skipped) = api
        .weekly_courses("9")
        .expect("单门课程异常不得让整份课表失败");
    assert_eq!(courses.len(), 1);
    assert_eq!(skipped, 2, "无法解析的课程数必须保留");
    assert_eq!(courses[0].course_name, "编译原理");
    assert_eq!(courses[0].day_of_week, 1);
}
