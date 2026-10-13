//! The script inspection window: the digested chapters as a list, one open

use crate::tui::input::Flow;
use crate::tui::{
    app::App,
    input::{dispatch_op, op_key},
    jobs::Job,
    screen::{Screen, ScriptPick, ScriptView},
    style::Level,
};
use bm_proto::{Op, OpRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) async fn key_script(
    app: &mut App,
    mut v: ScriptView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> Flow {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // ---- depth 3: the speaker picker over one segment ----------------------
    if let Some(mut pick) = v.pick.take() {
        let suggestions = v.suggestions(app, &pick);
        let last = suggestions.len().saturating_sub(1);
        let mut keep = true;
        match key.code {
            // One Esc closes the picker; the segment list keeps its cursor,
            KeyCode::Esc => {
                app.set_status(Level::Info, "pick cancelled — nothing changed");
                keep = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                pick.cursor = pick.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                pick.cursor = (pick.cursor + 1).min(last);
            }
            KeyCode::PageUp => {
                pick.cursor = pick.cursor.saturating_sub(ScriptPick::SHOW);
            }
            KeyCode::PageDown => {
                pick.cursor = (pick.cursor + ScriptPick::SHOW).min(last);
            }
            KeyCode::Home => pick.cursor = 0,
            KeyCode::End => pick.cursor = last,
            KeyCode::Backspace => {
                pick.filter.pop();
                pick.cursor = 0;
                pick.scroll = 0;
            }
            KeyCode::Char(c) if ctrl && c == 'u' => {
                pick.filter.clear();
                pick.cursor = 0;
                pick.scroll = 0;
            }
            KeyCode::Enter => {
                // The chosen row, clamped: typing may have left the cursor
                match suggestions.get(pick.cursor.min(last)).cloned() {
                    Some(name) if name == pick.expect => {
                        app.set_status(
                            Level::Info,
                            format!(
                                "segment {} already speaks as {name:?} — nothing to change",
                                pick.segment
                            ),
                        );
                    }
                    Some(name) => {
                        let req = OpRequest {
                            op: Op::FixSpeaker,
                            chapter: v.open,
                            segment: Some(pick.segment),
                            // What the screen showed, not a guess: the op
                            expect: Some(pick.expect.clone()),
                            speaker: Some(name.clone()),
                            ..Default::default()
                        };
                        if app.inflight.contains(&op_key(&req)) {
                            app.set_status(Level::Warn, "that re-point is already running");
                        } else {
                            dispatch_op(app, job_tx, http, req);
                            app.set_status(
                                Level::Ok,
                                format!("segment {} → {name} — watch the ledger", pick.segment),
                            );
                            // The picker closes: the segment row (re-read on
                            keep = false;
                        }
                    }
                    None => {
                        app.set_status(
                            Level::Warn,
                            "no speaker matches — Backspace widens, Esc cancels",
                        );
                        // Keep the picker up so the filter can be fixed
                    }
                }
            }
            KeyCode::Char(c) if !alt => {
                pick.filter.push(c);
                pick.cursor = 0;
                pick.scroll = 0;
            }
            _ => {}
        }
        // One store for every keep path, one flag for the two closes (Esc,
        if keep {
            v.pick = Some(pick);
        }
        app.screen = Screen::Script(v);
        return Flow::KeepRunning;
    }

    // ---- depth 2: the open chapter's segments -------------------------------
    if let Some(ch) = v.open {
        let seg_last = v.segments.len().saturating_sub(1);
        // The excerpt panel sits over the segments. While it is up the arrows
        if v.excerpt_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('e') => {
                    v.close_excerpts();
                    app.set_status(
                        Level::Info,
                        format!("ch{ch}: excerpts closed — s re-point a speaker · Esc back"),
                    );
                }
                // `saturating_*` throughout, because `End` parks the offset at
                KeyCode::Down | KeyCode::Char('j') => {
                    v.excerpt_scroll = v.excerpt_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    v.excerpt_scroll = v.excerpt_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => v.excerpt_scroll = v.excerpt_scroll.saturating_add(8),
                KeyCode::PageUp => v.excerpt_scroll = v.excerpt_scroll.saturating_sub(8),
                KeyCode::Home => v.excerpt_scroll = 0,
                // The draw clamps the offset to the last page, so `End` can
                KeyCode::End => v.excerpt_scroll = usize::MAX,
                _ => {}
            }
            app.screen = Screen::Script(v);
            return Flow::KeepRunning;
        }
        match key.code {
            KeyCode::Esc => {
                // Back to the list, cursor where it was. The segments are
                v.open = None;
                v.segments.clear();
                v.seg_cursor = 0;
                app.screen = Screen::Script(v);
                app.set_status(
                    Level::Info,
                    format!("back to the chapter list (ch{ch} left as read)"),
                );
                return Flow::KeepRunning;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                v.seg_cursor = v.seg_cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                v.seg_cursor = (v.seg_cursor + 1).min(seg_last);
            }
            KeyCode::PageUp => {
                v.seg_cursor = v.seg_cursor.saturating_sub(20);
            }
            KeyCode::PageDown => {
                v.seg_cursor = (v.seg_cursor + 20).min(seg_last);
            }
            KeyCode::Home => v.seg_cursor = 0,
            KeyCode::End => v.seg_cursor = seg_last,
            // The excerpt chain: the state this chapter ends on, and the
            KeyCode::Char('e') => {
                v.open_excerpts(&app.layout, ch);
                let fed = v.excerpt_fed.len();
                app.set_status(
                    Level::Info,
                    format!(
                        "ch{ch}: excerpt · fed from {fed} earlier chapter(s) · ↑↓ scroll · e or Esc closes"
                    ),
                );
            }
            // The re-point. Only a *line* can be re-attributed; on a sound
            KeyCode::Char('s') => match v.segments.get(v.seg_cursor) {
                Some(seg) if !seg.speaker.is_empty() => {
                    // The chapter's own roster, read once at pick-open: it is
                    // the suggestion list's prefix and the rows' "(this
                    // chapter)" tag, from the one file both describe.
                    let roster_cache = v
                        .open
                        .map(|ch| {
                            bm_core::read_json::<serde_json::Value>(&app.layout.script(ch))
                                .ok()
                                .and_then(|data| {
                                    data.get("roster").and_then(|r| r.as_array()).map(|list| {
                                        list.iter()
                                            .filter_map(|x| x.as_str())
                                            .map(str::to_string)
                                            .collect()
                                    })
                                })
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    v.pick = Some(ScriptPick {
                        segment: seg.n,
                        expect: seg.speaker.clone(),
                        filter: String::new(),
                        cursor: 0,
                        scroll: 0,
                        roster_cache,
                    });
                    app.set_status(
                        Level::Info,
                        "pick who speaks it — type to filter · ↑↓ · PgUp PgDn · Enter · Esc cancel",
                    );
                }
                Some(seg) => {
                    app.set_status(
                        Level::Warn,
                        format!(
                            "segment {} is a sound, not a line — nothing to re-point",
                            seg.n
                        ),
                    );
                }
                None => app.set_status(Level::Warn, "no segment selected"),
            },
            _ => {}
        }
        app.screen = Screen::Script(v);
        return Flow::KeepRunning;
    }

    // ---- depth 1: the chapter list ------------------------------------------
    let rows = v.rows();
    let last = rows.len().saturating_sub(1);
    match key.code {
        KeyCode::Esc => {
            // **Return, don't fall through**: the tail below writes
            app.screen = app.back_out();
            app.set_status(Level::Info, "closed the script window");
            return Flow::KeepRunning;
        }
        // Rows and columns, not one step each: the list is a twelve-wide grid, so
        KeyCode::Up | KeyCode::Char('k') => v.move_cursor(-1, 0),
        KeyCode::Down | KeyCode::Char('j') => v.move_cursor(1, 0),
        KeyCode::Left | KeyCode::Char('h') => v.move_cursor(0, -1),
        KeyCode::Right | KeyCode::Char('l') => v.move_cursor(0, 1),
        KeyCode::PageUp => v.move_cursor(-8, 0),
        KeyCode::PageDown => v.move_cursor(8, 0),
        KeyCode::Home => v.cursor = 0,
        KeyCode::End => v.cursor = last,
        KeyCode::Backspace => {
            v.filter.pop();
            v.cursor = 0;
        }
        KeyCode::Char(c) if ctrl && c == 'u' => {
            v.filter.clear();
            v.cursor = 0;
        }
        KeyCode::Enter => match v.selected() {
            Some(ch) => {
                let segments = v.read_segments(&app.layout, ch);
                if segments.is_empty() {
                    app.set_status(
                        Level::Error,
                        format!("ch{ch}: script exists but holds no segments"),
                    );
                } else {
                    v.open = Some(ch);
                    v.segments = segments;
                    v.seg_cursor = 0;
                    app.set_status(
                        Level::Info,
                        format!(
                            "ch{ch}: {} segments · s re-point a speaker · Esc back",
                            v.segments.len()
                        ),
                    );
                }
            }
            None => app.set_status(Level::Warn, "no chapter selected"),
        },
        KeyCode::Char(c) if !alt => {
            // Digits are the gesture ("41" → chapter 41); every letter is
            v.filter.push(c);
            v.cursor = 0;
        }
        _ => {}
    }
    // The filter is a view concern: the cursor indexes the *filtered* rows,
    v.cursor = v.cursor.min(v.rows().len().saturating_sub(1));
    app.screen = Screen::Script(v);
    Flow::KeepRunning
}
