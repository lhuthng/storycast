//! Single-line prompt overlay.
use crate::tui::{
    app::App,
    screen::TextPrompt,
    style::{centered, style_of},
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

pub(crate) fn draw_text_prompt(f: &mut ratatui::Frame, app: &App, prompt: &TextPrompt) {
    // The known-site note is drawn *above* the input line, so the dialog grows
    // upward from the text rather than pushing the text down: a suggestion that
    // scrolls the cursor out of sight is a suggestion that arrives too late.
    let note: Vec<Line> = match prompt.known_note() {
        None => Vec::new(),
        Some(n) => n
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    style_of(app.colour(), Color::Green),
                ))
            })
            .collect(),
    };
    let extra = note.len() as u16 + if note.is_empty() { 0 } else { 1 };
    let area = centered(f.area(), 88, 8 + extra);
    f.render_widget(Clear, area);
    let (before, after) = prompt.split();
    let mut body = vec![
        Line::from(Span::styled(
            prompt.hint.clone(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];
    if !note.is_empty() {
        body.push(Line::from(""));
        body.extend(note);
    }
    body.extend([
        Line::from(""),
        // What is on screen is exactly what will be submitted — the prompt is
        // the echo, so nothing is sent that the operator did not read back.
        Line::from(vec![
            Span::styled("> ", style_of(app.colour(), Color::Cyan)),
            Span::styled(before, style_of(app.colour(), Color::White)),
            Span::styled("▌", style_of(app.colour(), Color::Cyan)),
            Span::raw(after),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter submit · Esc cancel · ←→ move · Home/End · Ctrl-U clear · Ctrl-W delete word",
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    f.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(style_of(app.colour(), Color::Cyan))
                    .title(prompt.title.clone()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
