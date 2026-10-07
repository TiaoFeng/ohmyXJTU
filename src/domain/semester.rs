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
    ///
    /// 拒絕 `u16::MAX` 作為起始學年：`label()` 與 `Display` 都要輸出
    /// `start_year + 1`，而欄位是私有的、這裡又是唯一建構點，在此擋掉即可
    /// 保證該運算永不溢位（外部輸入如思源學堂的學期碼可能任意）。
    pub fn new(start_year: u16, ordinal: u8) -> Option<Self> {
        if start_year == 0 || ordinal == 0 || start_year.checked_add(1).is_none() {
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
        if parts.next().is_some() || end_year != start_year.checked_add(1)? {
            return None;
        }
        Self::new(start_year, ordinal)
    }

    /// 解析思源學堂的學期碼（形如 `2026-1`）。
    pub fn from_lms_code(code: &str) -> Option<Self> {
        let (year, ordinal) = code.trim().split_once('-')?;
        Self::new(year.parse().ok()?, ordinal.parse().ok()?)
    }

    /// 起始學年（僅供測試斷言解析結果）。
    #[cfg(test)]
    pub fn start_year(self) -> u16 {
        self.start_year
    }

    /// 學期序（1 起；僅供測試斷言解析結果）。
    #[cfg(test)]
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
/// 優先序：本次工作階段的明確選擇 → 考勤系統的當前學期（權威）→ 使用者
/// 上次的選擇；皆無時回報 [`TermResolution::NeedsChoice`]，由介面顯示選擇器。
///
/// `chosen` 僅代表「使用者在本工作階段按 `s` 選過的學期」——他明確指定
/// 要看的學期，因此優先於考勤的當前學期；它不持久化，重新開啟程式後仍以
/// 考勤為權威。
pub fn resolve_term(
    chosen: Option<TermCode>,
    attendance: Option<TermCode>,
    remembered: Option<TermCode>,
    today: NaiveDate,
) -> TermResolution {
    if let Some(term) = chosen {
        return TermResolution::Resolved {
            term,
            source: TermSource::Chosen,
        };
    }
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

/// 選擇器可用的學期：課程中出現過的學期，另含目前學期（可能來自記憶或本次
/// 選擇而不在課程清單中）。
pub fn term_options(courses: &[LmsCourse], term: TermCode) -> Vec<TermCode> {
    let mut options = course_terms(courses);
    if !options.contains(&term) {
        options.push(term);
        options.sort_unstable_by(|left, right| right.cmp(left));
    }
    options
}

/// 依學期將課程分為「納入本輪查詢」與「缺少學期資訊」兩類。
///
/// 回傳（納入的課程、缺少學期資訊而未納入的筆數）；其他學期的課程不參與
/// 本輪查詢，也不計入回傳的筆數。
pub fn courses_for_term(courses: Vec<LmsCourse>, term: TermCode) -> (Vec<LmsCourse>, usize) {
    let mut included: Vec<LmsCourse> = Vec::new();
    let mut skipped = 0_usize;
    for course in courses {
        match course_term(&course) {
            Some(code) if code == term => included.push(course),
            Some(_) => {}
            None => skipped += 1,
        }
    }
    (included, skipped)
}

#[cfg(test)]
#[path = "tests/semester_test.rs"]
mod semester_test;
