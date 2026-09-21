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
    let area = centered(f.area(), 88, 8);
    f.render_widget(Clear, area);
    let (before, after) = prompt.split();
    let body = vec![
        Line::from(Span::styled(
            prompt.hint.clone(),
            Style::default().fg(Color::DarkGray),
        )),
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
    ];
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
