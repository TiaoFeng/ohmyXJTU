//! 主題語意色測試：狀態語意色（Tone）的映射必須穩定且可辨。

use super::*;
use crate::domain::homework::HomeworkState;
use crate::sites::attendance::AttendanceStatus;
use crate::tone::Tone;

#[test]
fn flow_states_have_distinct_colors() {
    let effective = THEME.status_style(Tone::Success).fg;
    let unmatched = THEME.status_style(Tone::Warning).fg;
    assert_eq!(effective, Some(THEME.green), "有效应为成功绿");
    assert_eq!(unmatched, Some(THEME.yellow), "未匹配应为警告黄");
    assert_ne!(effective, unmatched, "两种流水状态不得同色");
}

#[test]
fn tones_map_to_their_semantic_colors() {
    assert_eq!(THEME.status_style(Tone::Success).fg, Some(THEME.green));
    assert_eq!(THEME.status_style(Tone::Warning).fg, Some(THEME.yellow));
    assert_eq!(THEME.status_style(Tone::Danger).fg, Some(THEME.red));
    assert_eq!(THEME.status_style(Tone::Info).fg, Some(THEME.blue));
    assert_eq!(THEME.status_style(Tone::Accent).fg, Some(THEME.accent));
    assert_eq!(THEME.status_style(Tone::Muted).fg, Some(THEME.muted));
}

#[test]
fn attendance_statuses_map_to_semantic_tones() {
    assert_eq!(AttendanceStatus::Normal.tone(), Tone::Success);
    assert_eq!(AttendanceStatus::Late.tone(), Tone::Warning);
    assert_eq!(AttendanceStatus::Absent.tone(), Tone::Danger);
    assert_eq!(AttendanceStatus::Leave.tone(), Tone::Info);
    assert_eq!(AttendanceStatus::Pending.tone(), Tone::Accent);
    assert_eq!(AttendanceStatus::NotRequired.tone(), Tone::Muted);
    assert_eq!(AttendanceStatus::Unknown.tone(), Tone::Warning);
}

#[test]
fn homework_states_map_to_semantic_tones() {
    assert_eq!(HomeworkState::Pending.tone(), Tone::Accent);
    assert_eq!(HomeworkState::Overdue.tone(), Tone::Danger);
    assert_eq!(HomeworkState::Completed.tone(), Tone::Success);
    assert_eq!(HomeworkState::Unknown.tone(), Tone::Warning);
}
