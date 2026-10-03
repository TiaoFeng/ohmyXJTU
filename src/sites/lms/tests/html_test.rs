//! HTML 轉純文字的單元測試（脫敏的固定片段）。

use super::*;

/// 只取純文字（多數測試不關心是否含圖片）。
fn plain(html: &str) -> Option<String> {
    convert(html).text
}

#[test]
fn paragraphs_are_separated_by_a_single_newline() {
    assert_eq!(
        plain("<p>第一章习题</p><p>第二章习题</p>").as_deref(),
        Some("第一章习题\n第二章习题")
    );
    assert_eq!(
        plain("<div><p>a</p><p>b</p></div>").as_deref(),
        Some("a\nb")
    );
}

#[test]
fn br_forces_a_line_break() {
    for html in ["a<br>b", "a<br/>b", "a<br />b", "a<BR>b"] {
        assert_eq!(plain(html).as_deref(), Some("a\nb"), "{html}");
    }
}

#[test]
fn source_whitespace_collapses_into_a_space() {
    // 原始碼的換行與縮排是排版，不應變成段落。
    assert_eq!(
        plain("<p>第一行\n        第二行</p>").as_deref(),
        Some("第一行 第二行")
    );
    assert_eq!(plain("a  &nbsp;&nbsp;  b").as_deref(), Some("a b"));
}

#[test]
fn entities_are_decoded() {
    assert_eq!(
        plain("<p>&lt;tag&gt; &amp; &#65; &quot;q&quot;</p>").as_deref(),
        Some("<tag> & A \"q\"")
    );
}

#[test]
fn inline_tags_keep_their_text() {
    assert_eq!(
        plain("<p><b>粗体</b><span>与</span><em>斜体</em></p>").as_deref(),
        Some("粗体与斜体")
    );
}

#[test]
fn list_items_become_lines() {
    assert_eq!(
        plain("<ul><li>第一题</li><li>第二题</li></ul>").as_deref(),
        Some("第一题\n第二题")
    );
}

#[test]
fn tables_keep_their_cells() {
    let text = plain("<table><tr><td>栏一</td><td>栏二</td></tr></table>").expect("有內容");
    assert!(text.contains("栏一"), "{text}");
    assert!(text.contains("栏二"), "{text}");
}

#[test]
fn script_and_style_are_skipped() {
    assert_eq!(
        plain(
            "<p>说明</p><script>var secret = 1;</script><style>.a { color: red; }</style><p>结尾</p>"
        )
        .as_deref(),
        Some("说明\n结尾")
    );
}

#[test]
fn blank_content_returns_none() {
    for html in [
        "",
        "   \n  ",
        "<p></p>",
        "<div><span></span></div>",
        "<br>",
        "<!-- 註解 -->",
    ] {
        assert_eq!(plain(html), None, "{html}");
    }
}

#[test]
fn deeply_nested_markup_is_bounded() {
    // 超過深度上限：放棄更深層的內容，但不 panic、不堆疊溢位。
    let deep = format!("{}目标{}", "<div>".repeat(200), "</div>".repeat(200));
    assert_eq!(plain(&deep), None);

    let shallow = format!("{}目标{}", "<div>".repeat(10), "</div>".repeat(10));
    assert_eq!(plain(&shallow).as_deref(), Some("目标"));
}

#[test]
fn malformed_markup_does_not_panic() {
    assert_eq!(plain("<p>a<div>b").as_deref(), Some("a\nb"));
    assert_eq!(plain("</p>孤立</div>").as_deref(), Some("孤立"));
    assert_eq!(plain("<p><b>未閉合").as_deref(), Some("未閉合"));
}

#[test]
fn long_content_is_truncated_with_an_ellipsis() {
    let long = "字".repeat(MAX_TEXT_CHARS + 10);
    let text = plain(&long).expect("非空內容");
    assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1);
    assert!(text.ends_with('…'));

    let exact = "字".repeat(MAX_TEXT_CHARS);
    let text = plain(&exact).expect("非空內容");
    assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
    assert!(!text.ends_with('…'));
}

/// 整份說明只有一張圖片：沒有可見文字，但必須標記含圖片。
#[test]
fn image_only_body_reports_media_without_text() {
    let content = convert(r#"<p><img src="https://lms.xjtu.edu.cn/a.png" alt="题目"></p>"#);
    assert_eq!(content.text, None, "圖片本身沒有可見文字");
    assert!(content.has_media, "應標記含圖片");
}

#[test]
fn text_beside_media_reports_both() {
    let content = convert(r#"<p>题目如下：</p><p><img src="a.png"></p>"#);
    assert_eq!(content.text.as_deref(), Some("题目如下："));
    assert!(content.has_media, "文字旁的圖片同樣要標記");
}

#[test]
fn media_elements_are_counted() {
    for html in [
        r#"<video src="a.mp4"></video>"#,
        r#"<iframe src="a.html"></iframe>"#,
        "<svg><circle r=\"1\"/></svg>",
        r#"<embed src="a.pdf">"#,
        r#"<object data="a.pdf"></object>"#,
        r#"<audio src="a.mp3"></audio>"#,
    ] {
        let content = convert(html);
        assert!(content.has_media, "{html} 應標記為含多媒體內容");
        assert_eq!(content.text, None, "{html} 沒有可見文字");
    }
}

/// 只是長得像標籤的字串（script 內容）不得算成圖片。
#[test]
fn markup_inside_script_is_not_counted_as_media() {
    let content = convert(r#"<script>document.write('<img src="a.png">')</script>题目"#);
    assert_eq!(content.text.as_deref(), Some("题目"));
    assert!(!content.has_media, "script 內的標籤不是元素");

    let content = convert("<p>图片见附件</p>");
    assert!(!content.has_media, "純文字說明不得標記含圖片");
}
