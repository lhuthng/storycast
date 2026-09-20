//! Auditioning a voice: play it, never commit to it.
//!
//! Three auditions — current, pointed, another — answering three questions,
//! run from `t` / `T` / `^T` or from the `:current` / `:try` / `:another`
//! words (same callees below, so keys and words can never disagree).
//!
//! * **current** plays an already-rendered segment (`Op::Segment`): the
//!   held line with the current voice, zero synthesis. In the picker the
//!   character is fixed so the line never rolls; in the cast overview it
//!   follows the highlighted speaker.
//! * **try** renders the held line with the pointed voice
//!   (`Op::PreviewVoice` with text): the one deliberate generation, and the
//!   only way to hear two voices on the same sentence before either is
//!   assigned. With the inductor down it synthesizes on this machine
//!   instead — no worker needs to be on.
//! * **another** renders another line with the pointed voice: one random
//!   pick may be a poor representative.
//!
//! Keys or filter, never both: picker step 2 and the cast overview open in
//! audition focus, where `t`/`T`/`^T` play and any other letter focuses the
//! filter instead. While the filter is focused every letter types (t/T
//! included) and the keys go quiet — the words still audition from the
//! command line. `Esc` blurs back to audition focus; `^R` focuses explicitly.
//!
//! The held line lives on the screen; `Enter` on a voice additionally locks it
//! per character (`App::locked_lines`), so reopening the picker resumes on the
//! sentence the voice was picked on instead of another random pick.
//!
//! `Enter` is the only key that changes the cast.

use crate::tui::{
    app::App,
    audition::{line_for, AuditionLine},
    input::{dispatch, dispatch_op, op_key},
    jobs::Job,
    style::{Conn, Level},
};
use bm_proto::{Op, OpRequest};

/// Render and play `voice` speaking the held line — or a fresh line for
/// `character` when nothing fitting is held.
///
/// Returns the line the caller should hold afterwards, so a second audition of
/// the same character speaks the same sentence. Returns `held` unchanged when
/// nothing was dispatched — a refusal must not silently drop the A/B.
///
/// There is deliberately no fixed-sample fallback: without a real line there
/// is nothing honest to say, and the sample was synthesis wearing a lab coat.
#[allow(clippy::too_many_arguments)] // each is a distinct decision input; bundling them would be a redesign
pub(crate) fn audition(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    character: &str,
    voice: &str,
    held: Option<&AuditionLine>,
    reroll: bool,
) -> Option<AuditionLine> {
    if voice.trim().is_empty() {
        app.set_status(
            Level::Warn,
            format!("no voice to audition for “{character}”"),
        );
        return held.cloned();
    }
    // One render at a time: the sidecar is a single model on one machine, and two
    // samples at once would also talk over each other.
    if let Some(v) = app.audition.clone() {
        app.set_status(
            Level::Warn,
            format!("{v} is still rendering — one audition at a time"),
        );
        return held.cloned();
    }

    let seed = bm_proto::now_secs();
    let fresh = line_for(
        if reroll { None } else { held },
        character,
        app.lines.as_ref(),
        seed,
    );
    let l = match fresh {
        Some(l) => l,
        None => {
            // No lines for this character yet — a newcomer the digest has not
            // reached. Say so rather than rendering nothing or quietly playing
            // something else.
            let why = if app.lines.is_none() {
                "lines are still loading"
            } else {
                "no lines in the scripts yet"
            };
            app.set_status(
                Level::Warn,
                format!("{character}: {why} — nothing to render"),
            );
            return held.cloned();
        }
    };

    // No backend, no worker, no sidecar: synthesize on this machine instead.
    // Same engine call the sidecar makes, so a fresh voice auditions with
    // nothing on — at the cost of loading the model here.
    if !matches!(app.conn, Conn::Up) {
        if app.layout.root.as_os_str().is_empty() {
            app.set_status(
                Level::Warn,
                "inductor unreachable and no local checkout — :B to connect",
            );
            return held.cloned();
        }
        app.audition = Some(voice.to_string());
        dispatch(
            app,
            job_tx,
            Job::PreviewLocal {
                layout: app.layout.clone(),
                voice: voice.to_string(),
                text: l.text.clone(),
            },
        );
        app.set_status(
            Level::Info,
            format!(
                "rendering “{}” for “{character}” ({voice}) locally…",
                bm_core::util::head_chars(&l.text, 48)
            ),
        );
        return Some(l);
    }
    app.audition = Some(voice.to_string());
    let dispatched = dispatch_op(
        app,
        job_tx,
        http,
        OpRequest {
            op: Op::PreviewVoice,
            character: Some(character.to_string()),
            voice: Some(voice.to_string()),
            text: Some(l.text.clone()),
            ..Default::default()
        },
    );
    if !dispatched {
        // The refusal already set a status. Clear the marker we just claimed, or
        // the screen would be wedged behind a render that never started.
        app.audition = None;
        return held.cloned();
    }
    app.set_status(
        Level::Info,
        format!(
            "rendering “{}” for “{character}” ({voice})…",
            bm_core::util::head_chars(&l.text, 48)
        ),
    );
    Some(l)
}

/// The sentence `t` tests: the locked one when this character has one, else
/// the held one when it is already theirs, else a fresh pick from the index.
/// `None` means there is nothing honest to play — the caller says so rather
/// than dispatching.
pub(crate) fn shown_line(
    app: &App,
    character: &str,
    current: Option<&AuditionLine>,
) -> Option<AuditionLine> {
    if let Some(l) = app.locked_lines.get(character) {
        return Some(l.clone());
    }
    match current {
        Some(l) if l.character == character => Some(l.clone()),
        _ => {
            let seed = bm_proto::now_secs();
            line_for(None, character, app.lines.as_ref(), seed)
        }
    }
}

/// Play one already-rendered segment for `voice` speaking exactly `text`: no
/// synthesis, just bytes.
///
/// Connected, those come from the inductor (`Op::Segment`); disconnected,
/// the same lookup runs against this checkout's files as a `Job::Segment` —
/// listening needs no backend. Either way a miss plays nothing and names the
/// render key instead — that is the whole point of the key.
pub(crate) fn segment(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    character: &str,
    voice: &str,
    text: &str,
) {
    if voice.trim().is_empty() {
        app.set_status(
            Level::Warn,
            format!("no voice to audition for “{character}”"),
        );
        return;
    }
    if let Some(v) = app.audition.clone() {
        app.set_status(
            Level::Warn,
            format!("{v} is still rendering — one audition at a time"),
        );
        return;
    }
    if !matches!(app.conn, Conn::Up) {
        if app.layout.root.as_os_str().is_empty() {
            app.set_status(
                Level::Warn,
                "inductor unreachable and no local checkout — :B to connect",
            );
            return;
        }
        // Same duplicate suppression the API path gets: one fetch at a time,
        // and the Done handler frees exactly this key.
        let key = op_key(&OpRequest {
            op: Op::Segment,
            ..Default::default()
        });
        if app.inflight.contains(&key) {
            app.set_status(
                Level::Warn,
                format!("{} is already running", Op::Segment.as_str()),
            );
            return;
        }
        app.inflight.push(key);
        app.audition = Some(voice.to_string());
        dispatch(
            app,
            job_tx,
            Job::Segment {
                layout: app.layout.clone(),
                character: character.to_string(),
                voice: voice.to_string(),
                text: text.to_string(),
            },
        );
        app.set_status(
            Level::Info,
            format!("testing {voice} on the shown line, locally…"),
        );
        return;
    }
    app.audition = Some(voice.to_string());
    let dispatched = dispatch_op(
        app,
        job_tx,
        http,
        OpRequest {
            op: Op::Segment,
            character: Some(character.to_string()),
            voice: Some(voice.to_string()),
            text: Some(text.to_string()),
            ..Default::default()
        },
    );
    if !dispatched {
        app.audition = None;
        return;
    }
    app.set_status(Level::Info, format!("testing {voice} on the shown line…"));
}

/// The voice currently assigned to `character`, if the roster knows one.
pub(crate) fn current_voice(app: &App, character: &str) -> Option<String> {
    app.roster
        .as_ref()?
        .cast
        .get(character)
        .filter(|v| !v.trim().is_empty())
        .cloned()
}

/// Which `:…` audition word is running: the held line on the current voice
/// (cache only), or on the pointed voice — the held line, or another one.
pub(crate) enum AuditionKind {
    Current,
    Pointed { reroll: bool },
}

/// Run a `:current` / `:try` / `:another` word from the command line: the
/// same three auditions the old `t` / `T` / `^T` keys ran, before those
/// letters were given back to the filter. Needs the voice list (picker
/// step 2) or the cast overview — anywhere else names the way there.
pub(crate) fn audition_word(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    kind: AuditionKind,
) {
    use crate::tui::screen::{PickStage, Screen};
    if let Screen::Pick(mut p) = app.screen.clone() {
        if p.stage != PickStage::Voice {
            app.set_status(
                Level::Info,
                "pick a character first (Enter) — :current / :try / :another need the voice list",
            );
            return;
        }
        match kind {
            AuditionKind::Current => pick_current(app, job_tx, http, &mut p),
            AuditionKind::Pointed { reroll } => pick_pointed(app, job_tx, http, &mut p, reroll),
        }
        app.screen = Screen::Pick(p);
    } else if let Screen::Cast(mut v) = app.screen.clone() {
        match kind {
            AuditionKind::Current => cast_current(app, job_tx, http, &mut v),
            AuditionKind::Pointed { reroll } => cast_pointed(app, job_tx, http, &mut v, reroll),
        }
        app.screen = Screen::Cast(v);
    } else {
        app.set_status(
            Level::Info,
            "audition needs the voice picker (:s) or the cast overview (S)",
        );
    }
}

/// `:current` on the picker: the current voice on the shown line, from
/// cache only — what the operator is about to replace, on the sentence in
/// front of them. The pointed voice is for `:try` (render) and Enter
/// (pick) — `:current` never follows the cursor. A miss names the render
/// word.
/// Shared with [`audition_word`]: the `:current` / `:try` / `:another`
/// words dispatch here too, so keys and words can never disagree.
pub(crate) fn pick_current(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    p: &mut crate::tui::screen::Picker,
) {
    match current_voice(app, &p.character) {
        None => app.set_status(
            Level::Warn,
            format!(
                "“{}” has no voice assigned yet — nothing to compare",
                p.character
            ),
        ),
        Some(cur) => match shown_line(app, &p.character, p.line.as_ref()) {
            None => app.set_status(Level::Warn, "no lines in the scripts yet — nothing to test"),
            Some(l) => {
                p.line = Some(l.clone());
                segment(app, job_tx, http, &p.character, &cur, &l.text);
            }
        },
    }
}

/// `:try` / `:another` on the picker: the held line — or another one —
/// rendered with the voice under the cursor. The one deliberate
/// generation: the only way to hear two voices on the same sentence
/// before either is assigned.
/// Shared with [`audition_word`]: the `:current` / `:try` / `:another`
/// words dispatch here too, so keys and words can never disagree.
pub(crate) fn pick_pointed(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    p: &mut crate::tui::screen::Picker,
    reroll: bool,
) {
    let list = crate::tui::model::filtered_voices(app, &p.filter);
    match list.get(p.cursor) {
        None => app.set_status(Level::Warn, "nothing to audition"),
        Some(v) => {
            let (character, voice) = (p.character.clone(), v.name.clone());
            p.line = audition(
                app,
                job_tx,
                http,
                &character,
                &voice,
                p.line.as_ref(),
                reroll,
            );
        }
    }
}

/// `:current` on the cast overview: the speaker's current voice on the
/// shown line, from cache only — never synthesis. A miss names the render
/// word instead of playing something nearby.
/// Shared with [`audition_word`]: the `:current` / `:try` / `:another`
/// words dispatch here too, so keys and words can never disagree.
pub(crate) fn cast_current(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    v: &mut crate::tui::screen::CastView,
) {
    let rows = app.cast_rows();
    let list = crate::tui::model::filtered_cast_rows(&rows, &v.filter);
    match list.get(v.cursor) {
        None => app.set_status(Level::Warn, "nothing to audition"),
        Some(row) if row.voice.is_empty() => app.set_status(
            Level::Warn,
            format!("“{}” has no voice assigned yet", row.character),
        ),
        Some(row) => match shown_line(app, &row.character, v.line.as_ref()) {
            None => app.set_status(Level::Warn, "no lines in the scripts yet — nothing to test"),
            Some(l) => {
                v.line = Some(l.clone());
                segment(app, job_tx, http, &row.character, &row.voice, &l.text);
            }
        },
    }
}

/// `:try` / `:another` on the cast overview: the held line — or another
/// one — rendered with the highlighted speaker's voice.
/// Shared with [`audition_word`]: the `:current` / `:try` / `:another`
/// words dispatch here too, so keys and words can never disagree.
pub(crate) fn cast_pointed(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    v: &mut crate::tui::screen::CastView,
    reroll: bool,
) {
    let rows = app.cast_rows();
    let list = crate::tui::model::filtered_cast_rows(&rows, &v.filter);
    match list.get(v.cursor) {
        None => app.set_status(Level::Warn, "nothing to audition"),
        Some(row) if row.voice.is_empty() => app.set_status(
            Level::Warn,
            format!("“{}” has no voice assigned yet", row.character),
        ),
        Some(row) => {
            let (character, voice) = (row.character.clone(), row.voice.clone());
            v.line = audition(
                app,
                job_tx,
                http,
                &character,
                &voice,
                v.line.as_ref(),
                reroll,
            );
        }
    }
}
