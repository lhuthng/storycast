//! Paint: tier dispatch plus the overlay match. Panes live in `draw/`.
mod cast;
mod cloud;
mod confirm;
mod crawl;
mod digest;
mod events;
mod footer;
mod graph;
mod help;
mod jobs;
mod llm;
mod machine;
mod machines;
mod picker;
mod policy;
mod prompt;
mod run;
mod script;
mod sound;
mod stats;
mod task_detail;
mod tasks;
mod workers;
mod workspace_list;
mod workspace_new;

use crate::tui::{
    app::{App, HitTarget, Panel},
    layout::{
        size_class, Size, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H, COMPACT_MACHINES_MAX_H,
        COMPACT_MACHINES_MIN_H, COMPACT_TASKS_MAX_H, COMPACT_TASKS_MIN_H, COMPACT_WORKERS_MAX_H,
        COMPACT_WORKERS_MIN_H, FULL_EVENTS_MIN_H, FULL_FOOTER_H, FULL_HEADER_H,
        FULL_MACHINES_MAX_H, FULL_MACHINES_MIN_H, FULL_TASKS_MAX_H, FULL_TASKS_MIN_H,
        FULL_WORKERS_MAX_H, FULL_WORKERS_MIN_H, MIN_H, MIN_W,
    },
    screen::Screen,
    style::centered_padded,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout as RLayout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

/// The stock pane frame: rounded corners and the theme's accent border.
pub(crate) fn pane_block(app: &App, title: impl Into<Line<'static>>) -> Block<'static> {
    pane_block_for(app, None, title)
}

/// A dashboard pane border. The focused pane gets a brighter border so mouse
pub(crate) fn draw_fixed_scrollbar(
    f: &mut ratatui::Frame,
    app: &App,
    area: Rect,
    position: usize,
    content_length: usize,
) {
    let track_h = area.height.saturating_sub(2) as usize;
    let x = area.x.saturating_add(area.width.saturating_sub(1));
    if track_h == 0 || area.width == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new("│"),
        Rect::new(x, area.y + 1, 1, track_h as u16),
    );
    let max = content_length.saturating_sub(1);
    let y = if max == 0 {
        area.y + 1
    } else {
        let offset = position
            .min(max)
            .checked_mul(track_h - 1)
            .and_then(|scaled| scaled.checked_div(max))
            .unwrap_or(0) as u16;
        area.y + 1 + offset
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("╋", app.style(Color::Cyan)))),
        Rect::new(x, y, 1, 1),
    );
}

pub(crate) fn pane_block_for(
    app: &App,
    panel: Option<Panel>,
    title: impl Into<Line<'static>>,
) -> Block<'static> {
    let colour = if panel.is_some_and(|p| p == app.focused_panel) {
        Color::Cyan
    } else {
        crate::tui::style::theme_accent()
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(app.style(colour))
        .title(title.into())
}

/// The size guard: the only thing on screen when the terminal cannot hold the
pub(crate) fn draw_too_small(f: &mut ratatui::Frame, app: &App, area: Rect) {
    // On a sliver there is no room for a bordered box, and a blank screen would
    if area.width < 30 || area.height < 5 {
        f.render_widget(
            Paragraph::new(format!(
                "terminal too small — need {MIN_W}×{MIN_H}, have {}×{}",
                area.width, area.height
            ))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(Span::styled(
            "terminal too small for the dashboard",
            app.style_bold(Color::Yellow),
        )),
        Line::from(""),
        Line::from(format!(
            "need at least {MIN_W}×{MIN_H}, have {}×{}",
            area.width, area.height
        )),
        Line::from(""),
        Line::from(Span::styled(
            "resize the window — the dashboard returns on its own",
            dim,
        )),
        Line::from(Span::styled("q quits", dim)),
    ];
    // Say when a dialog is still open underneath: its keys stay live, so an
    if app.dialog_open() {
        lines.push(Line::from(Span::styled(
            "a dialog is still open — Esc cancels it",
            app.style(Color::Cyan),
        )));
    }
    let box_ = centered_padded(area, 56, lines.len() as u16 + 2, 1);
    f.render_widget(Clear, box_);
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(app.style(Color::Yellow)),
            ),
        box_,
    );
}

/// The one-line identity strip above the panes, full tier only.
fn draw_header(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut left = vec![Span::styled(
        format!("ws: {}", crate::tui::model::workspace_label(&app.layout)),
        app.style_bold(crate::tui::style::theme_accent()),
    )];
    // A missing profile is not decoration: every runner refuses to start
    left.push(Span::styled(
        format!(
            "  profile: {}",
            crate::tui::model::profile_label(app.profile.as_ref())
        ),
        if app.profile.is_some() {
            dim
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
        left.push(Span::styled(format!("  engine: {engine}"), dim));
    }
    if let Some(analyzer) = app
        .settings
        .as_ref()
        .and_then(|s| s.get("analyzer"))
        .and_then(|e| e.as_str())
    {
        left.push(Span::styled(format!("  digest: {analyzer}"), dim));
    }
    if let Some(r) = crate::tui::style::range_label(&app.settings) {
        left.push(Span::styled(format!("  {r}"), dim));
    }
    let right = format!("theme: {} · C cycles", crate::tui::style::theme_label());
    f.render_widget(
        Paragraph::new(Line::from(left)),
        Rect {
            width: area.width.saturating_sub(right.len() as u16 + 2),
            ..area
        },
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(right, dim))).alignment(Alignment::Right),
        area,
    );
}

/// The border, and the chrome inside it, that every pane spends before its
const MACHINES_CHROME: u16 = 3;

const PANE_CHROME: u16 = 2;

/// Column floors for the Tasks/Stats row.
const STATS_MIN_W: u16 = 40;

const TASKS_MIN_W: u16 = 24;

/// The ceiling a pane gets at a given terminal height.
fn soft_ceiling(base: u16, share: u16, frame_h: u16) -> u16 {
    base.max(frame_h * share / 4)
}

/// Rows the Machines pane wants: one per machine, plus its chrome, clamped to
fn machines_height(app: &App, compact: bool, frame_h: u16, frame_w: u16, others: u16) -> u16 {
    // The graph is the one pane that cannot be sized from its content alone: the
    if !app.machines.is_empty() && app.machines_graph {
        let budget = frame_h.saturating_sub(others);
        if let Some(hub_art) = graph::form_for(budget.saturating_sub(PANE_CHROME)) {
            let w = frame_w.saturating_sub(2);
            // The plan's own arithmetic, at the window's first page, so the pane
            let rows = graph::plan(
                budget.saturating_sub(PANE_CHROME),
                w,
                app.machines.len(),
                0,
                hub_art,
            )
            .rows
            .len() as u16;
            return PANE_CHROME + rows;
        }
    }
    let (min, base) = if compact {
        (COMPACT_MACHINES_MIN_H, COMPACT_MACHINES_MAX_H)
    } else {
        (FULL_MACHINES_MIN_H, FULL_MACHINES_MAX_H)
    };
    // The empty state draws two lines of guidance, not a header row.
    let want = if app.machines.is_empty() {
        PANE_CHROME + 2
    } else {
        MACHINES_CHROME + app.machines.len() as u16
    };
    want.clamp(min, soft_ceiling(base, 1, frame_h))
}

/// Rows the Workers pane wants: one per live worker, plus a table header.
fn workers_height(app: &App, compact: bool, frame_h: u16) -> u16 {
    let (min, base) = if compact {
        (COMPACT_WORKERS_MIN_H, COMPACT_WORKERS_MAX_H)
    } else {
        (FULL_WORKERS_MIN_H, FULL_WORKERS_MAX_H)
    };
    let live = app.live_workers().len();
    let want = if live == 0 {
        PANE_CHROME + 2
    } else {
        PANE_CHROME + 1 + live as u16
    };
    // Half the frame's worth of ceiling: workers are the pane that grows with
    want.clamp(min, soft_ceiling(base, 2, frame_h))
}

/// Rows the Tasks/Stats row wants: one line per stage, plus the border.
fn tasks_height(app: &App, compact: bool, frame_h: u16) -> u16 {
    let (min, base) = if compact {
        (COMPACT_TASKS_MIN_H, COMPACT_TASKS_MAX_H)
    } else {
        (FULL_TASKS_MIN_H, FULL_TASKS_MAX_H)
    };
    // The pane always draws the four pipeline stages, so the count is fixed —
    let stages: u16 = if app.counts.as_object().is_none() {
        1
    } else if app.tasks.is_empty() {
        2
    } else {
        4
    };
    let stats = (PANE_CHROME + 1 + app.live_workers().len() as u16).min(6);
    // A quarter of the frame, and never more than the stats table needs.
    (PANE_CHROME + stages)
        .max(stats)
        .clamp(min, soft_ceiling(base, 1, frame_h))
}

pub(crate) fn draw(f: &mut ratatui::Frame, app: &mut App) {
    app.clear_hit_regions();
    let area = f.area();
    let size = size_class(area.width, area.height);
    if size == Size::TooSmall {
        // Nothing else is drawn: a clipped dashboard is worse than none.
        draw_too_small(f, app, area);
        return;
    }
    let compact = size == Size::Compact;

    // Every pane but Logs is sized to its content; **Logs takes the slack**.
    let rack = app.machines_graph;
    let workers_h = if rack {
        0
    } else {
        workers_height(app, compact, area.height)
    };
    let tasks_h = tasks_height(app, compact, area.height);
    let (header_h, events_min, footer_h) = if compact {
        (0, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H)
    } else {
        (FULL_HEADER_H, FULL_EVENTS_MIN_H, FULL_FOOTER_H)
    };
    let machines_h = machines_height(
        app,
        compact,
        area.height,
        area.width,
        header_h + workers_h + tasks_h + events_min + footer_h,
    );
    let constraints: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(machines_h),
            Constraint::Length(workers_h),
            Constraint::Length(tasks_h),
            Constraint::Min(COMPACT_EVENTS_MIN_H),
            Constraint::Length(COMPACT_FOOTER_H),
        ]
    } else {
        vec![
            Constraint::Length(FULL_HEADER_H),
            Constraint::Length(machines_h),
            Constraint::Length(workers_h),
            Constraint::Length(tasks_h),
            // The only flexible row. Everything above is its content's height,
            Constraint::Min(FULL_EVENTS_MIN_H),
            Constraint::Length(FULL_FOOTER_H),
        ]
    };
    let root = RLayout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    // Tasks and Stats sit side by side. Both are `Min`, not `Length`, because a
    let task_row = RLayout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(TASKS_MIN_W), Constraint::Min(STATS_MIN_W)])
        .split(if compact { root[2] } else { root[3] });
    tasks::draw_tasks(f, app, task_row[0]);
    stats::draw_stats(f, app, task_row[1]);

    if compact {
        machines::draw_machines(f, app, root[0], compact);
        if !rack {
            workers::draw_workers(f, app, root[1], true);
        }
        events::draw_events(f, app, root[3]);
        footer::draw_footer(f, app, root[4], true);
        // A rack is a picture, not a list: there is no row index to map a click
        if !app.machines_graph {
            app.add_hit_region(
                root[0],
                HitTarget::Panel {
                    panel: Panel::Machines,
                    row_start: app.machine_scroll,
                    row_y: root[0].y + 2,
                },
            );
        }
        app.add_hit_region(
            root[3],
            HitTarget::Panel {
                panel: Panel::Events,
                row_start: 0,
                row_y: root[3].y + 1,
            },
        );
        app.add_hit_region(
            root[4],
            HitTarget::Panel {
                panel: Panel::Footer,
                row_start: 0,
                row_y: root[4].y,
            },
        );
    } else {
        draw_header(f, app, root[0]);
        machines::draw_machines(f, app, root[1], compact);
        if !rack {
            workers::draw_workers(f, app, root[2], compact);
        }
        events::draw_events(f, app, root[4]);
        footer::draw_footer(f, app, root[5], false);
        if !app.machines_graph {
            app.add_hit_region(
                root[1],
                HitTarget::Panel {
                    panel: Panel::Machines,
                    row_start: app.machine_scroll,
                    row_y: root[1].y + 2,
                },
            );
        }
        app.add_hit_region(
            root[4],
            HitTarget::Panel {
                panel: Panel::Events,
                row_start: 0,
                row_y: root[4].y + 1,
            },
        );
        app.add_hit_region(
            root[5],
            HitTarget::Panel {
                panel: Panel::Footer,
                row_start: 0,
                row_y: root[5].y,
            },
        );
    }

    // Overlays paint last and cover everything beneath them. They are rendered
    match app.screen.clone() {
        Screen::Help { scroll } => help::draw_help(f, app, scroll),
        Screen::Text(p) => prompt::draw_text_prompt(f, app, &p),
        Screen::Pick(p) => picker::draw_picker(f, app, &p),
        Screen::Cast(v) => cast::draw_cast(f, app, &v),
        Screen::Run => run::draw_run(f, app),
        Screen::Confirm(c) => confirm::draw_confirm(f, app, &c),
        Screen::Machine(addr) => machine::draw_machine_info(f, app, &addr),
        Screen::Policy(v) => policy::draw_policy(f, app, &v),
        Screen::Digest(v) => digest::draw_digest(f, app, &v),
        Screen::Script(v) => script::draw_script(f, app, &v),
        Screen::Jobs { scroll, .. } => jobs::draw_jobs(f, app, scroll),
        Screen::Tasks(v) => tasks::draw_tasks_screen(f, app, &v),
        Screen::TaskDetail(d) => task_detail::draw_task_detail(f, app, &d),
        Screen::Sound(v) => sound::draw_sound(f, app, &v),
        Screen::Cloud(v) => cloud::draw_cloud(f, app, &v),
        Screen::Crawl { scroll, expanded } => crawl::draw_crawl(f, app, scroll, expanded),
        Screen::Llm(v) => llm::draw_llm(f, app, &v),
        Screen::WorkspaceNew(ws) => workspace_new::draw_workspace_new(f, app, &ws),
        Screen::WorkspaceList(ws) => workspace_list::draw_workspace_list(f, app, &ws),
        _ => {}
    }
}
