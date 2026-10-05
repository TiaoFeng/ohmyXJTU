//! 任務頁的列模型與排序：自訂義任務與思源學堂作業的合併清單。
//!
//! 任務頁把兩種來源的項目放在同一份清單裡：預設分段顯示（任务段在前、作业段
//! 在后），或依 `^L` 選定的方式把兩者混在一起排序。純資料模型、標籤規則與
//! 截止時間解析見 [`super`]。
//!
//! 由 [`super`] 重新匯出，呼叫端一律用 `crate::domain::todo::…`。

use std::cmp::Ordering;

use crate::domain::homework::HomeworkItem;

use super::{Priority, Task};

/// 分段標題：自訂義任務（顯示在作業段之前）。
pub const TASKS_HEADER: &str = "任务";
/// 分段標題：思源學堂作業。
pub const HOMEWORK_HEADER: &str = "作业";

/// 任務頁的一列；分段標題與空白列不可選取。
#[derive(Debug, Clone)]
pub enum PageRow<'a> {
    /// 分段標題（「任务」或「作业」）。
    Header(&'static str),
    /// 分段之間的空白列。
    Spacer,
    /// 自訂義任務。
    Task(&'a Task),
    /// 思源學堂作業。
    Homework(&'a HomeworkItem),
}

impl PageRow<'_> {
    /// 是否為可選取的列。
    pub fn is_selectable(&self) -> bool {
        matches!(self, Self::Task(_) | Self::Homework(_))
    }
}

/// 產生任務頁的列：任务段在前、作业段在后；只顯示非空的分段，分段之間留一列空白。
pub fn page_rows<'a>(tasks: &[&'a Task], homework: &[&'a HomeworkItem]) -> Vec<PageRow<'a>> {
    let mut rows = Vec::new();
    if !tasks.is_empty() {
        rows.push(PageRow::Header(TASKS_HEADER));
        rows.extend(tasks.iter().map(|task| PageRow::Task(task)));
    }
    if !homework.is_empty() {
        if !rows.is_empty() {
            rows.push(PageRow::Spacer);
        }
        rows.push(PageRow::Header(HOMEWORK_HEADER));
        rows.extend(homework.iter().map(|item| PageRow::Homework(item)));
    }
    rows
}

/// 可選取列的總數（任務在前、作業在後）。
pub fn selectable_len(tasks: usize, homework: usize) -> usize {
    tasks + homework
}

/// 選取索引在列模型中的位置（供清單高亮與 offset 映射；標題與空白列不計入）。
pub fn visual_index(rows: &[PageRow<'_>], selection: usize) -> Option<usize> {
    let mut seen = 0_usize;
    for (position, row) in rows.iter().enumerate() {
        if row.is_selectable() {
            if seen == selection {
                return Some(position);
            }
            seen += 1;
        }
    }
    None
}

/// 任務頁的排序方式（`^L`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// 預設：任務段在前、作業段在後（分段顯示）。
    #[default]
    Default,
    /// 依優先級（作業一律視為「高」）。
    Priority,
    /// 依截止時間（無截止時間者排最後）。
    Deadline,
}

impl SortMode {
    /// 顯示名稱（排序提示與標題列使用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "默认",
            Self::Priority => "优先级",
            Self::Deadline => "截止时间",
        }
    }

    /// 是否為混合排序：任務與作業混在一起，不再分段。
    pub fn is_sorted(self) -> bool {
        self != Self::Default
    }
}

/// 任務頁項目的排序鍵（任務與作業共用）。
///
/// 作業沒有優先級與本機識別碼：優先級一律視為 [`Priority::High`]，識別碼取活動
/// 識別碼。截止時間以絕對瞬間比較（任務直接取欄位、作業解析 `end_time`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// 是否沒有截止時間（`0` ＝有、`1` ＝無；讓有截止時間者排前面）。
    pub(super) missing_deadline: u8,
    /// 截止時間的絕對瞬間（無截止時間時為 0）。
    pub(super) deadline: i64,
    /// 優先級（`Ord` 為高 → 低）。
    pub(super) priority: Priority,
    /// 標題或內容（僅作穩定排序的第二鍵，不分大小寫）。
    pub(super) label: String,
    /// 識別碼：任務取流水號（補零以維持數值順序）、作業取活動識別碼。
    pub(super) id: String,
}

impl SortKey {
    /// 組出排序鍵。
    fn new(deadline: Option<i64>, priority: Priority, label: &str, id: String) -> Self {
        Self {
            missing_deadline: u8::from(deadline.is_none()),
            deadline: deadline.unwrap_or(0),
            priority,
            label: label.to_lowercase(),
            id,
        }
    }
}

/// 任務識別碼在排序鍵中的補零寬度：補零後字串比較等同數值比較。
pub(super) const ID_SORT_WIDTH: usize = 20;

/// 自訂義任務的排序鍵。
pub fn task_sort_key(task: &Task) -> SortKey {
    SortKey::new(
        task.deadline.map(|deadline| deadline.timestamp()),
        task.priority,
        &task.content,
        format!("{:0width$}", task.id, width = ID_SORT_WIDTH),
    )
}

/// 思源學堂作業的排序鍵（優先級一律視為「高」）。
pub fn homework_sort_key(item: &HomeworkItem) -> SortKey {
    SortKey::new(
        crate::domain::homework::parse_time(item.end_time.as_deref())
            .map(|deadline| deadline.timestamp()),
        Priority::High,
        &item.title,
        item.activity_id.clone(),
    )
}

/// 依排序方式比較兩個排序鍵。
///
/// 優先級：優先級 → 截止時間 → 標題 → 識別碼；
/// 截止時間：截止時間 → 優先級 → 標題 → 識別碼。
/// 兩者的截止時間都是升序，沒有截止時間者一律排在同組最後。
///
/// [`SortMode::Default`] 不是混合排序（呼叫端 [`crate::tui::app::App::task_page_entries`]
/// 會先以 [`SortMode::is_sorted`] 分流），該分支只為了窮盡性而與截止時間同序。
pub fn compare(mode: SortMode, left: &SortKey, right: &SortKey) -> Ordering {
    debug_assert!(mode.is_sorted(), "默认排序不经过 compare");
    match mode {
        SortMode::Priority => (
            left.priority,
            left.missing_deadline,
            left.deadline,
            &left.label,
            &left.id,
        )
            .cmp(&(
                right.priority,
                right.missing_deadline,
                right.deadline,
                &right.label,
                &right.id,
            )),
        SortMode::Default | SortMode::Deadline => (
            left.missing_deadline,
            left.deadline,
            left.priority,
            &left.label,
            &left.id,
        )
            .cmp(&(
                right.missing_deadline,
                right.deadline,
                right.priority,
                &right.label,
                &right.id,
            )),
    }
}
