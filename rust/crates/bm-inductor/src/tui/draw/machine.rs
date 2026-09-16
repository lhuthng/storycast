//! Machine detail overlay.
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use crate::tui::{app::App, style::{centered, seen_label}};

pub(crate) fn draw_machine_info(f: &mut ratatui::Frame, app: &App, addr: &str) {
    let area = centered(f.area(), 84, 18);
    f.render_widget(Clear, area);

    let Some(m) = app.machine_by_addr(addr) else {
        f.render_widget(
            Paragraph::new("that machine is no longer in the registry")
                .block(Block::default().borders(Borders::ALL).title("Machine")),
            area,
        );
        return;
    };

    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("  {k:<14}"), app.style(Color::Cyan)),
            Span::raw(v),
        ])
    };
    let mut lines = vec![
        kv("id", m.id.clone()),
        kv("addr", m.addr.clone()),
        kv("role", m.role.clone()),
        kv("state", m.state.as_str().to_string()),
        kv("ssh", m.ssh_target()),
        kv("ssh port", m.ssh_port.to_string()),
        kv("ssh key", {
            let def = app.ssh_defaults();
            let (path, src) =
                bm_core::provision::resolve_key(m.ssh_key.as_deref(), def.key.as_deref());
            match path {
                Some(p) => format!("{}  ({})", p.display(), src.label()),
                None => format!("—  ({})", src.label()),
            }
        }),
        kv("tts", m.tts_url.clone().unwrap_or_else(|| "—".into())),
        kv("last seen", seen_label(m)),
        kv(
            "capabilities",
            if m.capabilities.is_empty() {
                "—".into()
            } else {
                m.capabilities.join(", ")
            },
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  note (probe / provision output)",
            app.style_bold(Color::White),
        )),
    ];
    if m.note.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  — nothing recorded yet; press p to provision",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for l in m.note.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(app.style(Color::Cyan))
                    .title("Machine — Esc or i to close"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
