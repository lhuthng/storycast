//! Workers pane.
use crate::tui::{
    app::App,
    layout::COMPACT_WORKER_COLS,
    model::{beat_backed, live_beats, machine_name, reported_alias},
    style::{bar, cell, empty_body, stage_color, style_bold_of, style_of, worker_alias},
};
use ratatui::{
    layout::{Constraint, Rect},
    style::Color,
    text::{Line, Span},
    widgets::{Row, Table},
};

pub(crate) fn draw_workers(f: &mut ratatui::Frame, app: &App, area: Rect, compact: bool) {
    // Stale beats stay in state for the reaper's accounting but leave the
    // pane: a dead worker drawn as an idle row is indistinguishable from a
    // live one, which is exactly the "two hares" confusion.
    let live = live_beats(&app.beats, bm_proto::now_secs());
    // Ghost rows: the box declared them silent (Offline) after their last
    // beat. They stay out of the pane — a worker row on an ✗ machine is
    // exactly the contradiction this filter removes.
    let live: Vec<_> = live
        .into_iter()
        .filter(|b| beat_backed(&app.machines, b))
        .collect();
    // The count in the corner: how many workers are on, without counting rows.
    let block = super::pane_block(app, Line::from(format!("Workers · {} live", live.len())));
    if live.is_empty() {
        f.render_widget(
            empty_body(vec![
                if app.beats.is_empty() {
                    "no workers connected".into()
                } else if app
                    .beats
                    .iter()
                    .any(|b| !beat_backed(&app.machines, b))
                {
                    "workers silent — their boxes read offline".into()
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

    let colour = app.colour();
    let rows: Vec<Row> = live
        .iter()
        .map(|b| {
            let st = b
                .stage
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| "—".into());
            let ch = b
                .chapter
                .map(|c| c.to_string())
                .unwrap_or_else(|| "—".into());
            let pct = (b.progress.clamp(0.0, 1.0) * 100.0).round() as u32;
            let stage_line =
                Line::from(Span::styled(st.clone(), style_of(colour, stage_color(&st))));
            // The worker's own alias when it reports one (drawn once at
            // startup, kept across restarts); the id hash otherwise, for older
            // agents whose every restart renamed them.
            let name = reported_alias(&app.beats, &b.worker_id)
                .unwrap_or_else(|| worker_alias(&b.worker_id).0)
                .to_string();
            let tint = worker_alias(&name).1;
            let mut cells = vec![Line::from(Span::styled(name, style_of(colour, tint)))];
            // The registry handle when the box is known (`hawk`, the same
            // word the provision log used) — the reported OS hostname
            // otherwise. Never the raw "localhost" fallback alone next to
            // a known box: one box, one name on every pane.
            if !compact {
                cells.push(cell(machine_name(&app.machines, b).to_string()));
            }
            cells.extend([
                stage_line,
                cell(ch),
                cell(format!("{} {:>3}%", bar(b.progress, 10), pct)),
            ]);
            // Box load from the heartbeat (`5.2%`, `38% 6.1G`) — a dash
            // while the agent never measured (older agents, first beat).
            // Full tier only: the compact tier has no room to spare.
            // The worker-reported ETA column this replaces always showed
            // a dash (agents never send it); ETA now lives in Stats,
            // measured TUI-side from completion history.
            if !compact {
                cells.extend([
                    cell(
                        b.cpu_pct
                            .map(|c| format!("{c:.1}%"))
                            .unwrap_or_else(|| "—".into()),
                    ),
                    cell(match (b.mem_pct, b.mem_gb) {
                        (Some(p), Some(g)) => format!("{p:.0}% {g:.1}G"),
                        _ => "—".into(),
                    }),
                ]);
            }
            cells.push(cell(if b.activity.is_empty() {
                "—".into()
            } else {
                b.activity.clone()
            }));
            Row::new(cells)
        })
        .collect();

    let mut header = vec!["alias"];
    let mut widths: Vec<Constraint> = vec![Constraint::Length(11)];
    if !compact {
        header.push("machine");
        widths.push(Constraint::Length(14));
    }
    header.extend(["stage", "ch", "progress"]);
    if !compact {
        // Qualified: these are whole-box load (the agent reports
        // `global_cpu_usage` / used memory), not the task's share — bare
        // `cpu`/`ram` next to per-task progress read as the render's cost.
        header.extend(["box cpu", "box ram"]);
    }
    header.push("activity");
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
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Min(20),
        ]);
    }

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
}
