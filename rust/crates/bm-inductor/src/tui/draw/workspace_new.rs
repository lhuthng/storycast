//! The guided `workspace new` overlay: name → profile → crawler.
use crate::tui::{
    app::App,
    screen::{WorkspaceNew, WsStep},
    style::{centered, style_of},
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// Split a buffer at a character cursor, for the blinking-caret rendering.
fn split(buf: &str, cursor: usize) -> (String, String) {
    let b = buf
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(buf.len());
    (buf[..b].to_string(), buf[b..].to_string())
}

pub(crate) fn draw_workspace_new(f: &mut ratatui::Frame, app: &App, ws: &WorkspaceNew) {
    let area = centered(f.area(), 78, 22);
    f.render_widget(Clear, area);
    let dim = Style::default().fg(Color::DarkGray);

    let active = match ws.step {
        WsStep::Name => 0,
        WsStep::Profile => 1,
        // The EPUB path and the custom URL are the crawler step's follow-ups:
        WsStep::Crawler | WsStep::Epub | WsStep::CustomUrl => 2,
    };
    let mut crumb: Vec<Span> = Vec::new();
    for (i, step) in ["name", "profile", "crawler"].iter().enumerate() {
        if i > 0 {
            crumb.push(Span::raw("   "));
        }
        let (mark, colour) = if i < active {
            ("✔ ", Color::Green)
        } else if i == active {
            ("● ", Color::Cyan)
        } else {
            ("○ ", Color::DarkGray)
        };
        crumb.push(Span::styled(
            format!("{mark}{step}"),
            style_of(app.colour(), colour),
        ));
    }

    let mut body: Vec<Line> = vec![Line::from(crumb), Line::from("")];
    match ws.step {
        WsStep::Name => {
            body.push(Line::from(Span::styled("Workspace name", dim)));
            let (before, after) = split(&ws.name, ws.name_cursor);
            body.push(Line::from(vec![
                Span::styled("> ", style_of(app.colour(), Color::Cyan)),
                Span::styled(before, style_of(app.colour(), Color::White)),
                Span::styled("▌", style_of(app.colour(), Color::Cyan)),
                Span::raw(after),
            ]));
        }
        WsStep::Profile | WsStep::Crawler => {
            body.push(Line::from(vec![
                Span::styled("name     ", dim),
                Span::styled(ws.name.clone(), style_of(app.colour(), Color::White)),
            ]));
            if let Some(i) = ws.profile {
                if let Some(p) = ws.profiles.get(i) {
                    body.push(Line::from(vec![
                        Span::styled("profile  ", dim),
                        Span::styled(p.label.clone(), style_of(app.colour(), Color::White)),
                    ]));
                }
            }
            body.push(Line::from(""));
            let rows = (area.height as usize).saturating_sub(13).max(1);
            let start = ws.scroll.min(ws.list().len().saturating_sub(1));
            for (i, item) in ws.list().iter().enumerate().skip(start).take(rows) {
                let selected = i == ws.cursor;
                let marker = if selected { "▸ " } else { "  " };
                let mut spans = vec![
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
                            if selected { Color::Cyan } else { Color::White },
                        ),
                    ),
                ];
                if !item.note.is_empty() {
                    spans.push(Span::styled(format!("  {}", item.note), dim));
                }
                body.push(Line::from(spans));
            }
        }
        WsStep::Epub => {
            body.push(Line::from(vec![
                Span::styled("name     ", dim),
                Span::styled(ws.name.clone(), style_of(app.colour(), Color::White)),
            ]));
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "Path to the .epub — or a folder of volumes (books/)",
                dim,
            )));
            let (before, after) = split(&ws.epub, ws.epub_cursor);
            body.push(Line::from(vec![
                Span::styled("> ", style_of(app.colour(), Color::Cyan)),
                Span::styled(before, style_of(app.colour(), Color::White)),
                Span::styled("▌", style_of(app.colour(), Color::Cyan)),
                Span::raw(after),
            ]));
        }
        WsStep::CustomUrl => {
            body.push(Line::from(vec![
                Span::styled("name     ", dim),
                Span::styled(ws.name.clone(), style_of(app.colour(), Color::White)),
            ]));
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "Chapter URL template — the chapter number goes in {n}",
                dim,
            )));
            let (before, after) = split(&ws.url, ws.url_cursor);
            body.push(Line::from(vec![
                Span::styled("> ", style_of(app.colour(), Color::Cyan)),
                Span::styled(before, style_of(app.colour(), Color::White)),
                Span::styled("▌", style_of(app.colour(), Color::Cyan)),
                Span::raw(after),
            ]));
        }
    }

    body.push(Line::from(""));
    if let Some(e) = &ws.error {
        body.push(Line::from(Span::styled(
            e.clone(),
            style_of(app.colour(), Color::Red),
        )));
    } else {
        let hint = match ws.step {
            WsStep::Name => "type a name · Enter next · Esc cancel",
            WsStep::Profile | WsStep::Crawler => "↑↓ choose · Enter next · Esc back",
            WsStep::Epub => "type a .epub or a folder · Enter create · Esc back",
            WsStep::CustomUrl => "type a template with {n} · Enter create · Esc back",
        };
        body.push(Line::from(Span::styled(hint, dim)));
    }

    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(style_of(app.colour(), Color::Cyan))
                    .title("Workspace — new"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
