//! The J overlay: one row per background job, live state and elapsed time.
use crate::tui::{app::App, style::centered_padded};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

fn secs_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub(crate) fn draw_jobs(f: &mut ratatui::Frame, app: &App, scroll: usize) {
    let area = centered_padded(f.area(), 76, 16, 1);
    f.render_widget(Clear, area);

    let bold = app.style_bold(Color::White);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();

    if app.background_jobs.is_empty() {
        lines.push(Line::from(Span::styled("  no background jobs", dim)));
    }
    let total = app.background_jobs.len();
    for job in app.background_jobs.iter().skip(scroll.min(total)) {
        let (state, colour) = match job.started {
            Some(_) => ("running", Color::Green),
            None => ("queued", Color::Yellow),
        };
        let _ = colour;
        let anchor = job.started.unwrap_or(job.queued);
        let elapsed = anchor.elapsed().as_secs();
        lines.push(Line::from(vec![
            Span::styled(format!("{:<14}", job.name), app.style(Color::Cyan)),
            Span::styled(format!("{state:<8}"), app.style(colour_state(state))),
            Span::raw(format!("{:>7}  ", secs_label(elapsed))),
            Span::styled(job.activity.clone(), dim),
        ]));
    }
    if total == 0 {
        lines.push(Line::from(Span::styled(
            "  press B to start the backend, t to enqueue chapters",
            dim,
        )));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {total} job(s) — jobs run together unless they need the same thing"),
            dim,
        )));
    }

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" jobs (J) ", bold))
                .border_style(app.style(Color::DarkGray)),
        ),
        area,
    );
}

fn colour_state(state: &'static str) -> ratatui::style::Color {
    match state {
        "queued" => Color::Yellow,
        _ => Color::Green,
    }
}
