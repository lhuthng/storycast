//! Cast overview overlay.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    layout::{cols, size_class, Size, CAST_COLS_NARROW, CAST_COLS_WIDE, CAST_OVERLAY_W},
    model::{clamp_scroll, filtered_cast_rows, Verdict},
    screen::CastView,
    style::{
        cell, centered_padded, dash_if_empty, empty_body, selection_bg, style_bold_of, style_of,
    },
};
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Row, Table, Wrap},
};
use std::collections::BTreeMap;

/// The whole cast in one table: speaker, voice, that voice's metadata, and how
/// the assignment stands against the policy and the rest of the cast.
pub(crate) fn draw_cast(f: &mut ratatui::Frame, app: &mut App, view: &CastView) {
    // In the compact tier the overlay takes the whole screen: a 108-wide table
    // centred in a 76-column terminal loses 32 columns to margins it cannot
    // spare.
    let compact = size_class(f.area().width, f.area().height) == Size::Compact;
    let area = if compact {
        f.area()
    } else {
        centered_padded(f.area(), CAST_OVERLAY_W, 30, 2)
    };
    f.render_widget(Clear, area);

    let block = super::pane_block(app, "Cast · vi-VN — Esc to close");
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
            // The held line gets its own row only when there is one, so the table
            // keeps its height on a terminal that has none to spare.
            Constraint::Length(if view.line.is_some() { 3 } else { 2 }), // hints
        ])
        .split(inner);

    let all = app.cast_rows();
    let list = filtered_cast_rows(&all, &view.filter);
    let body = rows_area[2].height.saturating_sub(3) as usize;
    app.add_hit_region(
        rows_area[2],
        HitTarget::List {
            kind: ListTarget::Cast,
            row_start: view.scroll,
            row_y: rows_area[2].y + 2,
        },
    );

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
        .filter(|r| matches!(r.verdict(), Verdict::Unknown))
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
        // The fix travels with the count, in the one line that is about every
        // row at once. It used to be repeated on each unassigned row, which is
        // what a column of identical advice is.
        summary.push(Span::styled(
            format!("  ·  {unassigned} unassigned — :v fills gaps"),
            app.style(Color::Yellow),
        ));
    }
    if flagged > 0 {
        summary.push(Span::styled(
            format!("  ·  {flagged} to fix"),
            app.style_bold(Color::Red),
        ));
    } else if !all.is_empty() {
        summary.push(Span::styled(
            "  ·  all assignments valid",
            app.style(Color::Green),
        ));
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
            // The cursor shows exactly when typing would land: only while
            // the filter is focused (in audition focus `t` would play).
            Span::styled(
                if view.filter_focus { "▌" } else { "" },
                app.style(Color::Cyan),
            ),
        ])),
        rows_area[1],
    );

    // Header + two borders are inside `rows_area[2]`; only what is left can
    // hold rows.
    if all.is_empty() {
        let msg = if app.roster.is_none() {
            if app.roster_loading {
                "loading the roster…".to_string()
            } else {
                "roster not loaded — press R".to_string()
            }
        } else {
            "no speakers known yet — :t (translate) or :v (voices) first".to_string()
        };
        f.render_widget(
            empty_body(vec![msg]).wrap(Wrap { trim: true }),
            rows_area[2],
        );
    } else if list.is_empty() {
        f.render_widget(
            empty_body(vec![format!(
                "no speaker or voice matches “{}” — Backspace widens it, Ctrl-U clears",
                view.filter.trim()
            )])
            .wrap(Wrap { trim: true }),
            rows_area[2],
        );
    } else if body > 0 {
        let colour = app.colour();
        // Pick the column set from the width the table actually gets, not from
        // the terminal: the overlay has its own borders to pay for.
        let table_w = rows_area[2].width.saturating_sub(2);
        let wide = table_w >= cols(&CAST_COLS_WIDE);
        let speaker_w = if wide {
            CAST_COLS_WIDE[0]
        } else {
            CAST_COLS_NARROW[0]
        } as usize;
        let voice_w = if wide {
            CAST_COLS_WIDE[1]
        } else {
            CAST_COLS_NARROW[1]
        } as usize;
        // The count is padded to the column's own width, not the slack the
        // table gives it, so it sits against the voice it counts for.
        let shared_w = if wide {
            CAST_COLS_WIDE[2]
        } else {
            CAST_COLS_NARROW[2]
        } as usize;
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
                let mut cells = vec![cell(format!(
                    "{marker}{:<width$}",
                    bm_core::util::head_chars(&r.character, speaker_w - 2),
                    width = speaker_w - 2
                ))];
                cells.push(Line::from(voice_cells));
                // The last column is a number and nothing else: how many
                // *other* speakers are on this voice. It was a status column
                // once, and a fourth of its width was spent on `unknown` and
                // `vi-VN` — the two values every row had.
                cells.push(Line::from(Span::styled(
                    format!("{:<width$}", r.shared_with.len(), width = shared_w),
                    style_of(
                        colour,
                        match r.verdict() {
                            // A blocked or unknown voice stays red or yellow
                            // in the one place the table still has room for it;
                            // the prose for it was the same three words on
                            // every row that had it.
                            Verdict::Unknown => Color::Red,
                            _ if r.shared() => Color::Yellow,
                            _ => Color::DarkGray,
                        },
                    ),
                )));
                let mut row = Row::new(cells);
                if selected {
                    row = row.style(Style::default().bg(selection_bg()));
                }
                row
            })
            .collect();

        let title = if list.len() > body {
            format!("Cast — showing {} of {}", body.min(list.len()), list.len())
        } else {
            "Cast".to_string()
        };
        // The count is the flexible one, so the table fills whatever the
        // overlay got instead of stranding three short columns in the middle
        // of it.
        let mut header = vec!["speaker", "voice"];
        let mut widths: Vec<Constraint> = vec![
            Constraint::Length(CAST_COLS_NARROW[0]),
            Constraint::Length(CAST_COLS_NARROW[1]),
        ];
        if wide {
            widths[0] = Constraint::Length(CAST_COLS_WIDE[0]);
            widths[1] = Constraint::Length(CAST_COLS_WIDE[1]);
        }
        header.push("shared");
        widths.push(Constraint::Min(if wide {
            CAST_COLS_WIDE[2]
        } else {
            CAST_COLS_NARROW[2]
        }));

        let table = Table::new(rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::DarkGray))
                    .title(title),
            );
        f.render_widget(table, rows_area[2]);
    }

    let mut hints = if view.filter_focus {
        vec![
            Line::from(Span::styled(
                "typing — t/T filter too · :current :try :another still audition",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "↑↓ move · Esc back to audition keys · R reload roster",
                Style::default().fg(Color::DarkGray),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                "t cached test · T this line · ^T another line — each assigns nothing",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "other letters filter · ^R focuses filter · ↑↓ move · Esc close · R reload roster",
                Style::default().fg(Color::DarkGray),
            )),
        ]
    };
    // A random line is random until you are told which one it is. Say it, or the
    // operator is comparing two voices on a sentence they cannot see.
    if let Some(l) = &view.line {
        hints.push(Line::from(vec![
            Span::styled(
                format!("line for “{}”: ", l.character),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("“{}”", bm_core::util::head_chars(&l.text, 72)),
                app.style(Color::Cyan),
            ),
        ]));
    }
    f.render_widget(Paragraph::new(hints), rows_area[3]);
}
