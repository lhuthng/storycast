//! The jobs overlay: one entry per background job, its activity wrapped in full.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    style::{centered_padded, spinner},
};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

/// Column widths: `id`, `job`, `state`, `time`, then the activity takes the rest.
const ID: usize = 6;
const NAME: usize = 22;
const STATE: usize = 8;
const TIME: usize = 8;
/// Two spaces between the time column and the activity.
const HEAD: usize = ID + NAME + STATE + TIME + 2;

fn secs_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// `s` padded or cut to exactly `w` cells, so the columns never shift.
fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        format!("{s:<w$}")
    } else {
        format!("{}…", bm_core::util::head_chars(s, w.saturating_sub(1)))
    }
}

/// Word wrap to `w` cells; a word longer than the row is hard-split.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let gap = usize::from(!line.is_empty());
        if !line.is_empty() && line.chars().count() + gap + word.chars().count() > w {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        while line.chars().count() > w {
            out.push(line.chars().take(w).collect());
            line = line.chars().skip(w).collect();
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

pub(crate) fn draw_jobs(f: &mut ratatui::Frame, app: &mut App, scroll: usize) {
    let area = centered_padded(f.area(), 100, 24, 1);
    f.render_widget(Clear, area);
    app.add_hit_region(
        area,
        HitTarget::List {
            kind: ListTarget::Jobs,
            row_start: scroll,
            row_y: area.y + 2,
        },
    );

    let bold = app.style_bold(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

    // Newest first, running before queued: the row doing work is the one
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
    let hint_area = Rect {
        y: area.y + area.height - 2,
        height: 1,
        x: area.x + 1,
        width: area.width.saturating_sub(2),
    };

    // The activity column: whatever the fixed columns leave of the inner width.
    let inner = area.width.saturating_sub(2) as usize;
    let text_w = inner.saturating_sub(HEAD).max(12);

    let mut lines: Vec<Line> = Vec::new();
    if !jobs.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "{}{}{}{}  {}",
                fit("id", ID),
                fit("job", NAME),
                fit("state", STATE),
                fit("time", TIME),
                "activity"
            ),
            app.style_bold(Color::Gray),
        )));
    }
    for job in &jobs {
        let (state, colour) = match job.started {
            Some(_) => ("running", Color::Green),
            None => ("queued", Color::Yellow),
        };
        // Only a running job spins: a queued row spinning would claim
        let mark = match job.started {
            Some(_) => format!("{} ", spinner(app.tick)),
            None => "  ".to_string(),
        };
        let anchor = job.started.unwrap_or(job.queued);
        let elapsed = anchor.elapsed().as_secs();
        let chunks = wrap(&job.activity, text_w);
        for (i, chunk) in chunks.into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(format!("#{:<5}", job.id), dim),
                    Span::styled(
                        format!("{mark}{}", fit(&job.name, NAME - 2)),
                        app.style(Color::Cyan),
                    ),
                    Span::styled(fit(state, STATE), app.style(colour)),
                    Span::styled(format!("{:>TIME$}", secs_label(elapsed)), app.style(Color::Gray)),
                    Span::styled(format!("  {chunk}"), dim),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("{}{chunk}", " ".repeat(HEAD)),
                    dim,
                )));
            }
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(title, bold))
        .border_style(app.style(crate::tui::style::theme_accent()));
    if jobs.is_empty() {
        // A bare table with a header and no rows reads as "loading". All
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
        // The hint owns the last inner row; the body stops above it so a long
        // activity never hides behind it.
        let room = area.height.saturating_sub(3) as usize;
        let body: Vec<Line> = lines
            .into_iter()
            .skip(scroll)
            .take(room)
            .collect();
        f.render_widget(Paragraph::new(body).block(block), area);
    }

    // One honest hint line per state of the world: an empty list points at
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
