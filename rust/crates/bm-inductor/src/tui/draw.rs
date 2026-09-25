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
    app::{App, HitTarget, Panel},
    layout::{
        size_class, Size, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H, COMPACT_MACHINES_H,
        FULL_EVENTS_MIN_H, FULL_FOOTER_H, FULL_HEADER_H, FULL_MACHINES_H, MIN_H, MIN_W,
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
    pane_block_for(app, None, title)
}

/// A dashboard pane border. The focused pane gets a brighter border so mouse
/// focus is visible without hiding a row behind a synthetic status message.
/// Draw a deliberately fixed one-cell scroll thumb. Ratatui's stock
/// scrollbar makes the thumb length proportional to the viewport, which makes
/// a large list look like it has a large handle. The cross is always one cell;
/// only its vertical position changes.
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
    app.clear_hit_regions();
    let area = f.area();
    let size = size_class(area.width, area.height);
    if size == Size::TooSmall {
        // Nothing else is drawn: a clipped dashboard is worse than none.
        draw_too_small(f, app, area);
        return;
    }
    let compact = size == Size::Compact;

    // Workers owns the flexible middle of the dashboard. Its renderer decides
    // how much room the live rows need and places Tasks/Stats beneath them;
    // Machines and Logs remain bounded, independently scrollable panes.
    let constraints: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(COMPACT_MACHINES_H),
            Constraint::Min(3),
            Constraint::Min(COMPACT_EVENTS_MIN_H),
            Constraint::Length(COMPACT_FOOTER_H),
        ]
    } else {
        vec![
            Constraint::Length(FULL_HEADER_H),
            Constraint::Length(FULL_MACHINES_H),
            // Workers is the only flexible full-tier pane. Keeping Logs at its
            // readable floor means terminal height beyond the base layout goes
            // to live worker rows, which is what the tall-layout test expects.
            Constraint::Min(3),
            Constraint::Length(FULL_EVENTS_MIN_H),
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
        app.add_hit_region(
            root[0],
            HitTarget::Panel {
                panel: Panel::Machines,
                row_start: app.machine_scroll,
                row_y: root[0].y + 2,
            },
        );
        app.add_hit_region(
            root[2],
            HitTarget::Panel {
                panel: Panel::Events,
                row_start: 0,
                row_y: root[2].y + 1,
            },
        );
        app.add_hit_region(
            root[3],
            HitTarget::Panel {
                panel: Panel::Footer,
                row_start: 0,
                row_y: root[3].y,
            },
        );
    } else {
        draw_header(f, app, root[0]);
        machines::draw_machines(f, app, root[1], compact);
        workers::draw_workers(f, app, root[2], compact);
        events::draw_events(f, app, root[3]);
        footer::draw_footer(f, app, root[4], false);
        app.add_hit_region(
            root[1],
            HitTarget::Panel {
                panel: Panel::Machines,
                row_start: app.machine_scroll,
                row_y: root[1].y + 2,
            },
        );
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
