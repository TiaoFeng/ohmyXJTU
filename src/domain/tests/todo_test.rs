//! 自訂義任務模型、排序、輸入解析與列模型的單元測試。

use chrono::{DateTime, FixedOffset, TimeZone};

use super::*;

fn at(hour: u32, minute: u32, second: u32) -> DateTime<FixedOffset> {
    let offset = FixedOffset::east_opt(8 * 3600).unwrap();
    offset
        .with_ymd_and_hms(2026, 12, 31, hour, minute, second)
        .unwrap()
}

fn task(
    id: u64,
    content: &str,
    deadline: Option<DateTime<FixedOffset>>,
    priority: Priority,
) -> Task {
    Task {
        id,
        content: content.to_owned(),
        description: None,
        deadline,
        priority,
        completed: false,
    }
}

#[test]
fn parse_deadline_input_accepts_supported_shapes() {
    assert_eq!(parse_deadline_input("").unwrap(), None);
    assert_eq!(parse_deadline_input("   ").unwrap(), None);
    assert_eq!(
        parse_deadline_input("2026-12-31").unwrap(),
        Some(at(23, 59, 59))
    );
    assert_eq!(
        parse_deadline_input("2026-12-31 12:30").unwrap(),
        Some(at(12, 30, 0))
    );
    assert_eq!(
        parse_deadline_input("2026-12-31 12:30:05").unwrap(),
        Some(at(12, 30, 5))
    );
    assert_eq!(
        parse_deadline_input("2026-12-31T12:30").unwrap(),
        Some(at(12, 30, 0))
    );
    assert_eq!(
        parse_deadline_input(" 2026-12-31T12:30:05 ").unwrap(),
        Some(at(12, 30, 5))
    );
}

#[test]
fn parse_deadline_input_rejects_unknown_formats_without_leaking_input() {
    let err = parse_deadline_input("banana-明天").unwrap_err();
    assert!(err.contains("无法识别"), "应说明格式错误：{err}");
    assert!(!err.contains("banana"), "不得夹带输入值：{err}");
    assert!(
        parse_deadline_input("2026-13-40").is_err(),
        "非法日期应被拒绝"
    );
}

#[test]
fn priority_cycles_and_labels() {
    assert_eq!(Priority::ALL.len(), 3);
    assert_eq!(Priority::High.label(), "高");
    assert_eq!(Priority::Medium.label(), "中");
    assert_eq!(Priority::Low.label(), "低");
    assert_eq!(Priority::High.next(), Priority::Medium);
    assert_eq!(Priority::Low.next(), Priority::High);
    assert_eq!(Priority::High.previous(), Priority::Low);
    assert!(Priority::High < Priority::Low, "排序时高优先级在前");
}

#[test]
fn task_state_follows_completion_and_deadline() {
    let now = at(12, 0, 0);
    let overdue = task(1, "过期的", Some(at(11, 0, 0)), Priority::Low);
    assert_eq!(overdue.state(now), TaskState::Overdue);
    assert_eq!(overdue.group(), HomeworkGroup::Unfinished);

    let pending = task(2, "待办的", Some(at(13, 0, 0)), Priority::Low);
    assert_eq!(pending.state(now), TaskState::Pending);

    let no_deadline = task(3, "没有截止", None, Priority::Low);
    assert_eq!(no_deadline.state(now), TaskState::Pending);

    let mut done = task(4, "完成的", Some(at(11, 0, 0)), Priority::Low);
    done.completed = true;
    assert_eq!(done.state(now), TaskState::Done);
    assert_eq!(done.group(), HomeworkGroup::Completed);
}

#[test]
fn sort_tasks_orders_by_deadline_then_priority_then_content() {
    let mut tasks = vec![
        task(1, "无截止", None, Priority::High),
        task(2, "晚的", Some(at(20, 0, 0)), Priority::High),
        task(3, "早的低", Some(at(8, 0, 0)), Priority::Low),
        task(4, "早的高", Some(at(8, 0, 0)), Priority::High),
        task(5, "早的中", Some(at(8, 0, 0)), Priority::Medium),
    ];
    sort_tasks(&mut tasks);
    let order: Vec<&str> = tasks.iter().map(|task| task.content.as_str()).collect();
    assert_eq!(order, ["早的高", "早的中", "早的低", "晚的", "无截止"]);
}

#[test]
fn matches_looks_at_content_and_description_case_insensitively() {
    let mut task = task(1, "Submit Report", Some(at(12, 0, 0)), Priority::Low);
    task.description = Some("需要 PDF 格式".to_owned());
    assert!(task.matches("submit"));
    assert!(task.matches("REPORT"));
    assert!(task.matches("pdf"));
    assert!(task.matches("  "), "空关键字视为符合");
    assert!(!task.matches("missing"));
}

#[test]
fn page_rows_put_tasks_first_with_a_spacer_only_between_sections() {
    let tasks = [task(1, "任务一", None, Priority::Low)];
    let homework_items = [HomeworkItem {
        course_id: "1".to_owned(),
        course_name: "课程".to_owned(),
        activity_id: "11".to_owned(),
        title: "作业一".to_owned(),
        end_time: None,
        description: None,
        submit_by_group: Some(false),
        state: crate::domain::homework::HomeworkState::Pending,
        note: None,
    }];
    let task_refs: Vec<&Task> = tasks.iter().collect();
    let homework_refs: Vec<&HomeworkItem> = homework_items.iter().collect();

    let rows = page_rows(&task_refs, &homework_refs);
    assert_eq!(rows.len(), 5, "标题＋任务＋空白＋标题＋作业");
    assert!(matches!(rows[0], PageRow::Header(TASKS_HEADER)));
    assert!(matches!(rows[1], PageRow::Task(_)));
    assert!(matches!(rows[2], PageRow::Spacer));
    assert!(matches!(rows[3], PageRow::Header(HOMEWORK_HEADER)));
    assert!(matches!(rows[4], PageRow::Homework(_)));
    assert_eq!(selectable_len(task_refs.len(), homework_refs.len()), 2);

    // 選擇索引映射：0＝任務（第 1 列）、1＝作業（第 4 列）；標題與空白列不計入。
    assert_eq!(visual_index(&rows, 0), Some(1));
    assert_eq!(visual_index(&rows, 1), Some(4));
    assert_eq!(visual_index(&rows, 2), None);

    // 只有作業：不插空白列。
    let only_homework = page_rows(&[], &homework_refs);
    assert_eq!(only_homework.len(), 2);
    assert!(matches!(only_homework[0], PageRow::Header(HOMEWORK_HEADER)));
    assert_eq!(visual_index(&only_homework, 0), Some(1));

    // 只有任務：同樣不留空白列。
    let only_tasks = page_rows(&task_refs, &[]);
    assert_eq!(only_tasks.len(), 2);

    // 皆空：沒有列。
    assert!(page_rows(&[], &[]).is_empty());
}

#[test]
fn task_round_trips_through_json() {
    let original = Task {
        id: 7,
        content: "写实验报告".to_owned(),
        description: Some("第三章".to_owned()),
        deadline: Some(at(23, 59, 59)),
        priority: Priority::High,
        completed: true,
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains("\"high\""), "优先级以小写字符串保存：{json}");
    let parsed: Task = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);

    // 缺省欄位（舊檔或手寫檔）不應讓解析失敗。
    let minimal: Task = serde_json::from_str(r#"{"id":1,"content":"x"}"#).unwrap();
    assert_eq!(minimal.priority, Priority::Low);
    assert!(!minimal.completed);
    assert_eq!(minimal.deadline, None);
}
