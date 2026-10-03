//! Auditioning a voice: play it, never commit to it.

use crate::tui::{
    app::App,
    audition::{line_for, AuditionLine},
    input::{dispatch, dispatch_op, op_key},
    jobs::Job,
    style::{Conn, Level},
};
use bm_proto::{Op, OpRequest};

/// Render and play `voice` speaking the held line — or a fresh line for
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
pub(crate) enum AuditionKind {
    Current,
    Pointed { reroll: bool },
}

/// Run a `:current` / `:try` / `:another` word from the command line: the
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
pub(crate) fn pick_pointed(
    app: &mut App,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    http: &reqwest::Client,
    p: &mut crate::tui::screen::Picker,
    reroll: bool,
) {
    let list = crate::tui::model::filtered_voices(app, &p.filter);
    // Settled, because a group heading is a row and this is the one read of
    let pointed = crate::tui::model::settle_cursor(&list, p.cursor);
    p.cursor = pointed;
    match list.get(pointed).and_then(|r| r.voice()) {
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
