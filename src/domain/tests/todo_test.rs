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
        tag: None,
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
    assert_eq!(Priority::High.label(), "高");
    assert_eq!(Priority::Medium.label(), "中");
    assert_eq!(Priority::Low.label(), "低");
    // `→` 的方向是「低 → 中 → 高 → 低」（`←` 反向），與畫面選項的直覺一致。
    assert_eq!(Priority::Low.next(), Priority::Medium);
    assert_eq!(Priority::Medium.next(), Priority::High);
    assert_eq!(Priority::High.next(), Priority::Low);
    assert_eq!(Priority::Low.previous(), Priority::High);
    assert_eq!(Priority::Medium.previous(), Priority::Low);
    assert_eq!(Priority::High.previous(), Priority::Medium);
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
        tag: Some("实验".to_owned()),
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

// ── 排序 ────────────────────────────────────────────────

/// 測試用作業（截止時間為原始字串，模擬思源學堂的回應）。
fn homework(activity_id: &str, title: &str, end_time: Option<&str>) -> HomeworkItem {
    HomeworkItem {
        course_id: "1".to_owned(),
        course_name: "编译原理".to_owned(),
        activity_id: activity_id.to_owned(),
        title: title.to_owned(),
        end_time: end_time.map(str::to_owned),
        description: None,
        submit_by_group: Some(false),
        state: crate::domain::homework::HomeworkState::Pending,
        note: None,
    }
}

/// 依排序方式排序任務與作業的混合清單，回傳顯示用的標籤。
fn mixed(mode: SortMode) -> Vec<String> {
    let tasks = [
        task(1, "整理笔记", Some(at(20, 0, 0)), Priority::High),
        task(2, "写实验报告", None, Priority::Low),
        task(3, "复习", Some(at(9, 0, 0)), Priority::Medium),
    ];
    let homework = [
        homework("a-1", "第一次作业", Some("2026-12-30T12:00:00Z")),
        homework("a-2", "第二次作业", None),
    ];
    let mut keyed: Vec<(SortKey, String)> = tasks
        .iter()
        .map(|task| (task_sort_key(task), format!("任务:{}", task.content)))
        .chain(
            homework
                .iter()
                .map(|item| (homework_sort_key(item), format!("作业:{}", item.title))),
        )
        .collect();
    keyed.sort_by(|left, right| compare(mode, &left.0, &right.0));
    keyed.into_iter().map(|(_, label)| label).collect()
}

#[test]
fn sort_modes_are_labelled_and_only_default_is_unsorted() {
    assert_eq!(SortMode::default(), SortMode::Default);
    assert_eq!(SortMode::Default.label(), "默认");
    assert_eq!(SortMode::Priority.label(), "优先级");
    assert_eq!(SortMode::Deadline.label(), "截止时间");
    assert!(!SortMode::Default.is_sorted());
    assert!(SortMode::Priority.is_sorted());
    assert!(SortMode::Deadline.is_sorted());
}

#[test]
fn priority_sort_treats_homework_as_high_priority() {
    // 作業沒有優先級 → 視為「高」；同優先級內再依截止時間（a-1 較早），
    // 沒有截止時間者排在同優先級的最後。
    assert_eq!(
        mixed(SortMode::Priority),
        vec![
            "作业:第一次作业", // 高（作業）・截止 2026-12-30
            "任务:整理笔记",   // 高・截止 20:00
            "作业:第二次作业", // 高（作業）但无截止
            "任务:复习",       // 中
            "任务:写实验报告", // 低・无截止
        ]
    );
}

#[test]
fn deadline_sort_puts_the_earliest_first_and_missing_deadlines_last() {
    assert_eq!(
        mixed(SortMode::Deadline),
        vec![
            "作业:第一次作业", // 2026-12-30 20:00
            "任务:复习",       // 2026-12-31 09:00
            "任务:整理笔记",   // 2026-12-31 20:00
            "作业:第二次作业", // 无截止（高）
            "任务:写实验报告", // 无截止（低）
        ]
    );
}

#[test]
fn sort_breaks_ties_deterministically() {
    // 同優先級、同截止時間 → 依標題（不分大小寫）：作业的 Pending 標題較小。
    let tasks = [task(1, "Zeta", Some(at(9, 0, 0)), Priority::High)];
    let homework = [homework("a-1", "alpha", Some("2026-12-31T01:00:00Z"))];
    let mut keyed = vec![
        (task_sort_key(&tasks[0]), "任务"),
        (homework_sort_key(&homework[0]), "作业"),
    ];
    keyed.sort_by(|left, right| compare(SortMode::Priority, &left.0, &right.0));
    assert_eq!(
        keyed.iter().map(|(_, label)| *label).collect::<Vec<_>>(),
        vec!["作业", "任务"],
        "同优先级同截止时间时依标题排序"
    );

    // 標題也相同時，作業與任務的識別碼仍讓順序完全決定（不依賴來源順序）。
    let mut reversed = keyed;
    reversed.reverse();
    reversed.sort_by(|left, right| compare(SortMode::Priority, &left.0, &right.0));
    assert_eq!(
        reversed.iter().map(|(_, label)| *label).collect::<Vec<_>>(),
        vec!["作业", "任务"]
    );
}

#[test]
fn sort_keys_use_display_free_deadlines_and_numeric_task_ids() {
    // 未帶時區的截止時間視為校園時區（與 `parse_time` 一致），可與任務的截止時間
    // 直接比較瞬間；帶時區者先換算（`Z` ＝ UTC）。
    let item = homework("a-1", "第一次作业", Some("2026-12-31 09:00:00"));
    let key = homework_sort_key(&item);
    assert_eq!(key.deadline, at(9, 0, 0).timestamp());
    assert_eq!(key.missing_deadline, 0);
    assert_eq!(key.priority, Priority::High, "作业一律视为高优先级");
    assert_eq!(
        homework_sort_key(&homework("a-1", "第一次作业", Some("2026-12-31T01:00:00Z"))).deadline,
        at(9, 0, 0).timestamp(),
        "UTC 的截止时间应换算成同一瞬间"
    );
    assert_eq!(
        homework_sort_key(&homework("a-1", "第一次作业", None)).missing_deadline,
        1
    );

    // 任務識別碼補零，讓字串比較等同數值比較。
    assert_eq!(
        task_sort_key(&task(9, "a", None, Priority::Low)).id,
        "0".repeat(ID_SORT_WIDTH - 1) + "9"
    );
    let (small, large) = (
        task_sort_key(&task(9, "a", None, Priority::Low)),
        task_sort_key(&task(10, "a", None, Priority::Low)),
    );
    assert_eq!(
        compare(SortMode::Priority, &small, &large),
        std::cmp::Ordering::Less,
        "任务识别码补零后应按数值比较"
    );

    // 無截止時間：以 missing_deadline 區分，deadline 一律為 0。
    let key = task_sort_key(&task(1, "a", None, Priority::Low));
    assert_eq!(key.missing_deadline, 1);
    assert_eq!(key.deadline, 0);
}

// ── 標籤 ───────────────────────────────────────────────

/// 帶標籤的測試用任務。
fn tagged(id: u64, content: &str, tag: Option<&str>) -> Task {
    Task {
        tag: tag.map(str::to_owned),
        ..task(id, content, None, Priority::Low)
    }
}

#[test]
fn normalize_tag_trims_and_rejects_blank() {
    assert_eq!(normalize_tag("  实验 ").as_deref(), Some("实验"));
    assert_eq!(normalize_tag(""), None);
    assert_eq!(normalize_tag("   "), None, "只剩空白视为没有标签");
    assert_eq!(
        normalize_tag("实验 报告").as_deref(),
        Some("实验 报告"),
        "中间的空白保留"
    );
}

#[test]
fn tag_fits_counts_display_width() {
    // 六個漢字＝ 12 欄：剛好放得下，第七個放不下。
    assert_eq!(TAG_MAX_WIDTH, 12);
    assert_eq!(display_width("六个汉字"), 8);
    assert!(tag_fits("六个汉字", ""));
    assert!(tag_fits("六个汉字宽度", ""), "六个汉字刚好到上限");
    assert!(!tag_fits("六个汉字宽度", "啊"), "第七个汉字超出上限");
    // 全形字佔 2 欄、ASCII 佔 1 欄，一律以顯示寬度計算。
    assert!(tag_fits("abcdefghijk", "l"));
    assert!(!tag_fits("abcdefghijkl", "m"));
}

#[test]
fn tag_options_keep_first_occurrence_order() {
    let tasks = vec![
        tagged(1, "甲", Some("作业")),
        tagged(2, "乙", None),
        tagged(3, "丙", Some("实验")),
        tagged(4, "丁", Some("作业")),
        tagged(5, "戊", Some("   ")),
    ];
    assert_eq!(
        tag_options(&tasks),
        vec!["作业".to_owned(), "实验".to_owned()],
        "去重并保留首次出现顺序，空白标签忽略"
    );
    assert!(tag_options(&[]).is_empty());
}

#[test]
fn task_matches_also_checks_the_tag() {
    let task = tagged(1, "写实验报告", Some("物理"));
    assert!(task.matches("物理"), "标签应可被搜索命中");
    assert!(task.matches("实验"), "内容仍可命中");
    assert!(!task.matches("化学"));
    assert_eq!(
        tagged(1, "甲", Some("   ")).display_tag(),
        None,
        "只剩空白的标签视为没有标签"
    );
}
