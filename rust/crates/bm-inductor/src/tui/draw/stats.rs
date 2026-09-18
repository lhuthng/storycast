//! Stats pane: per-worker completions by stage, plus the TUI-side ETA.
//!
//! Rows are live workers, columns the four stages; each number is completed
//! tasks of that stage on that worker. The ETA column is measured here, not
//! reported: the stage's median task duration scaled by the beat's unworked
//! fraction. Full tier only — the compact tier already folds Tasks away,
//! and a six-column matrix cannot survive 76 columns.
use crate::tui::{
    app::App,
    model::{live_beats, reported_alias, task_eta},
    style::{cell, empty_body, style_bold_of, style_of, worker_alias},
};
use bm_proto::Stage;
use ratatui::{
    layout::{Constraint, Rect},
    style::Color,
    text::{Line, Span},
    widgets::{Block, Borders, Row, Table},
};

pub(crate) fn draw_stats(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Stats");
    let live = live_beats(&app.beats, bm_proto::now_secs());
    if live.is_empty() {
        f.render_widget(empty_body(vec!["no workers connected".into()]).block(block), area);
        return;
    }
    // Border plus header consume three rows; the rest is workers.
    let rows_cap = area.height.saturating_sub(3) as usize;
    let mut rows: Vec<Row> = Vec::new();
    for b in live.iter().take(rows_cap) {
        let name = reported_alias(&app.beats, &b.worker_id)
            .unwrap_or_else(|| worker_alias(&b.worker_id).0)
            .to_string();
        let tint = worker_alias(&name).1;
        let counts = app.stats.counts.get(&b.worker_id);
        let mut cells = vec![Line::from(Span::styled(name, style_of(app.colour, tint)))];
        for st in Stage::ALL {
            let n = counts.and_then(|c| c.get(st.as_str())).copied().unwrap_or(0);
            cells.push(cell(n.to_string()));
        }
        let eta = match b.stage {
            Some(st) => {
                let avg = app.stats.avg_task_secs.get(st.as_str()).copied();
                task_eta(avg, b.progress)
                    .map(bm_core::eta::human)
                    .unwrap_or_else(|| "—".into())
            }
            None => "—".into(),
        };
        cells.push(cell(eta));
        rows.push(Row::new(cells));
    }
    let header = ["worker", "crawl", "digest", "render", "merge", "eta"];
    let widths = [
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(app.colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
}
