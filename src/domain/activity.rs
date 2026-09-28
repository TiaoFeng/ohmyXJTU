//! 思源學堂活動的分組與排序。
//!
//! 活動依類型分為直播、課程內容、作業、資料與其他五組（顯示順序即
//! [`ActivityGroup::ALL`] 的順序）；同一組內以「截止時間 → 開始時間 →
//! 識別碼」穩定排序，缺少或無法解析的時間一律置後，不會被誤排到最前。

use crate::domain::homework::parse_time;
use crate::sites::lms::{ActivityKind, LmsActivity};

/// 活動分組（同時決定顯示順序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityGroup {
    /// 直播。
    #[default]
    LectureLive,
    /// 課程內容（錄播）。
    Lesson,
    /// 作業。
    Homework,
    /// 資料。
    Material,
    /// 其他（未知類型）。
    Other,
}

impl ActivityGroup {
    /// 全部組別（依顯示順序）。
    pub const ALL: [ActivityGroup; 5] = [
        ActivityGroup::LectureLive,
        ActivityGroup::Lesson,
        ActivityGroup::Homework,
        ActivityGroup::Material,
        ActivityGroup::Other,
    ];

    /// 顯示名稱。
    pub fn label(self) -> &'static str {
        match self {
            ActivityGroup::LectureLive => "直播",
            ActivityGroup::Lesson => "课程内容",
            ActivityGroup::Homework => "作业",
            ActivityGroup::Material => "资料",
            ActivityGroup::Other => "其他",
        }
    }

    /// 依活動類型分組。
    pub fn from_kind(kind: ActivityKind) -> Self {
        match kind {
            ActivityKind::LectureLive => Self::LectureLive,
            ActivityKind::Lesson => Self::Lesson,
            ActivityKind::Homework => Self::Homework,
            ActivityKind::Material => Self::Material,
            ActivityKind::Unknown => Self::Other,
        }
    }

    /// 下一個分組（循環）。
    pub fn next(self) -> Self {
        self.shifted(1)
    }

    /// 上一個分組（循環）。
    pub fn previous(self) -> Self {
        self.shifted(-1)
    }

    fn shifted(self, delta: i32) -> Self {
        let len = i32::try_from(Self::ALL.len()).unwrap_or(1);
        let index = Self::ALL
            .iter()
            .position(|group| *group == self)
            .map_or(0, |index| i32::try_from(index).unwrap_or(0));
        let next = (index + delta).rem_euclid(len);
        Self::ALL[usize::try_from(next).unwrap_or(0)]
    }
}

/// 指定分組的活動（已穩定排序）。
pub fn grouped(activities: &[LmsActivity], group: ActivityGroup) -> Vec<&LmsActivity> {
    let mut items: Vec<&LmsActivity> = activities
        .iter()
        .filter(|activity| ActivityGroup::from_kind(activity.kind()) == group)
        .collect();
    items.sort_by_key(|activity| sort_key(activity));
    items
}

/// 各分組的活動數（依 [`ActivityGroup::ALL`] 順序）。
pub fn counts(activities: &[LmsActivity]) -> [(ActivityGroup, usize); ActivityGroup::ALL.len()] {
    let mut counts = ActivityGroup::ALL.map(|group| (group, 0_usize));
    for activity in activities {
        let group = ActivityGroup::from_kind(activity.kind());
        if let Some(entry) = counts.iter_mut().find(|(candidate, _)| *candidate == group) {
            entry.1 += 1;
        }
    }
    counts
}

/// 排序鍵：截止（無者置後）→ 開始（無者置後）→ 數值識別碼 → 原始識別碼。
fn sort_key(activity: &LmsActivity) -> (u8, i64, u8, i64, u8, u64, String) {
    let end = parse_time(activity.end_time.as_deref());
    let start = parse_time(activity.start_time.as_deref());
    let (id_numeric, id_value) = match activity.id.parse::<u64>() {
        Ok(value) => (0_u8, value),
        Err(_) => (1_u8, 0_u64),
    };
    (
        u8::from(end.is_none()),
        end.map_or(0, |time| time.timestamp_millis()),
        u8::from(start.is_none()),
        start.map_or(0, |time| time.timestamp_millis()),
        id_numeric,
        id_value,
        activity.id.clone(),
    )
}

#[cfg(test)]
#[path = "tests/activity_test.rs"]
mod activity_test;
