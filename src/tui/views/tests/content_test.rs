//! 考勤流水欄寬計算測試（純函式，不經渲染）。

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
