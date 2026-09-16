//! Tasks pane plus the full-screen ledger overlay.
use crate::tui::{
    app::App,
    layout::size_class,
    layout::Size,
    model::{age_secs, clamp_scroll, filtered_tasks, task_state_counts},
    screen::TasksView,
    style::{
        cell, centered_padded, empty_body, stage_color, state_cell, state_color, style_bold_of,
        style_of, why_label, worker_name,
    },
};
use bm_proto::TaskState;
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
};

pub(crate) fn draw_tasks(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Tasks");
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
                "press t to enqueue a chapter range",
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
pub(crate) fn draw_tasks_screen(f: &mut ratatui::Frame, app: &App, view: &TasksView) {
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

    let all = &app.tasks;
    let shown = filtered_tasks(all, &view.filter);
    let shelved = all.iter().filter(|t| t.state == TaskState::Shelved).count();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.style(if shelved > 0 { Color::Red } else { Color::Cyan }))
        .title(if shelved > 0 {
            format!("Tasks — {shelved} shelved · Esc or q to close")
        } else {
            "Tasks — Esc or q to close".to_string()
        });
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 5 {
        return;
    }

    let rows = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // counts
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
    for (state, n) in task_state_counts(all) {
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
    f.render_widget(Paragraph::new(Line::from(summary)), rows[0]);

    // The filter line is always present, like the cast screen's, so an active
    // filter can never be invisible.
    let filter_line = if view.filter.trim().is_empty() {
        Line::from(Span::styled(
            "filter: (type a stage, state or chapter — e.g. shelved, digest, 42)",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::styled("filter: ", app.style(Color::Cyan)),
            Span::raw(view.filter.clone()),
            Span::styled("_", app.style(Color::Cyan)),
        ])
    };
    f.render_widget(Paragraph::new(filter_line), rows[1]);

    if all.is_empty() {
        f.render_widget(
            empty_body(vec![
                "no tasks in the ledger yet".into(),
                "press t to enqueue a chapter range".into(),
            ]),
            rows[2],
        );
    } else if shown.is_empty() {
        f.render_widget(
            empty_body(vec![
                format!("no task matches “{}”", view.filter.trim()),
                "Backspace widens the filter · Ctrl-U clears it".into(),
            ]),
            rows[2],
        );
    } else {
        let height = rows[2].height.saturating_sub(2) as usize;
        let cursor = view.cursor.min(shown.len() - 1);
        let mut scroll = view.scroll;
        clamp_scroll(cursor, &mut scroll, shown.len(), height);
        let (start, end) = (scroll, (scroll + height).min(shown.len()));
        let colour = app.colour;

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
                    state_cell(colour, t.state.as_str()),
                    cell(t.attempts.to_string()),
                    cell(worker_name(t.assigned_to.as_deref())),
                    cell(format!("{}s", age_secs(t.updated))),
                    cell(why_label(&t.detail)),
                ];
                // Rank by urgency: an actionable failure outranks a running
                // task, which outranks finished history.
                let mut row = match t.state {
                    TaskState::Shelved | TaskState::Failed => {
                        Row::new(cells).style(style_bold_of(colour, Color::Red))
                    }
                    TaskState::Assigned | TaskState::Running => {
                        Row::new(cells).style(style_of(colour, Color::Yellow))
                    }
                    TaskState::Done => Row::new(cells).style(Style::default().fg(Color::DarkGray)),
                    TaskState::Pending => Row::new(cells),
                };
                if idx == cursor {
                    row = row.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                row
            })
            .collect();

        let table = Table::new(table_rows, widths)
            .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
            .block(Block::default().borders(Borders::TOP).title(format!(
                "Tasks — {} of {} shown",
                shown.len(),
                all.len()
            )));
        f.render_widget(table, rows[2]);
    }

    // The hint names the keys that exist, in the order an operator needs them.
    let dim = Style::default().fg(Color::DarkGray);
    let hint = if shown.is_empty() {
        vec![
            Line::from(Span::styled("Esc or q closes", dim)),
            Line::from(Span::styled(
                "Backspace widens · Ctrl-U clears the filter",
                dim,
            )),
        ]
    } else {
        let t = &shown[view.cursor.min(shown.len() - 1)];
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{}  {}", t.id(), t.state.as_str()),
                    app.style_bold(state_color(t.state.as_str())),
                ),
                Span::styled("  ·  Enter details  ·  u retry  ·  F force re-run", dim),
            ]),
            Line::from(Span::styled(
                "j/k or ↑/↓ move · PgUp/PgDn page · type to filter · Backspace widens · Esc/q close",
                dim,
            )),
        ]
    };
    f.render_widget(Paragraph::new(hint), rows[3]);
}
