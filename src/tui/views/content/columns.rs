//! 內容清單的欄寬計算（純函式，不經渲染）。
//!
//! 四套欄寬演算法與其內容需求型別集中在此：課表、作業與活動（共用骨架）、
//! 考勤流水。呼叫端先以 [`super::row_width`] 取得可用列寬後傳入。

use crate::domain::homework::HomeworkItem;
use crate::domain::todo::Task;
use crate::model::LessonEntry;
use crate::sites::lms::LmsActivity;
use crate::text::display_width;

/// 星期欄寬（「周一」為兩個全角字）。
pub(super) const WEEKDAY_WIDTH: usize = 4;
/// 節次欄寬（最寬為 `11-12`，靠右對齊）。
pub(super) const SECTIONS_WIDTH: usize = 5;
/// 考勤狀態欄寬（最寬標籤為三個全角字，如「待考勤」）。
const STATUS_WIDTH: usize = 6;
/// 課表日期欄寬（`MM-DD`）。
const DATE_WIDTH: usize = 5;
/// 課表七欄之間的單欄間隔數。
const SCHEDULE_GAPS: usize = 6;
/// 課表課程欄的最小寬度（三個全角字，低於此值即代表終端過窄）。
const SCHEDULE_COURSE_MIN_WIDTH: usize = 6;
/// 課表地點欄的最小寬度。
const SCHEDULE_CLASSROOM_MIN_WIDTH: usize = 8;
/// 課表教師欄的最小寬度（低於此值時整欄收起）。
const SCHEDULE_TEACHER_MIN_WIDTH: usize = 6;

/// 課表清單的文字欄內容需求（可見列的最大顯示寬）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ScheduleNeeds {
    /// 課程名稱需求寬度。
    course: usize,
    /// 地點需求寬度。
    classroom: usize,
    /// 教師需求寬度。
    teacher: usize,
}

impl ScheduleNeeds {
    /// 由本週課程組出需求。
    pub(super) fn of(lessons: &[LessonEntry]) -> Self {
        Self {
            course: lessons
                .iter()
                .map(|lesson| display_width(&lesson.course_name))
                .max()
                .unwrap_or(0),
            classroom: lessons
                .iter()
                .map(|lesson| display_width(&lesson.classroom))
                .max()
                .unwrap_or(0),
            teacher: lessons
                .iter()
                .map(|lesson| display_width(&lesson.teacher))
                .max()
                .unwrap_or(0),
        }
    }
}

/// 課表清單的欄寬（單位為終端顯示欄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScheduleColumns {
    /// 課程欄寬。
    pub(super) course: usize,
    /// 地點欄寬。
    pub(super) classroom: usize,
    /// 教師欄寬（`0` 代表不顯示）。
    pub(super) teacher: usize,
}

/// 依可用寬度與內容需求決定課表欄寬；終端過窄而無法完整顯示時回傳 `None`。
///
/// 日期、星期、節次與考勤狀態為固定欄；欄位自左而右緊密排列，剩餘寬度留在
/// 列尾。課程、地點與教師欄都以可見內容的寬度為準（空間足夠即完整顯示）；
/// 空間不足時先隱藏教師欄、再縮地點，讓課程名稱最後才縮（下限為三個全角字）。
pub(super) fn schedule_columns(available: usize, needs: ScheduleNeeds) -> Option<ScheduleColumns> {
    let fixed = schedule_fixed_width();
    let mut classroom = needs.classroom.max(SCHEDULE_CLASSROOM_MIN_WIDTH);
    let mut teacher = if needs.teacher == 0 {
        0
    } else {
        needs.teacher.max(SCHEDULE_TEACHER_MIN_WIDTH)
    };
    let mut rest = available.saturating_sub(fixed + classroom + teacher);
    while rest < needs.course && (teacher > 0 || classroom > SCHEDULE_CLASSROOM_MIN_WIDTH) {
        if teacher > SCHEDULE_TEACHER_MIN_WIDTH {
            teacher -= 2;
        } else if teacher > 0 {
            teacher = 0;
        } else {
            classroom -= 2;
        }
        rest = available.saturating_sub(fixed + classroom + teacher);
    }
    if rest < SCHEDULE_COURSE_MIN_WIDTH {
        return None;
    }
    Some(ScheduleColumns {
        course: rest.min(needs.course.max(SCHEDULE_COURSE_MIN_WIDTH)),
        classroom,
        teacher,
    })
}

/// 課表固定欄寬合計（含欄間隔）。
fn schedule_fixed_width() -> usize {
    DATE_WIDTH + WEEKDAY_WIDTH + SECTIONS_WIDTH + STATUS_WIDTH + SCHEDULE_GAPS
}

/// 課表清單可完整顯示所需的最小列寬。
pub(super) fn schedule_min_row_width() -> usize {
    schedule_fixed_width() + SCHEDULE_CLASSROOM_MIN_WIDTH + SCHEDULE_COURSE_MIN_WIDTH
}

/// 作業狀態欄寬（最寬標籤「待提交」「已完成」「待核实」為三個全角字）。
const HOMEWORK_STATE_WIDTH: usize = 6;
/// 「小组」欄寬。
pub(super) const GROUP_WIDTH: usize = 4;
/// 完整截止時間欄寬（`YYYY-MM-DD HH:MM`）。
const DEADLINE_FULL_WIDTH: usize = 16;
/// 精簡截止時間欄寬（`MM-DD HH:MM`）。
const DEADLINE_COMPACT_WIDTH: usize = 11;
/// 截止時間欄的「截止」前綴。
pub(super) const DEADLINE_PREFIX: &str = "截止 ";
/// 標題欄的最小寬度（三個全角字）。
const TITLE_MIN_WIDTH: usize = 6;
/// 作業課程欄的最小寬度（三個全角字，低於此值即代表終端過窄）。
const HOMEWORK_COURSE_MIN_WIDTH: usize = 6;
/// 活動類型欄寬（最寬標籤「课程内容」為四個全角字）。
const ACTIVITY_KIND_WIDTH: usize = 8;
/// 任務列標籤欄（優先級）的寬度（「高」為兩個全角字）；標籤接在它之後。
pub(super) const TASK_PRIORITY_WIDTH: usize = 2;
/// 任務列標籤欄中分隔優先級與標籤的字元（全形中點，佔 2 欄）。
pub(super) const TAG_SEPARATOR: &str = "・";

/// 作業與活動清單的欄寬（單位為終端顯示欄）。
///
/// 兩者共用「標籤欄＋標題＋（狀態）＋截止時間＋（小组）」的骨架：作業的標籤
/// 欄是課程名稱、含狀態欄；活動的標籤欄是活動類型、沒有狀態欄（`state == 0`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowColumns {
    /// 首欄（課程名稱或活動類型）欄寬。
    pub(super) label: usize,
    /// 標題欄寬。
    pub(super) title: usize,
    /// 狀態欄寬；`0` 代表不顯示狀態欄。
    pub(super) state: usize,
    /// 截止時間欄寬。
    pub(super) deadline: usize,
    /// 是否顯示「截止」前綴。
    pub(super) deadline_prefix: bool,
    /// 是否使用不含年份的精簡日期。
    pub(super) compact: bool,
    /// 是否顯示「小组」欄。
    pub(super) group: bool,
}

/// 作業／活動清單的文字欄內容需求（可見列的最大顯示寬）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RowNeeds {
    /// 標籤欄（課程名稱或活動類型）需求寬度。
    label: usize,
    /// 標題欄需求寬度。
    title: usize,
}

impl RowNeeds {
    /// 由作業列組出需求。
    pub(super) fn of_homework(items: &[&HomeworkItem]) -> Self {
        Self {
            label: items
                .iter()
                .map(|item| display_width(&item.course_name))
                .max()
                .unwrap_or(0),
            title: items
                .iter()
                .map(|item| display_width(&item.title))
                .max()
                .unwrap_or(0),
        }
    }

    /// 由活動列組出需求（類型欄為固定寬度）。
    pub(super) fn of_activities(activities: &[&LmsActivity]) -> Self {
        Self {
            label: ACTIVITY_KIND_WIDTH,
            title: activities
                .iter()
                .map(|activity| display_width(&activity.display_title()))
                .max()
                .unwrap_or(0),
        }
    }

    /// 由自訂義任務列組出需求（標籤欄放優先級與選填標籤）。
    pub(super) fn of_tasks(tasks: &[&Task]) -> Self {
        Self {
            label: tasks
                .iter()
                .map(|task| match task.display_tag() {
                    Some(tag) => {
                        TASK_PRIORITY_WIDTH + display_width(TAG_SEPARATOR) + display_width(tag)
                    }
                    None => TASK_PRIORITY_WIDTH,
                })
                .max()
                .unwrap_or(TASK_PRIORITY_WIDTH),
            title: tasks
                .iter()
                .map(|task| display_width(&task.content))
                .max()
                .unwrap_or(0),
        }
    }

    /// 合併兩組需求（各欄取最大值）；任務頁的作業與任務共用同一組欄寬。
    pub(super) fn merge(self, other: Self) -> Self {
        Self {
            label: self.label.max(other.label),
            title: self.title.max(other.title),
        }
    }
}

/// 作業清單欄寬；終端過窄而無法完整顯示時回傳 `None`。
#[cfg(test)]
pub(super) fn homework_columns(available: usize, needs: RowNeeds) -> Option<RowColumns> {
    row_columns(
        available,
        HOMEWORK_COURSE_MIN_WIDTH,
        HOMEWORK_STATE_WIDTH,
        needs,
    )
}

/// 活動清單欄寬（類型欄固定寬度、沒有狀態欄）；終端過窄時回傳 `None`。
pub(super) fn activity_columns(available: usize, needs: RowNeeds) -> Option<RowColumns> {
    row_columns(available, ACTIVITY_KIND_WIDTH, 0, needs)
}

/// 作業清單可完整顯示所需的最小列寬（測試用；實際由 [`page_min_row_width`] 提供）。
#[cfg(test)]
pub(super) fn homework_min_row_width() -> usize {
    row_min_row_width(HOMEWORK_COURSE_MIN_WIDTH, HOMEWORK_STATE_WIDTH)
}

/// 任務頁清單欄寬（作業與任務共用同一組欄位骨架）。
///
/// `tasks_only` 為真（當前分組只有任務）時，標籤欄只需放優先級；否則至少要
/// 放得下課程名稱。終端過窄而無法完整顯示時回傳 `None`。
pub(super) fn page_columns(
    available: usize,
    needs: RowNeeds,
    tasks_only: bool,
) -> Option<RowColumns> {
    row_columns(
        available,
        page_label_min(tasks_only),
        HOMEWORK_STATE_WIDTH,
        needs,
    )
}

/// 任務頁清單可完整顯示所需的最小列寬。
pub(super) fn page_min_row_width(tasks_only: bool) -> usize {
    row_min_row_width(page_label_min(tasks_only), HOMEWORK_STATE_WIDTH)
}

fn page_label_min(tasks_only: bool) -> usize {
    if tasks_only {
        TASK_PRIORITY_WIDTH
    } else {
        HOMEWORK_COURSE_MIN_WIDTH
    }
}

/// 活動清單可完整顯示所需的最小列寬。
pub(super) fn activity_min_row_width() -> usize {
    row_min_row_width(ACTIVITY_KIND_WIDTH, 0)
}

/// 依可用寬度與內容需求決定欄寬。
///
/// 欄位自左而右緊密排列，用不到的寬度留在**列尾**——不像以往把標題欄撐滿、
/// 將後半欄位推到畫面右側。標籤欄與標題欄以可見內容的寬度為準，空間足夠即
/// 完整顯示；不足時先依序犧牲「小组」欄、截止前綴與年份，再依內容需求比例
/// 縮減，連下限都放不下時回傳 `None`（由呼叫端提示放大終端）。
fn row_columns(
    available: usize,
    label_min: usize,
    state: usize,
    needs: RowNeeds,
) -> Option<RowColumns> {
    /// 依偏好排序的欄位組合：（顯示「小组」欄、顯示「截止」前綴、截止欄寬、寬裕量）。
    ///
    /// 「寬裕量」是標籤欄與標題欄下限之外的額外寬度：空間足夠寬鬆才採用該
    /// 組合，否則退而求其次（寧可先少顯示前綴或年份，也不把兩個文字欄壓到下限）。
    const TIERS: [(bool, bool, usize, usize); 4] = [
        (true, true, DEADLINE_FULL_WIDTH, 8),
        (false, true, DEADLINE_FULL_WIDTH, 8),
        (false, false, DEADLINE_FULL_WIDTH, 4),
        (false, false, DEADLINE_COMPACT_WIDTH, 0),
    ];
    let minimum = label_min + TITLE_MIN_WIDTH;
    let mut fallback = None;
    for (group, prefix, deadline, slack) in TIERS {
        let rest = available.saturating_sub(row_fixed_width(state, deadline, prefix, group));
        if rest < minimum {
            continue;
        }
        let columns = row_columns_at(rest, label_min, state, deadline, prefix, group, needs);
        if rest >= minimum + slack {
            return Some(columns);
        }
        // 後續組合的可分配寬度只會更大，先記住目前最寬的可行組合。
        fallback = Some(columns);
    }
    fallback
}

/// 依欄位組合計算各欄寬（`rest` 為標籤欄與標題欄可分配的寬度）。
fn row_columns_at(
    rest: usize,
    label_min: usize,
    state: usize,
    deadline: usize,
    deadline_prefix: bool,
    group: bool,
    needs: RowNeeds,
) -> RowColumns {
    let (label, title) = split_text_widths(rest, label_min, needs);
    RowColumns {
        label,
        title,
        state,
        deadline,
        deadline_prefix,
        compact: deadline == DEADLINE_COMPACT_WIDTH,
        group,
    }
}

/// 在標籤欄與標題欄之間分配寬度。
///
/// 空間足夠時兩欄都取內容需求寬度（完整顯示，剩餘寬度留在列尾）；不足時依
/// 內容需求比例分配，並確保標題欄不低於下限。
fn split_text_widths(rest: usize, label_min: usize, needs: RowNeeds) -> (usize, usize) {
    let label_need = needs.label.max(label_min);
    let title_need = needs.title.max(TITLE_MIN_WIDTH);
    if label_need + title_need <= rest {
        return (label_need, title_need);
    }
    let total = label_need + title_need;
    let label = (rest * label_need / total).clamp(label_min, rest.saturating_sub(TITLE_MIN_WIDTH));
    (label, rest - label)
}

/// 指定欄位組合的固定寬度（不含標籤欄與標題欄，含所有欄間隔）。
fn row_fixed_width(state: usize, deadline: usize, prefix: bool, group: bool) -> usize {
    state
        + deadline
        + if prefix {
            display_width(DEADLINE_PREFIX)
        } else {
            0
        }
        + if group { GROUP_WIDTH } else { 0 }
        // 間隔數＝欄數 − 1（標籤｜標題｜（狀態）｜截止｜（小组））。
        + 2
        + usize::from(state > 0)
        + usize::from(group)
}

/// 清單可完整顯示所需的最小列寬（標籤欄與標題欄均取下限）。
fn row_min_row_width(label_min: usize, state: usize) -> usize {
    row_fixed_width(state, DEADLINE_COMPACT_WIDTH, false, false) + label_min + TITLE_MIN_WIDTH
}

/// 流水清單的時間／地點欄寬（單位為終端顯示欄）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FlowColumns {
    /// 時間欄寬。
    pub(super) time: usize,
    /// 地點欄寬。
    pub(super) place: usize,
}

/// 依可用寬度決定欄寬：先縮地點、再縮時間，下限分別為 8／14 欄。
///
/// 極窄畫面維持下限，超出的部分交由清單自然裁切；狀態欄因此固定起點。
pub(super) fn flow_columns(available: usize) -> FlowColumns {
    const TIME_DEFAULT: usize = 20;
    const PLACE_DEFAULT: usize = 16;
    const TIME_MIN: usize = 14;
    const PLACE_MIN: usize = 8;
    // 「未匹配」的顯示寬度（3 個全角字）。
    const STATUS: usize = 6;
    // 時間與地點、地點與狀態之間的空白。
    const GAPS: usize = 2;

    let place = available
        .saturating_sub(TIME_DEFAULT + STATUS + GAPS)
        .clamp(PLACE_MIN, PLACE_DEFAULT);
    let time = if place <= PLACE_MIN {
        available
            .saturating_sub(place + STATUS + GAPS)
            .clamp(TIME_MIN, TIME_DEFAULT)
    } else {
        TIME_DEFAULT
    };
    FlowColumns { time, place }
}

#[cfg(test)]
#[path = "tests/columns_test.rs"]
mod columns_test;
