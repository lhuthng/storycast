//! Machines pane.
use crate::tui::style::Conn;
use crate::tui::{
    app::App,
    layout::COMPACT_MACHINE_COLS,
    model::{clamp_scroll, live_workers},
    style::{cell, empty_body, seen_label, state_cell, style_bold_of, style_of},
};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Row, Table},
};

pub(crate) fn draw_machines(f: &mut ratatui::Frame, app: &mut App, area: Rect, compact: bool) {
    let disconnected = matches!(app.conn, Conn::Down(_));
    let colour = app.colour;
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title);

    if app.machines.is_empty() {
        let mut body = vec!["no machines in the cluster".to_string()];
        match &app.conn {
            Conn::Down(e) => {
                body.push(e.clone());
                body.push("press R to run the system (B backend only)".into());
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
            // `id` is the addr by construction, so the column only repeated
            // the neighbour. Live workers per box is the useful number here:
            // the operator's actual question is "is this box doing anything".
            let mut cells = vec![
                cell(format!("{cursor}{}", m.addr)),
                cell(live_workers(&app.beats, &m.addr, bm_proto::now_secs()).to_string()),
                cell(m.role.clone()),
                state_cell(colour, m.state.as_str()),
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
                row = row.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            row
        })
        .collect();

    let mut header = vec!["addr", "workers", "role", "state"];
    let mut widths: Vec<Constraint> = if compact {
        // Taken from the constant the compile-time guard checks.
        COMPACT_MACHINE_COLS[..4]
            .iter()
            .map(|w| Constraint::Length(*w))
            .collect()
    } else {
        vec![
            Constraint::Length(15),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(13),
        ]
    };
    if !compact {
        header.push("tts");
        widths.push(Constraint::Length(22));
    }
    header.push("seen");
    widths.push(Constraint::Length(if compact {
        COMPACT_MACHINE_COLS[4]
    } else {
        8
    }));

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
        .block(block);
    f.render_widget(table, area);
}
