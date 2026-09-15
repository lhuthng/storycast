//! Cast overview overlay.
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, Wrap},
};
use std::collections::BTreeMap;
use crate::tui::{app::App, layout::{CAST_COLS_NARROW, CAST_COLS_WIDE, cols, size_class, Size}, model::{Verdict, clamp_scroll, filtered_cast_rows}, screen::CastView, style::{cell, centered_padded, dash_if_empty, empty_body, gender_label, style_bold_of, style_of}};

/// The whole cast in one table: speaker, voice, that voice's metadata, and how
/// the assignment stands against the policy and the rest of the cast.
pub(crate) fn draw_cast(f: &mut ratatui::Frame, app: &App, view: &CastView) {
    // In the compact tier the overlay takes the whole screen: a 108-wide table
    // centred in a 76-column terminal loses 32 columns to margins it cannot
    // spare.
    let compact = size_class(f.area().width, f.area().height) == Size::Compact;
    let area = if compact {
        f.area()
    } else {
        centered_padded(f.area(), 108, 30, 2)
    };
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.style(Color::Cyan))
        .title("Cast · vi-VN — Esc to close · Enter picks a new voice for the highlighted speaker");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }

    let rows_area = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // summary
            Constraint::Length(1), // filter
            Constraint::Min(1),    // table
            Constraint::Length(2), // hints
        ])
        .split(inner);

    let all = app.cast_rows();
    let list = filtered_cast_rows(&all, &view.filter);

    // Summary: the health of the cast in one line, before any row is read.
    let mut in_use: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &all {
        if !r.voice.is_empty() {
            *in_use.entry(r.voice.as_str()).or_insert(0) += 1;
        }
    }
    let shared_voices = in_use.values().filter(|c| **c > 1).count();
    let unassigned = all.iter().filter(|r| r.unassigned()).count();
    let flagged = all
        .iter()
        .filter(|r| matches!(r.verdict(), Verdict::Blocked | Verdict::Unknown))
        .count();

    let mut summary = vec![
        Span::styled(
            format!("{} speakers", all.len()),
            app.style_bold(Color::White),
        ),
        Span::styled(
            format!("  ·  {} voices in use", in_use.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if shared_voices > 0 {
        summary.push(Span::styled(
            format!("  ·  {shared_voices} shared"),
            app.style(Color::Yellow),
        ));
    }
    if unassigned > 0 {
        summary.push(Span::styled(
            format!("  ·  {unassigned} unassigned"),
            app.style(Color::Yellow),
        ));
    }
    if flagged > 0 {
        summary.push(Span::styled(
            format!("  ·  {flagged} to fix"),
            app.style_bold(Color::Red),
        ));
    } else if !all.is_empty() {
        summary.push(Span::styled("  ·  all assignments valid", app.style(Color::Green)));
    }
    if let Some(r) = &app.roster {
        // Only when there is room: on a narrow terminal the provenance would
        // push the health summary — the reason the screen exists — off the end.
        if !compact {
            summary.push(Span::styled(
                format!("   [{} · engine {}]", r.source, r.engine),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(summary)), rows_area[0]);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(view.filter.clone(), app.style(Color::White)),
            Span::styled("▌", app.style(Color::Cyan)),
        ])),
        rows_area[1],
    );

    // Header + two borders are inside `rows_area[2]`; only what is left can
    // hold rows.
    let body = rows_area[2].height.saturating_sub(3) as usize;
    if all.is_empty() {
        let msg = if app.roster.is_none() {
            if app.roster_loading {
                "loading the roster…".to_string()
            } else {
                "roster not loaded — press R".to_string()
            }
        } else {
            "no speakers known yet — run t (translate) or v (voices) first".to_string()
        };
        f.render_widget(empty_body(vec![msg]).wrap(Wrap { trim: true }), rows_area[2]);
    } else if list.is_empty() {
        f.render_widget(
            empty_body(vec![format!(
                "no speaker or voice matches “{}” — Backspace clears it",
                view.filter.trim()
            )])
            .wrap(Wrap { trim: true }),
            rows_area[2],
        );
    } else if body > 0 {
        let colour = app.colour;
        // Pick the column set from the width the table actually gets, not from
        // the terminal: the overlay has its own borders to pay for.
        let table_w = rows_area[2].width.saturating_sub(2);
        let wide = table_w >= cols(&CAST_COLS_WIDE);
        let speaker_w = if wide { CAST_COLS_WIDE[0] } else { CAST_COLS_NARROW[0] } as usize;
        let voice_w = if wide { CAST_COLS_WIDE[1] } else { CAST_COLS_NARROW[1] } as usize;
        let accent_w = if wide { CAST_COLS_WIDE[3] } else { CAST_COLS_NARROW[2] } as usize;

        let mut scroll = view.scroll;
        clamp_scroll(view.cursor, &mut scroll, list.len(), body);
        let rows: Vec<Row> = list
            .iter()
            .enumerate()
            .skip(scroll)
            .take(body)
            .map(|(i, r)| {
                let selected = i == view.cursor;
                let marker = if selected { "▸ " } else { "  " };
                let mut voice_cells = vec![Span::styled(
                    format!(
                        "{:<width$}",
                        dash_if_empty(&r.voice),
                        width = voice_w.saturating_sub(6)
                    ),
                    if r.unassigned() {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        style_of(colour, Color::Green)
                    },
                )];
                if r.enrolled {
                    voice_cells.push(Span::styled(" clone", style_of(colour, Color::Magenta)));
                }
                let (status, status_colour) = match r.verdict() {
                    Verdict::Unassigned => ("unassigned — v fills gaps".to_string(), Color::DarkGray),
                    Verdict::Blocked => ("accent policy concern".to_string(), Color::Yellow),
                    Verdict::Unknown => ("unknown voice — stale cast?".to_string(), Color::Red),
                    Verdict::Ok if r.shared() => (
                        format!(
                            "shared with {} other{}",
                            r.shared_with.len(),
                            if r.shared_with.len() == 1 { "" } else { "s" }
                        ),
                        Color::Yellow,
                    ),
                    Verdict::Ok => ("ok".to_string(), Color::DarkGray),
                };
                let mut cells = vec![cell(format!(
                    "{marker}{:<width$}",
                    bm_core::util::head_chars(&r.character, speaker_w - 2),
                    width = speaker_w - 2
                ))];
                cells.push(Line::from(voice_cells));
                if wide {
                    cells.push(cell(format!("{:<7}", gender_label(&r.gender))));
                }
                cells.push(cell(format!(
                    "{:<width$}",
                    dash_if_empty(&r.accent),
                    width = accent_w
                )));
                cells.push(Line::from(Span::styled(status, style_of(colour, status_colour))));
                let mut row = Row::new(cells);
                if selected {
                    row = row.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                row
            })
            .collect();

        let title = if list.len() > body {
            format!("Cast — showing {} of {}", body.min(list.len()), list.len())
        } else {
            "Cast".to_string()
        };
        // The status column is the flexible one: it is the only column whose
        // text length varies with the verdict.
        let mut header = vec!["speaker", "voice"];
        let mut widths: Vec<Constraint> = vec![
            Constraint::Length(CAST_COLS_NARROW[0]),
            Constraint::Length(CAST_COLS_NARROW[1]),
        ];
        if wide {
            header.push("gender");
            widths[0] = Constraint::Length(CAST_COLS_WIDE[0]);
            widths[1] = Constraint::Length(CAST_COLS_WIDE[1]);
            widths.push(Constraint::Length(CAST_COLS_WIDE[2]));
        }
        header.push("accent");
        widths.push(Constraint::Length(if wide {
            CAST_COLS_WIDE[3]
        } else {
            CAST_COLS_NARROW[2]
        }));
        header.push("status");
        widths.push(Constraint::Min(if wide {
            CAST_COLS_WIDE[4]
        } else {
            CAST_COLS_NARROW[3]
        }));

        let table = Table::new(rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            );
        f.render_widget(table, rows_area[2]);
    }

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "type to filter · ↑↓ move · Enter choose a new voice · Esc close · R reload roster",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "concern = outside your accent policy · unknown = the roster has never heard of it",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        rows_area[3],
    );
}
