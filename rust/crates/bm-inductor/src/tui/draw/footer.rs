//! Footer: keys, status, roll-up.
use crate::tui::{
    app::App,
    layout::{KEYS_COMPACT, KEYS_FULL},
    model::task_rollup,
    style::{range_label, Conn},
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
        spans.push(Span::styled(
            format!("   {} job(s) running", app.pending),
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
            spans.push(Span::styled(
                format!("   ● live ({ago}s ago)"),
                app.style(Color::Green),
            ));
        }
        Conn::Down(_) => spans.push(Span::styled("   ● disconnected", app.style(Color::Red))),
        Conn::Unknown => spans.push(Span::styled("   ● connecting…", app.style(Color::Yellow))),
    }
    if !app.colour {
        spans.push(Span::styled(
            "   [mono]",
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Which book, and which genre — the two things every path below this line
    // is derived from, and the two that can now be switched at runtime. Shown
    // always, like the chapter range: a default nobody looked at is exactly how
    // work lands in the wrong workspace.
    spans.push(Span::styled(
        format!("   ws: {}", crate::tui::model::workspace_label(&app.layout)),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        format!(
            "   profile: {}",
            crate::tui::model::profile_label(app.profile.as_ref())
        ),
        // A missing profile is not decoration: every runner refuses to start
        // without one, so it reads as the problem it is.
        if app.profile.is_some() {
            Style::default().fg(Color::DarkGray)
        } else {
            app.style(Color::Yellow)
        },
    ));
    if let Some(engine) = app
        .settings
        .as_ref()
        .and_then(|s| s.get("engine"))
        .and_then(|e| e.as_str())
    {
        spans.push(Span::styled(
            format!("   engine: {engine}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(analyzer) = app
        .settings
        .as_ref()
        .and_then(|s| s.get("analyzer"))
        .and_then(|e| e.as_str())
    {
        spans.push(Span::styled(
            format!("   digest: {analyzer}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // The chapter range the prompts prefill and `B` reconciles: the thing that
    // decides whether work lands on ch1 or ch21. Shown always, so a default
    // nobody looked at can never surprise again.
    if let Some(r) = range_label(&app.settings) {
        spans.push(Span::styled(
            format!("   {r}"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut lines = keys;
    lines.push(Line::from(spans));
    if compact {
        // The Tasks pane is gone in this tier; the roll-up takes its place so
        // the counts are never simply missing.
        lines.push(task_rollup(&app.counts, app.colour));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::NONE)),
        area,
    );
}
