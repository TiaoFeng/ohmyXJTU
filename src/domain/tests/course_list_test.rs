//! 課程分區測試：當前學期置頂、歷史學期排序、未知置底與選取映射。

use super::*;
use crate::sites::lms::models::LmsSemester;

fn course(id: &str, code: Option<&str>) -> LmsCourse {
    LmsCourse {
        id: id.to_owned(),
        name: format!("课程{id}"),
        course_code: None,
        instructors: Vec::new(),
        semester: code.map(|code| LmsSemester {
            id: Some("s".to_owned()),
            code: Some(code.to_owned()),
            name: None,
            real_name: None,
        }),
        academic_year: None,
    }
}

fn term(text: &str) -> TermCode {
    TermCode::parse(text).expect("学期")
}

fn course_order(rows: &[CourseRow<'_>]) -> Vec<usize> {
    rows.iter()
        .filter_map(|row| match row {
            CourseRow::Course { course_index, .. } => Some(*course_index),
            CourseRow::Header(_) => None,
        })
        .collect()
}

#[test]
fn current_term_comes_first_with_history_sorted_newest_first() {
    let courses = vec![
        course("1", Some("2025-2")), // 歷史（較新）
        course("2", Some("2026-1")), // 當前學期
        course("3", None),           // 學期未知
        course("4", Some("2025-1")), // 歷史（較舊）
    ];
    let rows = course_rows(&courses, Some(term("2026-2027-1")));

    let headers: Vec<&str> = rows
        .iter()
        .filter_map(|row| match row {
            CourseRow::Header(text) => Some(text.as_str()),
            CourseRow::Course { .. } => None,
        })
        .collect();
    assert_eq!(
        headers,
        vec![
            "当前学期 · 2026-2027 学年 第 1 学期",
            "历史课程",
            "学期未知"
        ],
        "應有當前、歷史、未知三個分區"
    );
    assert_eq!(
        course_order(&rows),
        vec![1, 0, 3, 2],
        "順序應為當前學期 → 歷史（新到舊）→ 未知"
    );

    let historical: Vec<bool> = rows
        .iter()
        .filter_map(|row| match row {
            CourseRow::Course { historical, .. } => Some(*historical),
            CourseRow::Header(_) => None,
        })
        .collect();
    assert_eq!(
        historical,
        vec![false, true, true, true],
        "當前學期之外的課程（含未知）都用歷史樣式"
    );
}

#[test]
fn no_current_term_keeps_flat_original_order() {
    let courses = vec![
        course("1", Some("2025-2")),
        course("2", None),
        course("3", Some("2026-1")),
    ];
    let rows = course_rows(&courses, None);

    assert!(
        rows.iter().all(|row| matches!(
            row,
            CourseRow::Course {
                historical: false,
                unknown: false,
                ..
            }
        )),
        "無法判定學期時不得標記歷史或未知"
    );
    assert_eq!(course_order(&rows), vec![0, 1, 2], "應維持原始順序");
}

#[test]
fn visual_index_skips_headers_and_maps_every_course() {
    let courses = vec![
        course("1", Some("2025-2")),
        course("2", Some("2026-1")),
        course("3", None),
    ];
    let rows = course_rows(&courses, Some(term("2026-2027-1")));

    assert_eq!(visual_index(&rows, 1), Some(1), "當前學期課程緊接標題列");
    assert_eq!(
        visual_index(&rows, 0),
        Some(3),
        "歷史課程在第二個標題列之後"
    );
    assert_eq!(
        visual_index(&rows, 2),
        Some(5),
        "未知課程在第三個標題列之後"
    );
    assert_eq!(visual_index(&rows, 9), None, "不存在的課程沒有位置");
}
