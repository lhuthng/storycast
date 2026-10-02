//! The `:ws` picker: every book this checkout holds, and what each one carries.
use crate::tui::{
    app::App,
    screen::WsList,
    style::{centered, style_of},
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

pub(crate) fn draw_workspace_list(f: &mut ratatui::Frame, app: &App, ws: &WsList) {
    let area = centered(f.area(), 78, 18);
    f.render_widget(Clear, area);
    let dim = Style::default().fg(Color::DarkGray);

    let mut body: Vec<Line> = Vec::new();
    if ws.rows.is_empty() {
        body.push(Line::from(Span::styled(
            "no workspaces — this checkout is the implicit default",
            dim,
        )));
    }
    // Header, then one row per directory: the same `▸` + label + dim note the
    // guided list draws, so `:ws` and `:ws new` read as one screen.
    body.push(Line::from(vec![
        Span::styled("workspace", dim),
        Span::raw("  "),
        Span::styled("what it carries", dim),
    ]));
    body.push(Line::from(""));

    let rows = (area.height as usize).saturating_sub(8).max(1);
    let start = ws.scroll.min(ws.list().len().saturating_sub(1));
    for (i, item) in ws.list().iter().enumerate().skip(start).take(rows) {
        let selected = i == ws.cursor;
        let unusable = ws.unusable.contains_key(&i);
        let marker = if selected { "▸ " } else { "  " };
        body.push(Line::from(vec![
            Span::styled(
                marker,
                style_of(
                    app.colour(),
                    if selected { Color::Cyan } else { Color::Reset },
                ),
            ),
            Span::styled(
                item.label.clone(),
                style_of(
                    app.colour(),
                    match (selected, unusable) {
                        (true, _) => Color::Cyan,
                        (false, true) => Color::DarkGray,
                        (false, false) => Color::White,
                    },
                ),
            ),
            Span::styled(format!("  {}", item.note), dim),
        ]));
    }

    body.push(Line::from(""));
    if let Some(e) = &ws.error {
        body.push(Line::from(Span::styled(
            e.clone(),
            style_of(app.colour(), Color::Red),
        )));
    } else {
        body.push(Line::from(Span::styled(
            "↑↓ move · Enter switches — the cluster must be stopped (:X) · \
             Esc closes · :ws new <name> creates one",
            dim,
        )));
    }

    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(style_of(app.colour(), Color::Cyan))
                    .title("Workspace — switch"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}