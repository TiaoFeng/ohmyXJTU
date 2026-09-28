//! 思源學堂資料模型與使用者首頁解析測試（使用脫敏的固定回應）。

use serde_json::json;

use super::*;

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
            "end_time": "2026-09-30T23:59:00+08:00",
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
fn matching_brace_handles_escapes() {
    assert_eq!(matching_brace(r#"{"a":"}"}"#), Some(8));
    assert_eq!(matching_brace(r#"{"a":{"b":1}}"#), Some(12));
    assert_eq!(matching_brace("{"), None);
}
