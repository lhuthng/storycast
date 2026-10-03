//! The script inspection overlay: the chapter list, one chapter's segments
use crate::tui::{
    app::App,
    screen::{ScriptPick, ScriptView},
    style::centered,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

/// How many segment rows are visible at once in the chapter view. The
const SEG_VISIBLE: usize = 12;

pub(crate) fn draw_script(f: &mut ratatui::Frame, app: &mut App, v: &ScriptView) {
    let height = f
        .area()
        .height
        .saturating_sub(2)
        .clamp(10, 34)
        .min(f.area().height);
    let area = centered(f.area(), 86, height);
    f.render_widget(Clear, area);

    let title = if v.pick.is_some() {
        " script · picking a speaker "
    } else if v.excerpt_open {
        " script · excerpt "
    } else if v.open.is_some() {
        " script · segments "
    } else {
        " script · digested chapters "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(app.style(Color::Cyan))
        .title(Span::styled(title, app.style(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 {
        return;
    }
    let dim = Style::default().fg(Color::DarkGray);

    // Rows the excerpt's *body* may use. The paragraph below eats one for its top
    let visible = inner.height.saturating_sub(5) as usize;

    let mut lines: Vec<Line> = Vec::new();
    if let Some(pick) = &v.pick {
        draw_pick(&mut lines, app, v, pick, dim);
    } else if v.excerpt_open {
        // The one depth tall enough to scroll, and it windows **itself** the
        draw_excerpt(&mut lines, v, dim, inner.width as usize, visible);
    } else if v.open.is_some() {
        draw_segments(&mut lines, v, dim);
    } else {
        draw_list(&mut lines, v, dim);
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::TOP))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// Depth 2b: the open chapter's excerpt chain. The top half is this chapter's
fn draw_excerpt(lines: &mut Vec<Line>, v: &ScriptView, dim: Style, width: usize, visible: usize) {
    let ch = v.open.unwrap_or(0);

    // The body first, so the window is a slice of it and the header and the
    let mut body: Vec<Line> = Vec::new();
    body.push(Line::from(Span::styled(
        "  ── this chapter (the next chapter is fed this) ──",
        dim,
    )));
    if v.excerpt_own.trim().is_empty() {
        body.push(Line::from(Span::styled(
            "  (none — digested before the field existed, or the model returned nothing)",
            dim,
        )));
    } else {
        push_wrapped(&mut body, &v.excerpt_own, "  ", width);
    }
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "  ── fed to this chapter's attribution ──",
        dim,
    )));
    if v.excerpt_fed.is_empty() {
        body.push(Line::from(Span::styled(
            "  (none — excerpt_window is 0, or no earlier chapter has an excerpt)",
            dim,
        )));
    } else {
        for (m, text) in &v.excerpt_fed {
            body.push(Line::from(Span::styled(format!("  CH {m}:"), dim)));
            push_wrapped(&mut body, text, "    ", width);
        }
    }

    let total = body.len();
    // Clamped so the last page shows the end rather than blank rows. `visible`
    let first = v.excerpt_scroll.min(total.saturating_sub(visible));
    let last = (first + visible).min(total);

    lines.push(Line::from(Span::styled(
        format!("  ch{ch} · excerpt · ↑↓ scroll · e or Esc back to segments"),
        dim,
    )));
    lines.push(Line::from(""));
    lines.extend(body[first..last].iter().cloned());
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  rows {}-{last} of {total} · ↑↓ PgUp PgDn Home scroll",
            first + 1
        ),
        dim,
    )));
}

/// Append `text` wrapped to `width` columns, every line carrying `indent`.
fn push_wrapped(lines: &mut Vec<Line>, text: &str, indent: &str, width: usize) {
    let cols = width.saturating_sub(indent.chars().count()).max(16);
    for l in wrap_text(text, cols) {
        lines.push(Line::from(format!("{indent}{l}")));
    }
}

/// Wrap `s` to at most `width` columns, breaking on whitespace and
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        }
        while line.chars().count() > width {
            let head: String = line.chars().take(width).collect();
            let rest: String = line.chars().skip(width).collect();
            out.push(head);
            line = rest;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// Depth 1: the chapter grid-as-list. A book is numbers, so the rows are
fn draw_list(lines: &mut Vec<Line>, v: &ScriptView, dim: Style) {
    lines.push(Line::from(Span::styled(
        "  type to filter (digits) · ↑↓←→ move · Enter open · Esc close",
        dim,
    )));
    lines.push(Line::from(""));
    let rows = v.rows();
    if rows.is_empty() {
        let why = if v.chapters.is_empty() {
            "no data/script/NN.json here — :translate first"
        } else {
            "no chapter matches — Backspace widens"
        };
        lines.push(Line::from(Span::styled(format!("  {why}"), dim)));
        return;
    }
    // A window derived from the cursor, never remembered: the highlight is
    let per_row = ScriptView::PER_ROW;
    let total_rows = rows.len().div_ceil(per_row);
    let here_row = v.cursor / per_row;
    let first = here_row.saturating_sub(3);
    let last_row = (first + 7).min(total_rows);
    for row in first..last_row {
        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        for (col, n) in rows[row * per_row..].iter().take(per_row).enumerate() {
            let i = row * per_row + col;
            let style = if i == v.cursor {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(format!("{n:>4}  "), style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  {} chapter(s) · rows {}-{last_row} of {total_rows} · {} selected",
            rows.len(),
            first + 1,
            match v.selected() {
                Some(n) => format!("ch{n}"),
                None => "nothing".to_string(),
            }
        ),
        dim,
    )));
}

/// Depth 2: the open chapter's segments. One line each — number, speaker,
/// the opening of the sentence — because the operator is scanning for "the
/// line where the wrong person says that thing", and a speaker column is
fn draw_segments(lines: &mut Vec<Line>, v: &ScriptView, dim: Style) {
    let ch = v.open.unwrap_or(0);
    lines.push(Line::from(Span::styled(
        format!(
            "  ch{ch} · {} segments · ↑↓ move · s re-point the speaker · Esc back",
            v.segments.len()
        ),
        dim,
    )));
    lines.push(Line::from(""));
    if v.segments.is_empty() {
        lines.push(Line::from(Span::styled("  no segments", dim)));
        return;
    }
    let cursor = v.seg_cursor.min(v.segments.len() - 1);
    let first = cursor.saturating_sub(SEG_VISIBLE - 1);
    let end = (first + SEG_VISIBLE).min(v.segments.len());
    for seg in &v.segments[first..end] {
        let style = if seg.n == v.segments[cursor].n {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if seg.speaker.is_empty() {
            // A sound item: no speaker, no take — dimmed, but present, so
            dim
        } else {
            Style::default()
        };
        let speaker = if seg.speaker.is_empty() {
            "·sound·".to_string()
        } else {
            seg.speaker.clone()
        };
        let text: String = seg.text.chars().take(38).collect();
        lines.push(Line::from(vec![
            Span::styled(format!("{:>4} ", seg.n), style),
            Span::styled(format!("{:<14} ", truncate_fold(&speaker, 14)), style),
            Span::styled(text, style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  segments {}-{} of {} · {} under the cursor",
            first + 1,
            end,
            v.segments.len(),
            v.segments
                .get(cursor)
                .map(|s| s.speaker.clone())
                .unwrap_or_else(|| "?".into())
        ),
        dim,
    )));
}

/// Depth 3: the speaker picker. The suggestion list is the chapter's own
fn draw_pick(lines: &mut Vec<Line>, app: &App, v: &ScriptView, pick: &ScriptPick, dim: Style) {
    let suggestions = v.suggestions(app, pick);
    lines.push(Line::from(vec![
        Span::styled(
            format!("  segment {} ", pick.segment),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("now: {:?} — pick who speaks it", pick.expect), dim),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  filter: ", dim),
        Span::styled(pick.filter.clone(), app.style(Color::White)),
        Span::styled("▌", app.style(Color::Cyan)),
    ]));
    lines.push(Line::from(Span::styled(
        "  type to filter · ↑↓ move · PgUp PgDn page · Enter choose · Esc cancel",
        dim,
    )));
    lines.push(Line::from(""));
    if suggestions.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no speaker matches — Backspace widens, Esc cancels",
            dim,
        )));
        return;
    }
    // A derived window of five, the model list's own rule: the cursor
    let show = ScriptPick::SHOW;
    let cursor = pick.cursor.min(suggestions.len() - 1);
    let start = cursor
        .saturating_sub(show - 1)
        .min(suggestions.len().saturating_sub(show));
    // The roster block is the prefix of the suggestion list by
    // construction, so the tag is "is this name in the pick's own roster
    // cache" — the same data `suggestions` ordered the list from, not a
    let fold = bm_core::util::fold;
    let is_roster = |name: &str| pick.roster_cache.iter().any(|n| fold(n) == fold(name));
    for (i, name) in suggestions.iter().enumerate().skip(start).take(show) {
        let style = if i == cursor {
            app.style(Color::Cyan)
        } else {
            Style::default()
        };
        let marker = if i == cursor { "▸" } else { " " };
        let tag = if is_roster(name) {
            "  (this chapter)"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::raw(format!("   {marker} ")),
            Span::styled(name.clone(), style),
            Span::styled(tag.to_string(), dim),
        ]));
    }
    let total = suggestions.len();
    lines.push(Line::from(Span::styled(
        format!(
            "  {total} speaker(s), {}-{} · Enter re-points the segment",
            start + 1,
            (start + show).min(total)
        ),
        dim,
    )));
}

fn truncate_fold(s: &str, w: usize) -> String {
    let mut out: String = s.chars().take(w).collect();
    if out.chars().count() < s.chars().count() {
        out.push('…');
    }
    out
}
