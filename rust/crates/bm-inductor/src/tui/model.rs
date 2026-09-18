//! Pure selection: filters, cast rows, task queries. No widgets, no keys.
use crate::tui::{app::App, style::style_of};
use bm_proto::{Heartbeat, Machine, Roster, Task, TaskState, VoiceInfo};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::BTreeMap;

/// Fold Vietnamese diacritics to ASCII so a filter of `thai son` matches
/// `Thái Sơn`. Without it, filtering a Vietnamese cast means typing exact
/// diacritics on every keystroke. The table lives in `bm_core::util` (shared
/// with the segment-voice matcher); this re-export keeps call sites unchanged.
pub(crate) use bm_core::util::fold;

pub(crate) fn matches(filter: &str, haystack: &str) -> bool {
    let f = fold(filter.trim());
    f.is_empty() || fold(haystack).contains(&f)
}

/// Registry as persisted on disk: connection config joined with runtime, no
/// liveness. The fallback behind `effective_machines` when the inductor is
/// unreachable. Reads both shapes: the current `machine_state` + machines.json
/// join, and the pre-migration `machines` array.
pub(crate) fn registry_machines(layout_root: &std::path::Path) -> Vec<Machine> {
    if layout_root.as_os_str().is_empty() {
        return Vec::new();
    }
    let bm = layout_root.join(".bm");
    let text = match std::fs::read_to_string(bm.join("ledger.json")) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let doc: serde_json::Value = match serde_json::from_str(&text) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if let Some(a) = doc.get("machines").and_then(|m| m.as_array()) {
        let mut out: Vec<Machine> = a
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        out.sort_by(|a, b| a.addr.cmp(&b.addr));
        return out;
    }
    let boxes = bm_core::provision::load_boxes(&bm.join("machines.json"));
    let empty = serde_json::Map::new();
    let rt = doc
        .get("machine_state")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    bm_core::provision::join_all(boxes, rt)
}

pub(crate) fn filtered_characters(app: &App, filter: &str) -> Vec<String> {
    match &app.roster {
        None => Vec::new(),
        Some(r) => r
            .characters
            .iter()
            .filter(|c| matches(filter, c))
            .cloned()
            .collect(),
    }
}

pub(crate) fn filtered_voices(app: &App, filter: &str) -> Vec<VoiceInfo> {
    match &app.roster {
        None => Vec::new(),
        Some(r) => r
            .voices
            .iter()
            .filter(|v| {
                matches(filter, &v.name)
                    || matches(filter, &v.gender)
                    || matches(filter, &v.accent)
                    || matches(filter, &v.style)
            })
            .cloned()
            .collect(),
    }
}

/// Characters currently speaking with `voice`.
pub(crate) fn users_of(cast: &BTreeMap<String, String>, voice: &str) -> Vec<String> {
    cast.iter()
        .filter(|(_, v)| v.as_str() == voice)
        .map(|(k, _)| k.clone())
        .collect()
}

/// How one assignment sits against the accent policy.
///
/// `Blocked` and `Unknown` are deliberately distinct. A voice the roster lists
/// and the policy rejects is a *decision*; a voice the roster has never heard
/// of means the cast is stale, or the roster fell back to the offline table
/// because the sidecar is down. Reporting the second as the first would send
/// an operator hunting for a policy problem that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Assignable — the policy permits it, or it is an enrolled clone.
    Ok,
    /// The roster lists it and the accent policy rejects it.
    Blocked,
    /// Not in the roster at all.
    Unknown,
    /// No assignment yet.
    Unassigned,
}

/// One speaker's line in the cast overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CastRow {
    pub(crate) character: String,
    /// Empty when the speaker has no assignment yet.
    pub(crate) voice: String,
    pub(crate) gender: String,
    pub(crate) accent: String,
    /// The voice's style, used only as a filter key — the picker is where the
    /// catalogue is read, and the table has no room for a seventh column.
    pub(crate) style: String,
    /// The roster lists this voice at all.
    pub(crate) in_roster: bool,
    pub(crate) allowed: bool,
    pub(crate) enrolled: bool,
    /// Other speakers sharing this voice, sorted. Never counts unassigned
    /// speakers as sharing the empty voice.
    pub(crate) shared_with: Vec<String>,
}

impl CastRow {
    pub(crate) fn unassigned(&self) -> bool {
        self.voice.is_empty()
    }

    pub(crate) fn verdict(&self) -> Verdict {
        if self.voice.is_empty() {
            Verdict::Unassigned
        } else if self.allowed || self.enrolled {
            Verdict::Ok
        } else if self.in_roster {
            Verdict::Blocked
        } else {
            Verdict::Unknown
        }
    }

    /// A voice carrying more than one character is not an error — `Adam` alone
    /// voices eleven speakers here — but it is the thing worth noticing.
    pub(crate) fn shared(&self) -> bool {
        !self.shared_with.is_empty()
    }
}

/// Flatten a roster into one row per speaker.
///
/// Pure, so the duplicate and policy logic is testable without a terminal.
pub(crate) fn cast_rows(roster: &Roster) -> Vec<CastRow> {
    let meta: BTreeMap<&str, &VoiceInfo> =
        roster.voices.iter().map(|v| (v.name.as_str(), v)).collect();

    // The two sets are not always equal: a cast file can name a speaker no
    // script mentions any more, and `characters` can name one with no
    // assignment. The overview must show both.
    let mut names: Vec<String> = roster.characters.clone();
    for k in roster.cast.keys() {
        if !names.iter().any(|n| n == k) {
            names.push(k.clone());
        }
    }
    // `Narrator` is the one speaker whose position carries meaning — it is the
    // fallback voice — so it leads; everything else is alphabetical.
    names.sort_by_key(|n| (n != "Narrator", n.clone()));

    names
        .iter()
        .map(|name| {
            let voice = roster.cast.get(name).cloned().unwrap_or_default();
            let v = meta.get(voice.as_str()).copied();
            let shared_with = if voice.is_empty() {
                Vec::new()
            } else {
                let mut s: Vec<String> = roster
                    .cast
                    .iter()
                    .filter(|(k, val)| val.as_str() == voice && k.as_str() != name.as_str())
                    .map(|(k, _)| k.clone())
                    .collect();
                s.sort();
                s
            };
            CastRow {
                character: name.clone(),
                voice,
                gender: v.map(|x| x.gender.clone()).unwrap_or_default(),
                accent: v.map(|x| x.accent.clone()).unwrap_or_default(),
                style: v.map(|x| x.style.clone()).unwrap_or_default(),
                in_roster: v.is_some(),
                allowed: v.map(|x| x.allowed).unwrap_or(false),
                enrolled: v.map(|x| x.enrolled).unwrap_or(false),
                shared_with,
            }
        })
        .collect()
}

/// Diacritic-insensitive filter over the speaker, their voice, and that voice's
/// style, so `duc tri` finds everyone voiced by `Đức Trí` and `tin tuc` finds
/// every newsreader.
pub(crate) fn filtered_cast_rows(rows: &[CastRow], filter: &str) -> Vec<CastRow> {
    if filter.trim().is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| {
            matches(filter, &r.character) || matches(filter, &r.voice) || matches(filter, &r.style)
        })
        .cloned()
        .collect()
}

pub(crate) fn clamp_scroll(cursor: usize, scroll: &mut usize, len: usize, height: usize) {
    if height == 0 {
        *scroll = 0;
        return;
    }
    // Lookahead: keep two rows visible below the cursor when the list is long
    // enough, so the highlight never sits on the last visible row while more
    // items hide beneath it. Short windows shrink the padding instead of
    // fighting them — at height 1 this is exactly the old rule.
    let pad = 2.min(height.saturating_sub(1));
    if cursor < *scroll {
        *scroll = cursor;
    } else if cursor + pad >= *scroll + height {
        *scroll = cursor + 1 + pad - height;
    }
    let max = len.saturating_sub(height);
    if *scroll > max {
        *scroll = max;
    }
}

/// Tasks matching the filter, in the ledger's own order (chapter, then stage).
///
/// A filter term matches if it appears in the task id (`digest:3`), the stage
/// name, the state name, or the chapter number. Substring rather than prefix, so
/// `shel`, `shelv` and `shelved` all work; the hint line says so, because a
/// filter nobody can predict is a filter nobody uses.
pub(crate) fn filtered_tasks<'a>(tasks: &'a [Task], filter: &str) -> Vec<&'a Task> {
    let f = filter.trim().to_lowercase();
    tasks
        .iter()
        .filter(|t| {
            if f.is_empty() {
                return true;
            }
            let id = t.id().to_lowercase();
            let chapter = t.chapter.to_string();
            id.contains(&f)
                || t.stage.as_str().contains(&f)
                || t.state.as_str().contains(&f)
                || chapter.contains(&f)
        })
        .collect()
}

/// Beats inside the liveness window (90s — the same window the reaper and the
/// ETA use). The panes hide the rest: a dead worker rendered as an idle row
/// is how two hares happen.
pub(crate) fn live_beats(beats: &[Heartbeat], now: u64) -> Vec<&Heartbeat> {
    beats
        .iter()
        .filter(|b| now.saturating_sub(b.ts) < 90)
        .collect()
}

/// A worker's self-reported display name, when any beat carries one for this
/// id. The name is drawn once at worker startup and kept in `worker.alias`;
/// the panes show it verbatim so one worker never wears two names on one
/// screen. `None` means fall back to hashing the id (older agents).
pub(crate) fn reported_alias<'a>(beats: &'a [Heartbeat], id: &str) -> Option<&'a str> {
    beats
        .iter()
        .find(|b| b.worker_id == id && !b.alias.is_empty())
        .map(|b| b.alias.as_str())
}

/// Live workers on one box, by the addr their heartbeats carry. Powers the
/// Machines pane's `workers` column — the answer to "is this box actually
/// doing anything".
pub(crate) fn live_workers(beats: &[Heartbeat], addr: &str, now: u64) -> usize {
    live_beats(beats, now)
        .iter()
        .filter(|b| b.addr == addr)
        .count()
}

/// Display name for a beat's box: the registry handle the provision log
/// used (`hawk`), so the workers pane agrees with the events pane. Falls
/// back to the reported OS hostname, then the address — a box the registry
/// never saw still renders something true.
pub(crate) fn machine_name<'a>(machines: &'a [Machine], beat: &'a Heartbeat) -> &'a str {
    machines
        .iter()
        .find(|m| m.addr == beat.addr)
        .map(|m| {
            if m.name.is_empty() {
                if beat.hostname.is_empty() {
                    beat.addr.as_str()
                } else {
                    beat.hostname.as_str()
                }
            } else {
                m.name.as_str()
            }
        })
        .unwrap_or_else(|| {
            if beat.hostname.is_empty() {
                beat.addr.as_str()
            } else {
                beat.hostname.as_str()
            }
        })
}

/// `(state, count)` in `TaskState::ALL` order, zeroes skipped.
pub(crate) fn task_state_counts(tasks: &[Task]) -> Vec<(TaskState, usize)> {
    TaskState::ALL
        .into_iter()
        .map(|s| (s, tasks.iter().filter(|t| t.state == s).count()))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// How long ago a task last changed, in seconds. The ledger stores epoch seconds.
pub(crate) fn age_secs(updated: u64) -> u64 {
    bm_proto::now_secs().saturating_sub(updated)
}

/// One-line task roll-up, rendered in the footer when the terminal is too short
/// for the Tasks pane. Collapsing the pane must not lose the numbers.
pub(crate) fn task_rollup(counts: &serde_json::Value, colour: bool) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let Some(obj) = counts.as_object() else {
        return Line::from(Span::styled("tasks: waiting for the inductor…", dim));
    };
    if obj.is_empty() {
        return Line::from(Span::styled(
            "tasks: none queued — press t to enqueue a range",
            dim,
        ));
    }
    let (mut done, mut total, mut failed, mut shelved) = (0u64, 0u64, 0u64, 0u64);
    for c in obj.values() {
        let get = |k: &str| c.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        done += get("done");
        failed += get("failed");
        shelved += get("shelved");
        total += c
            .as_object()
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>())
            .unwrap_or(0);
    }
    let open = total.saturating_sub(done).saturating_sub(shelved);
    let mut spans = vec![
        Span::styled("tasks: ", dim),
        Span::styled(
            format!("{done}/{total} done"),
            style_of(colour, Color::Green),
        ),
        Span::styled(format!("  · {open} open"), dim),
    ];
    if failed > 0 {
        spans.push(Span::styled(
            format!("  · {failed} failed"),
            style_of(colour, Color::Yellow),
        ));
    }
    if shelved > 0 {
        spans.push(Span::styled(
            format!("  · {shelved} shelved"),
            style_of(colour, Color::Red),
        ));
    }
    spans.push(Span::styled("   · resize for per-stage detail", dim));
    Line::from(spans)
}
