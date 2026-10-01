//! 列表欄寬計算測試（純函式，不經渲染）。

use super::*;

#[test]
fn flow_columns_keep_defaults_when_width_allows() {
    let wide = flow_columns(80);
    assert_eq!((wide.time, wide.place), (20, 16));
    let exact = flow_columns(44);
    assert_eq!((exact.time, exact.place), (20, 16), "恰好放得下時不縮減");
}

#[test]
fn flow_columns_shrink_place_before_time() {
    let narrow = flow_columns(40);
    assert_eq!(narrow.time, 20, "寬度足夠時時間欄不縮");
    assert_eq!(narrow.place, 12);

    let at_min = flow_columns(36);
    assert_eq!(at_min.time, 20);
    assert_eq!(at_min.place, 8, "地點先縮到下限");
}

#[test]
fn flow_columns_shrink_time_at_the_end() {
    let narrower = flow_columns(34);
    assert_eq!(narrower.place, 8);
    assert_eq!(narrower.time, 18);

    let floor = flow_columns(28);
    assert_eq!((floor.time, floor.place), (14, 8), "低於下限時維持最小欄寬");
}

/// 作業／活動欄寬測試用的內容需求。
fn row_needs(label: usize, title: usize) -> RowNeeds {
    RowNeeds { label, title }
}

/// 課表欄寬測試用的內容需求。
fn schedule_needs(course: usize, classroom: usize, teacher: usize) -> ScheduleNeeds {
    ScheduleNeeds {
        course,
        classroom,
        teacher,
    }
}

#[test]
fn homework_columns_follow_content_width() {
    // 空間足夠時，兩個文字欄都取內容寬度（完整顯示），剩餘寬度留在列尾。
    let short = homework_columns(75, row_needs(14, 10)).expect("寬畫面可完整顯示");
    assert_eq!((short.label, short.title), (14, 10), "欄寬依內容而定");
    assert!(short.group, "寬畫面保留「小组」欄");
    assert!(short.deadline_prefix, "寬畫面保留「截止」前綴");
    let total = short.label
        + short.title
        + short.state
        + short.deadline
        + display_width(DEADLINE_PREFIX)
        + GROUP_WIDTH
        + 4;
    assert!(total <= 75, "欄位總寬不得超過可用寬度：{total}");

    // 內容更長時欄位跟著長大（上限為可用寬度），不會被固定上限截斷。
    let long = homework_columns(140, row_needs(40, 30)).expect("寬畫面可完整顯示");
    assert_eq!(
        (long.label, long.title),
        (40, 30),
        "長課程名稱與長標題在足夠寬的畫面應完整顯示"
    );
}

#[test]
fn homework_columns_share_width_by_content_when_tight() {
    // 空間不足時依內容需求比例分配。
    let tight = homework_columns(75, row_needs(24, 24)).expect("可完整顯示");
    assert_eq!((tight.label, tight.title), (20, 20));
    assert!(tight.group);
}

#[test]
fn homework_columns_trade_away_group_prefix_then_year() {
    let needs = row_needs(24, 24);
    // 先犧牲「小组」欄（仍保留「截止」前綴）。
    let no_group = homework_columns(54, needs).expect("可完整顯示");
    assert!(!no_group.group);
    assert!(no_group.deadline_prefix);
    assert_eq!((no_group.label, no_group.title), (12, 12));

    // 再犧牲「截止」前綴。
    let no_prefix = homework_columns(45, needs).expect("可完整顯示");
    assert!(!no_prefix.group);
    assert!(!no_prefix.deadline_prefix);
    assert!(!no_prefix.compact, "此時仍保留年份");
    assert_eq!((no_prefix.label, no_prefix.title), (10, 10));

    // 最後才壓縮日期為 `MM-DD HH:MM`。
    let compact = homework_columns(36, needs).expect("可完整顯示");
    assert!(compact.compact);
    assert_eq!(compact.deadline, DEADLINE_COMPACT_WIDTH);
    assert_eq!((compact.label, compact.title), (8, 8));
}

#[test]
fn homework_columns_refuse_widths_below_minimum() {
    let needs = row_needs(20, 20);
    assert_eq!(
        homework_columns(31, needs),
        None,
        "低於最小列寬應回報終端過窄"
    );
    let minimum = homework_columns(32, needs).expect("最小列寬仍可顯示");
    assert_eq!(
        (minimum.label, minimum.title),
        (TITLE_MIN_WIDTH, TITLE_MIN_WIDTH),
        "最小寬度下兩個文字欄都取下限"
    );
    assert_eq!(homework_min_row_width(), 32);
}

#[test]
fn activity_columns_omit_state_and_fix_kind_width() {
    let needs = row_needs(ACTIVITY_KIND_WIDTH, 22);
    let wide = activity_columns(75, needs).expect("寬畫面可完整顯示");
    assert_eq!(wide.label, ACTIVITY_KIND_WIDTH, "類型欄固定寬度");
    assert_eq!(wide.state, 0, "活動沒有狀態欄");
    assert_eq!(wide.deadline, DEADLINE_FULL_WIDTH);
    assert_eq!(wide.title, 22, "標題欄取內容寬度");

    let narrow = activity_columns(30, needs).expect("更窄畫面仍可顯示");
    assert!(narrow.compact);
    assert_eq!(
        activity_columns(26, needs),
        None,
        "低於最小列寬應回報終端過窄"
    );
    assert_eq!(activity_min_row_width(), 27);
}

#[test]
fn schedule_columns_follow_content_width_when_wide() {
    let needs = schedule_needs(14, 10, 8);
    let wide = schedule_columns(75, needs).expect("寬畫面可完整顯示");
    assert_eq!(
        (wide.course, wide.classroom, wide.teacher),
        (14, 10, 8),
        "三欄都以內容寬度顯示"
    );

    // 較長的地點與教師同樣完整顯示（不受固定上限限制）。
    let longer = schedule_columns(120, schedule_needs(30, 16, 8)).expect("寬畫面可完整顯示");
    assert_eq!((longer.course, longer.classroom), (30, 16));
}

#[test]
fn schedule_columns_shrink_teacher_before_classroom() {
    // 空間不足時先收教師、再縮地點，課程名稱最後才縮。
    let needs = schedule_needs(30, 12, 8);
    let tight = schedule_columns(55, needs).expect("可完整顯示");
    assert_eq!(tight.teacher, 0, "先收起教師欄");
    assert_eq!(
        tight.classroom, SCHEDULE_CLASSROOM_MIN_WIDTH,
        "再縮地點到下限"
    );
    assert_eq!(
        tight.course,
        55 - schedule_fixed_width() - SCHEDULE_CLASSROOM_MIN_WIDTH,
        "其餘寬度全部給課程欄"
    );
}

#[test]
fn schedule_columns_refuse_widths_below_minimum() {
    let needs = schedule_needs(30, 12, 8);
    assert_eq!(
        schedule_columns(39, needs),
        None,
        "低於最小列寬應回報終端過窄"
    );
    let minimum = schedule_columns(40, needs).expect("最小列寬仍可顯示");
    assert_eq!(minimum.course, SCHEDULE_COURSE_MIN_WIDTH);
    assert_eq!(minimum.classroom, SCHEDULE_CLASSROOM_MIN_WIDTH);
    assert_eq!(minimum.teacher, 0);
    assert_eq!(schedule_min_row_width(), 40);
}
