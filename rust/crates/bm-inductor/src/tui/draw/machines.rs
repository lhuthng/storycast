//! Machines pane.
use crate::tui::style::Conn;
use crate::tui::{
    app::{App, Panel},
    layout::COMPACT_MACHINE_COLS,
    model::{addr_label, clamp_scroll, live_workers, machine_kind, machine_label, policy_summary},
    style::{
        cell, empty_body, seen_label, selection_bg, state_glyph_cell, style_bold_of, style_of,
    },
};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Row, Table},
};

pub(crate) fn draw_machines(f: &mut ratatui::Frame, app: &mut App, area: Rect, compact: bool) {
    let disconnected = matches!(app.conn, Conn::Down(_));
    let colour = app.colour();
    let selected = app.selected;
    let title = if disconnected {
        "Machines — DISCONNECTED"
    } else {
        "Machines"
    };
    let border = if disconnected {
        style_of(colour, Color::Red)
    } else {
        Style::default()
    };
    // The right-hand title is the cluster count, where an operator checks
    // "how many boxes am I actually running" without counting rows.
    let block = super::pane_block_for(app, Some(Panel::Machines), title)
        .border_style(border)
        .title_bottom(Line::from(format!("{} up", app.machines.len())).right_aligned());

    if app.machines.is_empty() {
        let mut body = vec!["no machines in the cluster".to_string()];
        match &app.conn {
            Conn::Down(_) => {
                // The poll verdict is already the log line — the pane only
                // needs the way back up, which works from right here.
                body.push("inductor is down — :B to start it".into());
            }
            _ => body.push("type :add to add one by IP or hostname".into()),
        }
        f.render_widget(empty_body(body).block(block), area);
        return;
    }

    let height = area.height.saturating_sub(3) as usize;
    let len = app.machines.len();
    clamp_scroll(selected, &mut app.machine_scroll, len, height);
    let start = app.machine_scroll;
    let end = (start + height).min(len);

    let rows: Vec<Row> = app.machines[start..end]
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let idx = start + i;
            let cursor = if idx == selected { "▸ " } else { "  " };
            // Name, kind, address: `box-1 · aws · 18.1.2.3` tells an operator
            // whose box this is, where it came from, and how to reach it —
            // the address alone never said any of those. `id` is the addr by
            // construction, so it would only repeat the ip column.
            let mut cells = vec![
                cell(format!("{cursor}{}", machine_label(m))),
                cell(machine_kind(m).to_string()),
                cell(addr_label(m)),
                cell(live_workers(&app.beats, &m.addr, bm_proto::now_secs()).to_string()),
                cell(policy_summary(m)),
                state_glyph_cell(colour, m.state.as_str()),
            ];
            // The tts column is the widest and the least urgent; in the compact
            // tier it is the first thing to go, so the remaining columns keep
            // their full width instead of all clipping together.
            if !compact {
                cells.push(cell(m.tts_url.clone().unwrap_or_else(|| "—".into())));
            }
            cells.push(cell(seen_label(m)));
            let mut row = Row::new(cells);
            if idx == selected {
                // A faint background keeps the state hue legible on the
                // cursor line, which REVERSED inverted.
                row = row.style(Style::default().bg(selection_bg()));
            }
            row
        })
        .collect();

    // The cursor column: cells carry `▸ name`, so the header is indented to
    // match the rows and `machine` no longer sits a column left of its data.
    let mut header = vec![" machine", "kind", "ip", "workers", "policy", "state"];
    let mut widths: Vec<Constraint> = if compact {
        // Taken from the constant the compile-time guard checks.
        COMPACT_MACHINE_COLS[..6]
            .iter()
            .map(|w| Constraint::Length(*w))
            .collect()
    } else {
        // The 100-column floor leaves 98 inside the border, and this set sums
        // to exactly that. The old set summed to 102: at the floor, `seen` and
        // the tail of `state` were pushed off the pane entirely.
        //
        // `policy` gave up two of its eleven columns to `state`, because policy
        // is fixed-width by construction — `policy_summary` is always the four
        // stage letters and three `>` (seven) — while `state` is a word of up to
        // twelve. At thirteen it clipped `initializing` to `initializin` and a
        // two-word state would have hidden its own noun. A column that truncates
        // the verdict it exists to show is worse than a column with slack.
        vec![
            Constraint::Length(14),
            Constraint::Length(6),
            Constraint::Length(17),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(15),
        ]
    };
    if !compact {
        header.push("tts");
        widths.push(Constraint::Length(22));
    }
    header.push("seen");
    widths.push(Constraint::Length(if compact {
        COMPACT_MACHINE_COLS[6]
    } else {
        8
    }));

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
    super::draw_fixed_scrollbar(f, app, area, start, len);
}
