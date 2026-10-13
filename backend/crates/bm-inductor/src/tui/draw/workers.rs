//! Workers pane.
use crate::tui::{
    app::{App, HitTarget, Panel},
    layout::COMPACT_WORKER_COLS,
    model::{beat_backed, live_beats, machine_name, reported_alias, short_activity},
    style::{bar_parts, cell, empty_body, stage_color, style_bold_of, style_of, worker_alias},
};
use ratatui::{
    layout::{Constraint, Rect},
    style::Color,
    text::{Line, Span},
    widgets::{Row, Table},
};

/// `compact` is the tier, not the width: it decides which **columns** exist.
pub(crate) fn draw_workers(f: &mut ratatui::Frame, app: &mut App, area: Rect, compact: bool) {
    // Stale beats stay in state for the reaper's accounting but leave the
    let live = live_beats(&app.beats, bm_proto::now_secs());
    // Ghost rows: the box declared them silent (Offline) after their last
    let live: Vec<bm_proto::Heartbeat> = live
        .into_iter()
        .filter(|b| beat_backed(&app.machines, b))
        .cloned()
        .collect();
    // The count in the corner: how many workers are on, without counting rows.
    let block = super::pane_block_for(
        app,
        Some(Panel::Workers),
        Line::from(format!("Workers · {} live", live.len())),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // The pane is already sized to its content by `draw`, so this fills it
    let table_area = inner;
    let body = usize::from(table_area.height.saturating_sub(1));
    app.worker_scroll = app.worker_scroll.min(live.len().saturating_sub(body));
    app.add_hit_region(
        table_area,
        HitTarget::Panel {
            panel: Panel::Workers,
            row_start: app.worker_scroll.min(live.len()),
            row_y: table_area.y + 1,
        },
    );

    if live.is_empty() {
        f.render_widget(
            empty_body(vec![
                if app.beats.is_empty() {
                    "no workers connected".into()
                } else if app.beats.iter().any(|b| !beat_backed(&app.machines, b)) {
                    "workers silent — their boxes read offline".into()
                } else {
                    "no live workers — beats older than 90s are hidden".into()
                },
                "start one with:  bm-agent worker --inductor <this host>".into(),
            ]),
            table_area,
        );
    } else {
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
                let stage_style = style_of(colour, stage_color(&st));
                let stage_line = Line::from(Span::styled(st.clone(), stage_style));
                // The bar wears the task's own hue, so a cluster mid-render is a
                let (done, track) = bar_parts(b.progress, 12);
                let bar_line = Line::from(vec![
                    Span::styled(done, stage_style),
                    Span::styled(track, style_of(colour, Color::DarkGray)),
                    // The number stays plain: it is the part an operator reads
                    Span::raw(format!(" {pct:>3}%")),
                ]);
                // The worker's own alias when it reports one (drawn once at
                let name = reported_alias(&app.beats, &b.worker_id)
                    .unwrap_or_else(|| worker_alias(&b.worker_id).0)
                    .to_string();
                let tint = worker_alias(&name).1;
                let mut cells = vec![Line::from(Span::styled(name, style_of(colour, tint)))];
                // The registry handle when the box is known (`hawk`, the same
                if !compact {
                    cells.push(cell(machine_name(&app.machines, b).to_string()));
                }
                cells.extend([stage_line, cell(ch), bar_line]);
                // Box load from the heartbeat (`5.2%`, `38% 6.1G`) — a dash
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
                cells.push(cell(short_activity(b)));
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
            header.extend(["box cpu ", "box ram"]);
        }
        header.push("activity");
        if compact {
            // From the constant the compile-time guard checks; the activity column
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
                Constraint::Length(19),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Min(20),
            ]);
        }

        let table = Table::new(rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)));
        f.render_widget(table, table_area);
    }
}
