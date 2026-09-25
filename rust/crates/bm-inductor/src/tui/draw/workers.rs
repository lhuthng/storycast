//! Workers pane.
use crate::tui::{
    app::{App, HitTarget, Panel},
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

/// The `tts` cell: how many sidecars are resident and what they cost.
///
/// A dash, never a zero, when the agent never reported — an older agent has no
/// opinion about sidecars, and `0×` would read as "none running" on the one box
/// that has one.
fn tts_cell(colour: bool, b: &bm_proto::Heartbeat) -> Line<'static> {
    let Some(n) = b.sidecars else {
        return cell("—".into());
    };
    let text = match b.sidecar_gb {
        Some(g) if n > 0 => format!("{n}× {g:.1}G"),
        _ => format!("{n}×"),
    };
    Line::from(Span::styled(
        text,
        style_of(colour, if n > 1 { Color::Red } else { Color::Gray }),
    ))
}

/// `compact` is the tier, not the width: it decides which **columns** exist.
/// The box name, cpu, ram and tts columns only appear on the full tier, because
/// the compact column set is measured against `MIN_W` and there is no room for
/// them there. Height, by contrast, is this pane's content and is settled by
/// the caller.
pub(crate) fn draw_workers(f: &mut ratatui::Frame, app: &mut App, area: Rect, compact: bool) {
    // Stale beats stay in state for the reaper's accounting but leave the
    // pane: a dead worker drawn as an idle row is indistinguishable from a
    // live one, which is exactly the "two hares" confusion.
    let live = live_beats(&app.beats, bm_proto::now_secs());
    // Ghost rows: the box declared them silent (Offline) after their last
    // beat. They stay out of the pane — a worker row on an ✗ machine is
    // exactly the contradiction this filter removes.
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
    // rather than carving Tasks/Stats out of it. They are siblings in the root
    // layout now, which is what lets both of them be visible in the compact
    // tier — they used to be carved out of here, and the carve was skipped
    // entirely below 100x32.
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
                    cell(format!("{} {:>3}%", bar(b.progress, 12), pct)),
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
                        // How many sidecars are resident, and what they cost. The
                        // quantity that actually kills these boxes: one model is
                        // ~2.85 GB, so `2×` on an 8 GiB box is the OOM race — and
                        // one the scheduler stops feeding it (`MEM_PCT_CEILING`).
                        // Red above one, because this column exists to be noticed.
                        tts_cell(colour, b),
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
            // A trailing space on `box cpu`: ratatui places each header cell in a
            // Length column and truncates at its edge with no pad, so the 7-glyph
            // word in a 7-wide column drew as `box cp`. The column is 8, and the
            // space keeps the word clear of the edge even where the two abut.
            header.extend(["box cpu ", "box ram", "tts"]);
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
                Constraint::Length(19),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Min(20),
            ]);
        }

        let table = Table::new(rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)));
        f.render_widget(table, table_area);
    }
}
