use super::*;

/// How one assignment sits against the roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Assignable — the roster lists it, or it is an enrolled clone.
    Ok,
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
    pub(crate) style: String,
    /// The roster lists this voice at all.
    pub(crate) in_roster: bool,
    pub(crate) enrolled: bool,
    /// Other speakers sharing this voice, sorted. Never counts unassigned
    pub(crate) shared_with: Vec<String>,
}

impl CastRow {
    pub(crate) fn unassigned(&self) -> bool {
        self.voice.is_empty()
    }

    pub(crate) fn verdict(&self) -> Verdict {
        if self.voice.is_empty() {
            Verdict::Unassigned
        } else if self.in_roster || self.enrolled {
            Verdict::Ok
        } else {
            Verdict::Unknown
        }
    }

    /// A voice carrying more than one character is not an error — `Adam` alone
    pub(crate) fn shared(&self) -> bool {
        !self.shared_with.is_empty()
    }
}

/// Flatten a roster into one row per speaker.
pub(crate) fn cast_rows(roster: &Roster) -> Vec<CastRow> {
    let meta: BTreeMap<&str, &VoiceInfo> =
        roster.voices.iter().map(|v| (v.name.as_str(), v)).collect();

    // The two sets are not always equal: a cast file can name a speaker no
    let mut names: Vec<String> = roster.characters.clone();
    for k in roster.cast.keys() {
        if !names.iter().any(|n| n == k) {
            names.push(k.clone());
        }
    }
    // `Narrator` is the one speaker whose position carries meaning — it is the
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
                enrolled: v.map(|x| x.enrolled).unwrap_or(false),
                shared_with,
            }
        })
        .collect()
}

/// Diacritic-insensitive filter over the speaker, their voice, and that voice's
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
