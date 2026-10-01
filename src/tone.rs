//! 狀態語意色。
//!
//! 考勤與作業狀態先歸類為語意色（tone），再由介面
//!（[`crate::tui::theme::Theme::status_style`]）映射成實際顏色。語意與顯示
//! 文字分離：調整標籤文字不影響顏色，新增狀態時必須明確指定語意色，不會
//! 靜默落入兜底分支。
//!
//! 放在 crate 根（而非 `tui`）讓 `domain` 與 `sites` 的型別能提供 `tone()`
//! 而不依賴介面模組。

/// 狀態語意色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tone {
    /// 成功：正常、已完成、有效。
    Success,
    /// 警告：遲到、未匹配、待核实、未知。
    Warning,
    /// 錯誤：缺勤、逾期。
    Danger,
    /// 提示：請假。
    Info,
    /// 待處理：待考勤、待提交。
    Accent,
    /// 不需要關注：不考勤。
    Muted,
}
