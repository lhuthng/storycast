//! The digest manager's keys: pick a chapter, then run its two rounds by
//! clipboard.
//!
//! The gesture is deliberately tiny. `c` puts the current round's prompt on the
//! clipboard; the operator pastes it into whatever model they already have open;
//! `v` brings the answer back. Round 1's answer buys round 2's prompt, round 2's
//! answer finishes the chapter — so there is never more than one thing to do
//! next, and the screen says which.
//!
//! **Everything goes through the worker's own code.** The prompts come from
//! `bm_core::digest`, the answers are checked by the same validators, and the
//! result is reported to `/api/complete` with the same body a worker sends. A
//! manual digest is the automatic one with a person standing in for the model —
//! which is why it cannot put a chapter into the library that the worker's path
//! would have refused.
use crate::tui::input::command::Command;
use crate::tui::{
    app::App,
    clipboard,
    input::{command::do_command, dispatch},
    jobs::Job,
    screen::{DigestChapter, DigestView, Screen, DIGEST_COLS as COLS},
    style::Level,
};
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) async fn key_digest(
    app: &mut App,
    view: DigestView,
    key: KeyEvent,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
) -> bool {
    // "Digested" is one question with one answer, and the list, the filter and
    // the draw must all ask it the same way: the chapter has a script on disk.
    // Derived from the layout rather than remembered, so a digest that lands
    // while the screen is open is reflected on the next keypress.
    let layout = app.layout.clone();
    // Borrowed, not moved: the same `layout` builds prompts and validates
    // answers a few lines down, and one `Layout` is the whole point — a second
    // clone here is how two halves of this screen would come to disagree about
    // which workspace they are looking at. The question itself is
    // `Layout::digested`, so the filter, the list and the draw cannot diverge.
    let digested = |n: u32| layout.digested(n);

    let mut v = view;
    let Some(mut ch) = v.open.clone() else {
        // ---- the list ------------------------------------------------------
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.screen = Screen::Normal;
                app.set_status(Level::Info, "closed the digest manager");
                return false;
            }
            // **A grid, so the arrows mean what the picture means.** The chapters
            // are drawn `COLS` to a row, so ←/→ step one chapter and ↑/↓ step a
            // *row* — twelve. Stepping one chapter on ↑ would move the highlight
            // sideways, which is the one thing the eye does not expect of it.
            // `h`/`l` alias the horizontal pair, `j`/`k` the vertical, because
            // vim hands expect `j`/`k` to move by a line of whatever is on screen.
            KeyCode::Left | KeyCode::Char('h') => {
                v.cursor = v.cursor.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let rows = v.rows(&digested).len();
                v.cursor = (v.cursor + 1).min(rows.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                v.cursor = v.cursor.saturating_sub(COLS);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let rows = v.rows(&digested).len();
                v.cursor = (v.cursor + COLS).min(rows.saturating_sub(1));
            }
            // The cluster-wide digest switch, on the screen it belongs to. It
            // runs the *same command* the `:off` / `:on` words run, so there is
            // one implementation of a snapshot-and-restore and two ways to reach
            // it — the same shape as `:policy` pressing `P`.
            KeyCode::Char('x') | KeyCode::Char('s') => {
                let cmd = if key.code == KeyCode::Char('x') {
                    Command::DigestOff
                } else {
                    Command::DigestOn
                };
                app.screen = Screen::Digest(v.clone());
                do_command(app, cmd, http, job_tx);
                return false;
            }
            KeyCode::Char('f') => {
                v.hide_done = !v.hide_done;
                // The cursor indexes the *rows*, so a filter that shrinks the
                // list can leave it past the end — pointing at nothing while the
                // screen still highlights a line.
                let rows = v.rows(&digested).len();
                v.cursor = v.cursor.min(rows.saturating_sub(1));
                let total = v.chapters.len();
                let shown = v.rows(&digested).len();
                app.set_status(
                    Level::Info,
                    if v.hide_done {
                        format!("hiding the digested ones — {shown} of {total} left")
                    } else {
                        format!("showing every chapter — {total}")
                    },
                );
            }
            KeyCode::Enter => {
                if let Some(n) = v.selected(&digested) {
                    match open_chapter(&layout, n, &digested) {
                        Ok(ch) => {
                            app.set_status(
                                Level::Info,
                                format!("ch{n}: {} prompt on the clipboard", ch.round.as_str()),
                            );
                            v.open = Some(ch);
                        }
                        Err(e) => app.set_status(Level::Error, format!("ch{n}: {e}")),
                    }
                }
            }
            _ => {}
        }
        app.screen = Screen::Digest(v);
        return false;
    };

    // ---- inside a chapter: the two rounds ---------------------------------
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Back to the list, keeping the cursor where it was. A chapter that
            // finished says so in the list; one abandoned mid-round simply loses
            // its cast, which is the honest outcome of walking away.
            //
            // **Returns here rather than falling through to the write-back below**,
            // and that is the whole fix for "Esc does nothing": the tail sets
            // `v.open = Some(ch)` for the arms that *mutated* the chapter, so an
            // arm that closes it would have its work undone one line later. The
            // view is a clone, so every arm has to write it back — which means the
            // one arm that clears it has to be the one arm that leaves early.
            v.open = None;
            app.set_status(
                Level::Info,
                format!("back to the chapter list (ch{} left as it was)", ch.n),
            );
            app.screen = Screen::Digest(v);
            return false;
        }
        KeyCode::Char('c') => match clipboard::copy(&ch.prompt) {
            Ok(()) => {
                ch.note = format!(
                    "{} prompt copied — paste it into your model",
                    ch.round.as_str()
                );
                app.set_status(Level::Info, format!("ch{}: prompt copied", ch.n));
            }
            // A copy that fails must say so: otherwise the operator pastes a
            // stale clipboard and reads a validator complaint about text they
            // never saw, which is a bug report about the wrong component.
            Err(e) => {
                ch.note = e.clone();
                app.set_status(Level::Error, e);
            }
        },
        KeyCode::Char('v') => {
            match clipboard::paste() {
                Err(e) => {
                    ch.note = e.clone();
                    app.set_status(Level::Error, e);
                }
                Ok(pasted) => match accept(&layout, &mut ch, &pasted, &app.api, http) {
                    Ok(Some(job)) => {
                        dispatch(app, job_tx, job);
                        ch.note = "accepted — reporting it to the inductor".into();
                        app.set_status(Level::Info, format!("ch{}: digest accepted", ch.n));
                    }
                    Ok(None) => {
                        // Round 1 landed: round 2's prompt is already on the
                        // clipboard, so the operator's next move is the same one
                        // they just made.
                        app.set_status(
                            Level::Info,
                            format!("ch{}: cast accepted — script prompt copied", ch.n),
                        );
                    }
                    Err(e) => {
                        // The validator's own words. They are the instruction:
                        // the operator can paste the complaint back into their
                        // model and ask for a correction.
                        ch.note = e.clone();
                        app.set_status(Level::Error, format!("ch{}: {e}", ch.n));
                    }
                },
            }
        }
        _ => {}
    }
    // Arms that mutated the chapter write it back; Esc left early above, which is
    // why this is safe to do unconditionally *here*.
    v.open = Some(ch);
    app.screen = Screen::Digest(v);
    false
}

/// Start a chapter: build round 1's prompt and put it on the clipboard.
///
/// The copy happens here rather than on the first `c` so that opening a chapter
/// *is* the gesture — the operator presses Enter and then goes straight to their
/// model. `c` exists to get it back after a failed paste.
///
/// A re-digest says so in the note: the chapter already has a script, and the
/// inductor will invalidate the audio built from the old one.
fn open_chapter(
    layout: &bm_core::Layout,
    n: u32,
    digested: &dyn Fn(u32) -> bool,
) -> Result<DigestChapter, String> {
    let step = bm_core::digest::manual_prompt(layout, n, None).map_err(|e| format!("{e:#}"))?;
    let mut note = format!(
        "{} prompt copied — paste it into your model",
        step.round.as_str()
    );
    if digested(n) {
        note.push_str(" · this chapter is already digested, so finishing will re-render it");
    }
    // A copy that fails still leaves the chapter open with its prompt visible in
    // the note, so `c` can be retried rather than the whole screen bounced.
    let _ = clipboard::copy(&step.text);
    Ok(DigestChapter {
        n,
        round: step.round,
        prompt: step.text,
        cast: None,
        note,
        done: false,
    })
}

/// Take one pasted answer: validate it, and either ask for round 2 or finish.
///
/// `Ok(None)` means round 1 was accepted and round 2 is now on the clipboard.
/// `Ok(Some(job))` means the chapter is finished and the caller should report it.
fn accept(
    layout: &bm_core::Layout,
    ch: &mut DigestChapter,
    pasted: &str,
    api: &str,
    http: &reqwest::Client,
) -> Result<Option<Job>, String> {
    let answer = bm_core::digest::manual_accept(layout, ch.n, ch.round, pasted, ch.cast.as_ref())
        .map_err(|e| format!("{e:#}"))?;

    if let Some(context) = answer.cast {
        // Round 1 done. Round 2's prompt is rendered *against this cast*, which
        // is why the context is carried rather than re-derived — the worker makes
        // exactly this hand-off between its two calls.
        let step = bm_core::digest::manual_prompt(layout, ch.n, Some(&context))
            .map_err(|e| format!("{e:#}"))?;
        let copied = match clipboard::copy(&step.text) {
            Ok(()) => "script prompt copied".to_string(),
            Err(e) => format!("script prompt ready, but the copy failed: {e}"),
        };
        ch.cast = Some(context);
        ch.round = step.round;
        ch.prompt = step.text;
        ch.note = format!("cast accepted — {copied}");
        return Ok(None);
    }

    let Some(outcome) = answer.outcome else {
        return Err("the answer carried neither a cast nor a script".into());
    };
    ch.done = true;
    ch.note = format!(
        "done — {} segments, {} sound items",
        outcome.segments,
        outcome
            .script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.iter().filter(|i| bm_core::util::is_sound_item(i)).count())
            .unwrap_or(0),
    );
    for line in &outcome.log {
        if line.starts_with("   WARN") {
            ch.note.push_str(&format!(" ·{line}"));
        }
    }
    // Reported as a *worker report*, under the reserved manual id. The inductor
    // then does everything it does for a worker: merges the bible delta, writes
    // the script, marks the row Done — and invalidates the chapter's audio if the
    // script changed, which is what a re-digest needs.
    Ok(Some(Job::ManualDigest {
        api: api.to_string(),
        http: http.clone(),
        chapter: ch.n,
        script: outcome.script,
        delta: outcome.delta,
    }))
}
