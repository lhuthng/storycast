//! Log pane.
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use crate::tui::{app::App, model::reported_alias, style::{empty_body, log_head, style_of, wall_hms, worker_alias}};
use crate::tui::style::Level;

pub(crate) fn draw_events(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let wrap_w = area.width.saturating_sub(2) as usize;
    let viewport_h = area.height.saturating_sub(2) as usize;
    let total = app.events.len();

    let title = if app.events_scroll > 0 {
        format!("Logs — {} line(s) back · G for newest", app.events_scroll)
    } else {
        "Logs".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    if app.events.is_empty() {
        f.render_widget(
            empty_body(vec!["nothing has happened yet".into()]).block(block),
            area,
        );
        return;
    }

    let colour = app.colour;
    let lines: Vec<Line> = app
        .events
        .iter()
        .map(|l| {
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
                Span::styled(format!("{} ", l.level.glyph()), style_of(colour, l.level.color())),
            ];
            match log_head(&l.text) {
                Some(id) => {
                    // The reported alias when a beat carries one for this id
                    // — the same name the Workers pane shows — else the id
                    // hash. Without the lookup the log and the pane name the
                    // same worker differently.
                    let display = reported_alias(&app.beats, id)
                        .unwrap_or_else(|| worker_alias(id).0)
                        .to_string();
                    let tint = worker_alias(&display).1;
                    spans.push(Span::styled(format!("[{display}] "), style_of(colour, tint)));
                    spans.push(Span::styled(l.text.clone(), body_style));
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
            if w == 0 { 1 } else { w.div_ceil(wrap_w) }
        })
        .sum();
    let show_from = total.saturating_sub(app.events_scroll);
    let visual_skip: usize = lines
        .iter()
        .take(show_from)
        .map(|l| {
            let w = l.width();
            if w == 0 { 1 } else { w.div_ceil(wrap_w) }
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
}
