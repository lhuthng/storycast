//! The digest manager's keys: pick a chapter, then run its two rounds by
use crate::manual::Next;
use crate::tui::input::command::Command;
use crate::tui::input::Flow;
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
) -> Flow {
    // "Digested" is one question with one answer, and the list, the filter and
    let layout = app.layout.clone();
    // Borrowed, not moved: the same `layout` builds prompts and validates
    let digested = |n: u32| layout.digested(n);

    let mut v = view;
    let Some(mut ch) = v.open.clone() else {
        // ---- the list ------------------------------------------------------
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.screen = Screen::Normal;
                app.set_status(Level::Info, "closed the digest manager");
                return Flow::KeepRunning;
            }
            // **A grid, so the arrows mean what the picture means.** The chapters
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
            KeyCode::Char('x') | KeyCode::Char('s') => {
                let cmd = if key.code == KeyCode::Char('x') {
                    Command::DigestOff
                } else {
                    Command::DigestOn
                };
                app.screen = Screen::Digest(v.clone());
                do_command(app, cmd, http, job_tx);
                return Flow::KeepRunning;
            }
            KeyCode::Char('f') => {
                v.hide_done = !v.hide_done;
                // The cursor indexes the *rows*, so a filter that shrinks the
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
                    // The engine, because round 2's prompt carries the
                    let engine =
                        app.setting_str("engine", &bm_core::config::Settings::default().engine);
                    match open_chapter(&layout, &engine, n, &digested) {
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
        return Flow::KeepRunning;
    };

    // ---- inside a chapter: the two rounds ---------------------------------
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            // Back to the list, keeping the cursor where it was. A chapter that
            v.open = None;
            app.set_status(
                Level::Info,
                format!("back to the chapter list (ch{} left as it was)", ch.n),
            );
            app.screen = Screen::Digest(v);
            return Flow::KeepRunning;
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
                Ok(pasted) => {
                    let engine =
                        app.setting_str("engine", &bm_core::config::Settings::default().engine);
                    match accept(&layout, &engine, &mut ch, &pasted, &app.api, http) {
                        Ok(Some(job)) => {
                            dispatch(app, job_tx, job);
                            ch.note = "accepted — reporting it to the inductor".into();
                            app.set_status(Level::Info, format!("ch{}: digest accepted", ch.n));
                        }
                        Ok(None) => {
                            // Round 1 landed: round 2's prompt is already on the
                            app.set_status(
                                Level::Info,
                                format!("ch{}: cast accepted — script prompt copied", ch.n),
                            );
                        }
                        Err(e) => {
                            // The validator's own words. They are the instruction:
                            ch.note = e.clone();
                            app.set_status(Level::Error, format!("ch{}: {e}", ch.n));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    // Arms that mutated the chapter write it back; Esc left early above, which is
    v.open = Some(ch);
    app.screen = Screen::Digest(v);
    Flow::KeepRunning
}

/// Start a chapter: build round 1's prompt and put it on the clipboard.
fn open_chapter(
    layout: &bm_core::Layout,
    engine: &str,
    n: u32,
    digested: &dyn Fn(u32) -> bool,
) -> Result<DigestChapter, String> {
    let step = crate::manual::open(layout, engine, n, digested)?;
    let round = step
        .round()
        .ok_or_else(|| format!("ch{n} opened on a finished digest"))?;
    let mut note = format!(
        "{}{} prompt copied — paste it into your model",
        part_note(step.part()),
        round.as_str()
    );
    if digested(n) {
        note.push_str(" · this chapter is already digested, so finishing will re-render it");
    }
    // A copy that fails still leaves the chapter open with its prompt visible in
    let _ = clipboard::copy(step.text());
    Ok(DigestChapter {
        n,
        round,
        prompt: step.text().to_string(),
        cast: None,
        part: step.part(),
        note,
        done: false,
    })
}

/// `part 2/3 · ` in front of a note, and nothing when the chapter fits one
fn part_note(part: Option<bm_core::digest::ManualPart>) -> String {
    match part {
        Some(part) if part.total > 1 => format!("part {}/{} · ", part.index, part.total),
        _ => String::new(),
    }
}

/// Take one pasted answer: validate it, and either ask for round 2 or finish.
fn accept(
    layout: &bm_core::Layout,
    engine: &str,
    ch: &mut DigestChapter,
    pasted: &str,
    api: &str,
    http: &reqwest::Client,
) -> Result<Option<Job>, String> {
    let step = crate::manual::advance(layout, engine, ch.n, ch.round, pasted, ch.cast.as_ref())?;

    let outcome = match step {
        Next::Prompt {
            round,
            text,
            cast,
            part,
        } => {
            // A round was accepted and the next one is ready. That is either
            let copied = match clipboard::copy(&text) {
                Ok(()) => "prompt copied".to_string(),
                Err(e) => format!("prompt ready, but the copy failed: {e}"),
            };
            ch.cast = cast;
            ch.round = round;
            ch.prompt = text;
            ch.part = part;
            ch.note = format!(
                "{}{} accepted — {copied}",
                part_note(part),
                match round {
                    bm_core::digest::Round::Staging => "cast",
                    bm_core::digest::Round::Attribution => "script",
                }
            );
            return Ok(None);
        }
        Next::Done(outcome) => outcome,
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
    Ok(Some(Job::ManualDigest {
        api: api.to_string(),
        http: http.clone(),
        chapter: ch.n,
        script: outcome.script,
        delta: outcome.delta,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_digest_refuses_a_chapter_past_the_next_one() {
        // Skipping ahead would merge bible deltas out of order: only the
        let d = tempfile::tempdir().unwrap();
        let layout = bm_core::Layout::new(d.path());
        let err = open_chapter(&layout, "vieneu", 7, &|n| layout.digested(n)).unwrap_err();
        assert!(err.contains("not next"), "{err}");
    }

    #[test]
    fn manual_digest_opens_the_chapter_after_the_last_digested_one() {
        // The digest queue's bottleneck case: ch1 done, ch2 fresh — ch2 opens,
        let d = tempfile::tempdir().unwrap();
        bm_core::profile::install_fixture(d.path()).unwrap();
        let layout = bm_core::Layout::new(d.path());
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(2), "text").unwrap();
        std::fs::write(layout.chapter_txt(3), "text").unwrap();
        let digested = |n: u32| n == 1;
        assert!(open_chapter(&layout, "vieneu", 2, &digested).is_ok());
        assert!(open_chapter(&layout, "vieneu", 3, &digested).is_err());
    }
}
