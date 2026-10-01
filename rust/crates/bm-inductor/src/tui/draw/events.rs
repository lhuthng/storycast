//! Log pane.
use crate::tui::style::Level;
use crate::tui::{
    app::{App, Panel},
    model::reported_alias,
    style::{empty_body, log_head, style_of, wall_hms, worker_alias},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

pub(crate) fn draw_events(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let wrap_w = area.width.saturating_sub(2) as usize;
    let viewport_h = area.height.saturating_sub(2) as usize;
    // Publish the pane's real height so PgUp/PgDn page by exactly what one
    // screenful shows. The keys are the only place that can know this, and
    // they run between frames.
    app.events_rows = viewport_h.max(1);

    // The filter narrows first, so scroll distances and the buffer edge below
    // are in filtered lines, not raw ones.
    let shown: Vec<usize> = app
        .events
        .iter()
        .enumerate()
        .filter(|(_, l)| app.log_filter.matches(l))
        .map(|(i, _)| i)
        .collect();
    let total = shown.len();
    // Scroll is a distance from the filtered tail, so a filter with fewer
    // lines cannot leave it pointing past its own top.
    app.events_scroll = app.events_scroll.min(total);

    let title = {
        let base = format!("Logs [{}]", app.log_filter.label());
        if total == app.events.len() && app.events_scroll == 0 {
            format!("{base} · ←→ filter")
        } else if app.events_scroll >= total {
            // The buffer keeps EVENT_CAP lines and drops the rest; the top of the
            // buffer is a real edge, so the title names it instead of showing a
            // number that looks stuck. Reading is still one `G` (or one run of
            // PgDn) from the newest line.
            format!("{base} — oldest kept line · G for newest · ←→ filter")
        } else {
            format!(
                "Logs [{}] — {} of {} · G for newest · ←→ filter",
                app.log_filter.label(),
                app.events_scroll,
                total,
            )
        }
    };
    let block = super::pane_block_for(app, Some(Panel::Events), title);
    if total == 0 {
        f.render_widget(
            empty_body(vec![if app.events.is_empty() {
                "nothing has happened yet".into()
            } else {
                format!("no {} lines — ←→ steps the filter", app.log_filter.label())
            }])
            .block(block),
            area,
        );
        return;
    }

    let colour = app.colour();
    let lines: Vec<Line> = shown
        .iter()
        .map(|i| {
            let l = &app.events[*i];
            let body_style = match l.level {
                Level::Warn => style_of(colour, Color::Yellow),
                Level::Error => style_of(colour, Color::Red),
                _ => Style::default(),
            };
            let mut spans = vec![
                Span::styled(
                    format!("{} ", wall_hms(l.wall)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{} ", l.level.glyph()),
                    style_of(colour, l.level.color()),
                ),
            ];
            match log_head(&l.text) {
                Some(id) => {
                    // The reported alias when a beat carries one for this
                    // worker id — the same name the Workers pane shows.
                    // Anything else renders VERBATIM, never hashed: an
                    // address head like `192.168.2.2` once hashed to
                    // `[hawk]`, a worker that never existed, and the
                    // operator hunted it across every pane. A box-level
                    // line stays box-level; the Workers pane links it to
                    // its worker by address, not by a minted name.
                    let display = reported_alias(&app.beats, id)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| id.to_string());
                    let tint = worker_alias(&display).1;
                    spans.push(Span::styled(
                        format!("[{display}] "),
                        style_of(colour, tint),
                    ));
                    // Name it once: a `[192.168.2.2] …` line already carries
                    // its head, so repeating it after the display tag reads
                    // as two names for one thing.
                    let prefix = format!("[{id}] ");
                    spans.push(Span::styled(
                        l.text.strip_prefix(&prefix).unwrap_or(&l.text).to_string(),
                        body_style,
                    ));
                }
                None => spans.push(Span::styled(l.text.clone(), body_style)),
            }
            Line::from(spans)
        })
        .collect();

    // Compute the visual row offset so scrolling stays correct even when
    // wrapped lines change width on resize.  events_scroll counts logical
    // lines; we convert to display rows here.
    let total_visual: usize = lines
        .iter()
        .map(|l| {
            let w = l.width();
            if w == 0 {
                1
            } else {
                w.div_ceil(wrap_w)
            }
        })
        .sum();
    let show_from = total.saturating_sub(app.events_scroll);
    let visual_skip: usize = lines
        .iter()
        .take(show_from)
        .map(|l| {
            let w = l.width();
            if w == 0 {
                1
            } else {
                w.div_ceil(wrap_w)
            }
        })
        .sum();
    let scroll_row = visual_skip.min(total_visual.saturating_sub(viewport_h));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll_row as u16, 0)),
        area,
    );
    super::draw_fixed_scrollbar(
        f,
        app,
        area,
        total
            .saturating_sub(app.events_scroll)
            .saturating_sub(viewport_h),
        total,
    );
}
