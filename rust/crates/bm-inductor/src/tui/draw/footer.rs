//! Footer: keys, status, roll-up.
use crate::tui::{
    app::App,
    layout::{KEYS_COMPACT, KEYS_FULL},
    model::task_rollup,
    style::{pulse, spinner, Conn},
};
use bm_proto::TaskState;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

pub(crate) fn draw_footer(f: &mut ratatui::Frame, app: &App, area: Rect, compact: bool) {
    let dim = Style::default().fg(Color::DarkGray);
    // Two lines: one was 161 characters and clipped on every terminal, losing
    let keys: Vec<Line> = if compact { KEYS_COMPACT } else { KEYS_FULL }
        .iter()
        .map(|k| Line::from(Span::styled(*k, dim)))
        .collect();

    // The status line always reports, in order: what just happened, how many
    let mut spans = vec![
        Span::styled(
            format!("{} ", app.status.level.glyph()),
            app.style(app.status.level.color()),
        ),
        Span::styled(app.status.text.clone(), app.style(app.status.level.color())),
    ];
    if app.pending > 0 {
        // The spinner steps at the poll cadence, so "jobs are running" is
        spans.push(Span::styled(
            format!(
                "   {} {} job(s) running — Tab jobs",
                spinner(app.tick),
                app.pending
            ),
            app.style(Color::Yellow),
        ));
    }
    // Parked work is invisible in the panes' counts, so it is called out where
    let shelved = app
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Shelved)
        .count();
    if shelved > 0 {
        spans.push(Span::styled(
            format!("   {shelved} shelved — K tasks"),
            app.style_bold(Color::Red),
        ));
    }
    // **Held is the answer to "why is the cluster quiet"**, and a quiet cluster
    if let Some(d) = app.dispatch.as_ref().filter(|d| d.held) {
        spans.push(Span::styled(
            format!("   held · {} — :go", d.span),
            app.style_bold(Color::Yellow),
        ));
    }
    match &app.conn {
        Conn::Up => {
            let ago = app.refreshed.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            // The dot breathes: a *moving* live light proves the poll loop
            spans.push(Span::styled(
                format!("   {} live ({ago}s ago)", pulse(app.tick)),
                app.style(Color::Green),
            ));
        }
        Conn::Down(_) => spans.push(Span::styled("   ● disconnected", app.style(Color::Red))),
        Conn::Unknown => spans.push(Span::styled("   ○ connecting…", app.style(Color::Yellow))),
    }
    // The engine and chapter range moved to the full tier's header strip.
    if compact {
        spans.push(Span::styled(
            format!("   ws: {}", crate::tui::model::workspace_label(&app.layout)),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!(
                "   profile: {}",
                crate::tui::model::profile_label(app.profile.as_ref())
            ),
            if app.profile.is_some() {
                Style::default().fg(Color::DarkGray)
            } else {
                app.style(Color::Yellow)
            },
        ));
    }

    let mut lines = keys;
    lines.push(Line::from(spans));
    if compact {
        // The Tasks pane is gone in this tier; the roll-up takes its place so
        lines.push(task_rollup(&app.counts, app.colour()));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::NONE)),
        area,
    );
}
