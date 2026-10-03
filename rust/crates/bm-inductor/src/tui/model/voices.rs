use super::*;

/// What kind of voice this is — the two halves the picker sorts into.
///
/// `AutoAssign` voices are sample-pool members: the cast rolls one out of
/// `voice-pool.json` by tag, so two characters on one is the pool working,
/// not a mistake. `Unique` voices are the catalogue presets and hand-added
/// clones, which a character keeps to itself unless someone says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VoiceKind {
    AutoAssign,
    Unique,
}

/// The group heading a pooled voice is filed under: its tags, sorted and
/// deduped, so `young, female` and `female, young` are one group rather than
/// two.
fn tag_group(tags: &[String]) -> String {
    let mut t: Vec<&str> = tags.iter().map(String::as_str).collect();
    t.sort_unstable();
    t.dedup();
    t.join(" + ")
}

/// One line of the picker's voice list.
///
/// A `Group` is a heading and not a choice: arrows step over it and Enter
/// ignores it, so the list is a *row* list again and the cursor arithmetic
/// that scrolls and hit-tests it stays honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceRow {
    Group {
        kind: VoiceKind,
        /// The pool tags this group is `AutoAssign` voices of; empty for
        /// `Unique`, which is one group by definition.
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
///
/// A heading is a label, not a choice, so a cursor sitting on one moves down
/// to the first voice under it. Every read of "the pointed voice" goes through
/// this, which is why a cursor can never audition a heading.
pub(crate) fn settle_cursor(rows: &[VoiceRow], cursor: usize) -> usize {
    if !matches!(rows.get(cursor), Some(VoiceRow::Group { .. })) {
        return cursor;
    }
    (cursor + 1..rows.len())
        .find(|i| rows[*i].voice().is_some())
        .unwrap_or(cursor)
}

/// The row an *upward* move really lands on.
///
/// [`settle_cursor`] walks forward, which is right for a cursor arriving from
/// above — opening the screen, a click, a filter change. Up is the one motion
/// that arrives from *below*, and settling it forward puts the cursor straight
/// back on the voice it just left: from the first voice of a group, Up would
/// step onto the heading and then straight back down, which reads as a broken
/// key. So an upward move steps back over the heading to the previous group's
/// last voice instead. At the very top there is nothing above — the forward
/// settle then means "the first voice", not "sit on the heading".
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
///
/// Auto-assign voices first, one group per tag set, the groups alphabetical
/// (`female + old` before `male + young`); inside a group the least-used
/// sample first, because the point of a pool is to spend its emptiest voices
/// first and any other order sends the operator back to the voice they just
/// filled. Unique voices after, by name — nothing to balance there.
///
/// Pure, so the ordering is testable without a terminal.
pub(crate) fn filtered_voices(app: &App, filter: &str) -> Vec<VoiceRow> {
    let Some(r) = &app.roster else {
        return Vec::new();
    };
    let mut pooled: BTreeMap<String, Vec<(Vec<String>, VoiceInfo)>> = BTreeMap::new();
    let mut unique: Vec<VoiceInfo> = Vec::new();
    for v in r.voices.iter().filter(|v| {
        matches(filter, &v.name)
            || matches(filter, &v.gender)
            || matches(filter, &v.accent)
            || matches(filter, &v.style)
    }) {
        if v.pool_tags.is_empty() {
            unique.push(v.clone());
        } else {
            let users = users_of(&r.cast, &v.name);
            pooled
                .entry(tag_group(&v.pool_tags))
                .or_default()
                .push((users, v.clone()));
        }
    }
    // Folded, so a roster of accented names sorts the way it is read rather
    // than by code point.
    unique.sort_by(|a, b| {
        fold(&a.name)
            .cmp(&fold(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    let unique_count = unique.len();
    let mut out: Vec<VoiceRow> = Vec::new();
    for (tags, mut voices) in pooled {
        voices.sort_by(|a, b| {
            a.0.len()
                .cmp(&b.0.len())
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        out.push(VoiceRow::Group {
            kind: VoiceKind::AutoAssign,
            tags,
            count: voices.len(),
        });
        out.extend(
            voices
                .into_iter()
                .map(|(users, voice)| VoiceRow::Voice { voice, users }),
        );
    }
    if unique_count > 0 {
        out.push(VoiceRow::Group {
            kind: VoiceKind::Unique,
            tags: String::new(),
            count: unique_count,
        });
        out.extend(unique.into_iter().map(|voice| VoiceRow::Voice {
            users: users_of(&r.cast, &voice.name),
            voice,
        }));
    }
    out
}

/// Who is already speaking with a voice, in the one cell the list has room
/// for.
///
/// The character the swap is for comes first when it is among the users —
/// that is the voice it already has, and the reason its row is green — and
/// the rest collapse to a count. A voice eleven characters share is worth one
/// glance; eleven names are not, and the list they pushed off the screen was
/// the column saying it.
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
///
/// A pooled sample's roster gender is `unknown`: the registry describes it by
/// tag, not in the SDK's vocabulary. Reading the tag for what it plainly says
/// keeps the row from printing `—` two lines under a heading that just said
/// `female + young`.
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
