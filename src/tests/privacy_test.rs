//! 用户协议內容管線測試：Markdown 輕量解析、顯示寬度換行與內嵌版本一致性。

use crate::privacy::{self, DocLine, LineKind};
use crate::tui::text::display_width;

#[test]
fn parse_recognizes_headings_lists_quotes_and_rules() {
    let md =
        "# 标题\n\n## 一、章节\n\n普通段落 **粗体** 与 `代码`。\n\n- 项目一\n\n> 引用\n\n---\n";
    let lines = privacy::parse(md);
    assert_eq!(lines[0], DocLine::new("标题", LineKind::Title));
    assert_eq!(lines[2], DocLine::new("一、章节", LineKind::Heading));
    assert_eq!(
        lines[4],
        DocLine::new("普通段落 粗体 与 代码。", LineKind::Body)
    );
    assert_eq!(lines[6], DocLine::new("• 项目一", LineKind::Bullet));
    assert_eq!(lines[8], DocLine::new("│ 引用", LineKind::Quote));
    assert_eq!(lines[10].kind, LineKind::Rule);
}

#[test]
fn parse_tables_keep_cells_and_turn_separators_into_rules() {
    let md = "| 平台 | 目录 |\n| --- | :---: |\n| Linux | `~/.data/` |\n";
    let lines = privacy::parse(md);
    assert_eq!(lines[0], DocLine::new("平台 | 目录", LineKind::Table));
    assert_eq!(lines[1].kind, LineKind::Rule);
    assert_eq!(lines[2], DocLine::new("Linux | ~/.data/", LineKind::Table));
}

#[test]
fn parse_links_keep_label_or_fall_back_to_url() {
    let md = "见 [LICENSE](LICENSE) 与 [文档](https://example.com/doc)。空链接 [](https://example.com/x)。";
    let lines = privacy::parse(md);
    assert_eq!(
        lines[0].text,
        "见 LICENSE 与 文档。空链接 https://example.com/x。"
    );
}

#[test]
fn wrap_respects_display_width_for_cjk() {
    let lines = vec![DocLine::new("中文换行测试内容", LineKind::Body)];
    let wrapped = privacy::wrap(&lines, 6);
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
    let wrapped = privacy::wrap(&lines, 7);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(wrapped[0].text, "hello");
    assert_eq!(wrapped[1].text, "world");
}

#[test]
fn wrap_never_splits_ascii_runs_at_line_boundaries() {
    let lines = vec![DocLine::new("主机。TLS 保护传输内容", LineKind::Body)];
    let wrapped = privacy::wrap(&lines, 8);
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
        "《ohmyXJTU 用户协议同意與否之详细说明文字",
        LineKind::Body,
    )];
    let wrapped = privacy::wrap(&lines, 20);
    assert!(
        display_width(&wrapped[0].text) >= 16,
        "首行應盡量填滿：{:?}",
        wrapped[0].text
    );
    assert!(wrapped[0].text.contains("用户协议"));
}

#[test]
fn wrap_hard_breaks_long_words_by_grapheme() {
    let url = "https://example.com/some/very/long/path";
    let lines = vec![DocLine::new(url, LineKind::Body)];
    let wrapped = privacy::wrap(&lines, 8);
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
    let wrapped = privacy::wrap(&lines, 6);
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
    assert!(privacy::wrap(&lines, 0).is_empty());
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
    let lines = privacy::document();
    assert!(lines.len() >= 200, "协议内容不应被截断");
    let headings = lines
        .iter()
        .filter(|line| line.kind == LineKind::Heading)
        .count();
    assert_eq!(headings, 13, "应保留全部章節標題");
    assert_eq!(
        privacy::document(),
        privacy::parse(privacy::TEXT).as_slice()
    );
}
