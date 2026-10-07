//! 思源學堂的資料模型。
//!
//! 伺服器回傳的是 snake_case 欄位；列表項缺少必要欄位時由呼叫端跳過該項
//! （見 [`crate::sites::parse_lenient`]）。

use serde::Deserialize;

use super::super::{
    lenient_array, lenient_bool, optional_string_lenient, optional_string_or_number,
    optional_string_or_number_lenient, optional_u64_lenient, string_or_number,
};
use super::html;

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

/// 活動詳情正文欄位的形態。
///
/// 用來分辨「沒有這個欄位」（這項活動本來就沒有正文，屬正常）與「欄位存在、卻
/// 因型別不符而讀不出來」（本程式對回應結構的假設與實際不符，必須讓使用者看見）。
enum BodyField<'a> {
    /// 欄位存在且是可用（去除空白後非空）的字串。
    Text(&'a str),
    /// 欄位不存在，或是空字串／`null`：視為沒有這段內容。
    Empty,
    /// 欄位存在但不是字串（數字、物件、陣列…）。
    Unreadable,
}

/// 正文來源欄位存在、卻讀不出內容時的原因（介面顯示於說明區塊）。
///
/// 這三則是**診斷**訊息：只在「確實看到來源欄位、但型別與預期不符」時出現，
/// 代表本程式的欄位假設與實際回應不一致；正常情況（這項活動沒有正文）不會顯示。
/// 只描述原因本身，措辭由介面統一組裝（見 `views::content::push_description`）。
pub const BODY_NOT_OBJECT_NOTE: &str = "正文来源字段（data）不是对象";
/// 見 [`BODY_NOT_OBJECT_NOTE`]。
pub const BODY_FIELD_TYPE_NOTE: &str = "正文来源字段（data.description／content）类型不符";
/// 見 [`BODY_NOT_OBJECT_NOTE`]。
pub const TOP_LEVEL_BODY_NOTE: &str = "正文在回应顶层（description），本版未读取";

/// 活動附件（老師上傳的檔案）。
///
/// 只保留終端顯示所需的欄位：附件內容勢必得在瀏覽器下載（TUI 不做檔案落地），
/// 因此識別碼、下載與預覽網址都不解析——參考實作的下載網址還是自行用識別碼拼接
/// 出來的，不在回應裡。伺服器欄位是活動詳情回應的頂層 `uploads` 陣列。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LmsUpload {
    /// 檔名；缺欄位或型別異常時為 `None`。
    #[serde(default, deserialize_with = "optional_string_lenient")]
    pub name: Option<String>,
    /// 位元組數；缺欄位或型別異常時為 `None`。
    #[serde(default, deserialize_with = "optional_u64_lenient")]
    pub size: Option<u64>,
}

impl LmsUpload {
    /// 顯示用檔名：伺服器沒給名稱（或只有空白）時以「未命名附件」代替，
    /// 不讓附件因沒有檔名而從清單消失。
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("未命名附件")
    }

    /// 顯示用大小（`512 B`、`24 KB`、`1.2 MB`）；伺服器沒給大小時回 `None`。
    pub fn size_label(&self) -> Option<String> {
        /// 進位門檻（二進位單位，與參考實作一致）。
        const KB: u64 = 1024;
        const MB: u64 = 1024 * 1024;
        let size = self.size?;
        Some(if size < KB {
            format!("{size} B")
        } else if size < MB {
            format!("{} KB", size / KB)
        } else {
            format!("{:.1} MB", size as f64 / MB as f64)
        })
    }
}

/// 活動詳情可顯示的內容：說明正文、附件，或讀不到正文的原因。
///
/// 純文字不足以呈現整份正文：作業說明可能就是一張圖片、含有 `href` 目標的連結，
/// 或題目整個放在附件裡。因此除了正文，還回報 `has_media`／`has_links` 與
/// `attachments`，讓介面能標註並列出「要看完整內容得開網頁」的部分。
///
/// 反過來，「來源欄位存在卻讀不出正文」也不能與「沒有說明」混為一談：前者是
/// 本程式的欄位假設與實際回應不符，記在 [`Self::issue`] 讓介面顯示出來。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityContent {
    /// 純文字內容（沒有可見文字時為 `None`）。
    pub text: Option<String>,
    /// 是否含圖片、影片等無法以文字呈現的元素。
    pub has_media: bool,
    /// 是否含指向實際目標的連結（`href` 不會出現在純文字裡）。
    pub has_links: bool,
    /// 附件（老師上傳的檔案）；沒有一律為空 vec。
    pub attachments: Vec<LmsUpload>,
    /// 正文來源存在、卻讀不出內容時的原因（此時其餘欄位皆為空）。
    ///
    /// 純診斷用途：正常情況為 `None`。有值時代表本程式對回應結構的假設與實際
    /// 不符（見 [`BODY_NOT_OBJECT_NOTE`] 等常數），介面會顯示原因而不是看起來
    /// 「這項活動沒有說明」。
    pub issue: Option<&'static str>,
}

impl ActivityContent {
    /// 由 HTML 轉換結果組出內容（附件另外填入，見 [`LmsActivity::body`]）。
    fn from_text(extract: html::TextExtract) -> Self {
        Self {
            text: extract.text,
            has_media: extract.has_media,
            has_links: extract.has_links,
            attachments: Vec::new(),
            issue: None,
        }
    }

    /// 有沒有任何可顯示的內容（正文、圖片、連結、附件或讀取失敗的原因）。
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && !self.has_media
            && !self.has_links
            && self.attachments.is_empty()
            && self.issue.is_none()
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
    /// 活動正文區塊（`data`；只有詳情回應提供，列表項目為 `None`）。
    ///
    /// 作業與資料的說明放在 `data.description`，頁面型活動（課程簡介、教學進度…）
    /// 的正文放在 `data.content`；兩者都是 HTML。
    ///
    /// 刻意保留原始 JSON 而不直接反序列化：伺服器對此欄位的型別並不穩定（物件、
    /// 字串、數字都出現過），保留原始值才能在讀不出正文時分辨「沒有這個欄位」
    ///（這項活動本來就沒有正文）與「欄位存在但型別不符」（見 [`Self::body`]）。
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    /// 回應頂層的 `description`。
    ///
    /// 本版只讀 `data` 之下的正文；保留此欄位純為診斷——若實際回應把正文放在
    /// 頂層（本功能最初的解析目標），介面會提示「未讀取」，而不是靜默地看起來
    /// 「這項活動沒有說明」。
    #[serde(
        default,
        rename = "description",
        deserialize_with = "optional_string_lenient"
    )]
    pub top_level_description: Option<String>,
    /// 附件（老師上傳的檔案）。
    ///
    /// 只出現在活動詳情回應的頂層 `uploads`（不在 `data` 底下），列表項目不含
    /// 此欄位；參考實作同樣從頂層取用。
    #[serde(default, deserialize_with = "lenient_array")]
    pub uploads: Vec<LmsUpload>,
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

    /// 讀取正文區塊中指定欄位的形態。
    ///
    /// `data` 不是物件時 `Value::get` 一律回 `None`，因此這種情形在這裡看起來
    /// 與「欄位不存在」相同——分辨兩者是 [`Self::body_issue`] 的職責。
    fn body_field(&self, key: &str) -> BodyField<'_> {
        let Some(value) = self.data.as_ref().and_then(|data| data.get(key)) else {
            return BodyField::Empty;
        };
        match value {
            serde_json::Value::String(text) if !text.trim().is_empty() => BodyField::Text(text),
            serde_json::Value::String(_) | serde_json::Value::Null => BodyField::Empty,
            _ => BodyField::Unreadable,
        }
    }

    /// 可顯示的正文 HTML：`data.description` 優先，空白時改用 `data.content`。
    ///
    /// 兩處都試一次是參考實作的做法：頁面型活動的 `description` 實測為空字串，
    /// 正文只在 `content`。
    pub fn body_html(&self) -> Option<&str> {
        ["description", "content"]
            .into_iter()
            .find_map(|key| match self.body_field(key) {
                BodyField::Text(html) => Some(html),
                BodyField::Empty | BodyField::Unreadable => None,
            })
    }

    /// 活動的可顯示內容（正文＋附件，或讀不到正文的原因）。
    ///
    /// 正文取自 `data`、附件取自頂層 `uploads`（都只有詳情回應才有）。讀不到
    /// 正文時 `issue` 帶著原因，讓「欄位假設與實際回應不符」變成看得見的提示；
    /// 這種情形下附件仍會照常列出，因此原因不因附件有無而省略。
    pub fn body(&self) -> Option<ActivityContent> {
        // 沒有正文時仍可能有附件，因此兩者分開取再合併。
        let mut content = self
            .body_html()
            .map(html::convert)
            .map(ActivityContent::from_text)
            .unwrap_or_default();
        content.attachments.clone_from(&self.uploads);
        content.issue = self.body_issue();
        (!content.is_empty()).then_some(content)
    }

    /// 正文來源存在、卻讀不出內容時的原因；正常情況為 `None`。
    ///
    /// 只在「看得到來源欄位、卻拿不到正文」時回報，因此不會讓每項沒有說明的活動
    /// 都出現提示：
    /// - 已取得正文：其他欄位的型別問題不影響顯示，不提示。
    /// - `data` 不存在：這項活動本來就沒有正文；但頂層 `description` 若真有內容，
    ///   代表本版讀錯了欄位（見 [`TOP_LEVEL_BODY_NOTE`]）。
    /// - `data` 存在但不是物件：型別假設與實際不符（見 [`BODY_NOT_OBJECT_NOTE`]）。
    /// - `data` 是物件、但 `description`／`content` 存在且不是字串：內容被丟棄
    ///   （見 [`BODY_FIELD_TYPE_NOTE`]）。
    fn body_issue(&self) -> Option<&'static str> {
        if self.body_html().is_some() {
            return None;
        }
        let Some(data) = self.data.as_ref() else {
            return self
                .top_level_description
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
                .then_some(TOP_LEVEL_BODY_NOTE);
        };
        if !data.is_object() {
            return Some(BODY_NOT_OBJECT_NOTE);
        }
        ["description", "content"]
            .into_iter()
            .any(|key| matches!(self.body_field(key), BodyField::Unreadable))
            .then_some(BODY_FIELD_TYPE_NOTE)
    }
}

/// 一筆提交記錄。
///
/// 所有欄位都採寬容讀取：提交記錄的**筆數**決定作業是「已完成」還是「未提交」，
/// 因此任何欄位讀不出來時只損失該欄位，不可讓整筆記錄被跳過（參考實作對這些
/// 欄位同樣以 `safeString`／`safeInt` 讀取，讀不到就給預設值）。
#[derive(Debug, Clone, Deserialize)]
pub struct LmsSubmission {
    /// 提交識別碼；讀不出來時為 `None`（本程式不以此欄位判斷任何事）。
    #[serde(default, deserialize_with = "optional_string_or_number_lenient")]
    pub id: Option<String>,
    /// 提交時間。
    #[serde(default, deserialize_with = "optional_string_lenient")]
    pub submitted_at: Option<String>,
    /// 建立時間。
    #[serde(default, deserialize_with = "optional_string_lenient")]
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
    #[serde(default, deserialize_with = "optional_string_lenient")]
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
    /// 解析失敗而被跳過的記錄數（[`crate::sites::parse_lenient`] 的契約）。
    ///
    /// 不是伺服器欄位，而是解析結果的一部分：由查詢端在逐項解析後填入，
    /// 用於區分「真的沒有提交」與「有記錄但讀不出來」。
    #[serde(default)]
    pub skipped: usize,
}

impl LmsSubmissionList {
    /// 提交記錄數（不含被跳過的記錄）。
    pub fn count(&self) -> usize {
        self.list.len()
    }

    /// 有效提交數。
    ///
    /// 判據為 [`LmsSubmission::is_effective`]（單一定義）；介面顯示必須
    /// 與此保持一致。被跳過的記錄不計入，因此有跳過時這個數字可能偏低——
    /// 需要「能否確認」的判斷請用 [`Self::confirmed_effective_count`]。
    pub fn effective_count(&self) -> usize {
        self.list
            .iter()
            .filter(|submission| submission.is_effective())
            .count()
    }

    /// 可確認的有效提交數；`None` 代表無法確認（應顯示「待核实」）。
    ///
    /// 已有至少一筆有效提交時判定不受影響：多一筆讀不出來的記錄只會讓真實數字
    /// 更大，不會改變「已提交」。一筆有效提交都沒有、卻有記錄讀不出來時，讀不出來
    /// 的那筆可能才是有效提交，因此回 `None`——當成「零筆提交」會讓作業被誤判成
    /// 「未提交／逾期」（見 `domain::homework::judge` 的規則）。
    pub fn confirmed_effective_count(&self) -> Option<usize> {
        let effective = self.effective_count();
        (effective > 0 || self.skipped == 0).then_some(effective)
    }

    /// 有記錄無法解析時的說明；沒有跳過任何記錄時為 `None`。
    ///
    /// 格式與 [`crate::sites::lms::submission_failure_note`] 一致（兩者都是
    /// 「無法確認提交狀態」的原因），供作業摘要與介面顯示。
    pub fn unreadable_note(&self) -> Option<String> {
        (self.skipped > 0)
            .then(|| format!("无法确认提交状态：有 {} 条提交记录无法解析", self.skipped))
    }
}
