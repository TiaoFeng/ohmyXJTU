//! 課程列表的學期分區（視圖模型，與網路無關）。

use crate::domain::semester::{TermCode, course_term};
use crate::sites::lms::LmsCourse;

/// 課程列表的一列；分區標題列不可選取。
#[derive(Debug, Clone)]
pub enum CourseRow<'a> {
    /// 分區標題列。
    Header(String),
    /// 課程列。
    Course {
        /// 課程在原始清單中的索引（選取與開啟使用）。
        course_index: usize,
        /// 課程。
        course: &'a LmsCourse,
        /// 是否為非當前學期的歷史課程。
        historical: bool,
        /// 是否無法解析學期。
        unknown: bool,
    },
}

/// 產生課程列：當前學期置頂、其次歷史課程（學期新到舊）、最後是學期未知。
///
/// `current` 為 `None`（無法判定當前學期）時維持原始順序且不插入標題列；
/// 同一學期內維持伺服器原始順序（穩定排序）。
pub fn course_rows(courses: &[LmsCourse], current: Option<TermCode>) -> Vec<CourseRow<'_>> {
    let Some(current) = current else {
        return courses
            .iter()
            .enumerate()
            .map(|(course_index, course)| CourseRow::Course {
                course_index,
                course,
                historical: false,
                unknown: false,
            })
            .collect();
    };

    let mut rows = Vec::new();

    let current_rows: Vec<(usize, &LmsCourse)> = courses
        .iter()
        .enumerate()
        .filter(|(_, course)| course_term(course) == Some(current))
        .collect();
    if !current_rows.is_empty() {
        rows.push(CourseRow::Header(format!("当前学期 · {}", current.label())));
        rows.extend(
            current_rows
                .into_iter()
                .map(|(course_index, course)| CourseRow::Course {
                    course_index,
                    course,
                    historical: false,
                    unknown: false,
                }),
        );
    }

    let mut history: Vec<(usize, &LmsCourse, TermCode)> = courses
        .iter()
        .enumerate()
        .filter_map(|(course_index, course)| {
            let term = course_term(course)?;
            (term != current).then_some((course_index, course, term))
        })
        .collect();
    if !history.is_empty() {
        // 由新到舊；`sort_by_key` 穩定，同學期維持原始順序。
        history.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        rows.push(CourseRow::Header("历史课程".to_owned()));
        rows.extend(
            history
                .into_iter()
                .map(|(course_index, course, _)| CourseRow::Course {
                    course_index,
                    course,
                    historical: true,
                    unknown: false,
                }),
        );
    }

    let unknown: Vec<(usize, &LmsCourse)> = courses
        .iter()
        .enumerate()
        .filter(|(_, course)| course_term(course).is_none())
        .collect();
    if !unknown.is_empty() {
        rows.push(CourseRow::Header("学期未知".to_owned()));
        rows.extend(
            unknown
                .into_iter()
                .map(|(course_index, course)| CourseRow::Course {
                    course_index,
                    course,
                    historical: true,
                    unknown: true,
                }),
        );
    }

    rows
}

/// 課程索引在列模型中的位置（供清單選取映射；標題列不列入）。
pub fn visual_index(rows: &[CourseRow<'_>], course_index: usize) -> Option<usize> {
    rows.iter().position(
        |row| matches!(row, CourseRow::Course { course_index: index, .. } if *index == course_index),
    )
}

#[cfg(test)]
#[path = "tests/course_list_test.rs"]
mod course_list_test;
