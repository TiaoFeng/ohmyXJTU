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
        "前后空白应先去除"
    );
    // 帶 hash 的啟動網址會讓前端卡死，產出的網址一律不得含 '#'。
    let url = course_homework_url("27465").expect("网址");
    assert!(!url.contains('#'), "网址不得附带 hash 片段：{url}");
}

#[test]
fn course_homework_url_rejects_unsafe_identifiers() {
    for course_id in ["", "   ", "4 2", "4/2", "../x", "42?x=1", "４２"] {
        assert_eq!(
            course_homework_url(course_id),
            None,
            "识别码 {course_id:?} 不应被拼接进网址"
        );
    }
}

#[test]
fn safe_identifier_accepts_only_url_safe_tokens() {
    assert_eq!(safe_identifier(" 42 "), Some("42"), "前后空白应去除");
    assert_eq!(safe_identifier("abc-DEF_09"), Some("abc-DEF_09"));
    for value in ["", "   ", "1 2", "a/b", "a?b", "a#b", "中文", "a.b", "\n"] {
        assert_eq!(
            safe_identifier(value),
            None,
            "应拒绝不安全识别码：{value:?}"
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
    assert_eq!(skipped, 1, "缺少必要字段的项目应被跳过");
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
        "模型保留原始字符串，换算只发生在解析与显示层"
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
        "说明应去除 HTML 标签"
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

/// 正文只在 `data` 子物件內；頂層同名欄位不得被當成正文。
///
/// 但頂層真有內容時必須看得見：那代表本版讀錯了欄位，介面要提示而不是靜默地
/// 看起來「這項活動沒有說明」（見 [`TOP_LEVEL_BODY_NOTE`]）。
#[test]
fn ignores_top_level_description_field() {
    let value = json!({
        "id": 9001,
        "type": "homework",
        "description": "<p>顶层字段</p>",
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    assert_eq!(body_text(&activity), None);
    assert_eq!(
        activity.body().and_then(|body| body.issue),
        Some(TOP_LEVEL_BODY_NOTE)
    );
    // 空白的頂層欄位不算內容，不提示。
    for value in [
        json!({"id": 9001, "type": "homework"}),
        json!({"id": 9001, "type": "homework", "description": "   "}),
        json!({"id": 9001, "type": "homework", "description": null}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert!(activity.body().is_none(), "{value}");
    }
}

/// `data` 不是物件時只視為「沒有正文」，不得讓整份活動解析失敗；但必須提示原因。
///
/// 詳情解析失敗會使該課程的作業全部退回「待核实」，代價遠大於少一段說明；
/// 反過來，靜默地看起來「這項活動沒有說明」也讓人無法判斷欄位假設是否正確。
#[test]
fn tolerates_non_object_activity_data() {
    for value in [
        json!({"id": 9001, "type": "homework", "data": ""}),
        json!({"id": 9001, "type": "homework", "data": "<p>整份是字符串</p>"}),
        json!({"id": 9001, "type": "homework", "data": []}),
        json!({"id": 9001, "type": "homework", "data": 123}),
        json!({"id": 9001, "type": "homework", "data": true}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert_eq!(body_text(&activity), None, "{value}");
        assert_eq!(
            activity.body().and_then(|body| body.issue),
            Some(BODY_NOT_OBJECT_NOTE),
            "{value}"
        );
    }

    // `null` 是明確的「沒有這段內容」，不算型別異常。
    let value = json!({"id": 9001, "type": "homework", "data": null});
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    assert!(activity.body().is_none());
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
    // 正文已由 `content` 取得，不因另一個欄位型別異常而提示。
    assert_eq!(activity.body().and_then(|body| body.issue), None);
}

/// `data` 是物件、但正文欄位型別不符時提示原因（內容被丟棄，不能靜默）。
#[test]
fn reports_unreadable_body_field_types() {
    for value in [
        json!({"id": 9001, "type": "homework", "data": {"description": 123}}),
        json!({"id": 9001, "type": "homework", "data": {"description": {"a": 1}}}),
        json!({"id": 9001, "type": "homework", "data": {"description": ["x"]}}),
        json!({"id": 9001, "type": "homework", "data": {"content": 42}}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert_eq!(body_text(&activity), None, "{value}");
        assert_eq!(
            activity.body().and_then(|body| body.issue),
            Some(BODY_FIELD_TYPE_NOTE),
            "{value}"
        );
    }

    // 明確的空值（`null`）與空字串是「沒有這段內容」，不是型別異常。
    for value in [
        json!({"id": 9001, "type": "homework", "data": {"description": null}}),
        json!({"id": 9001, "type": "homework", "data": {"description": ""}}),
        json!({"id": 9001, "type": "homework", "data": {}}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert!(activity.body().is_none(), "{value}");
    }

    // 附件仍要照常列出，但正文被丟棄的原因不得因為「有東西可顯示」而省略。
    let value = json!({
        "id": 9001, "type": "homework",
        "data": {"description": 7},
        "uploads": [{"id": 1, "name": "题目.pdf", "size": 2048}]
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    let body = activity.body().expect("有附件时仍应显示说明区块");
    assert_eq!(body.attachments.len(), 1, "附件应照常列出");
    assert_eq!(body.issue, Some(BODY_FIELD_TYPE_NOTE));
}

/// 正常取得正文（或確實沒有正文）時不帶任何原因。
#[test]
fn normal_body_has_no_issue() {
    let value = json!({
        "id": 9001,
        "type": "homework",
        "data": {"description": "<p>第一章习题</p>"},
        "uploads": [{"id": 1, "name": "题目.pdf", "size": 2048}]
    });
    let activity: LmsActivity = crate::sites::deserialize_value(value, "查询活动详情").unwrap();
    let body = activity.body().expect("应取得正文");
    assert_eq!(body.text.as_deref(), Some("第一章习题"));
    assert_eq!(body.attachments.len(), 1, "附件仍应带出");
    assert_eq!(body.issue, None);
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
    let body = activity.body().expect("纯图片说明仍应有正文");
    assert_eq!(body.text, None, "图片没有可见文字");
    assert!(body.has_media, "应标记含图片");
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

/// 使用者資訊含代理對（emoji）時仍必須取得到 `user.id`。
///
/// 頁面以 UTF-16 跳脫表示非 BMP 字元（`\uD83D\uDE00`）；若解析器不組合代理對，
/// 整段 `globalData` 會解析失敗，`user_id` 取不到，個人作業的提交查詢全部失敗
/// 而顯示「待核实」。
#[test]
fn parses_user_id_when_the_page_contains_surrogate_pairs() {
    let html = r#"<html><script>
        var globalData = { user: { id: 5150, name: "\uD83D\uDE00 张三" }, dept: {} };
    </script></html>"#;
    assert_eq!(user_id_from_page(html).as_deref(), Some("5150"));

    // 整段無法解析時改按 user/dept 邊界擷取：該路徑同樣要看得到代理對。
    let html = r#"<script>var globalData = { flag: getFlag(), user: { id: 6, tag: "\uDE00" }, dept: {} };</script>"#;
    assert_eq!(user_id_from_page(html).as_deref(), Some("6"));
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
        .expect_err("维护页不得视为登录成功");
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
        .expect_err("登录页不得视为登录成功");
    assert!(matches!(err, AppError::SessionExpired), "{err:?}");

    // 正常首頁：應取得使用者識別碼。
    let client = Arc::new(FakeClient::new(vec![HttpResponse::new(
        200,
        "https://lms.xjtu.edu.cn/user/index",
        r#"<script>var globalData = {"user":{"id":7788},"dept":{}};</script>"#,
    )]));
    let context = PostLogin::new(client.as_ref(), AccessMode::Direct, None);
    let login = LmsSite.post_login(&context).expect("正常首页应可登录");
    assert_eq!(login.user_id.as_deref(), Some("7788"));
}

/// 回應必須看起來像提交列表：`list` 與 `uploads` 都沒有時不可當成「零筆提交」。
///
/// 當成 0 筆會讓作業被判定成「未提交／逾期」（`domain::homework::judge`），
/// 而這正是「無法確認時必須標示待核实」要避免的。少了 `list` 但仍有 `uploads`
///（伺服器在沒有任何提交時的形態）仍視為零筆。
#[test]
fn submission_list_without_expected_fields_is_a_protocol_error() {
    use std::sync::Arc;

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::FakeClient;
    use crate::session::{AccessMode, SessionManager, SiteKind};

    // （回應本文, 是否為合法的提交列表, 有效提交數）
    let cases = [
        (json!({"code": 1, "message": "参数错误"}), false, 0),
        (json!({}), false, 0),
        (json!({"list": []}), true, 0),
        (json!({"uploads": []}), true, 0),
        (
            json!({"list": [{"id": 1, "is_latest_version": true}]}),
            true,
            1,
        ),
    ];

    for (body, valid, expected) in cases {
        let payload = serde_json::to_vec(&body).expect("序列化固定回应");
        let client = Arc::new(FakeClient::with_responder(move |_request: &HttpRequest| {
            Ok(HttpResponse::new(
                200,
                "https://lms.xjtu.edu.cn/api/activities/9001/groups/42/submission_list",
                payload.clone(),
            ))
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

        // 以小組作業查詢：不需要先取得使用者 ID，一次請求即可。
        match api.submissions("9001", true, Some("42")) {
            Ok(list) => {
                assert!(valid, "不该把 {body} 当成提交列表");
                assert_eq!(list.effective_count(), expected, "{body}");
            }
            Err(err) => {
                assert!(!valid, "{body} 应可解析：{err}");
                assert!(matches!(err, AppError::Protocol(_)), "{err:?}");
            }
        }
    }
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

/// 提交記錄的欄位讀不出來時只損失該欄位，整筆記錄必須保留。
///
/// 丟棄整筆會讓「已完成」被誤判成「未提交／逾期」（`domain::homework::judge`）；
/// 參考實作對這些欄位也採寬容讀取（`safeInt`／`safeString`）。
#[test]
fn incomplete_submission_records_are_still_counted() {
    let value = json!({
        "list": [
            {"submitted_at": "2026-09-20 10:00:00", "is_latest_version": true},
            {"id": null, "is_latest_version": true},
            {"id": {"nested": 1}, "is_latest_version": true},
            {"id": 4, "submitted_at": 1758333600, "comment": {"text": "好"}}
        ],
    });

    let submissions: LmsSubmissionList =
        crate::sites::deserialize_value(value, "查询作业提交记录").expect("应可解析");
    assert_eq!(submissions.count(), 4, "字段缺漏或类型异常不得丢弃整笔记录");
    assert_eq!(submissions.skipped, 0);
    assert_eq!(submissions.confirmed_effective_count(), Some(4));
    assert_eq!(submissions.list[0].id, None, "缺字段的 id 视为缺漏");
    assert_eq!(submissions.list[1].id, None, "null 的 id 视为缺漏");
    assert_eq!(submissions.list[3].id.as_deref(), Some("4"));
    assert_eq!(
        submissions.list[0].timestamp(),
        Some("2026-09-20 10:00:00"),
        "时间为字符串时照常显示"
    );
    assert_eq!(
        submissions.list[3].timestamp(),
        None,
        "时间不是字符串时视为未知，不伪造时间"
    );
}

/// 提交記錄整批讀不出來時必須回報「無法確認」，不可當成「零筆提交」。
#[test]
fn unreadable_submission_records_are_reported_as_unknown() {
    use std::sync::Arc;

    use crate::config::{AccessPolicy, Config};
    use crate::http::HttpClient;
    use crate::http::fake::FakeClient;
    use crate::session::{AccessMode, SessionManager, SiteKind};

    let payload = serde_json::to_vec(&json!({"list": [7, null, "x"]})).expect("序列化固定回应");
    let client = Arc::new(FakeClient::with_responder(move |_request: &HttpRequest| {
        Ok(HttpResponse::new(
            200,
            "https://lms.xjtu.edu.cn/api/activities/9001/groups/42/submission_list",
            payload.clone(),
        ))
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

    let list = api
        .submissions("9001", true, Some("42"))
        .expect("查询本身应当成功");
    assert_eq!(list.count(), 0, "没有任何记录能解析");
    assert_eq!(list.skipped, 3, "无法解析的记录数必须保留");
    assert_eq!(
        list.confirmed_effective_count(),
        None,
        "有记录读不出来时不得当成零笔提交"
    );

    // 摘要必須顯示「待核实」而不是「未提交」。
    let detail: LmsActivity = crate::sites::deserialize_value(
        json!({
            "id": "9001",
            "type": "homework",
            "submit_by_group": true,
            "group_id": "42"
        }),
        "查询活动详情",
    )
    .expect("活动详情");
    let summary = api.submission_summary_for(&detail).expect("摘要");
    assert_eq!(summary.count, None, "无法确认时提交数必须为未知");
    let note = summary.note.expect("应说明无法确认的原因");
    assert!(note.contains("3 条提交记录无法解析"), "{note}");
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

/// 附件在詳情回應的**頂層** `uploads`（不在 `data` 底下）；只保留名稱與大小。
#[test]
fn parses_activity_uploads_from_the_detail_top_level() {
    let value = json!({
        "id": 9001,
        "type": "homework",
        "title": "第 3 次作业",
        "data": { "description": "", "content": "" },
        "uploads": [
            {"id": 1, "name": "题目.pdf", "type": "application/pdf", "size": 1234567},
            {"id": 2, "name": "参考答案.docx", "size": "24576"},
            {"id": 3, "name": "   ", "size": null},
            {"id": 4}
        ]
    });
    let activity: LmsActivity =
        crate::sites::deserialize_value(value, "查询活动详情").expect("应可解析");
    assert_eq!(activity.uploads.len(), 4);
    assert_eq!(activity.uploads[0].display_name(), "题目.pdf");
    assert_eq!(activity.uploads[0].size_label().as_deref(), Some("1.2 MB"));
    assert_eq!(activity.uploads[1].display_name(), "参考答案.docx");
    assert_eq!(activity.uploads[1].size_label().as_deref(), Some("24 KB"));
    // 沒有名稱或大小的附件仍留在清單裡，不會因為欄位缺漏就消失。
    assert_eq!(activity.uploads[2].display_name(), "未命名附件");
    assert_eq!(activity.uploads[2].size_label(), None);
    assert_eq!(activity.uploads[3].display_name(), "未命名附件");

    // 正文為空、只有附件時仍算「有可顯示的內容」，介面才不會整段藏起來。
    let content = activity.body().expect("只有附件也是可显示的内容");
    assert_eq!(content.text, None);
    assert_eq!(content.attachments.len(), 4);
}

/// 附件欄位缺漏或型別異常都不影響其他欄位（詳情失敗會讓整批作業退回「待核实」）。
#[test]
fn tolerates_missing_or_malformed_uploads() {
    for value in [
        json!({"id": 1, "type": "homework"}),
        json!({"id": 1, "type": "homework", "uploads": null}),
        json!({"id": 1, "type": "homework", "uploads": {}}),
        json!({"id": 1, "type": "homework", "uploads": "题目.pdf"}),
    ] {
        let activity: LmsActivity = crate::sites::deserialize_value(value.clone(), "查询活动详情")
            .unwrap_or_else(|err| panic!("{value}: {err}"));
        assert!(activity.uploads.is_empty(), "{value}");
    }

    // 個別項目異常時跳過該項，其餘照常解析。
    let value = json!({
        "id": 1,
        "type": "homework",
        "uploads": [{"name": "题目.pdf", "size": 1024}, 7, null]
    });
    let activity: LmsActivity =
        crate::sites::deserialize_value(value, "查询活动详情").expect("应可解析");
    assert_eq!(activity.uploads.len(), 1);
    assert_eq!(activity.uploads[0].size_label().as_deref(), Some("1 KB"));
}

/// 大小顯示沿用參考實作的進位門檻（B／KB／MB）。
#[test]
fn upload_size_labels_follow_the_reference_thresholds() {
    let label = |size: Option<u64>| LmsUpload { name: None, size }.size_label();
    assert_eq!(label(Some(0)).as_deref(), Some("0 B"));
    assert_eq!(label(Some(1023)).as_deref(), Some("1023 B"));
    assert_eq!(label(Some(1024)).as_deref(), Some("1 KB"));
    assert_eq!(label(Some(1024 * 1024 - 1)).as_deref(), Some("1023 KB"));
    assert_eq!(label(Some(1024 * 1024)).as_deref(), Some("1.0 MB"));
    assert_eq!(label(Some(5 * 1024 * 1024)).as_deref(), Some("5.0 MB"));
    assert_eq!(label(None), None);
}
