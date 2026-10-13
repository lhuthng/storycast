//! The digest manager overlay: the chapter list, and one chapter's two rounds.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    // Aliased to the **view's** constant rather than declared here: the arrow
    screen::{DigestView, DIGEST_COLS as COLS},
    style::centered,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

/// The overlay's own width, named so the draw and the column maths cannot drift
const WIDTH: u16 = 76;
/// One grid cell: the number, right-aligned, plus the gap after it.
const CELL: usize = 6;
const _: () = assert!(COLS * CELL <= (WIDTH as usize) - 4);

/// How tall the overlay is, per mode.
fn overlay_height(area: ratatui::layout::Rect, open: bool) -> u16 {
    if open {
        return 18.min(area.height);
    }
    area.height.saturating_sub(2).clamp(8, 34).min(area.height)
}

pub(crate) fn draw_digest(f: &mut ratatui::Frame, app: &mut App, v: &DigestView) {
    let height = overlay_height(f.area(), v.open.is_some());
    let area = centered(f.area(), WIDTH, height);
    f.render_widget(Clear, area);
    if v.open.is_none() {
        app.add_hit_region(
            area,
            HitTarget::List {
                kind: ListTarget::Digest,
                row_start: 0,
                row_y: area.y + 4,
            },
        );
    }
    let dim = Style::default().fg(Color::DarkGray);
    // "Digested" is asked of the layout — the same `Layout::digested` the keys
    let digested = |n: u32| app.layout.digested(n);

    let mut lines: Vec<Line> = Vec::new();
    match &v.open {
        None => draw_list(&mut lines, v, &digested, dim, height),
        Some(ch) => draw_chapter(&mut lines, app, ch, dim),
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(app.style(Color::Cyan))
                    .title(match &v.open {
                        None => " digest manager ",
                        Some(ch) => {
                            if ch.done {
                                " digest manager · done "
                            } else {
                                " digest manager · working "
                            }
                        }
                    }),
            )
            // Wrapping, so a long validator complaint is readable rather than
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_list(
    lines: &mut Vec<Line>,
    v: &DigestView,
    digested: &dyn Fn(u32) -> bool,
    dim: Style,
    height: u16,
) {
    // The overlay's two borders, plus the chrome inside it — two hint lines, two
    let rows_visible = (height as usize).saturating_sub(7).max(1);
    let rows = v.rows(digested);
    // **These must not wrap.** The overlay's height is computed from a *count* of
    lines.push(Line::from(Span::styled(
        "  ←→ chapter · ↑↓ row · Enter open · f filter · Esc close",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "  x stop digest everywhere · s restore each box's policy",
        dim,
    )));
    lines.push(Line::from(""));
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  every chapter here is already digested — `f` to show them",
            dim,
        )));
        return;
    }
    // A grid of numbers, not a table: the operator is looking for a chapter they
    let total_rows = rows.len().div_ceil(COLS);
    let cursor_row = v.cursor / COLS;
    let first_row = cursor_row.saturating_sub(rows_visible.saturating_sub(1));
    let last_row = (first_row + rows_visible).min(total_rows);
    for row in first_row..last_row {
        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        for (col, n) in rows[row * COLS..].iter().take(COLS).enumerate() {
            let i = row * COLS + col;
            let here = i == v.cursor;
            let done = digested(*n);
            let style = if here {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if done {
                // Dimmed rather than hidden: the filter is opt-in, and seeing what
                dim.add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            spans.push(Span::styled(format!("{:>w$}  ", n, w = CELL - 2), style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    let total = v.chapters.len();
    let shown = rows.len();
    let counts = if v.hide_done {
        format!("{shown} of {total} chapters")
    } else {
        format!("{total} chapters")
    };
    // **Say where the selection is.** With a window, the number alone does not tell
    lines.push(Line::from(Span::styled(
        format!(
            "  {counts} · rows {}-{last_row} of {total_rows} · {} selected",
            first_row + 1,
            match v.selected(digested) {
                Some(n) => format!("ch{n}"),
                None => "nothing".to_string(),
            }
        ),
        dim,
    )));
}

fn draw_chapter(
    lines: &mut Vec<Line>,
    app: &App,
    ch: &crate::tui::screen::DigestChapter,
    dim: Style,
) {
    let round = ch.round.as_str();
    lines.push(Line::from(vec![
        Span::styled(
            format!("  ch{}", ch.n),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  ·  round {} of 2 ({round})  ·  c copy  v paste  Esc back",
                if ch.done {
                    2
                } else {
                    1 + (ch.cast.is_some() as u8) as usize
                }
            ),
            dim,
        ),
    ]));
    lines.push(Line::from(""));
    // What just happened. On a failed paste this is the validator's own words,
    let note_style = if ch.done {
        app.style(Color::Green)
    } else if ch.note.contains("failed")
        || ch.note.contains("needs")
        || ch.note.contains("invalid")
        || ch.note.contains("incomplete")
    {
        app.style(Color::Red)
    } else {
        Style::default()
    };
    for line in ch.note.lines() {
        lines.push(Line::from(Span::styled(format!("  {line}"), note_style)));
    }
    lines.push(Line::from(""));
    // The prompt is tens of kilobytes; the point of showing anything is to let
    let first = ch
        .prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    lines.push(Line::from(Span::styled(
        format!(
            "  clipboard: {} bytes, opening \"{}\"",
            ch.prompt.len(),
            first
        ),
        dim,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if ch.done {
            "  reported to the inductor — Esc to pick the next chapter"
        } else {
            "  paste the answer into your model, copy its reply, then press v"
        },
        dim,
    )));
}
