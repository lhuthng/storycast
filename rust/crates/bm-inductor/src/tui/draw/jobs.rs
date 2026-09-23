//! The jobs overlay: one row per background job, live state and elapsed time.
//!
//! Tab (or `J`) opens it; the footer's `N job(s) running` is this list, so the
//! title carries the same split the footer only totals: running against
//! queued. Running sorts first — the queued tail is history, the running row
//! is where an operator's eye lands.
use crate::tui::{app::App, style::centered_padded, style::spinner};
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
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
    let area = centered_padded(f.area(), 76, 18, 1);
    f.render_widget(Clear, area);

    let bold = app.style_bold(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

    // Newest first, running before queued: the row doing work is the one
    // worth finding without scrolling, and a fresh queue lands on top where
    // the eye already is.
    let mut jobs: Vec<&crate::tui::jobs::BackgroundJob> = app.background_jobs.iter().collect();
    jobs.sort_by_key(|j| (j.started.is_none(), std::cmp::Reverse(j.queued)));
    let running = jobs.iter().filter(|j| j.started.is_some()).count();
    let queued = jobs.len() - running;
    let title = match (running, queued) {
        (0, 0) => " jobs — all clear ".to_string(),
        (_, 0) => format!(" jobs — {running} running "),
        (0, _) => format!(" jobs — {queued} queued "),
        (_, _) => format!(" jobs — {running} running · {queued} queued "),
    };

    // The hint lives on the last inner row, where a footer would sit,
    // instead of over the border.
    let hint_area = Rect {
        y: area.y + area.height - 2,
        height: 1,
        x: area.x + 1,
        width: area.width.saturating_sub(2),
    };

    let rows: Vec<Row> = jobs
        .iter()
        .skip(scroll.min(jobs.len()))
        .map(|job| {
            let (state, colour) = match job.started {
                Some(_) => ("running", Color::Green),
                None => ("queued", Color::Yellow),
            };
            // Only a running job spins: a queued row spinning would claim
            // work it is not doing — the exact lie the honest state word
            // exists to prevent.
            let mark = match job.started {
                Some(_) => format!("{} ", spinner(app.tick)),
                None => "  ".to_string(),
            };
            let anchor = job.started.unwrap_or(job.queued);
            let elapsed = anchor.elapsed().as_secs();
            Row::new([
                Line::from(Span::styled(format!("#{:<4}", job.id), dim)),
                Line::from(Span::styled(
                    format!("{mark}{:<12}", job.name),
                    app.style(Color::Cyan),
                )),
                Line::from(Span::styled(format!("{state:<8}"), app.style(colour))),
                Line::from(Span::styled(
                    format!("{:>6}  ", secs_label(elapsed)),
                    app.style(Color::Gray),
                )),
                Line::from(Span::styled(job.activity.clone(), dim)),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(title, bold))
        .border_style(app.style(crate::tui::style::theme_accent()));
    if jobs.is_empty() {
        // A bare table with a header and no rows reads as "loading". All
        // clear says itself, centred, the way every other pane does.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "nothing in flight",
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(ratatui::layout::Alignment::Center)
            .block(block),
            area,
        );
    } else {
        let header =
            Row::new(["id", "job", "state", "time", "activity"]).style(app.style_bold(Color::Gray));
        let table = Table::new(
            rows,
            [
                Constraint::Length(6),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(block);
        f.render_widget(table, area);
    }

    // One honest hint line per state of the world: an empty list points at
    // the two commands that fill it, and a full one names the way out —
    // Tab, the same key that opened this.
    let hint = if jobs.is_empty() {
        "B starts the backend · :t enqueues chapters"
    } else {
        "jobs sharing a resource queue behind it · Tab or Esc closes"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {hint}"), dim))),
        hint_area,
    );
}
