//! 底部提示列的寬度適配測試。
//!
//! 提示列是所有頁面共用的固定欄位：若只按固定順序串接，終端稍窄時尾端的提示
//! （尤其是「詳情可以往下讀」）就會被裁掉，使用者只看得到通用按鍵。這裡鎖住
//! 「依重要度取捨、不切成半句、放不下要標示」三個契約。

use super::*;

/// 固定的測試片段：登入狀態｜當下可捲動｜頁面操作｜通用按鍵。
fn segments() -> Vec<String> {
    [
        "[未登录 自动]",
        SCROLL_HINT,
        "[ ] 分组",
        "q 退出",
        "enter 收起详情",
        "^P 账户设置",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[test]
fn keeps_every_segment_when_wide_enough() {
    let expected = "[未登录 自动]  PgUp/PgDn 滚动  [ ] 分组  q 退出  enter 收起详情  ^P 账户设置";
    assert_eq!(fit_hints(&segments(), 200), expected);
    assert_eq!(
        fit_hints(
            &segments(),
            u16::try_from(display_width(expected)).expect("宽度")
        ),
        expected,
        "恰好放得下时不应省略任何片段"
    );
}

#[test]
fn drops_whole_trailing_segments_instead_of_cutting_them() {
    let segments = segments();
    let full = fit_hints(&segments, 400);
    assert_eq!(full, segments.join(HINT_SEPARATOR), "够宽时应完整列出");

    // 少一欄：最後一段（連同分隔與省略號）放不下，必須整段消失而不是被截半。
    let narrower = display_width(&full) - 1;
    let text = fit_hints(&segments, u16::try_from(narrower).expect("宽度"));
    assert!(
        !text.contains(segments.last().expect("有片段")),
        "超出的片段应整段舍弃：{text}"
    );
    assert!(text.ends_with('…'), "有片段被舍弃时应标注：{text}");
    assert!(display_width(&text) <= narrower, "不得超出可用宽度：{text}");

    // 留下的必須是完整片段的前綴（片段本身不含分隔用的雙空白）。
    let body = text.strip_suffix(HINT_ELLIPSIS).expect("应有省略号");
    let shown = body.split(HINT_SEPARATOR).count();
    assert_eq!(
        body,
        segments[..shown].join(HINT_SEPARATOR),
        "不得留下被切半的片段：{text}"
    );
}

#[test]
fn never_exceeds_the_available_width() {
    for width in 20..=200_u16 {
        let text = fit_hints(&segments(), width);
        assert!(
            display_width(&text) <= usize::from(width),
            "width={width} 的提示超出可用宽度：{text:?}"
        );
    }
}

#[test]
fn always_keeps_the_leading_segments() {
    // 窄畫面只保留最前面的片段（登入狀態、可捲動提示），而不是砍掉它們。
    for width in 20..=200_u16 {
        let text = fit_hints(&segments(), width);
        assert!(text.starts_with("[未登录 自动]"), "width={width}: {text}");
    }
}

#[test]
fn marks_the_omission_only_when_something_was_dropped() {
    assert!(!fit_hints(&segments(), 200).ends_with('…'));
    assert!(fit_hints(&segments(), 40).ends_with('…'));
    // 單一片段放得下時不補省略號。
    assert_eq!(
        fit_hints(&["[未登录 自动]".to_owned()], 40),
        "[未登录 自动]"
    );
}

#[test]
fn falls_back_to_the_first_segment_when_nothing_fits() {
    // 正常情況下 `ui::too_small` 會先擋住，這裡只保證不 panic、不輸出空字串。
    assert_eq!(fit_hints(&segments(), 3), "[未登录 自动]");
    assert_eq!(fit_hints(&[], 40), "");
}
