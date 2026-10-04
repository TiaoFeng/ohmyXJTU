//! 介面與背景執行緒共用的呈現資料模型。
//!
//! 這些型別由背景執行緒（[`crate::task`]）查詢完成後組裝，交給介面
//! （[`crate::tui`]）直接顯示，因此不屬於任何一側：放在此處讓兩邊都能引用，
//! 不必讓背景層依賴介面模組，介面也不必重複定義一次。

use chrono::NaiveDate;

use crate::domain::attendance_match::LessonAttendance;
use crate::sites::attendance::FlowRecord;
use crate::sites::lms::{ActivityContent, ActivityKind, LmsSubmission};

/// 本週的一堂課。
#[derive(Debug, Clone)]
pub struct LessonEntry {
    /// 上課日期。
    pub date: NaiveDate,
    /// 節次，例如 `1-2`（顯示用；排序以 [`Self::start_section`] 為準）。
    pub sections: String,
    /// 開始節次（數值，供排序）。
    pub start_section: u32,
    /// 結束節次（數值，供排序）。
    pub end_section: u32,
    /// 課程名稱。
    pub course_name: String,
    /// 上課地點。
    pub classroom: String,
    /// 教師。
    pub teacher: String,
    /// 週次說明。
    pub weeks: String,
    /// 考勤顯示狀態（含「待考勤」與「待核实」兩種合成狀態）。
    pub attendance: LessonAttendance,
}

/// 課表頁資料。
#[derive(Debug, Clone, Default)]
pub struct ScheduleData {
    /// 學期說明，例如 `2026-2027-1`。
    pub semester: String,
    /// 本週週次。
    pub week: u32,
    /// 學期總週數（切換週次的邊界）。
    pub total_weeks: u32,
    /// 本週課程。
    pub lessons: Vec<LessonEntry>,
    /// 因格式問題被跳過的課程筆數。
    pub skipped: usize,
    /// 頁面提示（例如學期外的空狀態原因、考勤記錄被分頁上限截斷）。
    pub notice: Option<String>,
}

/// 考勤流水頁資料。
#[derive(Debug, Clone, Default)]
pub struct FlowData {
    /// 本頁流水。
    pub records: Vec<FlowRecord>,
    /// 目前頁碼。
    pub page: u32,
    /// 總頁數。
    pub total_pages: u32,
    /// 總筆數。
    pub total: u64,
}

/// 活動詳情檢視資料。
#[derive(Debug, Clone, Default)]
pub struct ActivityDetailView {
    /// 活動識別碼。
    pub id: String,
    /// 標題。
    pub title: String,
    /// 活動類型。
    pub kind: ActivityKind,
    /// 活動說明與附件（正文＋附件）；`None` 代表沒有可顯示的內容。
    pub description: Option<ActivityContent>,
    /// 截止時間。
    pub end_time: Option<String>,
    /// 是否小組作業；`None` 代表無法確認（詳情缺少該欄位）。
    pub submit_by_group: Option<bool>,
    /// 提交記錄；`None` 代表無法確認（僅作業有提交狀態）。
    pub submissions: Option<Vec<LmsSubmission>>,
    /// 補充說明（例如無法確認提交狀態的原因）。
    pub note: Option<String>,
}
