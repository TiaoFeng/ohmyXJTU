//! 用户协议內容管線測試：Markdown 輕量解析、表格排版、顯示寬度換行與版本一致性。

use crate::privacy::{self, Block, DocLine, LineKind};
use crate::text::display_width;

/// 以文字列建立文件區塊。
fn blocks(lines: Vec<DocLine>) -> Vec<Block> {
    lines.into_iter().map(Block::Line).collect()
}

/// 取出文字列（非表格）的內容。
fn line_text(block: &Block) -> &str {
    match block {
        Block::Line(line) => &line.text,
        Block::Table(_) => panic!("预期为文字列：{block:?}"),
    }
}

/// 取出文字列（非表格）。
fn line(block: &Block) -> &DocLine {
    match block {
        Block::Line(line) => line,
        Block::Table(_) => panic!("预期为文字列：{block:?}"),
    }
}

#[test]
fn parse_recognizes_headings_lists_quotes_and_rules() {
    let md =
        "# 标题\n\n## 一、章节\n\n普通段落 **粗体** 与 `代码`。\n\n- 项目一\n\n> 引用\n\n---\n";
    let blocks = privacy::parse(md);
    assert_eq!(
        blocks[0],
        Block::Line(DocLine::new("标题", LineKind::Title))
    );
    assert_eq!(
        blocks[2],
        Block::Line(DocLine::new("一、章节", LineKind::Heading))
    );
    assert_eq!(
        blocks[4],
        Block::Line(DocLine::new("普通段落 粗体 与 代码。", LineKind::Body))
    );
    assert_eq!(
        blocks[6],
        Block::Line(DocLine::new("• 项目一", LineKind::Bullet))
    );
    assert_eq!(
        blocks[8],
        Block::Line(DocLine::new("│ 引用", LineKind::Quote))
    );
    assert_eq!(line(&blocks[10]).kind, LineKind::Rule);
}

#[test]
fn parse_collects_consecutive_rows_into_a_table_block() {
    let md = "| 平台 | 目录 |\n| --- | :---: |\n| Linux | `~/.data/` |\n\n后续段落\n";
    let blocks = privacy::parse(md);
    let table = match &blocks[0] {
        Block::Table(table) => table,
        other => panic!("应解析为表格：{other:?}"),
    };
    assert_eq!(table.headers(), vec!["平台", "目录"]);
    assert_eq!(table.rows(), vec![vec!["Linux", "~/.data/"]]);
    // 對齊列被消耗（不再是全寬分隔線），其餘列仍是一般文字。
    assert_eq!(blocks.len(), 3);
    assert_eq!(line(&blocks[1]).kind, LineKind::Body);
    assert_eq!(line_text(&blocks[2]), "后续段落");
}

#[test]
fn table_without_an_alignment_row_has_no_header() {
    let blocks = privacy::parse("| a | b |\n| 1 | 2 |\n");
    let table = match &blocks[0] {
        Block::Table(table) => table,
        other => panic!("应解析为表格：{other:?}"),
    };
    assert!(table.headers().is_empty());
    assert_eq!(table.rows().len(), 2);
}

#[test]
fn parse_links_keep_label_or_fall_back_to_url() {
    let md = "见 [LICENSE](LICENSE) 与 [文档](https://example.com/doc)。空链接 [](https://example.com/x)。";
    let blocks = privacy::parse(md);
    assert_eq!(
        line_text(&blocks[0]),
        "见 LICENSE 与 文档。空链接 https://example.com/x。"
    );
}

#[test]
fn wrap_respects_display_width_for_cjk() {
    let lines = vec![DocLine::new("中文换行测试内容", LineKind::Body)];
    let wrapped = privacy::wrap(&blocks(lines), 6);
    assert!(wrapped.len() > 1, "内容应被换行");
    for line in &wrapped {
        assert!(display_width(&line.text) <= 6, "行超宽：{:?}", line.text);
    }
    let joined: String = wrapped.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(joined, "中文换行测试内容");
}

#[test]
fn wrap_prefers_word_boundaries_for_ascii() {
    let lines = vec![DocLine::new("hello world", LineKind::Body)];
    let wrapped = privacy::wrap(&blocks(lines), 7);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(wrapped[0].text, "hello");
    assert_eq!(wrapped[1].text, "world");
}

#[test]
fn wrap_never_splits_ascii_runs_at_line_boundaries() {
    let lines = vec![DocLine::new("主机。TLS 保护传输内容", LineKind::Body)];
    let wrapped = privacy::wrap(&blocks(lines), 8);
    let texts: Vec<&str> = wrapped.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(
        texts,
        ["主机。", "TLS 保护", "传输内容"],
        "TLS 不得被拆到两行"
    );
}

#[test]
fn wrap_fills_short_leading_word_with_following_cjk() {
    // 短 ASCII 詞後面接長中文串時，不可提早斷行浪費整行。
    let lines = vec![DocLine::new(
        "《ohmyXJTU 用户协议同意与否之详细说明文字",
        LineKind::Body,
    )];
    let wrapped = privacy::wrap(&blocks(lines), 20);
    assert!(
        display_width(&wrapped[0].text) >= 16,
        "首行应尽量填满：{:?}",
        wrapped[0].text
    );
    assert!(wrapped[0].text.contains("用户协议"));
}

#[test]
fn wrap_hard_breaks_long_words_by_grapheme() {
    let url = "https://example.com/some/very/long/path";
    let lines = vec![DocLine::new(url, LineKind::Body)];
    let wrapped = privacy::wrap(&blocks(lines), 8);
    assert!(wrapped.len() > 1);
    for line in &wrapped {
        assert!(display_width(&line.text) <= 8);
    }
    let joined: String = wrapped.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(joined, url);
}

#[test]
fn wrap_keeps_blank_lines_and_line_kinds() {
    let lines = vec![
        DocLine::new("", LineKind::Body),
        DocLine::new("第一段文字很长需要换行", LineKind::Bullet),
    ];
    let wrapped = privacy::wrap(&blocks(lines), 6);
    assert_eq!(wrapped[0], DocLine::new("", LineKind::Body));
    assert!(wrapped.len() > 2);
    assert!(
        wrapped[1..]
            .iter()
            .all(|line| line.kind == LineKind::Bullet),
        "续行应沿用原种类"
    );
}

#[test]
fn wrap_with_zero_width_returns_empty() {
    let lines = vec![DocLine::new("abc", LineKind::Body)];
    assert!(privacy::wrap(&blocks(lines), 0).is_empty());
}

#[test]
fn embedded_version_matches_module_version() {
    assert_eq!(
        privacy::embedded_version(privacy::TEXT),
        Some(privacy::VERSION)
    );
}

#[test]
fn embedded_text_has_expected_shape() {
    let text = privacy::TEXT;
    assert!(text.starts_with("# ohmyXJTU 用户协议"));
    assert!(text.contains("十三、版本历史"));
    assert!(
        text.contains(&format!("| {} |", privacy::VERSION)),
        "版本历史应含当前版本"
    );
    let blocks = privacy::document();
    assert!(blocks.len() >= 200, "协议内容不应被截断");
    let headings = blocks
        .iter()
        .filter(|block| matches!(block, Block::Line(line) if line.kind == LineKind::Heading))
        .count();
    assert_eq!(headings, 13, "应保留全部章节标题");
    let tables = blocks
        .iter()
        .filter(|block| matches!(block, Block::Table(_)))
        .count();
    assert_eq!(tables, 4, "四张表格都应收成表格区块");
    assert_eq!(
        privacy::document(),
        privacy::parse(privacy::TEXT).as_slice()
    );
}

#[test]
fn wrap_keeps_every_line_within_the_width() {
    for width in [24, 32, 40, 64, 80, 120, 200] {
        for line in privacy::wrap(privacy::document(), width) {
            assert!(
                display_width(&line.text) <= width,
                "{width} 栏时超宽：{:?}",
                line.text
            );
        }
    }
}

#[test]
fn wrap_keeps_table_content_at_narrow_widths() {
    let lines = privacy::wrap(privacy::document(), 40);
    let compact: String = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("")
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    for cell in [
        "login.xjtu.edu.cn",
        "会上传的内容",
        "credentials.vault",
        "fpVisitorId",
        "统一身份认证",
        "2026-09-29",
    ] {
        let needle: String = cell
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert!(compact.contains(&needle), "{cell} 在排版后不应消失");
    }
}

#[test]
fn wrapped_table_fields_carry_their_label_columns() {
    let lines = privacy::wrap(privacy::document(), 40);
    let field = lines
        .iter()
        .find(|line| line.kind == LineKind::TableField && line.text.contains("会上传的内容"))
        .expect("§5.1 的卡片字段");
    assert_eq!(field.label_len, 16, "缩进 2 ＋ 标签 12 ＋ 分隔 2");
    assert!(field.text.starts_with("  会上传的内容："));
}
