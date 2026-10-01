//! 表格排版測試：對齊表格與卡片兩種模式的選擇、欄位對齊、懸掛縮排與內容不遺失。

use crate::privacy::{DocLine, LineKind, Table};
use crate::text::display_width;

/// 建立測試表格。
fn table(headers: &[&str], rows: &[&[&str]]) -> Table {
    Table::new(
        headers.iter().map(|cell| (*cell).to_owned()).collect(),
        rows.iter()
            .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
            .collect(),
    )
}

/// 子字串在該列中的起始顯示欄。
fn column_start(line: &str, needle: &str) -> usize {
    let offset = line.find(needle).expect("子字串应存在于该列");
    display_width(&line[..offset])
}

/// 所有列的內容（不含空白）串接，用於檢查內容未被排版弄丟。
fn compact_text(lines: &[DocLine]) -> String {
    lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("")
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[test]
fn short_table_is_rendered_as_an_aligned_grid() {
    let table = table(
        &["平台", "目录"],
        &[&["Linux", "~/.local/share/"], &["macOS", "~/Library/"]],
    );
    let lines = table.layout(60);

    assert_eq!(lines.len(), 4, "標頭＋細線＋兩列：{lines:?}");
    assert_eq!(lines[0].kind, LineKind::TableHeader);
    assert_eq!(lines[1].kind, LineKind::TableRule);
    assert_eq!(lines[2].kind, LineKind::TableRow);
    assert_eq!(lines[3].kind, LineKind::TableRow);

    // 第一欄寬 = max(平台, Linux, macOS) = 5，加分隔 3 欄 → 第二欄起於第 8 欄。
    assert_eq!(column_start(&lines[0].text, "目录"), 8);
    assert_eq!(column_start(&lines[2].text, "~/.local/share/"), 8);
    assert_eq!(column_start(&lines[3].text, "~/Library/"), 8);
    assert_eq!(display_width(&lines[1].text), 8 + 15, "細線應等於表格寬");
    assert!(lines.iter().all(|line| line.label_len == 0));
}

#[test]
fn boundary_switches_between_grid_and_cards() {
    let table = table(&["平台", "目录"], &[&["Linux", "~/.local/share/"]]);
    // 表格總寬 = 5 + 3 + 15 = 23。
    assert_eq!(table.layout(23)[0].kind, LineKind::TableHeader);

    let narrow = table.layout(22);
    assert_eq!(narrow[0].kind, LineKind::TableTitle);
    assert_eq!(narrow[0].text, "▸ Linux");
    assert_eq!(narrow[0].label_len, 2, "標題符號以標籤樣式呈現");
    assert!(narrow[1].text.starts_with("  目录：~/.local/share"));
    assert!(narrow.iter().all(|line| display_width(&line.text) <= 22));
}

#[test]
fn wide_table_becomes_cards_with_aligned_labels() {
    let table = table(
        &["主机", "用途", "会上传的内容"],
        &[&[
            "login.xjtu.edu.cn",
            "统一身份认证",
            "账号、RSA 加密后的密码、验证码答案与装置标识，内容相当长需要换行显示",
        ]],
    );
    let lines = table.layout(40);

    assert_eq!(lines[0].kind, LineKind::TableTitle);
    assert_eq!(lines[0].text, "▸ login.xjtu.edu.cn");
    assert_eq!(lines[1].kind, LineKind::TableField);
    assert!(lines[1].text.starts_with("  用途"));
    assert!(lines[1].text.ends_with("：统一身份认证"));
    assert!(lines[2].text.starts_with("  会上传的内容："));

    // 標籤欄寬 = 「会上传的内容」的 12 欄，加縮排 2 與分隔 2 → 值起於第 16 欄。
    assert_eq!(lines[1].label_len, 16);
    assert_eq!(lines[2].label_len, 16);
    assert!(
        lines[2..].iter().all(|line| line.label_len >= 16),
        "續行必須維持懸掛縮排：{lines:?}"
    );
    for line in &lines {
        assert!(
            display_width(&line.text) <= 40,
            "不得超出可用寬度：{line:?}"
        );
    }
}

#[test]
fn card_labels_share_the_same_value_column() {
    let table = table(
        &["文件", "内容", "敏感程度"],
        &[&["a.vault", "账号与密码", "高"]],
    );
    let lines = table.layout(30);

    assert!(lines[1].text.starts_with("  内容    ："));
    assert!(lines[2].text.starts_with("  敏感程度："));
    assert_eq!(column_start(&lines[1].text, "账号与密码"), 12);
    assert_eq!(column_start(&lines[2].text, "高"), 12);
}

#[test]
fn long_labels_stand_alone_when_space_is_tight() {
    let table = table(
        &["文件", "内容", "敏感程度"],
        &[&["a.vault", "账号与密码", "高"]],
    );
    // 標籤欄 12 + 縮排 2 + 分隔 2 = 16，只剩 2 欄給值 → 標籤獨立成列。
    let lines = table.layout(18);

    assert!(
        lines.iter().any(|line| line.text == "  内容    ："),
        "標籤應獨立成列：{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.text == "账号与密码"),
        "值應由列首開始，不被標籤欄擠壓：{lines:?}"
    );
}

#[test]
fn card_text_survives_every_width() {
    let value =
        "账号、RSA 加密后的密码、验证码答案与装置标识 fpVisitorId 等，多为服务器下发的流程参数";
    let table = table(
        &["主机", "用途", "会上传的内容"],
        &[&["lms.xjtu.edu.cn", "思源学堂", value]],
    );
    let needle: String = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    for width in [24, 32, 40, 64] {
        let lines = table.layout(width);
        assert!(!lines.is_empty(), "{width} 欄時不應排成空白");
        for line in &lines {
            assert!(
                display_width(&line.text) <= width,
                "{width} 欄時超寬：{line:?}"
            );
        }
        assert!(
            compact_text(&lines).contains(&needle),
            "{width} 欄排版後內容遺失：{lines:?}"
        );
    }
}

#[test]
fn headerless_table_renders_rows_without_a_header_line() {
    let table = table(
        &[],
        &[&["Linux", "~/.local/share/"], &["macOS", "~/Library/"]],
    );
    let lines = table.layout(60);

    assert_eq!(lines.len(), 2, "沒有標頭就沒有標頭列與細線：{lines:?}");
    assert_eq!(lines[0].text, "Linux │ ~/.local/share/");
    assert_eq!(lines[1].text, "macOS │ ~/Library/");
}

#[test]
fn parse_consumes_the_alignment_row() {
    let lines = [
        "| 平台 | 目录 |",
        "| --- | :---: |",
        "| Linux | `~/.data/` |",
    ];
    let table = Table::parse(&lines).expect("应解析为表格");

    assert_eq!(table.headers(), vec!["平台", "目录"]);
    assert_eq!(table.rows(), vec![vec!["Linux", "~/.data/"]]);
}

#[test]
fn parse_without_alignment_row_keeps_every_row_as_data() {
    let lines = ["| a | b |", "| 1 | 2 |"];
    let table = Table::parse(&lines).expect("应解析为表格");

    assert!(table.headers().is_empty(), "沒有對齊列就沒有標頭");
    assert_eq!(
        table.rows(),
        vec![
            vec!["a".to_owned(), "b".to_owned()],
            vec!["1".to_owned(), "2".to_owned()]
        ]
    );
}

#[test]
fn empty_table_renders_nothing() {
    let empty = Table::new(Vec::new(), Vec::new());
    assert!(empty.layout(80).is_empty());
    assert!(empty.layout(0).is_empty());
}
