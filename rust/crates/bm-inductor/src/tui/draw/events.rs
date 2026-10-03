//! Log pane.
use crate::tui::style::Level;
use crate::tui::{
    app::{App, Panel},
    model::{reported_alias, task_event, TaskEvent},
    style::{empty_body, log_head, stage_color, style_bold_of, style_of, wall_hms, worker_alias},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

/// A task line, compact: `[T:merge:23] Complete in 14.6s (retrieved)`. The tag
fn push_task_event(
    spans: &mut Vec<Span<'static>>,
    colour: bool,
    ev: &TaskEvent,
    beats: &[bm_proto::Heartbeat],
    reason_style: Style,
) {
    let dim = style_of(colour, Color::DarkGray);
    let (stage, task) = match ev {
        TaskEvent::Done { stage, task, .. }
        | TaskEvent::Failed { stage, task, .. }
        | TaskEvent::Shelved { stage, task, .. } => (*stage, *task),
    };
    spans.push(Span::styled(
        format!("[T:{task}] "),
        style_bold_of(colour, stage_color(stage)),
    ));
    match ev {
        TaskEvent::Done { secs, worker, .. } => {
            spans.push(Span::styled(
                "Complete ",
                style_bold_of(colour, Color::Green),
            ));
            spans.push(Span::raw("in ".to_string()));
            spans.push(Span::styled(
                (*secs).to_string(),
                style_of(colour, Color::Cyan),
            ));
            let remote = beats
                .iter()
                .any(|b| b.worker_id == *worker && !bm_core::is_local_node(&b.addr));
            if remote {
                spans.push(Span::styled(" (retrieved)".to_string(), dim));
            }
        }
        TaskEvent::Failed { note, reason, .. } => {
            spans.push(Span::styled("Failed", style_bold_of(colour, Color::Red)));
            if let Some(n) = note {
                spans.push(Span::styled(format!(" ({n})"), dim));
            }
            if !reason.is_empty() {
                spans.push(Span::raw(": ".to_string()));
                spans.push(Span::styled((*reason).to_string(), reason_style));
            }
        }
        TaskEvent::Shelved { reason, .. } => {
            spans.push(Span::styled("Shelved", style_bold_of(colour, Color::Red)));
            spans.push(Span::styled(" (press u)".to_string(), dim));
            if !reason.is_empty() {
                spans.push(Span::raw(": ".to_string()));
                spans.push(Span::styled((*reason).to_string(), reason_style));
            }
        }
    }
}

pub(crate) fn draw_events(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let wrap_w = area.width.saturating_sub(2) as usize;
    let viewport_h = area.height.saturating_sub(2) as usize;
    // Publish the pane's real height so PgUp/PgDn page by exactly what one
    app.events_rows = viewport_h.max(1);

    // The filter narrows first, so scroll distances and the buffer edge below
    let shown: Vec<usize> = app
        .events
        .iter()
        .enumerate()
        .filter(|(_, l)| app.log_filter.matches(l))
        .map(|(i, _)| i)
        .collect();
    let total = shown.len();
    // Scroll is a distance from the filtered tail, so a filter with fewer
    app.events_scroll = app.events_scroll.min(total);

    let title = {
        let base = format!("Logs [{}]", app.log_filter.label());
        if total == app.events.len() && app.events_scroll == 0 {
            format!("{base} · ←→ filter")
        } else if app.events_scroll >= total {
            // The buffer keeps EVENT_CAP lines and drops the rest; the top of the
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
            // A task line renders compact (tag, outcome, duration) instead of
            if let Some(ev) = task_event(&l.text) {
                push_task_event(&mut spans, colour, &ev, &app.beats, body_style);
                return Line::from(spans);
            }
            match log_head(&l.text) {
                Some(id) => {
                    // The reported alias when a beat carries one for this
                    let display = reported_alias(&app.beats, id)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| id.to_string());
                    let tint = worker_alias(&display).1;
                    spans.push(Span::styled(
                        format!("[{display}] "),
                        style_of(colour, tint),
                    ));
                    // Name it once: a `[192.168.2.2] …` line already carries
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
