//! Cloud view: what the EC2 account holds, and what the registry has linked.
//!
//! Deliberately not merged into the Machines pane. A `Machine` is a linked box
//! with an ssh key and a provision path; an `AwsInstance` is an EC2 resource
//! that may be linked to nothing. The mark this view adds — "not in registry" —
//! is the whole point of showing the two side by side.
use crate::tui::{
    app::App,
    model::{clamp_scroll, is_live_state},
    screen::CloudView,
    style::{cell, centered_padded, empty_body, selection_bg, style_bold_of, style_of},
};
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Row, Table, Wrap},
};

/// One instance's address, public first — the same order the registry keys on.
fn address(i: &bm_core::provision::AwsInstance) -> String {
    if !i.public_ip.is_empty() {
        i.public_ip.clone()
    } else if !i.private_ip.is_empty() {
        i.private_ip.clone()
    } else {
        "-".into()
    }
}

pub(crate) fn draw_cloud(f: &mut ratatui::Frame, app: &App, view: &CloudView) {
    // The overlay has its own borders to pay for, so a narrow terminal gets the
    // whole screen rather than a squeezed table with margins it cannot spare.
    let area = centered_padded(f.area(), 108, 30, 2);
    f.render_widget(Clear, area);

    let block = super::pane_block(app, "Cloud · EC2 account — Esc to close");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }

    let rows_area = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // summary
            Constraint::Min(1),    // table
            Constraint::Length(2), // hints
        ])
        .split(inner);

    let live = app.cloud.iter().filter(|i| is_live_state(&i.state)).count();
    let unlinked = app
        .cloud
        .iter()
        .filter(|i| {
            let a = address(i);
            !app.machines.iter().any(|m| m.addr == a)
        })
        .count();

    let mut summary = vec![
        Span::styled(
            format!("{} instance(s)", app.cloud.len()),
            app.style_bold(Color::White),
        ),
        Span::styled(
            format!("  ·  {live} live"),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if unlinked > 0 {
        summary.push(Span::styled(
            format!("  ·  {unlinked} not linked"),
            app.style(Color::Yellow),
        ));
    } else if !app.cloud.is_empty() {
        summary.push(Span::styled("  ·  all linked", app.style(Color::Green)));
    }
    f.render_widget(Paragraph::new(Line::from(summary)), rows_area[0]);

    let body = rows_area[1].height.saturating_sub(3) as usize;
    if let Some(e) = &app.cloud_error {
        f.render_widget(
            empty_body(vec![
                format!("could not read the account: {e}"),
                "check `aws login` / `aws discover`, then r to retry".into(),
            ])
            .wrap(Wrap { trim: true }),
            rows_area[1],
        );
    } else if app.cloud.is_empty() {
        f.render_widget(
            empty_body(vec![
                "no instances carry the marker tag".to_string(),
                "`:up` launches some, or r to re-read the account".into(),
            ])
            .wrap(Wrap { trim: true }),
            rows_area[1],
        );
    } else if body > 0 {
        let colour = app.colour();
        let mut scroll = view.scroll;
        clamp_scroll(view.cursor, &mut scroll, app.cloud.len(), body);
        let rows: Vec<Row> = app
            .cloud
            .iter()
            .enumerate()
            .skip(scroll)
            .take(body)
            .map(|(i, inst)| {
                let selected = i == view.cursor;
                let marker = if selected { "▸ " } else { "  " };
                let addr = address(inst);
                let linked = app.machines.iter().any(|m| m.addr == addr);
                let link = if linked {
                    Span::styled("linked", style_of(colour, Color::Green))
                } else {
                    Span::styled("not linked", style_of(colour, Color::Yellow))
                };
                let cells = vec![
                    cell(format!("{marker}{:<20}", inst.id)),
                    cell(format!("{:<12}", inst.instance_type)),
                    Line::from(Span::styled(
                        format!("{:<9}", inst.state),
                        style_of(colour, crate::tui::style::state_color(&inst.state)),
                    )),
                    cell(format!("{:<15}", addr)),
                    cell(if inst.spot {
                        "spot".into()
                    } else {
                        "od".into()
                    }),
                    Line::from(link),
                ];
                let mut row = Row::new(cells);
                if selected {
                    row = row.style(Style::default().bg(selection_bg()));
                }
                row
            })
            .collect();

        let title = if app.cloud.len() > body {
            format!(
                "Cloud — showing {} of {}",
                body.min(app.cloud.len()),
                app.cloud.len()
            )
        } else {
            "Cloud".to_string()
        };
        let header = vec!["id", "type", "state", "address", "cap", "registry"];
        let widths = vec![
            Constraint::Length(22),
            Constraint::Length(12),
            Constraint::Length(9),
            Constraint::Length(15),
            Constraint::Length(4),
            Constraint::Min(10),
        ];
        let table = Table::new(rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            );
        f.render_widget(table, rows_area[1]);
    }

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "`:up [count]` launch · `:down` terminate · `:prov` provision a linked box",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "↑↓ move · r re-read the account · Esc close",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        rows_area[2],
    );
}
