//! Tasks pane plus the full-screen ledger overlay.
use crate::tui::{
    app::{App, HitTarget, ListTarget, Panel},
    layout::size_class,
    layout::Size,
    model::{abandoned, age_secs, clamp_scroll, filtered_tasks, task_state_counts, Facet},
    screen::TasksView,
    style::{
        cell, centered_padded, empty_body, selection_bg, stage_color, state_color,
        state_glyph_cell, style_bold_of, style_of, why_label, worker_racing,
    },
};
use bm_proto::TaskState;
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
};

pub(crate) fn draw_tasks(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = super::pane_block_for(app, Some(Panel::Workers), "Tasks");
    let mut lines: Vec<Line> = Vec::new();

    match app.counts.as_object() {
        None => {
            lines.push(Line::from(Span::styled(
                "waiting for the inductor…",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(obj) if obj.is_empty() => {
            lines.push(Line::from(Span::styled(
                "no tasks queued",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                ":t (translate) enqueues a chapter range",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(obj) => {
            let mut stages: Vec<&String> = obj.keys().collect();
            stages.sort();
            for st in stages {
                let c = &obj[st.as_str()];
                let get = |k: &str| c.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let done = get("done");
                let shelved = get("shelved");
                let failed = get("failed");
                let total: u64 = c
                    .as_object()
                    .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
                    .unwrap_or(0);
                let open = total.saturating_sub(done).saturating_sub(shelved);
                let mut spans = vec![
                    Span::styled(format!("{st:8}"), app.style_bold(stage_color(st))),
                    Span::raw(format!("{done}/{total} done")),
                    Span::styled(
                        format!("  · {open} open"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                if failed > 0 {
                    spans.push(Span::styled(
                        format!("  · {failed} failed"),
                        app.style(Color::Yellow),
                    ));
                }
                if shelved > 0 {
                    spans.push(Span::styled(
                        format!("  · {shelved} shelved"),
                        app.style(Color::Red),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
    }

    let mut shelved: Vec<String> = app
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Shelved)
        .map(|t| format!("{}:{}", t.stage, t.chapter))
        .collect();
    shelved.sort();
    shelved.dedup();
    if !shelved.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "shelved: {} — press K to open the list, u to retry",
                shelved.join(" ")
            ),
            app.style(Color::Red),
        )));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The Tasks overlay: every task, its state, and the reason it is where it is.
pub(crate) fn draw_tasks_screen(f: &mut ratatui::Frame, app: &mut App, view: &TasksView) {
    // Like the cast overview: full screen on the compact tier, a wide panel
    // otherwise. A ledger table squeezed into 76 columns loses the detail
    // column, which is the one thing this screen exists to show.
    let compact = size_class(f.area().width, f.area().height) == Size::Compact;
    let area = if compact {
        f.area()
    } else {
        centered_padded(f.area(), 116, 28, 2)
    };
    f.render_widget(Clear, area);

    // Clone the compact ledger so hit-region bookkeeping can coexist with the
    // borrowed rows used by the table renderer.
    let all = app.tasks.clone();
    // Who is still answering, read once: the facet filter, the counts and the
    // row tint below all have to agree about a silent box, and three reads
    // could catch a beat landing between them.
    let live = app.live_worker_ids();
    let shown = filtered_tasks(&all, &view.filter, view.facet, &live);
    let shelved = all.iter().filter(|t| t.state == TaskState::Shelved).count();
    let abandoned_rows = all.iter().filter(|t| abandoned(t, &live)).count();
    let block = super::pane_block(
        app,
        if shelved > 0 {
            format!("Tasks — {shelved} shelved · Esc or q to close")
        } else {
            "Tasks — Esc or q to close".to_string()
        },
    )
    .border_style(app.style(if shelved > 0 {
        Color::Red
    } else {
        crate::tui::style::theme_accent()
    }));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 6 {
        return;
    }

    let rows = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // counts
            Constraint::Length(1), // facets
            Constraint::Length(1), // filter
            Constraint::Min(1),    // table
            Constraint::Length(2), // hints
        ])
        .split(inner);

    // Counts first: "is anything wrong" before "which chapter".
    let mut summary = vec![Span::styled(
        format!("{} tasks", all.len()),
        app.style_bold(Color::White),
    )];
    for (state, n) in task_state_counts(&all) {
        summary.push(Span::styled(
            format!("  ·  {n} {}", state.as_str()),
            app.style(state_color(state.as_str())),
        ));
    }
    if view.filter.trim().is_empty() {
        summary.push(Span::styled(
            format!("  ·  {} shown", shown.len()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // The one number no state column can give: a row that is *out* with a box
    // that stopped answering. It is not a state — it is a state plus a fact
    // about somebody else — so it is appended rather than counted as a state,
    // and only when there is one.
    if abandoned_rows > 0 {
        summary.push(Span::styled(
            format!("  ·  {abandoned_rows} abandoned"),
            app.style_bold(Color::Magenta),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(summary)), rows[0]);
    app.add_hit_region(
        rows[3],
        HitTarget::List {
            kind: ListTarget::Tasks,
            row_start: view.scroll,
            row_y: rows[3].y + 2,
        },
    );

    // The facet bar: the chips `←/→` steps through, in the order it steps
    // them *and* drawn in that order, because a bar that showed them
    // differently would lie about where the next keypress goes. The active
    // chip is bracketed rather than merely coloured, so it survives a theme
    // with no colour.
    let dim = Style::default().fg(Color::DarkGray);
    let mut facets: Vec<Span> = vec![Span::styled("←→ ", dim)];
    for f in Facet::ALL {
        if f == view.facet {
            facets.push(Span::styled(
                format!("[{}]", f.label()),
                app.style_bold(Color::White),
            ));
        } else {
            facets.push(Span::styled(f.label().to_string(), dim));
        }
        facets.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(facets)), rows[1]);

    // The filter line is always present, like the cast screen's, so an active
    // filter can never be invisible.
    let filter_line = if view.filter.trim().is_empty() {
        Line::from(Span::styled(
            "filter: (a stage, state or chapter — shelved, digest, 42; combines with the facet)",
            dim,
        ))
    } else {
        Line::from(vec![
            Span::styled("filter: ", app.style(Color::Cyan)),
            Span::raw(view.filter.clone()),
            Span::styled("_", app.style(Color::Cyan)),
        ])
    };
    f.render_widget(Paragraph::new(filter_line), rows[2]);

    if all.is_empty() {
        f.render_widget(
            empty_body(vec![
                "no tasks in the ledger yet".into(),
                ":t (translate) to enqueue a chapter range".into(),
            ]),
            rows[3],
        );
    } else if shown.is_empty() {
        let which = if view.facet == Facet::All {
            String::new()
        } else {
            format!("{} ", view.facet.label())
        };
        let mut why = vec![format!(
            "no {which}task matches “{}”",
            view.filter.trim()
        )];
        // Both halves of the narrowing are named, because either one alone can
        // be the reason the list is empty and the operator is looking at the
        // one they set three keypresses ago.
        if view.facet != Facet::All {
            why.push("←→ steps the facet · Tab clears both the facet and the filter".into());
        } else {
            why.push("Backspace widens the filter · Ctrl-U clears it".into());
        }
        f.render_widget(empty_body(why), rows[3]);
    } else {
        let height = rows[3].height.saturating_sub(2) as usize;
        let cursor = view.cursor.min(shown.len() - 1);
        let mut scroll = view.scroll;
        clamp_scroll(cursor, &mut scroll, shown.len(), height);
        let (start, end) = (scroll, (scroll + height).min(shown.len()));
        let colour = app.colour();

        let (widths, header): (Vec<Constraint>, Vec<&str>) = if compact {
            (
                vec![
                    Constraint::Length(4),  // ch
                    Constraint::Length(6),  // stage
                    Constraint::Length(9),  // state
                    Constraint::Length(3),  // att
                    Constraint::Length(11), // worker
                    Constraint::Length(6),  // age
                    Constraint::Min(8),     // detail
                ],
                vec!["ch", "stage", "state", "att", "worker", "age", "detail"],
            )
        } else {
            (
                vec![
                    Constraint::Length(5),
                    Constraint::Length(8),
                    Constraint::Length(10),
                    Constraint::Length(4),
                    Constraint::Length(14),
                    Constraint::Length(8),
                    Constraint::Min(20),
                ],
                vec![
                    "ch",
                    "stage",
                    "state",
                    "att",
                    "worker",
                    "updated",
                    "detail (why)",
                ],
            )
        };

        let table_rows: Vec<Row> = shown[start..end]
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let idx = start + i;
                let mark = if idx == cursor { "▸" } else { " " };
                let cells = vec![
                    cell(format!("{mark}{}", t.chapter)),
                    cell(t.stage.as_str().to_string()),
                    state_glyph_cell(colour, t.state.as_str()),
                    cell(t.attempts.to_string()),
                    cell(worker_racing(t)),
                    cell(format!("{}s", age_secs(t.updated))),
                    cell(why_label(&t.detail)),
                ];
                // Rank by urgency: an actionable failure outranks a running
                // task, which outranks finished history. A row held by a box
                // that has gone quiet outranks both — it is the one state that
                // will not move on its own, and magenta is the only colour on
                // this table not already spoken for by a severity the ledger
                // itself chose.
                let mut row = if abandoned(t, &live) {
                    Row::new(cells).style(style_bold_of(colour, Color::Magenta))
                } else {
                    match t.state {
                        TaskState::Shelved | TaskState::Failed => {
                            Row::new(cells).style(style_bold_of(colour, Color::Red))
                        }
                        TaskState::Assigned | TaskState::Running => {
                            Row::new(cells).style(style_of(colour, Color::Yellow))
                        }
                        TaskState::Done => {
                            Row::new(cells).style(Style::default().fg(Color::DarkGray))
                        }
                        TaskState::Pending => Row::new(cells),
                    }
                };
                if idx == cursor {
                    // Tint, not REVERSED: reversing wiped the row's severity
                    // style, so the highlighted shelved row stopped reading
                    // red — the one state the operator hunts for.
                    row = row.style(Style::default().bg(selection_bg()));
                }
                row
            })
            .collect();

        let title = if view.facet == Facet::All {
            format!("Tasks — {} of {} shown", shown.len(), all.len())
        } else {
            format!(
                "Tasks — {} · {} of {} shown",
                view.facet.label(),
                shown.len(),
                all.len()
            )
        };
        let table = Table::new(table_rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
            .block(Block::default().borders(Borders::TOP).title(title));
        f.render_widget(table, rows[3]);
    }

    // The hint names the keys that exist, in the order an operator needs them.
    let hint = if shown.is_empty() {
        vec![
            Line::from(Span::styled("Esc or q closes", dim)),
            Line::from(Span::styled(
                "←→ facet · Backspace widens the filter · Tab clears both",
                dim,
            )),
        ]
    } else {
        let t = &shown[view.cursor.min(shown.len() - 1)];
        // Only a row that is *out* with someone has anything to release, and
        // the holder is what `W` names. Suggesting either key on a pending row
        // would be advice that no-ops, which is how a hint line teaches an
        // operator to distrust it.
        let held = if matches!(t.state, TaskState::Assigned | TaskState::Running) {
            t.assigned_to
                .as_deref()
                .or_else(|| t.racers.first().map(String::as_str))
        } else {
            None
        };
        let gone = abandoned(t, &live);
        let release = match held {
            Some(w) => Span::styled(
                format!("x release · W all of {w} · "),
                if gone { app.style(Color::Magenta) } else { dim },
            ),
            None => Span::styled("x/W need a task that is out with a box · ", dim),
        };
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{}  {}", t.id(), t.state.as_str()),
                    app.style_bold(state_color(t.state.as_str())),
                ),
                Span::styled(
                    "  ·  Enter details  ·  u retry  ·  F force re-run  ·  R remerge all  ·  E rerender all",
                    dim,
                ),
            ]),
            // The filter line is one row above and labels itself, so this
            // line spends its width on the keys instead of repeating it — at
            // 114 columns `Esc/q close` is the first thing to fall off the
            // end, and it is the one key on the row nobody can guess.
            Line::from(vec![
                Span::styled("↑/↓ move · PgUp/PgDn page · ←→ facet · ", dim),
                release,
                Span::styled("A requeue the silent · Esc/q close", dim),
            ]),
        ]
    };
    f.render_widget(Paragraph::new(hint), rows[4]);
}
