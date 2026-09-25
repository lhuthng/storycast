//! System overview overlay.
use crate::tui::model::{machine_kind, machine_label, policy_summary, Verdict};
use crate::tui::style::Conn;
use crate::tui::{
    app::App, input::runconfig::run_preview, model::task_rollup, style::centered_padded,
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

/// System overview: backend, config, voices, tasks — everything one launch
/// needs, on one screen. Modelled on the cast overview: read here, act with
/// `Enter` (launch) or `e` (edit the config it shows).
pub(crate) fn draw_run(f: &mut ratatui::Frame, app: &App) {
    let area = centered_padded(f.area(), 76, 30, 1);
    f.render_widget(Clear, area);

    let block = super::pane_block(app, "System — Enter launches · e edits config · Esc closes");
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
            "render",
            format!("{} take(s) per offer (:batch)", cfg.render_batch),
        ),
        kv(
            "mix",
            format!(
                "speed {} · fx {} · music {} · inject {}",
                cfg.speed, cfg.effect_volume, cfg.music_volume, cfg.inject_volume
            ),
        ),
    ];
    match &app.roster {
        None if app.roster_loading => {
            lines.push(kv("voices", "loading roster…".to_string()));
        }
        None => {
            // The Run screen itself has no reload key — say the truth (Esc,
            // then R on the dashboard) rather than a key this screen eats.
            lines.push(kv(
                "voices",
                "roster not loaded — Esc, then R on the dashboard".to_string(),
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
    lines.push(task_rollup(&app.counts, app.colour()));
    // The work split: each box's stage order, so what runs where is visible
    // before Enter launches anything. `M>R>D>C` is most-preferred first, and a
    // lower-case letter is a stage switched off for that box.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  work split (P on the dashboard edits it)",
        app.style_bold(Color::White),
    )));
    if app.machines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no machines registered — a local-only run",
            dim,
        )));
    } else {
        const SHOWN: usize = 6;
        for m in app.machines.iter().take(SHOWN) {
            let chain: Vec<&str> = m
                .effective_task_policy()
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.stage.as_str())
                .collect();
            let detail = if chain.is_empty() {
                "all stages off — P to enable".to_string()
            } else {
                chain.join(" > ")
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "  {:<16}",
                        format!("{} ({})", machine_label(m), machine_kind(m))
                    ),
                    app.style(Color::Cyan),
                ),
                Span::raw(format!("{:<9}", policy_summary(m))),
                Span::styled(detail, dim),
            ]));
        }
        if app.machines.len() > SHOWN {
            lines.push(Line::from(Span::styled(
                format!("  … {} more", app.machines.len() - SHOWN),
                dim,
            )));
        }
    }
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
