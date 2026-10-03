//! The LLM setup overlay: providers, keys, models, and which one digests.
use crate::tui::{app::App, screen::LlmView, style::centered};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};

pub(crate) fn draw_llm(f: &mut ratatui::Frame, app: &mut App, v: &LlmView) {
    let dim = Style::default().fg(Color::DarkGray);
    let cfg = bm_core::config::LlmConfig::load(&app.layout.root);
    let ids = LlmView::ids(&cfg);
    let active = cfg.resolve().map(|r| r.provider);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "  ↑↓ move · a activate · k key · u URL · m model · f fetch models · Esc close",
            dim,
        )),
        Line::from(""),
    ];
    for (i, id) in ids.iter().enumerate() {
        let e = cfg.providers.get(id).cloned().unwrap_or_default();
        let cursor = if i == v.cursor { "▸" } else { " " };
        let is_active = active.as_deref() == Some(id.as_str());
        let mark = if is_active { "●" } else { "○" };
        let key_ok = e.has_key() || cfg.kind_of(id) == bm_core::config::LlmKind::Ollama;
        let key = if e.has_key() { "key:set" } else { "key:—" };
        let key_style = if key_ok {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let model = if e.model.trim().is_empty() {
            "model:—"
        } else {
            e.model.trim()
        };
        let row_style = if i == v.cursor {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(format!(" {cursor} {mark} ")),
            Span::styled(format!("{id:<12}"), row_style),
            Span::styled(format!(" {key:<7}"), key_style),
            Span::raw(" "),
            Span::styled(
                if is_active {
                    format!("{model}  ACTIVE")
                } else {
                    model.to_string()
                },
                if is_active {
                    app.style(Color::Green)
                } else {
                    Style::default()
                },
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "      {}",
                if e.base_url.trim().is_empty() {
                    "url:—".to_string()
                } else {
                    e.base_url.trim().to_string()
                }
            ),
            dim,
        )));
        // The fetched list, offered as a pick on its own provider's row.
        if v.picking && i == v.cursor && app.llm_models_for == *id && !app.llm_models.is_empty() {
            const SHOW: usize = 5;
            let total = app.llm_models.len();
            let cursor = v.model_cursor.min(total - 1);
            let start = cursor
                .saturating_sub(SHOW - 1)
                .min(total.saturating_sub(SHOW));
            lines.push(Line::from(Span::styled(
                format!(
                    "      {total} models ({}–{})",
                    start + 1,
                    (start + SHOW).min(total)
                ),
                dim,
            )));
            for (m, name) in app.llm_models.iter().enumerate().skip(start).take(SHOW) {
                let mc = if m == v.model_cursor { "▸" } else { " " };
                let current = if *name == e.model { "  (current)" } else { "" };
                lines.push(Line::from(vec![
                    Span::raw(format!("     {mc} ")),
                    Span::styled(
                        name.clone(),
                        if m == v.model_cursor {
                            app.style(Color::Cyan)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(current.to_string(), dim),
                ]));
            }
        }
    }
    if ids.is_empty() {
        lines.push(Line::from(Span::styled("  no providers configured", dim)));
    }
    if active.is_none() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  no active provider — digests refuse until one is activated",
            app.style(Color::Yellow),
        )));
    }
    if !v.note.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("  {}", v.note), dim)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  keys live in .bm/llm.json — the next offer carries the active key+model, no other sync",
        dim,
    )));

    // The box grows with the fetched list; clamped to the terminal.
    let h = (lines.len() + 2).min(f.area().height as usize).max(10) as u16;
    let area = centered(f.area(), 78, h);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(app.style(Color::Cyan))
                .title("LLM providers — L"),
        ),
        area,
    );
}
