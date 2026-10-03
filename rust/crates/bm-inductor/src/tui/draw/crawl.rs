//! The crawl view's painter. Reads [`crate::tui::crawl::rows`] — or
//! [`crate::tui::crawl::detail`] when the operator has asked for the whole
//! configuration — and paints it; it decides nothing about what the rows say.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    crawl::{Row, KEY_W},
    style::centered_padded,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

/// Fold a note to `width`, hanging the continuation under the note's own
/// indent. Word wrap on whitespace, and a word longer than the line (a URL, a
/// path) is left whole rather than cut — a cut URL reads as a different URL.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let indent: String = text.chars().take_while(|c| c.is_whitespace()).collect();
    let width = width.max(20);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            format!("{indent}{word}")
        } else {
            format!("{line} {word}")
        };
        if candidate.chars().count() > width && !line.is_empty() {
            out.push(std::mem::take(&mut line));
            line = format!("{indent}{word}");
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub(crate) fn draw_crawl(f: &mut ratatui::Frame, app: &mut App, scroll: usize, expanded: bool) {
    let area = centered_padded(f.area(), 96, 34, 1);
    f.render_widget(Clear, area);
    app.add_hit_region(
        area,
        HitTarget::List {
            kind: ListTarget::Crawl,
            row_start: 0,
            row_y: area.y + 1,
        },
    );

    // The verdict is the default and the configuration is one keypress away.
    // Which one is on screen is named in the title bar, because a screen that
    // hides half of itself without saying so is the same mistake the concise
    // view was made to fix.
    let settings = app.effective_settings();
    let rows = if expanded {
        crate::tui::crawl::detail(&app.layout, &settings)
    } else {
        crate::tui::crawl::rows(&app.layout, &settings)
    };
    let key = app.style(Color::Cyan);
    let fault = app.style_bold(Color::Yellow);
    let dim = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line> = Vec::new();
    for row in &rows {
        match row {
            Row::Blank => lines.push(Line::from("")),
            Row::Section(s) => {
                lines.push(Line::from(Span::styled(
                    s.clone(),
                    app.style_bold(Color::White)
                        .add_modifier(Modifier::UNDERLINED),
                )));
            }
            Row::Field {
                key: k,
                value,
                warn,
            } => {
                let name = Span::styled(format!("  {k:<KEY_W$} "), key);
                let body =
                    Span::styled(value.clone(), if *warn { fault } else { Style::default() });
                lines.push(Line::from(vec![name, body]));
            }
            // Indented continuation of the field above it: a params value, a
            // site's shape, the caveat under a host. Wrapped here rather than
            // in the rows, because the width belongs to the terminal — and a
            // clipped caveat is the same as no caveat.
            Row::Note(n) => {
                for w in wrap(n, area.width.saturating_sub(2) as usize) {
                    lines.push(Line::from(Span::styled(w, dim)));
                }
            }
        }
    }

    let toggle = if expanded {
        "Enter verdict"
    } else {
        "Enter detail"
    };
    let block = super::pane_block(
        app,
        format!("Crawl — {toggle} · Esc or c to close · ↑↓ PgUp PgDn scroll"),
    );
    let inner_h = area.height.saturating_sub(2) as usize;
    let max = lines.len().saturating_sub(inner_h);
    let offset = scroll.min(max) as u16;
    f.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}
