//! 學期代碼：解析、映射與判定。
//!
//! 考勤系統以「`2026-2027` 學年＋第 T 學期」表示學期（`Semester::term_name()`
//! 產生 `2026-2027-1`）；思源學堂的課程則以 `semester.code`（形如 `2026-1`，
//! 意為 2026 起始學年的第 1 學期）標註歸屬。本模組集中兩者的映射，
//! 並在無法自動判定當前學期時提供候選與建議供使用者選擇。
//!
//! 注意：兩套系統的學期數字 ID 不可互相比較，必須以本模組的 [`TermCode`]
//! 為中介。

use chrono::{Datelike as _, NaiveDate};

use crate::sites::lms::LmsCourse;

/// 學期代碼：`YYYY-YYYY+1-T`（例：`2026-2027-1`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermCode {
    start_year: u16,
    ordinal: u8,
}

impl TermCode {
    /// 由起始學年與學期序（1 起）建立。
    pub fn new(start_year: u16, ordinal: u8) -> Option<Self> {
        if start_year == 0 || ordinal == 0 {
            return None;
        }
        Some(Self {
            start_year,
            ordinal,
        })
    }

    /// 解析 `YYYY-YYYY+1-T` 形式（考勤系統的 `term_name()` 即為此形式）。
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('-');
        let start_year: u16 = parts.next()?.parse().ok()?;
        let end_year: u16 = parts.next()?.parse().ok()?;
        let ordinal: u8 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || end_year != start_year + 1 {
            return None;
        }
        Self::new(start_year, ordinal)
    }

    /// 解析思源學堂的學期碼（形如 `2026-1`）。
    pub fn from_lms_code(code: &str) -> Option<Self> {
        let (year, ordinal) = code.trim().split_once('-')?;
        Self::new(year.parse().ok()?, ordinal.parse().ok()?)
    }

    /// 起始學年。
    pub fn start_year(self) -> u16 {
        self.start_year
    }

    /// 學期序（1 起）。
    pub fn ordinal(self) -> u8 {
        self.ordinal
    }

    /// 顯示標籤，例如 `2026-2027 学年 第 1 学期`。
    pub fn label(self) -> String {
        format!(
            "{}-{} 学年 第 {} 学期",
            self.start_year,
            self.start_year + 1,
            self.ordinal
        )
    }
}

impl std::fmt::Display for TermCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}-{}-{}",
            self.start_year,
            self.start_year + 1,
            self.ordinal
        )
    }
}

/// 當前學期的判定來源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermSource {
    /// 考勤系統的當前學期（權威來源）。
    Attendance,
    /// 使用者上次的選擇（保存在設定檔）。
    Remembered,
    /// 使用者本次選擇。
    Chosen,
}

impl TermSource {
    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Attendance => "考勤系统",
            Self::Remembered => "上次选择",
            Self::Chosen => "本次选择",
        }
    }
}

/// 學期判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermResolution {
    /// 已判定。
    Resolved {
        /// 學期。
        term: TermCode,
        /// 判定來源。
        source: TermSource,
    },
    /// 無法判定，需使用者選擇；`suggestion` 僅作預選，不得靜默採用。
    NeedsChoice {
        /// 依日期推算的建議學期。
        suggestion: Option<TermCode>,
    },
}

/// 依優先序判定要載入的學期。
///
/// 考勤系統的當前學期為權威來源；其次沿用使用者上次的選擇；兩者皆無時
/// 回報 [`TermResolution::NeedsChoice`]，由介面顯示選擇器。
pub fn resolve_term(
    attendance: Option<TermCode>,
    remembered: Option<TermCode>,
    today: NaiveDate,
) -> TermResolution {
    if let Some(term) = attendance {
        return TermResolution::Resolved {
            term,
            source: TermSource::Attendance,
        };
    }
    if let Some(term) = remembered {
        return TermResolution::Resolved {
            term,
            source: TermSource::Remembered,
        };
    }
    TermResolution::NeedsChoice {
        suggestion: suggest_term(today),
    }
}

/// 依月份推算可能的當前學期（僅供選擇器預選，不得靜默採用）。
///
/// 規則：9–1 月為第一學期、2–6 月為第二學期、7–8 月為第三學期（小學期）。
pub fn suggest_term(today: NaiveDate) -> Option<TermCode> {
    let year = u16::try_from(today.year()).ok()?;
    let (start_year, ordinal) = match today.month() {
        9..=12 => (year, 1),
        1 => (year.checked_sub(1)?, 1),
        2..=6 => (year.checked_sub(1)?, 2),
        7..=8 => (year.checked_sub(1)?, 3),
        _ => return None,
    };
    TermCode::new(start_year, ordinal)
}

/// 課程標註的學期；`None` 代表缺少或無法解析學期資訊。
pub fn course_term(course: &LmsCourse) -> Option<TermCode> {
    course
        .semester
        .as_ref()
        .and_then(|semester| semester.code.as_deref())
        .and_then(TermCode::from_lms_code)
}

/// 課程清單中出現過的學期（由新到舊、去重）。
pub fn course_terms(courses: &[LmsCourse]) -> Vec<TermCode> {
    let mut terms: Vec<TermCode> = courses.iter().filter_map(course_term).collect();
    terms.sort_unstable_by(|left, right| right.cmp(left));
    terms.dedup();
    terms
}

#[cfg(test)]
#[path = "tests/semester_test.rs"]
mod semester_test;
