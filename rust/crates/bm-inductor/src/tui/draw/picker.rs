//! Voice picker overlay.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    model::{
        clamp_scroll, filtered_characters, filtered_voices, gender_of, settle_cursor, used_by,
        VoiceKind, VoiceRow,
    },
    screen::{PickStage, Picker},
    style::{centered, empty_body, style_bold_of, style_of},
};
use bm_proto::VoiceInfo;
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};
use std::collections::BTreeMap;

/// Step 2's column widths, in characters. Named because the used-by cell is
/// computed from what is left: these four are the whole of the fixed part, and
/// a column added here has to be paid for there or the last cell silently
/// loses its padding.
const MARKER_W: usize = 2;
/// A voice name and its tag suffix, `young-male-10`, with room for the
/// catalogue's accented presets.
const NAME_W: usize = 20;
const GENDER_W: usize = 8;
/// `auditioning…` / `auditioned`, the only two states a row can carry that
/// the voice itself does not.
const AUDITION_W: usize = 12;

/// One cell, exactly `width` wide: truncated, then padded.
///
/// A cut cell says so with an `…` rather than stopping mid-word, because a
/// name that runs into the next column reads as one longer name — which is
/// exactly the bug the fixed widths exist to prevent.
///
/// `head_chars` counts characters, and every string in this table is
/// precomposed single-width text, so the pad lands where the terminal ends it.
fn pad(text: &str, width: usize) -> String {
    let body = match text.chars().count() {
        0 => String::new(),
        n if n <= width => text.to_string(),
        _ => format!(
            "{}…",
            bm_core::util::head_chars(text, width.saturating_sub(1))
        ),
    };
    format!("{:<width$}", body, width = width)
}

/// `1 voice` / `2 voices`, for the group headings.
fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// What a group heading says it is holding.
fn group_label(kind: VoiceKind, tags: &str) -> String {
    match kind {
        // A pooled sample with no tags at all is still auto-assignable — it
        // was vetted when it was added — it just has nothing to group by.
        VoiceKind::AutoAssign if tags.is_empty() => "Auto Assign · untagged".to_string(),
        VoiceKind::AutoAssign => format!("Auto Assign · {tags}"),
        VoiceKind::Unique => "Unique".to_string(),
    }
}

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
    app.add_hit_region(
        rows[3],
        HitTarget::List {
            kind: ListTarget::Picker,
            row_start: picker.scroll,
            row_y: rows[3].y,
        },
    );
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
                    "no speakers known yet — :t (translate) or :v (voices) first".to_string()
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
                                "unassigned — :v (voices) fills gaps".to_string(),
                                Style::default().fg(Color::DarkGray),
                            ));
                        } else {
                            spans.push(Span::styled(
                                format!("{current:<14}"),
                                style_of(colour, Color::Green),
                            ));
                            if let Some(v) = meta.get(&current) {
                                spans.push(Span::styled(
                                    // The same `gender_of` as step 2: a pooled
                                    // sample's roster gender is `unknown`, and
                                    // its tag already says what it is. Accent
                                    // and language are gone with it: on an
                                    // offline roster they read `unknown` and
                                    // `vi-VN` on every single row, and a column
                                    // that says one thing on every line is not
                                    // a column.
                                    gender_of(v).to_string(),
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
                // The cursor is a row index and a heading is a row, so it can
                // be sitting on one; the marker follows the voice it means.
                let cursor = settle_cursor(&list, picker.cursor);
                // Every column is a fixed width and the used-by cell is
                // whatever is left over. A voice name, an accent or a list of
                // eleven characters must not push the cells after it — that
                // is what made the columns unreadable and the alignment a
                // guess.
                let used_w = (rows[3].width as usize)
                    .saturating_sub(MARKER_W + NAME_W + GENDER_W + AUDITION_W);
                let items: Vec<Line> = list
                    .iter()
                    .enumerate()
                    .skip(scroll)
                    .take(height)
                    .map(|(i, row)| match row {
                        VoiceRow::Group { kind, tags, count } => Line::from(Span::styled(
                            format!(
                                "── {} · {}",
                                group_label(*kind, tags),
                                plural(*count, "voice")
                            ),
                            style_of(colour, Color::Cyan),
                        )),
                        VoiceRow::Voice { voice: v, users } => {
                            let selected = i == cursor;
                            let marker = if selected { "\u{25b8} " } else { "  " };
                            let used = used_by(users, &picker.character);
                            // Green is the incumbent: this is the voice the
                            // character already speaks with.
                            let (used_text, used_colour) =
                                if users.iter().any(|u| u == &picker.character) {
                                    (used, Color::Green)
                                } else if users.is_empty() {
                                    (used, Color::DarkGray)
                                } else {
                                    (used, Color::Yellow)
                                };
                            let badge = if auditioning.as_deref() == Some(v.name.as_str()) {
                                ("auditioning\u{2026}", Color::Yellow)
                            } else if picker.previewed.iter().any(|p| p == &v.name) {
                                ("auditioned", Color::Green)
                            } else {
                                ("", Color::DarkGray)
                            };
                            Line::from(vec![
                                Span::styled(marker.to_string(), style_of(colour, Color::Cyan)),
                                Span::styled(
                                    pad(&v.name, NAME_W),
                                    if selected {
                                        style_bold_of(colour, Color::White)
                                    } else if v.allowed {
                                        Style::default()
                                    } else {
                                        // An accent-policy voice is not hidden —
                                        // it is dimmed, because refusing to
                                        // assign it is the policy's job and
                                        // refusing to show it is not.
                                        Style::default().fg(Color::DarkGray)
                                    },
                                ),
                                Span::styled(
                                    pad(gender_of(v), GENDER_W),
                                    Style::default().fg(Color::DarkGray),
                                ),
                                Span::styled(
                                    pad(&used_text, used_w),
                                    style_of(colour, used_colour),
                                ),
                                Span::styled(pad(badge.0, AUDITION_W), style_of(colour, badge.1)),
                            ])
                        }
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
