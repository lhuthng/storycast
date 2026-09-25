//! Sound-design overlay: three pools, one table each, and the guard made
//! visible.
//!
//! The screen has one job beyond listing the entries: **an operator must never
//! press a key that does nothing and be left guessing why**. So the highlighted
//! entry's usage is written out in full under the table, the remove key in the
//! action bar changes colour and gains `✗ in use` when it is dead, and the
//! status column says which entries are still reached before the cursor gets
//! near them.
use crate::tui::{
    app::{App, HitTarget, ListTarget},
    layout::{
        cols, size_class, Size, SOUND_COLS_NARROW, SOUND_COLS_WIDE, SOUND_KEYS_HEAD,
        SOUND_KEYS_REMOVE, SOUND_KEYS_REMOVE_DEAD, SOUND_KEYS_TAIL, SOUND_OVERLAY_H,
        SOUND_OVERLAY_W,
    },
    model::clamp_scroll,
    sound::{self, SoundView},
    style::{cell, centered_padded, empty_body, selection_bg, style_bold_of, style_of},
};
use bm_core::audio_pool::PoolKind;
use ratatui::{
    layout::{Constraint, Direction, Layout as RLayout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Row, Table, Wrap},
};

pub(crate) fn draw_sound(f: &mut ratatui::Frame, app: &mut App, view: &SoundView) {
    // The compact tier takes the whole screen: a `SOUND_OVERLAY_W`-wide table
    // centred in a 76-column terminal loses a third of its columns to margins
    // it cannot spare.
    let compact = size_class(f.area().width, f.area().height) == Size::Compact;
    let area = if compact {
        f.area()
    } else {
        centered_padded(f.area(), SOUND_OVERLAY_W, SOUND_OVERLAY_H, 2)
    };
    f.render_widget(Clear, area);

    let block = super::pane_block(app, "Sound design — Esc to close");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 6 {
        return;
    }

    let rows_area = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // layer tabs
            Constraint::Length(1), // summary and the gain chain
            Constraint::Min(3),    // the pool
            Constraint::Length(3), // why the highlighted entry is or is not removable
            Constraint::Length(2), // the action bar
        ])
        .split(inner);
    app.add_hit_region(rows_area[0], HitTarget::SoundTabs);

    // Tabs, with each layer's size so the screen says how much is behind them.
    let mut tabs: Vec<Span> = Vec::new();
    for (i, kind) in PoolKind::ALL.iter().enumerate() {
        if i > 0 {
            tabs.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
        let n = app.sound.as_ref().map(|d| d.pools[kind].len()).unwrap_or(0);
        let label = format!(" {} {n} ", kind.label());
        let style = if *kind == view.layer {
            app.style_bold(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        tabs.push(Span::styled(label, style));
    }
    tabs.push(Span::styled(
        "   ←→ or Tab switches layer",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(Line::from(tabs)), rows_area[0]);

    let Some(data) = &app.sound else {
        // Two different nothings: still loading, or refused. An empty pool and
        // an unreadable one look identical and only one is safe to edit, so
        // they are never drawn the same way.
        let lines = match &app.sound_error {
            Some(e) => vec![
                "the pools could not be read, so nothing is shown".to_string(),
                e.clone(),
                String::new(),
                "R retries. Editing is refused rather than done against a guess: an".to_string(),
                "empty pool written over an unreadable one would lose every entry.".to_string(),
            ],
            None => vec!["loading the three pools and the scripts…".to_string()],
        };
        f.render_widget(empty_body(lines).wrap(Wrap { trim: true }), rows_area[2]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "R reload · Esc close",
                Style::default().fg(Color::DarkGray),
            ))),
            rows_area[4],
        );
        return;
    };

    let rows = sound::rows(data, view.layer);
    let in_use = rows.iter().filter(|r| r.in_use()).count();
    let broken: usize = rows.iter().map(|r| r.missing.len()).sum();
    let mut summary = vec![
        Span::styled(
            format!("{} sound(s)", rows.len()),
            app.style_bold(Color::White),
        ),
        Span::styled(
            format!("  ·  {in_use} in use"),
            if in_use > 0 {
                app.style(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            format!("  ·  {} removable", rows.len() - in_use),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if broken > 0 {
        summary.push(Span::styled(
            format!("  ·  {broken} clip(s) missing"),
            app.style_bold(Color::Red),
        ));
    }
    // The whole gain chain, so a pool level is read in context rather than as
    // an absolute: this layer's master, times the operator's `:mix` volume.
    // Only when there is room — at the minimum width it would push the counts,
    // which are the reason the line exists, off the end.
    if !compact {
        let master = sound::master_level(&data.map, view.layer);
        let volume = app.setting_f64(
            match view.layer {
                PoolKind::Effect => "effect_volume",
                PoolKind::Music => "music_volume",
                PoolKind::Inject => "inject_volume",
            },
            1.0,
        );
        summary.push(Span::styled(
            format!(
                "   [{} {master:.2} × volume {volume:.2}]",
                view.layer.master_knob()
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(summary)), rows_area[1]);

    // The pool itself.
    let body = rows_area[2].height.saturating_sub(3) as usize;
    app.add_hit_region(
        rows_area[2],
        HitTarget::List {
            kind: ListTarget::Sound,
            row_start: view.scroll,
            row_y: rows_area[2].y + 2,
        },
    );
    if rows.is_empty() {
        f.render_widget(
            empty_body(vec![
                format!("the {} pool is empty", view.layer.label()),
                "a adds the first sound — it needs a name, at least one take and at least one tag"
                    .to_string(),
            ])
            .wrap(Wrap { trim: true }),
            rows_area[2],
        );
    } else if body > 0 {
        let colour = app.colour();
        let table_w = rows_area[2].width.saturating_sub(2);
        let wide = table_w >= cols(&SOUND_COLS_WIDE);
        let widths = if wide {
            SOUND_COLS_WIDE
        } else {
            SOUND_COLS_NARROW
        };
        let sound_w = widths[0] as usize;
        let tags_w = widths[1] as usize;

        let mut scroll = view.scroll;
        clamp_scroll(view.cursor, &mut scroll, rows.len(), body);
        let table_rows: Vec<Row> = rows
            .iter()
            .enumerate()
            .skip(scroll)
            .take(body)
            .map(|(i, r)| {
                let selected = i == view.cursor;
                let marker = if selected { "▸ " } else { "  " };
                // The column gets the short form and the sentence under the
                // table the long one — a rule list in a 22-column cell clips
                // mid-quote and reads as noise rather than as a warning.
                let (status, level) = r.status();
                let cells = vec![
                    cell(format!(
                        "{marker}{:<width$}",
                        bm_core::util::head_chars(&r.name, sound_w - 2),
                        width = sound_w - 2
                    )),
                    cell(format!(
                        "{:<width$}",
                        bm_core::util::head_chars(&r.sound.tags.join(", "), tags_w),
                        width = tags_w
                    )),
                    cell(format!("{:<4}", r.takes())),
                    cell(format!(
                        "{:<width$}",
                        r.shape(view.layer),
                        width = widths[3] as usize
                    )),
                    Line::from(Span::styled(status, style_of(colour, level.color()))),
                ];
                let mut row = Row::new(cells);
                if selected {
                    row = row.style(Style::default().bg(selection_bg()));
                }
                row
            })
            .collect();

        let title = if rows.len() > body {
            format!(
                "{} pool — showing {} of {}",
                view.layer.label(),
                body.min(rows.len()),
                rows.len()
            )
        } else {
            format!("{} pool", view.layer.label())
        };
        let header = vec!["sound", "tags", "takes", "shape", "status"];
        let constraints: Vec<Constraint> = widths
            .iter()
            .enumerate()
            .map(|(i, w)| {
                if i + 1 == widths.len() {
                    Constraint::Min(*w)
                } else {
                    Constraint::Length(*w)
                }
            })
            .collect();
        f.render_widget(
            Table::new(table_rows, constraints)
                .header(Row::new(header).style(style_bold_of(colour, Color::Gray)))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(title),
                ),
            rows_area[2],
        );
    }

    // Why the highlighted entry is, or is not, removable. The verdict is the
    // same text the column above shows, so the two cannot disagree; the two
    // lines under it are the consequence and the way out.
    let mut why: Vec<Line> = Vec::new();
    match rows.get(view.cursor) {
        None => why.push(Line::from(Span::styled(
            "nothing selected — a adds a sound",
            Style::default().fg(Color::DarkGray),
        ))),
        Some(r) => {
            let (verdict, level) = r.verdict();
            why.push(Line::from(vec![
                Span::styled(format!("“{}” — ", r.name), app.style_bold(level.color())),
                Span::styled(verdict, app.style(level.color())),
            ]));
            if r.in_use() {
                why.push(Line::from(Span::styled(
                    "    a scene names tags, not sounds — clear the reference in assets/scene-map.json, or re-digest the script, first",
                    Style::default().fg(Color::Gray),
                )));
            } else {
                why.push(Line::from(Span::styled(
                    format!(
                        "    takes: {} · tags: {}",
                        r.sound.files.join(", "),
                        r.sound.tags.join(", ")
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            why.push(Line::from(Span::styled(
                if r.missing.is_empty() {
                    format!("    shape: {}", r.shape(view.layer))
                } else {
                    format!("    MISSING CLIP: {}", r.missing.join(", "))
                },
                Style::default().fg(if r.missing.is_empty() {
                    Color::DarkGray
                } else {
                    Color::Red
                }),
            )));
            // A bed does not play at the level the pool gives it, and the level
            // is the thing the operator is about to change with `l`. Stated on
            // its own full-width line rather than appended to `shape`: that
            // string is a 23-column table cell and the factor would push the
            // duration off the end of it.
            if let Some(g) = r.render_gain() {
                why.push(Line::from(Span::styled(
                    format!("    renders at ×{g} of that level — a bed is mixed under the speech"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
    f.render_widget(Paragraph::new(why), rows_area[3]);

    // The action bar. `d` is the one key whose availability is a fact about the
    // highlighted entry, so it is the one key drawn differently — and it says
    // why, in the bar itself, rather than only in the status line after it has
    // been pressed.
    let dim = Style::default().fg(Color::DarkGray);
    let removable = rows.get(view.cursor).map(|r| !r.in_use()).unwrap_or(false);
    let mut keys: Vec<Span> = vec![Span::styled(SOUND_KEYS_HEAD, dim)];
    if removable {
        keys.push(Span::styled("d", app.style_bold(Color::Green)));
        keys.push(Span::styled(SOUND_KEYS_REMOVE, dim));
    } else {
        keys.push(Span::styled("d", app.style_bold(Color::Red)));
        keys.push(Span::styled(
            SOUND_KEYS_REMOVE_DEAD,
            app.style_bold(Color::Red),
        ));
    }
    keys.push(Span::styled(SOUND_KEYS_TAIL, dim));
    f.render_widget(
        Paragraph::new(vec![
            Line::from(keys),
            Line::from(Span::styled(
                "saved here; a worker gets the new pool on its next provision",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        rows_area[4],
    );
}
