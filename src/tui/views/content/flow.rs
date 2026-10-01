//! 考勤流水頁：分頁清單與流水詳情。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;

use crate::sites::attendance::FlowRecord;
use crate::text::fit_display;
use crate::tui::app::App;
use crate::tui::theme::THEME;
use crate::tui::ui::render_list;

use super::columns::{FlowColumns, flow_columns};
use super::{detail_panel, empty, empty_note, row_width, split_detail, title_suffix};

/// 依目前資料繪製考勤流水頁。
pub(super) fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app.attendance.ready().map_or_else(
        || "考勤流水".to_owned(),
        |data| {
            format!(
                "考勤流水 · 第 {}/{} 页（共 {} 条）{}",
                data.page,
                data.total_pages,
                data.total,
                title_suffix(
                    app.updated_at.attendance.as_ref(),
                    app.attendance.is_loading()
                )
            )
        },
    );

    match app.attendance.ready() {
        None => {
            empty(
                frame,
                area,
                &title,
                app.attendance.note(),
                app.attendance.is_loading(),
            );
        }
        Some(data) if data.records.is_empty() => {
            empty_note(frame, area, &title, "本页没有流水记录");
        }
        Some(_) => {
            let (list_area, detail_area) = split_detail(area, app.flow_detail);
            let (items, detail) = {
                let Some(data) = app.attendance.ready() else {
                    return;
                };
                let index = app.page_selection().min(data.records.len() - 1);
                // 清單有邊框與高亮符號：欄寬需以實際列寬計算，狀態欄才能對齊。
                let columns = flow_columns(row_width(list_area));
                let items = data
                    .records
                    .iter()
                    .map(|record| flow_item(record, columns))
                    .collect::<Vec<_>>();
                let detail = app.flow_detail.then(|| flow_lines(&data.records[index]));
                (items, detail)
            };
            render_list(frame, list_area, &title, items, &mut app.flow_state);
            if let (Some(area), Some(lines)) = (detail_area, detail) {
                detail_panel(frame, area, "流水详情", lines);
            }
        }
    }
}

fn flow_item(record: &FlowRecord, columns: FlowColumns) -> ListItem<'static> {
    let time = fit_display(
        record.collect_time.as_deref().unwrap_or("（无时间）"),
        columns.time,
    );
    let place = fit_display(
        record.classroom_name.as_deref().unwrap_or("（无地点）"),
        columns.place,
    );
    ListItem::new(Line::from(vec![
        Span::styled(format!("{time} "), Style::default().fg(THEME.text)),
        Span::styled(format!("{place} "), THEME.muted_style()),
        Span::styled(
            flow_status_label(record),
            THEME.status_style(flow_status_label(record)),
        ),
    ]))
}

fn flow_lines(record: &FlowRecord) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            format!("时间：{}", record.collect_time.as_deref().unwrap_or("未知")),
            THEME.muted_style(),
        )),
        Line::from(Span::styled(
            format!(
                "地点：{}",
                record.classroom_name.as_deref().unwrap_or("未知")
            ),
            THEME.muted_style(),
        )),
        Line::from(vec![
            Span::styled("状态：", THEME.muted_style()),
            Span::styled(
                flow_status_label(record),
                THEME.status_style(flow_status_label(record)),
            ),
        ]),
    ]
}

fn flow_status_label(record: &FlowRecord) -> &'static str {
    if record.effective {
        "有效"
    } else {
        "未匹配"
    }
}
