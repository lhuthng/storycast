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
    // exactly the keys nobody can guess.
    let keys: Vec<Line> = if compact { KEYS_COMPACT } else { KEYS_FULL }
        .iter()
        .map(|k| Line::from(Span::styled(*k, dim)))
        .collect();

    // The status line always reports, in order: what just happened, how many
    // jobs are still running, whether the inductor is reachable.
    let mut spans = vec![
        Span::styled(
            format!("{} ", app.status.level.glyph()),
            app.style(app.status.level.color()),
        ),
        Span::styled(app.status.text.clone(), app.style(app.status.level.color())),
    ];
    if app.pending > 0 {
        // The spinner steps at the poll cadence, so "jobs are running" is
        // visible from across the room even when the job is quiet.
        spans.push(Span::styled(
            format!("   {} {} job(s) running", spinner(app.tick), app.pending),
            app.style(Color::Yellow),
        ));
    }
    // Parked work is invisible in the panes' counts, so it is called out where
    // the eye already is — with the key that opens the list that can free it.
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
    match &app.conn {
        Conn::Up => {
            let ago = app.refreshed.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            // The dot breathes: a *moving* live light proves the poll loop
            // is alive, which a static ● could not (a frozen poll once read
            // as connected for minutes).
            spans.push(Span::styled(
                format!("   {} live ({ago}s ago)", pulse(app.tick)),
                app.style(Color::Green),
            ));
        }
        Conn::Down(_) => spans.push(Span::styled("   ● disconnected", app.style(Color::Red))),
        Conn::Unknown => spans.push(Span::styled("   ○ connecting…", app.style(Color::Yellow))),
    }
    // The engine and chapter range moved to the full tier's header strip.
    // Workspace and profile stay here in the compact tier: there is no header
    // row at 76×20, and a default nobody looked at is exactly how work lands
    // in the wrong book.
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
        // the counts are never simply missing.
        lines.push(task_rollup(&app.counts, app.colour()));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::NONE)),
        area,
    );
}
