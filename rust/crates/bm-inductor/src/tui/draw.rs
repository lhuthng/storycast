//! Paint: tier dispatch plus the overlay match. Panes live in `draw/`.
mod cast;
mod cloud;
mod confirm;
mod digest;
mod events;
mod footer;
mod help;
mod jobs;
mod machine;
mod machines;
mod picker;
mod policy;
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
        COMPACT_WORKERS_H, FULL_EVENTS_MIN_H, FULL_FOOTER_H, FULL_HEADER_H, FULL_MACHINES_H,
        FULL_TASKS_H, FULL_WORKERS_H, MIN_H, MIN_W,
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
/// The old square `Block::default()` read as five identical boxes; the
/// rounded outline plus one hue separates chrome from data, which is what
/// makes a five-pane dashboard scannable.
pub(crate) fn pane_block(app: &App, title: impl Into<Line<'static>>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(app.style(crate::tui::style::theme_accent()))
        .title(title.into())
}

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
                    .border_type(BorderType::Rounded)
                    .border_style(app.style(Color::Yellow)),
            ),
        box_,
    );
}

/// The one-line identity strip above the panes, full tier only.
///
/// The footer used to carry the workspace, profile, engine, analyzer and
/// chapter range after the status — the eye had to wade past constants to
/// find what just happened. Here the identity lives top-left where a title
/// would be, and the theme chip sits right so `C` is discoverable.
fn draw_header(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let mut left = vec![Span::styled(
        format!("ws: {}", crate::tui::model::workspace_label(&app.layout)),
        app.style_bold(crate::tui::style::theme_accent()),
    )];
    // A missing profile is not decoration: every runner refuses to start
    // without one, so it reads as the problem it is (yellow, not dim).
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
    // that Logs keeps rows. Logs is the pane that must stay readable. The
    // header is full-tier only: the compact tier sits exactly on its 20-row
    // floor, and the identity it carries lives in the footer there.
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
            Constraint::Length(FULL_HEADER_H),
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

    if compact {
        machines::draw_machines(f, app, root[0], compact);
        workers::draw_workers(f, app, root[1], compact);
        events::draw_events(f, app, root[2]);
        footer::draw_footer(f, app, root[3], true);
    } else {
        draw_header(f, app, root[0]);
        machines::draw_machines(f, app, root[1], compact);
        workers::draw_workers(f, app, root[2], compact);
        // The tasks row splits: the queue summary keeps the left, the
        // Stats matrix (workers × stages plus TUI-side ETA) takes a fixed
        // 46 on the right — 38 of columns, 6 of gaps, 2 of border.
        let task_row = RLayout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(30), Constraint::Length(46)])
            .split(root[3]);
        tasks::draw_tasks(f, app, task_row[0]);
        stats::draw_stats(f, app, task_row[1]);
        events::draw_events(f, app, root[4]);
        footer::draw_footer(f, app, root[5], false);
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
        Screen::Policy(v) => policy::draw_policy(f, app, &v),
        Screen::Digest(v) => digest::draw_digest(f, app, &v),
        Screen::Jobs { scroll, .. } => jobs::draw_jobs(f, app, scroll),
        Screen::Tasks(v) => tasks::draw_tasks_screen(f, app, &v),
        Screen::TaskDetail(d) => task_detail::draw_task_detail(f, app, &d),
        Screen::Sound(v) => sound::draw_sound(f, app, &v),
        Screen::Cloud(v) => cloud::draw_cloud(f, app, &v),
        _ => {}
    }
}
