//! The per-machine work-policy overlay.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    screen::PolicyView,
    style::{centered, stage_color, style_of},
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

pub(crate) fn draw_policy(f: &mut ratatui::Frame, app: &mut App, v: &PolicyView) {
    let area = centered(f.area(), 66, 12);
    f.render_widget(Clear, area);
    let colour = app.colour();
    let dim = Style::default().fg(Color::DarkGray);
    let last = v.prefs.len().saturating_sub(1);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "  ↑↓ move · Space grab + carry · Enter toggle · Esc close — saved as you go",
            dim,
        )),
        Line::from(""),
    ];
    for (i, p) in v.prefs.iter().enumerate() {
        let cursor = if i == v.cursor { "▸" } else { " " };
        let grab = if v.grabbed == Some(i) { "⇅" } else { " " };
        let mark = if p.enabled { "[x]" } else { "[ ]" };
        let rank = if i == 0 {
            "highest"
        } else if i == last {
            "lowest"
        } else {
            ""
        };
        let name_style = if p.enabled {
            style_of(colour, stage_color(p.stage.as_str()))
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        };
        lines.push(Line::from(vec![
            Span::raw(format!(" {cursor}{grab} {mark} ")),
            Span::styled(format!("{:<7}", p.stage.as_str()), name_style),
            Span::styled(format!("  {rank}"), dim),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  the first enabled stage with work wins; if it has none, the next one",
        dim,
    )));

    app.add_hit_region(
        area,
        HitTarget::List {
            kind: ListTarget::Policy,
            row_start: 0,
            row_y: area.y + 3,
        },
    );

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(app.style(Color::Cyan))
                .title(format!("Policy — {} · {}", v.label, v.addr)),
        ),
        area,
    );
}
