//! The script inspection window: the digested chapters as a list, one open
//! chapter's segments with their speakers, and `s` to re-point one.
//!
//! Three depths, one view, the way the digest manager is one view with two
//! modes: the chapter list, the open chapter's segments, and the speaker
//! picker as a panel over the segments. `Esc` means "step back" at every
//! depth — picker, then segments, then the window closes — and never
//! lands anywhere the operator did not come from.
//!
//! The dispatch on `Enter` in the picker is [`Op::FixSpeaker`] — the same
//! op `:speaker chapter segment expect speaker` runs — because the check
//! is the feature: the window shows who the segment speaks as *now*, and
//! sends exactly that as `expect`, so a stale screen cannot edit the wrong
//! line. The op refuses on a mismatch and names the neighbours; nothing
//! here duplicates that.
//!
//! The chapter list takes typed digits as its filter — a book is numbers,
//! and `41` finding chapter 41 is the whole gesture. That is why `q` is
//! not bound here: the list owns every letter, and the window closes with
//! `Esc`, the key its title line advertises.

use crate::tui::{
    app::App,
    input::{dispatch_op, op_key},
    jobs::Job,
    screen::{Screen, ScriptPick, ScriptView},
    style::Level,
};
use crate::tui::input::Flow;
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
            // and the next Esc steps back to the chapter list.
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
                // past the end of the (shorter) filtered list. Whether the
                // picker stays up is decided by the `keep` flag below — one
                // store at the bottom, no per-arm bookkeeping.
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
                            // re-reads the script and refuses on a mismatch,
                            // which is the guard that makes a stale window
                            // refuse instead of mis-edit.
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
                            // the next open) is the confirmation.
                            keep = false;
                        }
                    }
                    None => {
                        app.set_status(
                            Level::Warn,
                            "no speaker matches — Backspace widens, Esc cancels",
                        );
                        // Keep the picker up so the filter can be fixed
                        // rather than retyped from scratch.
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
        // and an Enter that dispatched). No per-arm `v.pick` bookkeeping to
        // get backwards.
        if keep {
            v.pick = Some(pick);
        }
        app.screen = Screen::Script(v);
        return Flow::KeepRunning;
    }

    // ---- depth 2: the open chapter's segments -------------------------------
    if let Some(ch) = v.open {
        let seg_last = v.segments.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                // Back to the list, cursor where it was. The segments are
                // re-read on the next open, so a digest that landed while
                // this chapter was up is picked up for free.
                v.open = None;
                v.segments.clear();
                v.seg_cursor = 0;
                app.screen = Screen::Script(v);
                app.set_status(Level::Info, format!("back to the chapter list (ch{ch} left as read)"));
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
            // The re-point. Only a *line* can be re-attributed; on a sound
            // row the key says so rather than opening a picker whose Enter
            // could only fail.
            KeyCode::Char('s') => match v.segments.get(v.seg_cursor) {
                Some(seg) if !seg.speaker.is_empty() => {
                    // The chapter's own roster, read once at pick-open: it is
                    // the suggestion list's prefix and the rows' "(this
                    // chapter)" tag, from the one file both describe.
                    let roster_cache = v.open
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
                        format!("segment {} is a sound, not a line — nothing to re-point", seg.n),
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
            // `Screen::Script(v)` back, and an arm that closed the window
            // would have its close undone one line later — the exact bug the
            // digest manager's Esc arm documents.
            app.screen = app.back_out();
            app.set_status(Level::Info, "closed the script window");
            return Flow::KeepRunning;
        }
        KeyCode::Up | KeyCode::Char('k') => v.cursor = v.cursor.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => v.cursor = (v.cursor + 1).min(last),
        KeyCode::PageUp => v.cursor = v.cursor.saturating_sub(12),
        KeyCode::PageDown => v.cursor = (v.cursor + 12).min(last),
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
                    app.set_status(Level::Error, format!("ch{ch}: script exists but holds no segments"));
                } else {
                    v.open = Some(ch);
                    v.segments = segments;
                    v.seg_cursor = 0;
                    app.set_status(
                        Level::Info,
                        format!("ch{ch}: {} segments · s re-point a speaker · Esc back", v.segments.len()),
                    );
                }
            }
            None => app.set_status(Level::Warn, "no chapter selected"),
        },
        KeyCode::Char(c) if !alt => {
            // Digits are the gesture ("41" → chapter 41); every letter is
            // accepted too because the filter matches on the number's text
            // anyway, and a stray letter narrowing to nothing is visible in
            // one keypress, not a mystery.
            v.filter.push(c);
            v.cursor = 0;
        }
        _ => {}
    }
    // The filter is a view concern: the cursor indexes the *filtered* rows,
    // so a change that shrinks the list can leave it past the end.
    v.cursor = v.cursor.min(v.rows().len().saturating_sub(1));
    app.screen = Screen::Script(v);
    Flow::KeepRunning
}
