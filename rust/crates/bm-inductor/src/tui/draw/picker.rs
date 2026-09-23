//! Voice picker overlay.
use crate::tui::{
    app::App,
    model::{clamp_scroll, filtered_characters, filtered_voices, users_of},
    screen::{PickStage, Picker},
    style::{centered, dash_if_empty, empty_body, gender_label, style_bold_of, style_of},
};
use bm_proto::VoiceInfo;
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};
use std::collections::BTreeMap;

pub(crate) fn draw_picker(f: &mut ratatui::Frame, app: &mut App, picker: &Picker) {
    let area = centered(f.area(), 96, 24);
    f.render_widget(Clear, area);

    let step = match picker.stage {
        PickStage::Character => "step 1 of 2 — choose a character",
        PickStage::Voice => "step 2 of 2 — choose a voice",
    };
    let title = match picker.stage {
        PickStage::Character => format!("Swap voice · {step}"),
        PickStage::Voice => format!("Swap voice · {step} · for “{}”", picker.character),
    };
    let block = super::pane_block(app, title);

    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 5 {
        return;
    }
    let rows = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // filter
            Constraint::Length(1), // provenance / policy
            Constraint::Length(1), // what an audition would play
            Constraint::Min(1),    // list
            Constraint::Length(2), // hints
        ])
        .split(inner);

    // Filter line. The cursor shows exactly when typing would land:
    // always on step 1, on step 2 only while the filter is focused
    // (in audition focus `t` would play, not type).
    let typing = picker.stage == PickStage::Character || picker.filter_focus;
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(picker.filter.clone(), app.style(Color::White)),
            Span::styled(if typing { "▌" } else { "" }, app.style(Color::Cyan)),
        ])),
        rows[0],
    );

    // Provenance: never let a fallback roster masquerade as the live one —
    // and never hide a usable roster behind a spinner while a live upgrade
    // is still in flight.
    let provenance = match (&app.roster, &app.roster_error, app.roster_loading) {
        (Some(r), _, _) => {
            let (label, colour) = if r.source == "live" {
                ("live roster", Color::Green)
            } else {
                ("OFFLINE roster — metadata may be incomplete", Color::Yellow)
            };
            Line::from(vec![
                Span::styled(
                    format!("{label} · engine {}   ", r.engine),
                    app.style(colour),
                ),
                Span::styled(r.policy_note.clone(), Style::default().fg(Color::DarkGray)),
            ])
        }
        (_, _, true) => Line::from(Span::styled(
            "loading roster from disk…",
            app.style(Color::Yellow),
        )),
        (_, Some(e), _) => Line::from(Span::styled(
            format!("roster unavailable: {e}   (Esc to close, R to retry)"),
            app.style(Color::Red),
        )),
        (None, None, _) => Line::from(Span::styled(
            "roster not loaded — press R",
            app.style(Color::Yellow),
        )),
    };
    f.render_widget(Paragraph::new(provenance), rows[1]);

    // What an audition would play, and what it would replace. Both matter and
    // neither is guessable from the table: the incumbent is not in the list of
    // candidates, and a random line is random until you are told which one it is.
    let audition_ctx: Line = match picker.stage {
        PickStage::Character => Line::from(Span::styled(
            "audition with :current :try :another once a character is chosen",
            Style::default().fg(Color::DarkGray),
        )),
        PickStage::Voice => {
            let incumbent = app
                .roster
                .as_ref()
                .and_then(|r| r.cast.get(&picker.character))
                .filter(|v| !v.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "unassigned".into());
            let held = match &picker.line {
                Some(l) => format!("“{}”", bm_core::util::head_chars(&l.text, 56)),
                None if app.lines.is_none() => "reading scripts…".to_string(),
                None => "not picked yet — :try".to_string(),
            };
            Line::from(vec![
                Span::styled("current: ", Style::default().fg(Color::DarkGray)),
                Span::styled(incumbent, app.style(Color::Yellow)),
                Span::styled("   line: ", Style::default().fg(Color::DarkGray)),
                Span::styled(held, app.style(Color::Cyan)),
            ])
        }
    };
    f.render_widget(Paragraph::new(audition_ctx), rows[2]);

    let height = rows[3].height as usize;
    let colour = app.colour();
    // Which voice is rendering right now. Read from the `App`, not the picker:
    // the cast overview can start an audition too, so "in flight" is a property
    // of the process and not of this screen.
    let auditioning = app.audition.clone();
    // Precomputed so the row closures below capture plain data rather than a
    // borrow of `app`, which is also being borrowed for the roster itself.
    let cast: BTreeMap<String, String> = app
        .roster
        .as_ref()
        .map(|r| r.cast.clone())
        .unwrap_or_default();
    let meta: BTreeMap<String, VoiceInfo> = app
        .roster
        .as_ref()
        .map(|r| {
            r.voices
                .iter()
                .map(|v| (v.name.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    match picker.stage {
        PickStage::Character => {
            let list = filtered_characters(app, &picker.filter);
            if list.is_empty() {
                let msg = if app.roster.is_none() {
                    "no roster yet".to_string()
                } else if picker.filter.trim().is_empty() {
                    "no speakers known yet — run t (translate) or v (voices) first".to_string()
                } else {
                    format!(
                        "no speaker matches “{}” — Enter accepts it as a new character",
                        picker.filter.trim()
                    )
                };
                f.render_widget(empty_body(vec![msg]).wrap(Wrap { trim: true }), rows[3]);
            } else {
                let mut scroll = picker.scroll;
                clamp_scroll(picker.cursor, &mut scroll, list.len(), height);
                let items: Vec<Line> = list
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(height)
                    .map(|(i, name)| {
                        let selected = i == picker.cursor;
                        let marker = if selected { "▸ " } else { "  " };
                        let current = cast.get(name).cloned().unwrap_or_default();
                        let mut spans = vec![
                            Span::styled(marker.to_string(), style_of(colour, Color::Cyan)),
                            Span::styled(
                                format!("{name:<28}"),
                                if selected {
                                    style_bold_of(colour, Color::White)
                                } else {
                                    Style::default()
                                },
                            ),
                        ];
                        if current.is_empty() {
                            spans.push(Span::styled(
                                "unassigned — v (voices) fills gaps".to_string(),
                                Style::default().fg(Color::DarkGray),
                            ));
                        } else {
                            spans.push(Span::styled(
                                format!("{current:<14}"),
                                style_of(colour, Color::Green),
                            ));
                            if let Some(v) = meta.get(&current) {
                                spans.push(Span::styled(
                                    format!(
                                        "{} · {} · {}",
                                        gender_label(&v.gender),
                                        dash_if_empty(&v.accent),
                                        v.language
                                    ),
                                    Style::default().fg(Color::DarkGray),
                                ));
                            }
                        }
                        Line::from(spans)
                    })
                    .collect();
                f.render_widget(Paragraph::new(items), rows[3]);
            }
        }
        PickStage::Voice => {
            let list = filtered_voices(app, &picker.filter);
            if list.is_empty() {
                f.render_widget(
                    empty_body(vec![
                        "no voice matches that filter".to_string(),
                        "Esc goes back to the character list".to_string(),
                    ])
                    .wrap(Wrap { trim: true }),
                    rows[3],
                );
            } else {
                let mut scroll = picker.scroll;
                clamp_scroll(picker.cursor, &mut scroll, list.len(), height);
                let items: Vec<Line> = list
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(height)
                    .map(|(i, v)| {
                        let selected = i == picker.cursor;
                        let marker = if selected { "▸ " } else { "  " };
                        let users = users_of(&cast, &v.name);
                        let (status, colour_of_status) = if users.contains(&picker.character) {
                            ("current".to_string(), Color::Green)
                        } else if !users.is_empty() {
                            (format!("in use: {}", users.join(", ")), Color::Yellow)
                        } else if !v.allowed {
                            ("accent policy concern".to_string(), Color::Yellow)
                        } else {
                            ("available".to_string(), Color::DarkGray)
                        };
                        let mut spans = vec![
                            Span::styled(marker.to_string(), style_of(colour, Color::Cyan)),
                            Span::styled(
                                format!("{:<14}", v.name),
                                if selected {
                                    style_bold_of(colour, Color::White)
                                } else if v.allowed {
                                    Style::default()
                                } else {
                                    Style::default().fg(Color::DarkGray)
                                },
                            ),
                            Span::styled(
                                format!("{:<8}", gender_label(&v.gender)),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<14}", dash_if_empty(&v.accent)),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<7}", v.language),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<16}", dash_if_empty(&v.style)),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ];
                        if v.enrolled {
                            spans.push(Span::styled("clone ", style_of(colour, Color::Magenta)));
                        }
                        if auditioning.as_deref() == Some(v.name.as_str()) {
                            spans.push(Span::styled(
                                "auditioning… ",
                                style_of(colour, Color::Yellow),
                            ));
                        } else if picker.previewed.iter().any(|p| p == &v.name) {
                            spans.push(Span::styled("auditioned ", style_of(colour, Color::Green)));
                        }
                        spans.push(Span::styled(status, style_of(colour, colour_of_status)));
                        Line::from(spans)
                    })
                    .collect();
                f.render_widget(Paragraph::new(items), rows[3]);
            }
        }
    }

    // Hint rows, split by stage so the available keys are always accurate.
    let hints: Vec<Line> = match picker.stage {
        PickStage::Character => vec![
            Line::from(Span::styled(
                "type to filter · ↑↓ move · Enter choose · Esc close · R reload roster",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "Enter on a non-matching name adds it as a new character",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        PickStage::Voice if !picker.filter_focus => vec![
            Line::from(Span::styled(
                "t incumbent · T candidate · ^T another line — none of these assign",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "other letters filter · ^R focuses filter · ↑↓ move · Enter assign · Esc back",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        PickStage::Voice => vec![
            Line::from(Span::styled(
                "typing — t/T filter too · :current :try :another still audition",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "↑↓ move · Enter assign · Esc back to audition keys",
                Style::default().fg(Color::DarkGray),
            )),
        ],
    };
    f.render_widget(Paragraph::new(hints), rows[4]);
}
