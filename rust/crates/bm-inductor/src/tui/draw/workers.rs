//! Workers pane.
use ratatui::{
    layout::{Constraint, Rect},
    style::Color,
    text::{Line, Span},
    widgets::{Block, Borders, Row, Table},
};
use crate::tui::{app::App, layout::COMPACT_WORKER_COLS, model::{live_beats, reported_alias}, style::{bar, cell, empty_body, stage_color, style_bold_of, style_of, worker_alias}};

pub(crate) fn draw_workers(f: &mut ratatui::Frame, app: &App, area: Rect, compact: bool) {
    let block = Block::default().borders(Borders::ALL).title("Workers");
    // Stale beats stay in state for the reaper's accounting but leave the
    // pane: a dead worker drawn as an idle row is indistinguishable from a
    // live one, which is exactly the "two hares" confusion.
    let live = live_beats(&app.beats, bm_proto::now_secs());
    if live.is_empty() {
        f.render_widget(
            empty_body(vec![
                if app.beats.is_empty() {
                    "no workers connected".into()
                } else {
                    "no live workers — beats older than 90s are hidden".into()
                },
                "start one with:  bm-agent worker --inductor <this host>".into(),
            ])
            .block(block),
            area,
        );
        return;
    }

    let colour = app.colour;
    let rows: Vec<Row> = live
        .iter()
        .map(|b| {
            let st = b.stage.map(|s| s.as_str().to_string()).unwrap_or_else(|| "—".into());
            let ch = b.chapter.map(|c| c.to_string()).unwrap_or_else(|| "—".into());
            let pct = (b.progress.clamp(0.0, 1.0) * 100.0).round() as u32;
            let stage_line = Line::from(Span::styled(
                st.clone(),
                style_of(colour, stage_color(&st)),
            ));
            // The worker's own alias when it reports one (drawn once at
            // startup, kept across restarts); the id hash otherwise, for older
            // agents whose every restart renamed them.
            let name = reported_alias(&app.beats, &b.worker_id)
                .unwrap_or_else(|| worker_alias(&b.worker_id).0)
                .to_string();
            let tint = worker_alias(&name).1;
            let mut cells = vec![Line::from(Span::styled(
                name,
                style_of(colour, tint),
            ))];
            // The machine column is derivable from the Machines pane; the
            // activity string is not, so the machine column goes first.
            if !compact {
                cells.push(cell(if b.hostname.is_empty() {
                    b.addr.clone()
                } else {
                    b.hostname.clone()
                }));
            }
            cells.extend([
                stage_line,
                cell(ch),
                cell(format!("{} {:>3}%", bar(b.progress, 10), pct)),
                cell(if b.activity.is_empty() { "—".into() } else { b.activity.clone() }),
                cell(b.eta_secs.map(bm_core::eta::human).unwrap_or_else(|| "—".into())),
            ]);
            Row::new(cells)
        })
        .collect();

    let mut header = vec!["worker"];
    let mut widths: Vec<Constraint> = vec![Constraint::Length(14)];
    if !compact {
        header.push("machine");
        widths.push(Constraint::Length(14));
    }
    header.extend(["stage", "ch", "progress", "activity", "eta"]);
    if compact {
        // From the constant the compile-time guard checks; the activity column
        // is the one that absorbs any slack on a wider terminal.
        widths.extend(COMPACT_WORKER_COLS[1..].iter().enumerate().map(|(i, w)| {
            if i == 3 {
                Constraint::Min(*w)
            } else {
                Constraint::Length(*w)
            }
        }));
    } else {
        widths.extend([
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(17),
            Constraint::Min(20),
            Constraint::Length(8),
        ]);
    }

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
}
