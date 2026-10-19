use super::*;

/// The group heading a voice is filed under: its tags, sorted and
fn tag_group(tags: &[String]) -> String {
    let mut t: Vec<&str> = tags.iter().map(String::as_str).collect();
    t.sort_unstable();
    t.dedup();
    t.join(" + ")
}

/// One line of the picker's voice list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceRow {
    Group {
        /// The pool tags this group's voices share; empty for a named voice
        /// that only manual assignment reaches.
        tags: String,
        /// Voices in the group, so a heading says how much is under it.
        count: usize,
    },
    Voice {
        voice: VoiceInfo,
        /// Characters already speaking with it, alphabetical.
        users: Vec<String>,
    },
}

impl VoiceRow {
    /// The voice this row points at, if it points at one.
    pub(crate) fn voice(&self) -> Option<&VoiceInfo> {
        match self {
            VoiceRow::Voice { voice, .. } => Some(voice),
            VoiceRow::Group { .. } => None,
        }
    }
}

/// The row a cursor should really mean.
pub(crate) fn settle_cursor(rows: &[VoiceRow], cursor: usize) -> usize {
    if !matches!(rows.get(cursor), Some(VoiceRow::Group { .. })) {
        return cursor;
    }
    (cursor + 1..rows.len())
        .find(|i| rows[*i].voice().is_some())
        .unwrap_or(cursor)
}

/// The row an *upward* move really lands on.
pub(crate) fn settle_cursor_back(rows: &[VoiceRow], cursor: usize) -> usize {
    if !matches!(rows.get(cursor), Some(VoiceRow::Group { .. })) {
        return cursor;
    }
    match (0..cursor).rev().find(|i| rows[*i].voice().is_some()) {
        Some(i) => i,
        None => settle_cursor(rows, cursor),
    }
}

/// The picker's voice list, grouped and ordered the way it is chosen from.
pub(crate) fn filtered_voices(app: &App, filter: &str) -> Vec<VoiceRow> {
    let Some(r) = &app.roster else {
        return Vec::new();
    };
    let mut groups: BTreeMap<String, Vec<(Vec<String>, VoiceInfo)>> = BTreeMap::new();
    for v in r.voices.iter().filter(|v| {
        matches(filter, &v.name)
            || matches(filter, &v.gender)
            || matches(filter, &v.accent)
            || matches(filter, &v.style)
    }) {
        let users = users_of(&r.cast, &v.name);
        groups
            .entry(tag_group(&v.pool_tags))
            .or_default()
            .push((users, v.clone()));
    }
    // Tagged groups first, alphabetical; the untagged voices — manual
    // assignment only — last, folded by name so they read as sorted.
    let mut keys: Vec<String> = groups.keys().cloned().collect();
    keys.sort_by_key(|k| (k.is_empty(), k.clone()));
    let mut out: Vec<VoiceRow> = Vec::new();
    for tags in keys {
        let Some(mut voices) = groups.remove(&tags) else {
            continue;
        };
        if tags.is_empty() {
            voices.sort_by(|a, b| {
                fold(&a.1.name)
                    .cmp(&fold(&b.1.name))
                    .then_with(|| a.1.name.cmp(&b.1.name))
            });
        } else {
            voices.sort_by(|a, b| {
                a.0.len()
                    .cmp(&b.0.len())
                    .then_with(|| a.1.name.cmp(&b.1.name))
            });
        }
        out.push(VoiceRow::Group {
            tags,
            count: voices.len(),
        });
        out.extend(
            voices
                .into_iter()
                .map(|(users, voice)| VoiceRow::Voice { voice, users }),
        );
    }
    out
}

/// Who is already speaking with a voice, in the one cell the list has room
pub(crate) fn used_by(users: &[String], character: &str) -> String {
    let Some(first) = users
        .iter()
        .find(|u| u.as_str() == character)
        .or_else(|| users.first())
    else {
        return String::new();
    };
    match users.len() {
        1 => first.clone(),
        n => format!("{}, +{}", first, n - 1),
    }
}

/// What the gender column says.
pub(crate) fn gender_of(v: &VoiceInfo) -> &str {
    if !matches!(v.gender.as_str(), "" | "unknown") {
        return gender_label(&v.gender);
    }
    // `female` first, as everywhere else: `female` contains `male`.
    for tag in ["female", "male", "neutral"] {
        if v.pool_tags.iter().any(|t| t == tag) {
            return tag;
        }
    }
    gender_label(&v.gender)
}

/// Characters currently speaking with `voice`.
pub(crate) fn users_of(cast: &BTreeMap<String, String>, voice: &str) -> Vec<String> {
    cast.iter()
        .filter(|(_, v)| v.as_str() == voice)
        .map(|(k, _)| k.clone())
        .collect()
}
