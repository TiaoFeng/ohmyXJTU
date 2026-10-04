//! 課表週次計算與本週課程。

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Duration, NaiveDate};

use crate::sites::attendance::TimetableCourse;

/// 一般學期的週數。
const REGULAR_TERM_WEEKS: u32 = 22;
/// 小學期（學期編號以 `-3` 結尾）的週數。
const SHORT_TERM_WEEKS: u32 = 8;
/// 解析週次字串時接受的最大週次（資料保護上限，非學期長度）。
const MAX_PARSED_WEEK: u32 = 60;

/// 一門課的固定時段與其上課週次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourseSlot {
    /// 課程名稱。
    pub course_name: String,
    /// 教師。
    pub teacher: Option<String>,
    /// 上課地點。
    pub classroom: Option<String>,
    /// 星期（1 = 週一）。
    pub day_of_week: u32,
    /// 開始節次。
    pub start_section: u32,
    /// 結束節次。
    pub end_section: u32,
    /// 上課週次。
    pub weeks: BTreeSet<u32>,
}

impl CourseSlot {
    /// 該週是否有課。
    pub fn is_in_week(&self, week: u32) -> bool {
        self.weeks.contains(&week)
    }

    /// 該週的上課日期。
    pub fn date_in_week(&self, semester_start: NaiveDate, week: u32) -> Option<NaiveDate> {
        let day_offset = i64::from(self.day_of_week.clamp(1, 7)) - 1;
        let week_offset = i64::from(week.max(1) - 1) * 7;
        semester_start.checked_add_signed(Duration::days(week_offset + day_offset))
    }

    /// 週次字串，例如 `1-4,6-16`。
    pub fn weeks_label(&self) -> String {
        format_weeks(&self.weeks)
    }
}

/// 解析週次字串，例如 `1-4,6,8-16`。
///
/// 無法解析的片段會被忽略，不會讓整份課表失敗。
pub fn parse_weeks(text: &str) -> BTreeSet<u32> {
    let mut weeks = BTreeSet::new();
    for part in text.split([',', '，']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((start, end)) => {
                if let (Ok(start), Ok(end)) =
                    (start.trim().parse::<u32>(), end.trim().parse::<u32>())
                {
                    // 上限保護，避免異常字串產生超大集合。
                    for week in start.min(end)..=end.max(start).min(MAX_PARSED_WEEK) {
                        weeks.insert(week);
                    }
                }
            }
            None => {
                if let Ok(week) = part.parse::<u32>() {
                    weeks.insert(week);
                }
            }
        }
    }
    weeks
}

/// 課程合併鍵：（課名、教師、地點、星期、起節、訖節）。
type CourseKey = (String, String, String, u32, u32, u32);

/// 將課表中的課程按（課名、教師、地點、星期、起節、訖節）合併週次。
///
/// 同一門課可能因週次分段而回傳多筆，必須合併後才能正確判斷「本週是否有課」。
pub fn merge_courses(courses: &[TimetableCourse]) -> Vec<CourseSlot> {
    let mut grouped: BTreeMap<CourseKey, BTreeSet<u32>> = BTreeMap::new();
    for course in courses {
        let key = (
            course.course_name.trim().to_owned(),
            course
                .teacher_name
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            course
                .classroom_name
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            course.day_of_week,
            course.start_section,
            course.end_section,
        );
        let weeks = grouped.entry(key).or_default();
        weeks.extend(parse_weeks(&course.week_ranges));
    }

    grouped
        .into_iter()
        .filter(|(_, weeks)| !weeks.is_empty())
        .map(
            |(
                (course_name, teacher, classroom, day_of_week, start_section, end_section),
                weeks,
            )| {
                CourseSlot {
                    course_name,
                    teacher: non_empty(teacher),
                    classroom: non_empty(classroom),
                    day_of_week,
                    start_section,
                    end_section,
                    weeks,
                }
            },
        )
        .collect()
}

/// 第 `week` 週的週一與週日。
///
/// 以學期開始日為第 1 週第 1 天錨定（與 [`CourseSlot::date_in_week`] 同一套
/// 公式）：無論學期開始日為星期幾，課程日期與考勤查詢範圍都落在同一組日期。
pub fn week_bounds(semester_start: NaiveDate, week: u32) -> Option<(NaiveDate, NaiveDate)> {
    let week_offset = i64::from(week.max(1) - 1) * 7;
    let monday = semester_start.checked_add_signed(Duration::days(week_offset))?;
    let sunday = monday.checked_add_signed(Duration::days(6))?;
    Some((monday, sunday))
}

/// 由學期開始日計算第幾週；日期早於學期時回傳第 1 週。
pub fn week_number(semester_start: NaiveDate, today: NaiveDate) -> u32 {
    let days = (today - semester_start).num_days();
    if days < 0 {
        return 1;
    }
    u32::try_from(days / 7 + 1).unwrap_or(1)
}

/// 學期長度：小學期（`-3` 結尾）為 8 週，其餘為 22 週。
pub fn semester_length(term_name: &str) -> u32 {
    if term_name.trim().ends_with("-3") {
        SHORT_TERM_WEEKS
    } else {
        REGULAR_TERM_WEEKS
    }
}

/// 將週次夾在學期範圍內，避免超出學期的日期被計算出來。
pub fn clamp_week(week: u32, term_name: &str) -> u32 {
    week.clamp(1, semester_length(term_name))
}

/// 學期週數上限：以「課表裡最晚有課的週次」為準（與參考實作的考勤來源一致）；
/// 課表尚無資料時至少涵蓋 `current_week`，避免出現「第 N/M 周」而 N > M。
///
/// `current_week` 必須是**今天的週次**，而不是使用者選定／目前顯示的週次：
/// 上限若跟著選定值走，往回翻週就會讓上限一起縮小，使用者便再也翻不回本週
///（`total_weeks(Some(19), 21)` 是 21，但 `total_weeks(Some(19), 19)` 只剩 19）。
///
/// 刻意**不由學期結束日推算**：考勤入口的結束日涵蓋考試週與假期（實測可到
/// 第 23 週），而教務系統的「總周次」（`ZZC`）考勤 API 並未提供。
pub fn total_weeks(max_course_week: Option<u32>, current_week: u32) -> u32 {
    max_course_week.unwrap_or(0).max(current_week).max(1)
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

/// 將週次集合格式化為 `1-4,6-16`。
fn format_weeks(weeks: &BTreeSet<u32>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut start: Option<u32> = None;
    let mut previous: Option<u32> = None;

    for &week in weeks {
        match (start, previous) {
            (Some(first), Some(last)) if week == last + 1 => {
                previous = Some(week);
                if parts.is_empty() {
                    parts.push(String::new());
                }
                parts.pop();
                parts.push(if first == week {
                    first.to_string()
                } else {
                    format!("{first}-{week}")
                });
            }
            _ => {
                start = Some(week);
                previous = Some(week);
                parts.push(week.to_string());
            }
        }
    }
    parts.join(",")
}

#[cfg(test)]
#[path = "tests/schedule_test.rs"]
mod schedule_test;
