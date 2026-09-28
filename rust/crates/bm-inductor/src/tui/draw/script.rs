//! The script inspection overlay: the chapter list, one chapter's segments
//! with their speakers, and the speaker picker over them.
//!
//! One box, three depths, the way the digest manager is one box with two
//! modes. The title names the depth so the operator always knows what `Esc`
//! will do — the title *is* the breadcrumb, and a window that only shows
//! "which window am I in" while hiding "how deep am I" would be half a
//! breadcrumb.
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
/// cursor scrolls the window rather than growing the box, so a 90-segment
/// chapter is navigable in a 20-row terminal.
const SEG_VISIBLE: usize = 12;

pub(crate) fn draw_script(f: &mut ratatui::Frame, app: &mut App, v: &ScriptView) {
    let height = f.area().height.saturating_sub(2).clamp(10, 34).min(f.area().height);
    let area = centered(f.area(), 86, height);
    f.render_widget(Clear, area);

    let title = if v.pick.is_some() {
        " script · picking a speaker "
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

    let mut lines: Vec<Line> = Vec::new();
    if let Some(pick) = &v.pick {
        draw_pick(&mut lines, app, v, pick, dim);
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

/// Depth 1: the chapter grid-as-list. A book is numbers, so the rows are
/// dense — several chapters a line, the highlight bracketed, dimmed when
/// the chapter's script is somehow absent (a hand-deleted file).
fn draw_list(lines: &mut Vec<Line>, v: &ScriptView, dim: Style) {
    lines.push(Line::from(Span::styled(
        "  type to filter (digits) · ↑↓ move · Enter open · Esc close",
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
    // visible by construction, the same rule the digest grid uses.
    let per_row = 12usize;
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
/// how that is found.
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
            // the numbers stay the numbers the op takes.
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
/// roster first — the digest's answer to "who is here" — then everyone
/// else alphabetically. The header names the segment and its current
/// speaker, so the confirm-carrying `expect` is visible before Enter.
fn draw_pick(lines: &mut Vec<Line>, app: &App, v: &ScriptView, pick: &ScriptPick, dim: Style) {
    let suggestions = v.suggestions(app, pick);
    lines.push(Line::from(vec![
        Span::styled(
            format!("  segment {} ", pick.segment),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("now: {:?} — pick who speaks it", pick.expect),
            dim,
        ),
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
    // scrolls rather than the box growing.
    let show = ScriptPick::SHOW;
    let cursor = pick.cursor.min(suggestions.len() - 1);
    let start = cursor
        .saturating_sub(show - 1)
        .min(suggestions.len().saturating_sub(show));
    // The roster block is the prefix of the suggestion list by
    // construction, so the tag is "is this name in the pick's own roster
    // cache" — the same data `suggestions` ordered the list from, not a
    // second copy of the rule.
    let fold = bm_core::util::fold;
    let is_roster = |name: &str| pick.roster_cache.iter().any(|n| fold(n) == fold(name));
    for (i, name) in suggestions.iter().enumerate().skip(start).take(show) {
        let style = if i == cursor {
            app.style(Color::Cyan)
        } else {
            Style::default()
        };
        let marker = if i == cursor { "▸" } else { " " };
        let tag = if is_roster(name) { "  (this chapter)" } else { "" };
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
