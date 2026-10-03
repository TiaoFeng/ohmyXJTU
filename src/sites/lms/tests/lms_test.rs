//! 思源學堂資料模型與使用者首頁解析測試（使用脫敏的固定回應）。

use serde_json::json;

use super::*;

/// 只取正文純文字（多數斷言不關心是否含圖片）。
fn body_text(activity: &LmsActivity) -> Option<String> {
    activity.body().and_then(|body| body.text)
}

#[test]
fn course_homework_url_uses_verified_route_without_hash() {
    assert_eq!(
        course_homework_url("27465").as_deref(),
        Some("https://lms.xjtu.edu.cn/course/27465/homework")
    );
    assert_eq!(
        course_homework_url(" 4711 ").as_deref(),
        Some("https://lms.xjtu.edu.cn/course/4711/homework"),
        "前後空白應先去除"
    );
    // 帶 hash 的啟動網址會讓前端卡死，產出的網址一律不得含 '#'。
    let url = course_homework_url("27465").expect("網址");
    assert!(!url.contains('#'), "網址不得附帶 hash 片段：{url}");
}

#[test]
fn course_homework_url_rejects_unsafe_identifiers() {
    for course_id in ["", "   ", "4 2", "4/2", "../x", "42?x=1", "４２"] {
        assert_eq!(
            course_homework_url(course_id),
            None,
            "識別碼 {course_id:?} 不應被拼接進網址"
        );
    }
}

#[test]
fn safe_identifier_accepts_only_url_safe_tokens() {
    assert_eq!(safe_identifier(" 42 "), Some("42"), "前後空白應去除");
    assert_eq!(safe_identifier("abc-DEF_09"), Some("abc-DEF_09"));
    for value in ["", "   ", "1 2", "a/b", "a?b", "a#b", "中文", "a.b", "\n"] {
        assert_eq!(
            safe_identifier(value),
            None,
            "應拒絕不安全識別碼：{value:?}"
        );
    }
}

#[test]
fn parses_courses_and_skips_incomplete_items() {
    let value = json!([
        {
            "id": 4711,
            "name": "编译原理",
            "course_code": "COMP3001",
            "instructors": [{"id": 7, "name": "李老师"}],
            "academic_year": {"id": 1, "name": "2026-2027"},
            "semester": {"id": 1, "name": "第一学期", "real_name": "秋季学期"}
        },
        {
            "name": "缺少 id 的课程"
        }
    ]);

    let (courses, skipped): (Vec<LmsCourse>, usize) =
        crate::sites::parse_lenient(value, "查询我的课程").unwrap();
    assert_eq!(courses.len(), 1);
    assert_eq!(skipped, 1, "缺少必要欄位的項目應被跳過");
    assert_eq!(courses[0].id, "4711");
    assert_eq!(courses[0].instructor_names(), "李老师");
    assert_eq!(courses[0].semester_label(), "2026-2027 秋季学期");
}

#[test]
fn parses_activities_and_kinds() {
    let value = json!([
        {
            "id": 9001,
            "course_id": "4711",
            "type": "homework",
            "title": "第 3 次作业",
            "end_time": "2026-09-30T15:59:00.000Z",
            "submit_by_group": false
        },
        {
            "id": "9002",
            "course_id": 4711,
            "type": "material",
            "title": "课件"
        }
    ]);

    let (activities, skipped): (Vec<LmsActivity>, usize) =
        crate::sites::parse_lenient(value, "查询课程活动").unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(activities[0].kind(), ActivityKind::Homework);
    assert_eq!(activities[1].kind(), ActivityKind::Material);
    assert_eq!(activities[1].display_title(), "课件");
    assert_eq!(activities[0].course_id.as_deref(), Some("4711"));
    assert_eq!(
        activities[0].end_time.as_deref(),
        Some("2026-09-30T15:59:00.000Z"),
        "模型保留原始字串，換算只發生在解析與顯示層"
    );
}

#[test]
fn classifies_server_types_case_insensitively() {
    assert_eq!(
        ActivityKind::from_server(" Homework "),
        ActivityKind::Homework
    );
    assert_eq!(
        ActivityKind::from_server("LECTURE_LIVE"),
        ActivityKind::LectureLive
    );
    assert_eq!(
        ActivityKind::from_server("Lecture_Live"),
        ActivityKind::LectureLive
    );
    assert_eq!(ActivityKind::from_server("lesson"), ActivityKind::Lesson);
    assert_eq!(ActivityKind::from_server("mystery"), ActivityKind::Unknown);
    assert_eq!(ActivityKind::from_server(""), ActivityKind::Unknown);
}

#[test]
fn parses_submission_lists() {
    let value = json!({
        "list": [
            {"id": 1, "submitted_at": "2026-09-20T10:00:00+08:00", "is_latest_version": true},
            {"id": "2", "created_at": "2026-09-21 09:30:00"}
        ],
        "uploads": []
    });

    let submissions: LmsSubmissionList =
        crate::sites::deserialize_value(value, "查询作业提交记录").unwrap();
    assert_eq!(submissions.count(), 2);
    assert_eq!(
        submissions.list[0].timestamp(),
        Some("2026-09-20T10:00:00+08:00")
    );
    assert_eq!(submissions.list[1].timestamp(), Some("2026-09-21 09:30:00"));
}

/// 活動正文（作業說明）來自詳情回應的嵌套 `data`；列表端不含此區塊。
#[test]
fn parses_activity_body_from_nested_data() {
    // 形態對齊參考實作：作業說明在 data.description（HTML）。
    let value = json!({
        "id": 9001,
        "type": "homework",
        "title": "第 3 次作业",
        "submit_by_group": false,
        "data": {"description": "<p>第一章习题</p><p>交到邮箱</p>", "content": ""}
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    assert_eq!(
        body_text(&activity).as_deref(),
        Some("第一章习题\n交到邮箱"),
        "說明應去除 HTML 標籤"
    );

    // 頁面型活動：description 為空白時改用 content。
    let value = json!({
        "id": 9002,
        "type": "material",
        "title": "课程简介",
        "data": {"description": "   ", "content": "<div>课程介绍</div>"}
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    assert_eq!(body_text(&activity).as_deref(), Some("课程介绍"));

    // 沒有 data（或沒有可見文字）時不得產生說明。
    for value in [
        json!({"id": 1, "type": "homework"}),
        json!({"id": 1, "type": "homework", "data": {}}),
        json!({"id": 1, "type": "homework", "data": {"description": ""}}),
        json!({"id": 1, "type": "homework", "data": {"description": "<p></p>"}}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert_eq!(body_text(&activity), None, "{value}");
    }
}

/// 正文只在 `data` 子物件內；頂層同名欄位（舊版解析的目標）不得被採用。
#[test]
fn ignores_top_level_description_field() {
    let value = json!({
        "id": 9001,
        "type": "homework",
        "description": "<p>顶层字段</p>",
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    assert_eq!(body_text(&activity), None);
}

/// `data` 不是物件時只視為「沒有正文」，不得讓整份活動解析失敗。
///
/// 詳情解析失敗會使該課程的作業全部退回「待核实」，代價遠大於少一段說明。
#[test]
fn tolerates_non_object_activity_data() {
    for value in [
        json!({"id": 9001, "type": "homework", "data": ""}),
        json!({"id": 9001, "type": "homework", "data": "<p>整份是字串</p>"}),
        json!({"id": 9001, "type": "homework", "data": []}),
        json!({"id": 9001, "type": "homework", "data": 123}),
        json!({"id": 9001, "type": "homework", "data": null}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert_eq!(body_text(&activity), None, "{value}");
    }
}

/// 正文子物件「內部」欄位型別異常時只忽略該欄位，不得讓整份活動解析失敗。
///
/// 詳情解析失敗會使該課程的作業全部退回「待核实」（提交狀態也一起失去），代價
/// 遠大於少一段說明。
#[test]
fn tolerates_non_string_activity_body_fields() {
    for value in [
        json!({"id": 9001, "type": "homework", "data": {"description": 123}}),
        json!({"id": 9001, "type": "homework", "data": {"description": {"a": 1}}}),
        json!({"id": 9001, "type": "homework", "data": {"description": ["x"]}}),
        json!({"id": 9001, "type": "homework", "data": {"description": null}}),
        json!({"id": 9001, "type": "homework", "data": {"content": 42}}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert_eq!(body_text(&activity), None, "{value}");
    }

    // 一個欄位型別異常不得影響另一個正常欄位（頁面型活動的正文在 content）。
    let value = json!({
        "id": 9002,
        "type": "material",
        "data": {"description": 999, "content": "<div>课程介绍</div>"}
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    assert_eq!(body_text(&activity).as_deref(), Some("课程介绍"));
}

/// 整份說明只有一張圖片：不得當成「沒有說明」，要讓介面能標註。
#[test]
fn reports_media_only_body() {
    let value = json!({
        "id": 9003,
        "type": "homework",
        "title": "图片作业",
        "data": {"description": "<p><img src=\"/a.png\"></p>"}
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    let body = activity.body().expect("純圖片說明仍應有正文");
    assert_eq!(body.text, None, "圖片沒有可見文字");
    assert!(body.has_media, "應標記含圖片");
}

#[test]
fn extracts_user_id_from_home_page() {
    // globalData 內含巢狀物件、字串中的括號與轉義引號，仍須正確取到 user.id。
    let html = r#"<html><script>
        var globalLoading = false;
        var globalData = {"user":{"id":7788,"name":"张三","note":"} 不是结尾 \" 引号"},"dept":{}};
        var other = {"user":{"id":1}};
    </script></html>"#;
    assert_eq!(user_id_from_page(html).as_deref(), Some("7788"));

    assert_eq!(user_id_from_page("<html>没有 globalData</html>"), None);
    assert_eq!(user_id_from_page(r#"var globalData = {"dept":{}};"#), None);
}

#[test]
fn parses_user_id_from_loose_javascript_page() {
    // 真實頁面：未加引號的鍵、None、尾逗號（參考實作 _parse_js_object 的形態）。
    let html = r#"<html><script>
        var globalData = { user: { id: 4210, name: "张三", dept: None, role: "Student", }, dept: { id: 3 }, locale: "zh-CN" };
    </script></html>"#;
    assert_eq!(user_id_from_page(html).as_deref(), Some("4210"));

    // globalData 無法整體解析時，仍可按 user/dept 邊界擷取 user.id。
    let html =
        r#"<script>var globalData = { flag: getFlag(), user: { id: 9 }, dept: {} };</script>"#;
    assert_eq!(user_id_from_page(html).as_deref(), Some("9"));

    // 字串型別的使用者 ID。
    let html = r#"<script>var globalData = { user: { id: "7788" }, dept: {} };</script>"#;
    assert_eq!(user_id_from_page(html).as_deref(), Some("7788"));

    // 缺少 user.id：不得以其他欄位代替。
    let html = r#"<script>var globalData = { user: { name: "张三" }, dept: {} };</script>"#;
    assert_eq!(user_id_from_page(html), None);
}

#[test]
fn post_login_rejects_maintenance_and_login_pages() {
    use std::sync::Arc;

    use crate::http::HttpResponse;
    use crate::http::fake::FakeClient;
    use crate::session::{AccessMode, PostLogin};

    // 5xx 維護頁：不得標記站點已登入。
    let client = Arc::new(FakeClient::new(vec![HttpResponse::new(
        500,
        "https://lms.xjtu.edu.cn/user/index",
        "<html>系统维护中</html>",
    )]));
    let context = PostLogin::new(client.as_ref(), AccessMode::Direct, None);
    let err = LmsSite
        .post_login(&context)
        .expect_err("維護頁不得視為登入成功");
    assert!(matches!(err, AppError::Http { status: 500 }), "{err:?}");

    // 被導回統一認證：同樣不是登入成功。
    let client = Arc::new(FakeClient::new(vec![HttpResponse::new(
        200,
        "https://login.xjtu.edu.cn/cas/login?service=lms",
        "<html></html>",
    )]));
    let context = PostLogin::new(client.as_ref(), AccessMode::Direct, None);
    let err = LmsSite
        .post_login(&context)
        .expect_err("登入頁不得視為登入成功");
    assert!(matches!(err, AppError::SessionExpired), "{err:?}");

    // 正常首頁：應取得使用者識別碼。
    let client = Arc::new(FakeClient::new(vec![HttpResponse::new(
        200,
        "https://lms.xjtu.edu.cn/user/index",
        r#"<script>var globalData = {"user":{"id":7788},"dept":{}};</script>"#,
    )]));
    let context = PostLogin::new(client.as_ref(), AccessMode::Direct, None);
    let login = LmsSite.post_login(&context).expect("正常首頁應可登入");
    assert_eq!(login.user_id.as_deref(), Some("7788"));
}

#[test]
fn effective_count_excludes_old_versions() {
    let value = json!({
        "list": [
            {"id": 1, "is_latest_version": true},
            {"id": 2, "is_latest_version": false},
            {"id": 3},
            {"id": 4, "is_latest_version": 0},
            {"id": 5, "is_latest_version": "false"}
        ],
    });

    let submissions: LmsSubmissionList =
        crate::sites::deserialize_value(value, "查询作业提交记录").unwrap();
    assert_eq!(submissions.count(), 5);
    assert_eq!(
        submissions.effective_count(),
        2,
        "旧版本（false／0／\"false\"）不计入有效提交"
    );
}

/// 課程識別碼不合法時必須回報協定錯誤，且不得發出任何請求。
#[test]
fn course_activities_rejects_unsafe_identifiers_without_requests() {
    use std::sync::Arc;

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::FakeClient;
    use crate::session::{AccessMode, SessionManager, SiteKind};

    let client = Arc::new(FakeClient::with_responder(|_request: &HttpRequest| {
        panic!("识别码不合法时不得发出请求");
    }));
    let direct: Arc<dyn HttpClient> = client.clone();
    let webvpn: Arc<dyn HttpClient> = client;
    let config = Config {
        access_policy: AccessPolicy::Direct,
        ..Config::default()
    };
    let mut session = SessionManager::with_clients(&config, direct, webvpn);
    session.register(Box::new(LmsSite));
    session.mark_logged_in(SiteKind::Lms, AccessMode::Direct, Vec::new());
    let mut api = LmsApi::new(&mut session);

    for course_id in ["1/2", "42?x=1", "中文"] {
        let err = api
            .course_activities(course_id)
            .expect_err("非法识别码应被拒绝");
        assert!(matches!(err, AppError::Protocol(_)), "{err:?}");
    }
}
