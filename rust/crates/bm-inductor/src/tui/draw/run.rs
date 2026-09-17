//! System overview overlay.
use crate::tui::model::Verdict;
use crate::tui::style::Conn;
use crate::tui::{
    app::App, input::runconfig::run_preview, model::task_rollup, style::centered_padded,
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// System overview: backend, config, voices, tasks — everything one launch
/// needs, on one screen. Modelled on the cast overview: read here, act with
/// `Enter` (launch) or `e` (edit the config it shows).
pub(crate) fn draw_run(f: &mut ratatui::Frame, app: &App) {
    let area = centered_padded(f.area(), 76, 26, 1);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.style(Color::Cyan))
        .title("System — Enter launches · e edits config · Esc closes");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 10 {
        return;
    }

    let cfg = run_preview(app);
    let dim = Style::default().fg(Color::DarkGray);
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("  {k:<10}"), app.style(Color::Cyan)),
            Span::raw(v),
        ])
    };
    let live_workers = app
        .beats
        .iter()
        .filter(|b| bm_proto::now_secs().saturating_sub(b.ts) < 90)
        .count();
    let models = if cfg.models.is_empty() {
        "(single model)".to_string()
    } else {
        cfg.models.join(", ")
    };

    let mut lines = vec![
        kv(
            "backend",
            match app.conn {
                Conn::Up => format!("answering at {}", app.api),
                Conn::Down(_) => "DOWN — B starts it".to_string(),
                Conn::Unknown => "connecting…".to_string(),
            },
        ),
        kv("workers", format!("{live_workers} live")),
        kv(
            "range",
            if cfg.count == 0 {
                "no chapters (e to set)".to_string()
            } else {
                format!(
                    "chapters {}–{} {}",
                    cfg.start,
                    cfg.start + cfg.count.saturating_sub(1),
                    if cfg.live {
                        "(live)"
                    } else if cfg.saved {
                        "(saved)"
                    } else {
                        "(defaults — e to set)"
                    },
                )
            },
        ),
        kv("digest", format!("{} ({models})", cfg.analyzer)),
        kv("engine", cfg.engine.clone()),
        kv(
            "mix",
            format!(
                "speed {} · fx {} · music {}",
                cfg.speed, cfg.effect_volume, cfg.music_volume
            ),
        ),
    ];
    match &app.roster {
        None if app.roster_loading => {
            lines.push(kv("voices", "loading roster…".to_string()));
        }
        None => {
            lines.push(kv(
                "voices",
                "roster not loaded — press R to retry".to_string(),
            ));
        }
        Some(r) => {
            let rows = app.cast_rows();
            let unassigned = rows.iter().filter(|x| x.unassigned()).count();
            let flagged = rows
                .iter()
                .filter(|x| matches!(x.verdict(), Verdict::Blocked | Verdict::Unknown))
                .count();
            lines.push(kv(
                "voices",
                format!(
                    "{} voices · {} cast · {unassigned} unassigned · {flagged} to fix",
                    r.voices.len(),
                    r.cast.len()
                ),
            ));
        }
    }
    lines.push(Line::from(""));
    lines.push(task_rollup(&app.counts, app.colour));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter starts the backend if down, then runs the range above",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        "e edits range, analyzer and model chain (saved to settings)",
        dim,
    )));
    lines.push(Line::from(Span::styled(
        ":mix edits speed and fx/music volumes, requeues every merge",
        dim,
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}
