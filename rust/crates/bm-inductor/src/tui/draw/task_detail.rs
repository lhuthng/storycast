//! One task in full.
use crate::tui::{
    app::App,
    model::age_secs,
    screen::TaskDetail,
    style::{centered_padded, empty_body, state_color, worker_name},
};
use bm_proto::TaskState;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

/// One task in full: everything the ledger knows, `detail` first among them.
pub(crate) fn draw_task_detail(f: &mut ratatui::Frame, app: &App, view: &TaskDetail) {
    let area = centered_padded(f.area(), 92, 22, 2);
    f.render_widget(Clear, area);

    let id = format!("{}:{}", view.stage, view.chapter);
    let found = app
        .tasks
        .iter()
        .find(|t| t.stage == view.stage && t.chapter == view.chapter);

    let block = super::pane_block(
        app,
        format!("Task {id} — Esc back · u retry · F force re-run · q dashboard"),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(t) = found else {
        f.render_widget(
            empty_body(vec![
                format!("task {id} is no longer in the ledger"),
                "it may have left the loaded range — Esc goes back".into(),
            ]),
            inner,
        );
        return;
    };

    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("  {k:<12}"), app.style(Color::Cyan)),
            Span::raw(v),
        ])
    };
    let lease = match t.lease_until {
        None => "—".to_string(),
        Some(l) => {
            let now = bm_proto::now_secs();
            if l > now {
                format!("{}s left", l - now)
            } else {
                format!("expired {}s ago", now - l)
            }
        }
    };
    let mut lines = vec![
        kv("chapter", t.chapter.to_string()),
        kv("stage", t.stage.as_str().to_string()),
        Line::from(vec![
            Span::styled("  state       ", app.style(Color::Cyan)),
            Span::styled(
                t.state.as_str().to_string(),
                app.style_bold(state_color(t.state.as_str())),
            ),
        ]),
        kv(
            "attempts",
            format!("{} of 3 before it is shelved", t.attempts),
        ),
        kv("worker", worker_name(t.assigned_to.as_deref())),
        kv("lease", lease),
        kv("affinity", t.affinity.clone().unwrap_or_else(|| "—".into())),
        kv("updated", format!("{}s ago", age_secs(t.updated))),
    ];
    // A batched render, read off the one row that knows: the offer names a
    // head and records the rest on it (`Task::batch`), so this is the only row
    // that can answer "why are ten rows assigned to one box". Showing it on a
    // member would need the same fact stored twice, which is how the two copies
    // come to disagree.
    if !t.batch.is_empty() {
        lines.push(kv(
            "batch",
            format!(
                "one offer, {} takes, settled together — also covers {}",
                t.batch.len() + 1,
                t.batch.join(", ")
            ),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  why (task.detail — what the worker reported)",
        app.style_bold(Color::White),
    )));
    if t.detail.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  — nothing recorded: this task has not run yet",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let colour = match t.state {
            TaskState::Shelved | TaskState::Failed => Color::Red,
            _ => Color::Gray,
        };
        for l in t.detail.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {l}"),
                app.style(colour),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        match t.state {
            TaskState::Shelved => {
                "  u re-queues it with the strikes forgiven; F also clears its partial output"
            }
            TaskState::Done => "  F runs it again from scratch, clearing what made it look done",
            _ => "  u re-queues it now; F also clears its partial output",
        },
        app.style(Color::Yellow),
    )));

    // Wrapped, and scrollable: a worker's reason can be a stack trace, and a
    // detail clipped at the pane edge is the bug this screen exists to fix.
    f.render_widget(
        Paragraph::new(lines)
            .scroll((view.scroll as u16, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}
