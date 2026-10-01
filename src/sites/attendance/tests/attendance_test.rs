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
    };
    assert_eq!(page.total_pages(), 3);

    let empty = FlowPage {
        records: Vec::new(),
        total: 0,
        page: 1,
        page_size: 10,
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
    };
    assert_eq!(zero_size.total_pages(), 1);

    // `total` 遠超 `u32` 可表示範圍時以上限表示，不截斷成看似合理的錯誤頁數。
    let huge = FlowPage {
        records: Vec::new(),
        total: u64::MAX,
        page: 1,
        page_size: 20,
    };
    assert_eq!(huge.total_pages(), u32::MAX);

    // 可正常表示的範圍不受影響。
    let large = FlowPage {
        records: Vec::new(),
        total: 1_000_000,
        page: 1,
        page_size: 20,
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
