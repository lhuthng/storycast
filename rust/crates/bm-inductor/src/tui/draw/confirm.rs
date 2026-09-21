//! Confirm overlay.
use crate::tui::{app::App, screen::Confirm, style::centered};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

pub(crate) fn draw_confirm(f: &mut ratatui::Frame, app: &App, c: &Confirm) {
    let width = 76.min(f.area().width);
    let height = (c.body.len() as u16 + 5).min(f.area().height);
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);

    let colour = if c.danger { Color::Red } else { Color::Cyan };
    let mut lines: Vec<Line> = c
        .body
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::default())))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Enter / y ", app.style(Color::Green)),
        Span::raw("confirm    "),
        Span::styled("Esc / n ", app.style(Color::Red)),
        Span::raw("cancel"),
    ]));

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(app.style(colour))
                    .title(c.title.clone()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
