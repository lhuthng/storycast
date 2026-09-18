//! Paint: tier dispatch plus the overlay match. Panes live in `draw/`.
mod cast;
mod confirm;
mod events;
mod footer;
mod help;
mod jobs;
mod machine;
mod machines;
mod picker;
mod prompt;
mod run;
mod sound;
mod stats;
mod task_detail;
mod tasks;
mod workers;

use crate::tui::{
    app::App,
    layout::{
        size_class, Size, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H, COMPACT_MACHINES_H,
        COMPACT_WORKERS_H, FULL_EVENTS_MIN_H, FULL_FOOTER_H, FULL_MACHINES_H, FULL_TASKS_H,
        FULL_WORKERS_H, MIN_H, MIN_W,
    },
    screen::Screen,
    style::centered_padded,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout as RLayout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// The size guard: the only thing on screen when the terminal cannot hold the
/// dashboard. It names the requirement, the current size, and the way out.
pub(crate) fn draw_too_small(f: &mut ratatui::Frame, app: &App, area: Rect) {
    // On a sliver there is no room for a bordered box, and a blank screen would
    // be indistinguishable from a hang. One clipped line still says what is
    // wrong, which is the whole point of the guard.
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
    // operator who shrank the terminal mid-prompt is not stranded.
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
                    .border_style(app.style(Color::Yellow)),
            ),
        box_,
    );
}

pub(crate) fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let size = size_class(area.width, area.height);
    if size == Size::TooSmall {
        // Nothing else is drawn: a clipped dashboard is worse than none.
        draw_too_small(f, app, area);
        return;
    }
    let compact = size == Size::Compact;

    // Compact gives up the Tasks pane — its numbers move to the footer — so
    // that Logs keeps rows. Logs is the pane that must stay readable.
    let (machines_h, workers_h) = if compact {
        (COMPACT_MACHINES_H, COMPACT_WORKERS_H)
    } else {
        (FULL_MACHINES_H, FULL_WORKERS_H)
    };
    let constraints: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(machines_h),
            Constraint::Length(workers_h),
            Constraint::Min(COMPACT_EVENTS_MIN_H),
            Constraint::Length(COMPACT_FOOTER_H),
        ]
    } else {
        vec![
            Constraint::Length(machines_h),
            Constraint::Length(workers_h),
            Constraint::Length(FULL_TASKS_H),
            Constraint::Min(FULL_EVENTS_MIN_H),
            Constraint::Length(FULL_FOOTER_H),
        ]
    };
    let root = RLayout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    machines::draw_machines(f, app, root[0], compact);
    workers::draw_workers(f, app, root[1], compact);
    if compact {
        events::draw_events(f, app, root[2]);
        footer::draw_footer(f, app, root[3], true);
    } else {
        // The tasks row splits: the queue summary keeps the left, the new
        // Stats matrix (workers × stages plus TUI-side ETA) takes a fixed
        // 46 on the right — 38 of columns, 6 of gaps, 2 of border.
        let task_row = RLayout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(30), Constraint::Length(46)])
            .split(root[2]);
        tasks::draw_tasks(f, app, task_row[0]);
        stats::draw_stats(f, app, task_row[1]);
        events::draw_events(f, app, root[3]);
        footer::draw_footer(f, app, root[4], false);
    }

    // Overlays paint last and cover everything beneath them. They are rendered
    // even in the compact tier: a confirmation must stay answerable.
    match app.screen.clone() {
        Screen::Help { scroll } => help::draw_help(f, app, scroll),
        Screen::Text(p) => prompt::draw_text_prompt(f, app, &p),
        Screen::Pick(p) => picker::draw_picker(f, app, &p),
        Screen::Cast(v) => cast::draw_cast(f, app, &v),
        Screen::Run => run::draw_run(f, app),
        Screen::Confirm(c) => confirm::draw_confirm(f, app, &c),
        Screen::Machine(addr) => machine::draw_machine_info(f, app, &addr),
        Screen::Jobs { scroll, .. } => jobs::draw_jobs(f, app, scroll),
        Screen::Tasks(v) => tasks::draw_tasks_screen(f, app, &v),
        Screen::TaskDetail(d) => task_detail::draw_task_detail(f, app, &d),
        Screen::Sound(v) => sound::draw_sound(f, app, &v),
        _ => {}
    }
}
