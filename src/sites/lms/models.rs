//! 思源學堂的資料模型。
//!
//! 伺服器回傳的是 snake_case 欄位；列表項缺少必要欄位時由呼叫端跳過該項
//! （見 [`crate::sites::parse_lenient`]）。

use serde::Deserialize;

use super::super::{optional_string_or_number, string_or_number};

/// 活動類型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityKind {
    /// 作業。
    Homework,
    /// 教材。
    Material,
    /// 課程內容。
    Lesson,
    /// 直播。
    LectureLive,
    /// 其他或未知類型。
    #[default]
    Unknown,
}

impl ActivityKind {
    /// 由伺服器字串對應類型（去除前後空白、忽略大小寫）。
    pub fn from_server(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "homework" => Self::Homework,
            "material" => Self::Material,
            "lesson" => Self::Lesson,
            "lecture_live" => Self::LectureLive,
            _ => Self::Unknown,
        }
    }

    /// 簡體中文標籤。
    pub fn label(self) -> &'static str {
        match self {
            Self::Homework => "作业",
            Self::Material => "资料",
            Self::Lesson => "课程内容",
            Self::LectureLive => "直播",
            Self::Unknown => "其他",
        }
    }
}

/// 學年。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsAcademicYear {
    /// 識別碼。
    #[serde(default, deserialize_with = "optional_string_or_number")]
    pub id: Option<String>,
    /// 名稱，例如 `2026-2027`。
    #[serde(default)]
    pub name: Option<String>,
}

/// 學期。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsSemester {
    /// 識別碼。
    #[serde(default, deserialize_with = "optional_string_or_number")]
    pub id: Option<String>,
    /// 學期代碼，形如 `2026-1`（起始學年-學期序）；課程與考勤學期的配對依賴此欄位。
    #[serde(default)]
    pub code: Option<String>,
    /// 名稱。
    #[serde(default)]
    pub name: Option<String>,
    /// 顯示名稱。
    #[serde(default)]
    pub real_name: Option<String>,
}

/// 授課教師。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsInstructor {
    /// 識別碼。
    #[serde(default, deserialize_with = "optional_string_or_number")]
    pub id: Option<String>,
    /// 姓名。
    #[serde(default)]
    pub name: Option<String>,
}

/// 課程。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsCourse {
    /// 課程識別碼。
    #[serde(deserialize_with = "string_or_number")]
    pub id: String,
    /// 課程名稱。
    pub name: String,
    /// 課程代碼。
    #[serde(default)]
    pub course_code: Option<String>,
    /// 授課教師。
    #[serde(default)]
    pub instructors: Vec<LmsInstructor>,
    /// 所屬學期。
    #[serde(default)]
    pub semester: Option<LmsSemester>,
    /// 所屬學年。
    #[serde(default)]
    pub academic_year: Option<LmsAcademicYear>,
}

impl LmsCourse {
    /// 授課教師姓名（以「、」串接）。
    pub fn instructor_names(&self) -> String {
        let names: Vec<&str> = self
            .instructors
            .iter()
            .filter_map(|instructor| instructor.name.as_deref())
            .collect();
        names.join("、")
    }

    /// 學期顯示名稱。
    pub fn semester_label(&self) -> String {
        let year = self
            .academic_year
            .as_ref()
            .and_then(|year| year.name.clone());
        let semester = self
            .semester
            .as_ref()
            .and_then(|semester| semester.real_name.clone().or_else(|| semester.name.clone()));
        match (year, semester) {
            (Some(year), Some(semester)) => format!("{year} {semester}"),
            (Some(year), None) => year,
            (None, Some(semester)) => semester,
            (None, None) => String::new(),
        }
    }
}

/// 活動（作業、資料、課程內容…）。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsActivity {
    /// 活動識別碼。
    #[serde(deserialize_with = "string_or_number")]
    pub id: String,
    /// 所屬課程識別碼。
    #[serde(default, deserialize_with = "optional_string_or_number")]
    pub course_id: Option<String>,
    /// 活動類型字串（`type`）。
    #[serde(rename = "type", default)]
    pub kind: String,
    /// 標題。
    #[serde(default)]
    pub title: Option<String>,
    /// 開始時間。
    #[serde(default)]
    pub start_time: Option<String>,
    /// 截止時間。
    #[serde(default)]
    pub end_time: Option<String>,
    /// 是否以小組為單位提交。
    #[serde(default)]
    pub submit_by_group: Option<bool>,
    /// 小組識別碼（小組作業時使用）。
    #[serde(default, deserialize_with = "optional_string_or_number")]
    pub group_id: Option<String>,
    /// 說明（HTML）。
    #[serde(default)]
    pub description: Option<String>,
    /// 伺服器記錄的提交次數。
    #[serde(default)]
    pub user_submit_count: Option<u64>,
    /// 是否已發布。
    #[serde(default)]
    pub published: Option<bool>,
}

impl LmsActivity {
    /// 活動類型。
    pub fn kind(&self) -> ActivityKind {
        ActivityKind::from_server(&self.kind)
    }

    /// 標題（缺欄位時以型別代替）。
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| self.kind().label().to_owned())
    }
}

/// 一筆提交記錄。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsSubmission {
    /// 提交識別碼。
    #[serde(deserialize_with = "string_or_number")]
    pub id: String,
    /// 提交時間。
    #[serde(default)]
    pub submitted_at: Option<String>,
    /// 建立時間。
    #[serde(default)]
    pub created_at: Option<String>,
    /// 是否為最新版本（寬容解析：布林、`0`/`1` 或字串）。
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_latest_version: Option<bool>,
    /// 伺服器狀態碼（型別未定，保留原始值供後續脫敏樣本核實）。
    #[serde(default)]
    pub status: Option<serde_json::Value>,
    /// 是否為重新提交。
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_resubmitted: Option<bool>,
    /// 是否為重做。
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_redo: Option<bool>,
    /// 是否允許撤回（僅代表「可以」撤回，不代表已撤回）。
    #[serde(default, deserialize_with = "lenient_bool")]
    pub can_retract: Option<bool>,
    /// 分數。
    #[serde(default)]
    pub score: Option<serde_json::Value>,
    /// 批註。
    #[serde(default)]
    pub comment: Option<String>,
}

impl LmsSubmission {
    /// 顯示用的提交時間。
    pub fn timestamp(&self) -> Option<&str> {
        self.submitted_at
            .as_deref()
            .or(self.created_at.as_deref())
            .filter(|value| !value.trim().is_empty())
    }

    /// 是否計入有效提交（排除非最新版本）。
    ///
    /// 「有效提交」的單一判據：作業清單的「已完成」與詳情頁的計數都必須
    /// 使用它。`is_latest_version` 缺失時無法證明是舊版本，仍計入（介面
    /// 另以「未知」標示）。草稿／撤回的精確判據（`status`、`can_retract`
    /// 等語意）尚待實網脫敏樣本核實，暫不臆測。
    pub fn is_effective(&self) -> bool {
        self.is_latest_version != Some(false)
    }
}

/// 提交記錄列表。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsSubmissionList {
    /// 提交記錄。
    #[serde(default)]
    pub list: Vec<LmsSubmission>,
}

impl LmsSubmissionList {
    /// 提交記錄數。
    pub fn count(&self) -> usize {
        self.list.len()
    }

    /// 有效提交數。
    ///
    /// 判據為 [`LmsSubmission::is_effective`]（單一定義）；介面顯示必須
    /// 與此保持一致。
    pub fn effective_count(&self) -> usize {
        self.list
            .iter()
            .filter(|submission| submission.is_effective())
            .count()
    }
}

/// 寬容布林：接受布林、`0`/`1` 與其字串形式；其他型別視為未知（`None`）。
fn lenient_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        serde_json::Value::Bool(flag) => Some(flag),
        serde_json::Value::Number(number) => number.as_i64().map(|number| number != 0),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }))
}
