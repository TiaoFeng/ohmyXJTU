//! 考勤系統的資料模型。

use serde::Deserialize;

use crate::tone::Tone;

use super::super::{
    date_string, lenient_flag, optional_string_lenient, optional_string_or_number_lenient,
    string_lenient, string_or_number, u32_lenient, u32_or_string,
};

/// 考勤狀態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttendanceStatus {
    /// 尚未進行考勤。
    Pending,
    /// 正常。
    Normal,
    /// 遲到。
    Late,
    /// 缺勤。
    Absent,
    /// 請假。
    Leave,
    /// 該次課程不需要考勤。
    NotRequired,
    /// 伺服器回傳了未識別的狀態。
    Unknown,
}

impl AttendanceStatus {
    /// 由伺服器字串對應狀態。
    pub fn from_server(value: &str) -> Self {
        match value.trim().to_ascii_uppercase().as_str() {
            "PENDING" => Self::Pending,
            "NORMAL" => Self::Normal,
            "LATE" => Self::Late,
            "ABSENT" => Self::Absent,
            "LEAVE" => Self::Leave,
            "NOT_REQUIRED" => Self::NotRequired,
            _ => Self::Unknown,
        }
    }

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待考勤",
            Self::Normal => "正常",
            Self::Late => "迟到",
            Self::Absent => "缺勤",
            Self::Leave => "请假",
            Self::NotRequired => "不考勤",
            Self::Unknown => "未知",
        }
    }

    /// 狀態語意色。
    pub fn tone(self) -> Tone {
        match self {
            Self::Pending => Tone::Accent,
            Self::Normal => Tone::Success,
            Self::Late => Tone::Warning,
            Self::Absent => Tone::Danger,
            Self::Leave => Tone::Info,
            Self::NotRequired => Tone::Muted,
            Self::Unknown => Tone::Warning,
        }
    }

    /// 嚴重度：同一堂課有多筆記錄時取最嚴重者。
    ///
    /// 「不考勤」與「未知」不參與比較，避免覆蓋明確的考勤結果。
    pub fn severity(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::Leave => 2,
            Self::Late => 3,
            Self::Absent => 4,
            Self::Pending => 0,
            Self::NotRequired | Self::Unknown => 0,
        }
    }
}

/// 學期資訊。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Semester {
    /// 伺服器端的學期識別碼。
    #[serde(deserialize_with = "string_or_number")]
    pub semester_id: String,
    /// 學年度，例如 `2026-2027`。
    pub academic_year: String,
    /// 學期名稱，例如「第一学期」。
    pub semester_name: String,
    /// 學期開始日期（`YYYY-MM-DD`）。
    pub start_date: String,
    /// 學期結束日期（`YYYY-MM-DD`；選填，型別異常時視為缺漏）。
    ///
    /// 只用於「學期是否已結束」的判斷：讀不出來時不啟用該判斷，不該讓整個
    /// 學期查詢失敗（那會連帶毀掉課表頁）。
    #[serde(default, deserialize_with = "optional_string_lenient")]
    pub end_date: Option<String>,
}

impl Semester {
    /// 學期編號，例如 `2026-2027-1`；名稱無法識別時回 `None`。
    ///
    /// 不得臆造序號（例如 `0`）：假的 `2026-2027-0` 看似有效，卻永遠配不上
    /// 任何課程；參考實作對未知名稱同樣是顯性失敗（KeyError）。
    pub fn term_name(&self) -> Option<String> {
        let ordinal = match self.semester_name.trim() {
            name if name.contains('一') => 1,
            name if name.contains('二') => 2,
            name if name.contains('三') => 3,
            name if name.contains('四') => 4,
            _ => return None,
        };
        Some(format!("{}-{ordinal}", self.academic_year.trim()))
    }

    /// 顯示用學期標籤：可識別時為學期編號，否則回退顯示原始名稱。
    pub fn display_label(&self) -> String {
        self.term_name().unwrap_or_else(|| {
            let name = self.semester_name.trim();
            if name.is_empty() {
                "未知学期".to_owned()
            } else {
                name.to_owned()
            }
        })
    }
}

/// 整學期課表中的一門課（`weekRanges` 為原始週次字串）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimetableCourse {
    /// 課程名稱。
    pub course_name: String,
    /// 教師。
    #[serde(default)]
    pub teacher_name: Option<String>,
    /// 上課地點。
    #[serde(default)]
    pub classroom_name: Option<String>,
    /// 星期（1 = 週一）。
    #[serde(deserialize_with = "u32_or_string")]
    pub day_of_week: u32,
    /// 開始節次。
    #[serde(deserialize_with = "u32_or_string")]
    pub start_section: u32,
    /// 結束節次。
    #[serde(deserialize_with = "u32_or_string")]
    pub end_section: u32,
    /// 週次字串，例如 `1-4,6-16`。
    #[serde(default)]
    pub week_ranges: String,
}

/// 一筆課程考勤記錄。
///
/// 除了參與比對的欄位（節次、日期、地點與教師、狀態），其餘欄位一律寬容讀取：
/// `records_between` 逐項解析、跳過讀不出來的記錄（見
/// [`crate::sites::parse_lenient`]），因此「本程式沒有用到的欄位」不該成為整筆
/// 記錄被丟棄的理由。
///
/// **比對用的地點與教師刻意維持嚴格**（型別異常即丟棄整筆）：比對時任一方缺值
/// 視為「不比對該欄位」（見 `attendance_match::matches_optional`），因此把它們
/// 寬容化成 `None` 會讓一筆讀不出地點的記錄**匹配任何同日期同節次的課**，把
/// 「待核实」變成假的「正常」——丟掉整筆才是安全的方向。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaterRecord {
    /// 記錄識別碼（本程式不以此欄位判斷任何事；讀不出來時為空字串）。
    #[serde(default, deserialize_with = "string_lenient")]
    pub result_id: String,
    /// 開始節次。
    #[serde(deserialize_with = "u32_or_string")]
    pub start_section: u32,
    /// 結束節次。
    #[serde(deserialize_with = "u32_or_string")]
    pub end_section: u32,
    /// 第幾週（比對不使用它；讀不出來時為 0）。
    #[serde(default, deserialize_with = "u32_lenient")]
    pub course_week: u32,
    /// 上課地點。
    #[serde(default)]
    pub classroom_name: Option<String>,
    /// 教師。
    #[serde(default)]
    pub teacher_name: Option<String>,
    /// 伺服器回傳的考勤狀態字串。
    pub attendance_status: String,
    /// 上課日期（解析時正規化為 `YYYY-MM-DD`；格式無法解讀時該筆記錄會被跳過）。
    #[serde(deserialize_with = "date_string")]
    pub attendance_date: String,
    /// 所屬學期識別碼（本程式不以此欄位判斷任何事；型別異常時視為缺漏）。
    #[serde(default, deserialize_with = "optional_string_or_number_lenient")]
    pub semester_id: Option<String>,
}

impl WaterRecord {
    /// 解析後的考勤狀態。
    pub fn status(&self) -> AttendanceStatus {
        AttendanceStatus::from_server(&self.attendance_status)
    }
}

/// 一筆考勤流水（刷卡紀錄）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRecord {
    /// 流水識別碼（本程式不以此欄位判斷任何事；讀不出來時為空字串）。
    #[serde(default, deserialize_with = "string_lenient")]
    pub id: String,
    /// 打卡地點（純展示；型別異常時只損失該欄位）。
    #[serde(default, deserialize_with = "optional_string_lenient")]
    pub classroom_name: Option<String>,
    /// 打卡時間（純展示；型別異常時只損失該欄位）。
    #[serde(default, deserialize_with = "optional_string_lenient")]
    pub collect_time: Option<String>,
    /// 是否落在某堂課的考勤範圍內。
    ///
    /// 寬容讀取：伺服器對布林欄位的型別並不統一（參考實作的 `KqHttp.bool`
    /// 同時嘗試 `effective` 與 `isEffective`，並接受 `"1"`／`"true"` 等字串）。
    /// 缺少或無法解讀時視為 `false`（保守地不宣稱該筆記錄有效），不讓整頁
    /// 流水因單一欄位的型別而解析失敗。
    #[serde(default, alias = "isEffective", deserialize_with = "lenient_flag")]
    pub effective: bool,
}

/// 分頁的考勤流水。
#[derive(Debug, Clone)]
pub struct FlowPage {
    /// 本頁流水。
    pub records: Vec<FlowRecord>,
    /// 總筆數。
    pub total: u64,
    /// 目前頁碼（從 1 開始）。
    pub page: u32,
    /// 每頁筆數。
    pub page_size: u32,
    /// 本頁中無法解析而被跳過的筆數。
    pub skipped: usize,
}

impl FlowPage {
    /// 總頁數。
    pub fn total_pages(&self) -> u32 {
        if self.page_size == 0 {
            return 1;
        }
        // `total` 由伺服器回報，理論上可以是任意 `u64`：改用 `try_from`，
        // 超出 `u32` 時以上限表示（`as` 會靜默截斷成看似合理卻錯誤的頁數）。
        u32::try_from(self.total.div_ceil(u64::from(self.page_size)).max(1)).unwrap_or(u32::MAX)
    }
}

/// 指定期間的課程考勤記錄（含是否被分頁上限截斷）。
#[derive(Debug, Clone)]
pub struct RecordBatch {
    /// 取回的記錄。
    pub records: Vec<WaterRecord>,
    /// 是否因分頁上限而未取完全部記錄。
    pub truncated: bool,
    /// 無法解析而被跳過的筆數（對應課程會顯示「待核实」）。
    pub skipped: usize,
}
