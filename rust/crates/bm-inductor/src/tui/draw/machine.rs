//! Machine detail overlay.
use crate::tui::{
    app::App,
    model::{machine_kind, policy_summary},
    style::{centered, seen_label, state_age_label},
};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

pub(crate) fn draw_machine_info(f: &mut ratatui::Frame, app: &App, addr: &str) {
    let area = centered(f.area(), 84, 19);
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
    let mut lines = vec![kv("addr", m.addr.clone())];
    // The registry handle the provision log uses — without it the detail
    // screen cannot answer "is this the box `[hawk]` was about".
    if !m.name.is_empty() {
        lines.push(kv("name", m.name.clone()));
    }
    lines.extend([
        kv("kind", machine_kind(m).to_string()),
        kv("role", m.role.clone()),
        kv("policy", {
            let enabled: Vec<&str> = m
                .effective_task_policy()
                .iter()
                .filter(|p| p.enabled)
                .map(|p| p.stage.as_str())
                .collect();
            if enabled.is_empty() {
                "none enabled — P to configure".into()
            } else {
                format!("{}   ({})", enabled.join(" > "), policy_summary(m))
            }
        }),
        kv("state", m.state.as_str().to_string()),
        // How long it has been that way: "initializing 0:12" and "initializing
        // 0:20" want opposite reactions, and the state alone cannot tell them
        // apart.
        kv("state age", state_age_label(m)),
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
    ]);
    if m.note.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "  — nothing recorded yet; :prov provisions it",
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
                    .border_type(BorderType::Rounded)
                    .border_style(app.style(Color::Cyan))
                    .title("Machine — Esc or i to close"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
