//! 學期代碼映射與判定測試。

use chrono::NaiveDate;

use super::*;
use crate::sites::lms::LmsCourse;

fn date(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("固定日期")
}

fn course(id: &str, semester_code: Option<&str>) -> LmsCourse {
    let semester = match semester_code {
        Some(code) => serde_json::json!({ "code": code }),
        None => serde_json::Value::Null,
    };
    serde_json::from_value(serde_json::json!({
        "id": id,
        "name": format!("课程 {id}"),
        "semester": semester,
    }))
    .expect("构造课程")
}

#[test]
fn parses_and_formats_term_codes() {
    let term = TermCode::parse("2026-2027-1").expect("解析");
    assert_eq!(term.start_year(), 2026);
    assert_eq!(term.ordinal(), 1);
    assert_eq!(term.to_string(), "2026-2027-1");
    assert_eq!(term.label(), "2026-2027 学年 第 1 学期");

    // 非相鄰學年、欄位不足、非數字都必須拒絕。
    assert_eq!(TermCode::parse("2026-2028-1"), None);
    assert_eq!(TermCode::parse("2026-2027"), None);
    assert_eq!(TermCode::parse("2026-2027-0"), None);
    assert_eq!(TermCode::parse("abc"), None);
    assert_eq!(TermCode::parse("2026-2027-1-2"), None);
}

/// 起始學年不得取到 `u16::MAX`：`label()`／`Display` 會輸出 `start_year + 1`。
///
/// 修復前 `TermCode::parse("65535-65535-1")` 與 `from_lms_code("65535-1")` 都能
/// 造出該值，debug 版本會在字串格式化時因溢位 panic（release 則靜默回繞）。
#[test]
fn rejects_start_years_that_cannot_be_incremented() {
    assert_eq!(TermCode::parse("65535-65535-1"), None);
    assert_eq!(TermCode::parse("65535-65536-1"), None);
    assert_eq!(TermCode::from_lms_code("65535-1"), None);

    // 上限的前一年仍可正常運作。
    let term = TermCode::parse("65534-65535-1").expect("解析");
    assert_eq!(term.label(), "65534-65535 学年 第 1 学期");
    assert_eq!(term.to_string(), "65534-65535-1");
}

#[test]
fn maps_lms_semester_codes() {
    assert_eq!(
        TermCode::from_lms_code("2023-1"),
        TermCode::parse("2023-2024-1")
    );
    assert_eq!(
        TermCode::from_lms_code("2026-2"),
        TermCode::parse("2026-2027-2")
    );
    assert_eq!(TermCode::from_lms_code("2023"), None);
    assert_eq!(TermCode::from_lms_code("abc-1"), None);
    assert_eq!(TermCode::from_lms_code("2023-0"), None);
}

#[test]
fn chosen_term_wins_over_attendance_and_remembered() {
    let chosen = TermCode::parse("2025-2026-2").expect("解析");
    let attendance = TermCode::parse("2026-2027-1").expect("解析");
    let remembered = TermCode::parse("2024-2025-1").expect("解析");
    assert_eq!(
        resolve_term(
            Some(chosen),
            Some(attendance),
            Some(remembered),
            date("2026-09-28")
        ),
        TermResolution::Resolved {
            term: chosen,
            source: TermSource::Chosen,
        },
        "本次明确选择应优先于考勤当前学期"
    );
    assert_eq!(
        resolve_term(None, Some(attendance), Some(remembered), date("2026-09-28")),
        TermResolution::Resolved {
            term: attendance,
            source: TermSource::Attendance,
        },
        "没有明确选择时以考勤为权威"
    );
    assert_eq!(
        resolve_term(None, None, Some(remembered), date("2026-09-28")),
        TermResolution::Resolved {
            term: remembered,
            source: TermSource::Remembered,
        }
    );
    assert_eq!(
        resolve_term(None, None, None, date("2026-09-28")),
        TermResolution::NeedsChoice {
            suggestion: TermCode::parse("2026-2027-1"),
        },
        "都无法判定时提供建议但不得自行采用"
    );
}

#[test]
fn suggests_recent_term_by_month() {
    assert_eq!(
        suggest_term(date("2026-09-28")),
        TermCode::parse("2026-2027-1")
    );
    assert_eq!(
        suggest_term(date("2026-12-31")),
        TermCode::parse("2026-2027-1")
    );
    assert_eq!(
        suggest_term(date("2027-01-15")),
        TermCode::parse("2026-2027-1")
    );
    assert_eq!(
        suggest_term(date("2027-03-01")),
        TermCode::parse("2026-2027-2")
    );
    assert_eq!(
        suggest_term(date("2027-07-10")),
        TermCode::parse("2026-2027-3")
    );
}

#[test]
fn extracts_course_terms() {
    let courses = vec![
        course("1", Some("2026-1")),
        course("2", Some("2026-1")),
        course("3", Some("2025-2")),
        course("4", None),
        course("5", Some("bad-code")),
    ];

    assert_eq!(course_term(&courses[0]), TermCode::parse("2026-2027-1"));
    assert_eq!(course_term(&courses[3]), None);
    assert_eq!(course_term(&courses[4]), None);

    assert_eq!(
        course_terms(&courses),
        vec![
            TermCode::parse("2026-2027-1").unwrap(),
            TermCode::parse("2025-2026-2").unwrap(),
        ],
        "去重并由新到旧"
    );
}

#[test]
fn term_options_include_the_current_term_when_missing_from_courses() {
    let courses = vec![course("1", Some("2026-1")), course("2", Some("2025-2"))];

    // 目前學期不在課程中（例如沿用上次選擇）時仍須列入可選，且維持由新到舊。
    let missing = TermCode::parse("2024-2025-2").expect("解析");
    assert_eq!(
        term_options(&courses, missing),
        vec![
            TermCode::parse("2026-2027-1").unwrap(),
            TermCode::parse("2025-2026-2").unwrap(),
            missing,
        ]
    );

    // 已在課程清單中時不得重複加入。
    let present = TermCode::parse("2026-2027-1").expect("解析");
    assert_eq!(
        term_options(&courses, present),
        vec![
            TermCode::parse("2026-2027-1").unwrap(),
            TermCode::parse("2025-2026-2").unwrap(),
        ]
    );
}

#[test]
fn splits_courses_by_term_and_counts_missing_semesters() {
    let courses = vec![
        course("1", Some("2026-1")),
        course("2", Some("2025-2")),
        course("3", None),
        course("4", Some("bad-code")),
        course("5", Some("2026-1")),
    ];
    let term = TermCode::parse("2026-2027-1").expect("解析");

    let (included, skipped) = courses_for_term(courses, term);
    assert_eq!(
        included
            .iter()
            .map(|course| course.id.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "5"],
        "只纳入目标学期的课程"
    );
    assert_eq!(skipped, 2, "缺少或无法解析学期的课程计入跳过数");
}
