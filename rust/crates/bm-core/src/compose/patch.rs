use super::*;
use crate::audio_pool::PoolKind;
use crate::audio_pool::Sound;
use anyhow::Result;
/// What a resolve did — or, in a dry run, would do.
/// What a resolve did — or, in a dry run, would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every **direct** dependency folded in, in order, with the hash it was
    /// folded in at.
    pub deps: Vec<DepRecord>,
    /// How many packs the closure held — the same count `deps` gives when the
    /// graph is flat, and more when a dependency has dependencies.
    pub tree: usize,
    /// Dependencies whose tree has moved since the last resolve. Empty is the
    /// steady state; non-empty is the child being stale, which is a
    /// comparison and never a guess.
    pub stale: Vec<String>,
    /// Entries and files removed because this resolve owned them.
    pub withdrawn: usize,
    /// Keys and files added (or overridden by a later dependency).
    pub added: usize,
    /// Entries an operator had edited, left alone and dropped from the record.
    pub adopted: usize,
    /// Layered files whose bytes a resolve would rewrite while the record stays
    /// the same — a list that gained an entry the accounting cannot name. The
    /// counters describe the records; this describes the text.
    pub rewritten: usize,
    pub dry_run: bool,
}

impl Report {
    /// Whether the tree would change. A dry run that reports this must write.
    pub fn changed(&self) -> bool {
        self.withdrawn > 0 || self.added > 0 || self.rewritten > 0
    }

    /// One line for the operator, in the order the questions are asked.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{} dep(s): {} withdrawn, {} filled in",
            self.deps.len(),
            self.withdrawn,
            self.added
        )];
        if self.tree > self.deps.len() {
            parts.push(format!("{} packs in the tree", self.tree));
        }
        if self.adopted > 0 {
            parts.push(format!("{} kept (edited here)", self.adopted));
        }
        if self.rewritten > 0 {
            parts.push(format!("{} rewritten", self.rewritten));
        }
        if !self.stale.is_empty() {
            parts.push(format!("STALE: {} moved", self.stale.join(", ")));
        }
        let verb = if self.dry_run { "would be" } else { "is" };
        format!("{} — {verb} {}", parts.join("; "), self.verb_nothing())
    }

    fn verb_nothing(&self) -> &'static str {
        if self.changed() {
            "changed"
        } else {
            "up to date"
        }
    }
}

/// Which registry a filename is, if it is one.
pub(crate) fn kind_of(registry: &str) -> Option<PoolKind> {
    PoolKind::ALL.into_iter().find(|k| k.registry() == registry)
}

/// A hash of one registry value, so a resolve can tell an entry it inserted
/// from the same key the operator has since edited.
pub(crate) fn value_hash(sound: &Sound) -> String {
    profile::content_hash(serde_json::to_string(sound).unwrap_or_default().as_bytes())
}

/// A content hash of a whole dependency tree.
/// `pub(crate)` because the release side (`profile::compute_dep_manifest`) pins
/// its manifest hash against this one: a dependency release and the record a
/// child's resolve carries must never be able to disagree.
pub(crate) fn tree_hash(dir: &Path) -> Result<String> {
    let files = profile::files_under(dir, &[""]);
    Ok(profile::manifest_hash(&profile::hash_files(dir, files)?))
}

// ---------------------------------------------------------------------------
// Layered files
//
// The pools merge by key. Every other file used to be all-or-nothing, which put
// a ceiling on what a root asset could be: `common` is the world — rain, night,
// a market, and the rules that score them — and a genre that shipped its own
// `scene-map.json` replaced the world's outright, so the world's rules had to be
// copied into every genre and a level fixed in one reached none of the others.
//
// These files layer now, member by member, and the merge is by **raw text**: an
// inherited rule arrives with the dependency's own bytes, and every member the
// file already had keeps its own. Nothing is re-serialised, so no `_note` is
// reflowed and no key order is lost — the argument `save_pool` is built on, one
// level down.

/// How one member of a layered file merges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// The file's own member wins whole; a dependency's is added only when the
    /// file has none, and a stronger dependency's replaces a weaker one's. The
    /// default, and the only mode a `_note` ever gets: a note is prose about
    /// *this* file, and half-inheriting one is a sentence about something else.
    Whole,
    /// A map of name -> value: the file's names win, the dependency's missing
    /// ones are added, a later dependency overrides an earlier one. The pool
    /// rule, applied to a member instead of a whole file.
    ByKey,
    /// An ordered list: the file's entries stay first and each dependency's
    /// follow, in `deps` order. Order is the whole point — the scene map's
    /// rules are ordered specific-to-general and the first match wins, so an
    /// inherited rule may only ever sit *behind* the file's own. It is also
    /// what makes an override work: a genre that restates a world rule is seen
    /// first, and the world's copy never runs.
    Concat,
}

/// A file whose members layer, and how each one merges.
pub(crate) struct Layered {
    pub(crate) file: &'static str,
    /// Members that do not take the `fallback` mode. A `_`-prefixed name is
    /// always [`Mode::Whole`] whatever this says.
    pub(crate) members: &'static [(&'static str, Mode)],
    pub(crate) fallback: Mode,
}

/// The layered files. Everything not named here is still copied whole, and a
/// file that is one of these never appears in the marker's `files`.
pub(crate) const LAYERED: &[Layered] = &[
    Layered {
        file: "scene-map.json",
        members: &[
            ("rules", Mode::Concat),
            ("music_palette", Mode::ByKey),
            ("reverb_presets", Mode::ByKey),
        ],
        fallback: Mode::Whole,
    },
    Layered {
        file: "tag-aliases.json",
        members: &[],
        fallback: Mode::ByKey,
    },
    Layered {
        file: "LICENSES.json",
        members: &[],
        fallback: Mode::ByKey,
    },
];

pub(crate) fn layered(file: &str) -> Option<&'static Layered> {
    LAYERED.iter().find(|l| l.file == file)
}

impl Layered {
    pub(crate) fn mode(&self, member: &str) -> Mode {
        if member.starts_with('_') {
            return Mode::Whole;
        }
        self.members
            .iter()
            .find(|(name, _)| *name == member)
            .map(|(_, mode)| *mode)
            .unwrap_or(self.fallback)
    }
}

/// Escape a name so the record separators cannot appear inside one.
///
/// Load-bearing, not tidiness: a member name is arbitrary text and the names
/// here are prose — `LICENSES.json`'s categories are literally `sound effects
/// (effects/, injects/)` — so without this a record could be read as a
/// different kind of record and a withdrawal would silently drop it. `%` is
/// escaped first, or the escapes themselves would not round-trip.
fn escape(part: &str) -> String {
    part.replace('%', "%25")
        .replace('/', "%2F")
        .replace('+', "%2B")
}

pub(crate) fn unescape(part: &str) -> String {
    part.replace("%2B", "+")
        .replace("%2F", "/")
        .replace("%25", "%")
}

/// The marker key for a whole member's value.
pub(crate) fn whole_key(member: &str) -> String {
    escape(member)
}

/// The marker key for one key of a keyed member.
pub(crate) fn keyed_key(member: &str, key: &str) -> String {
    format!("{}/{}", escape(member), escape(key))
}

/// The marker key for the entries one dependency appended to a list.
pub(crate) fn concat_key(member: &str, dep: &str) -> String {
    format!("{}+{}", escape(member), escape(dep))
}

/// A list record is `<count>:<hash>`, because a list cannot be withdrawn by
/// value alone: the record has to say how many entries to drop.
pub(crate) fn list_record(count: usize, hash: &str) -> String {
    format!("{count}:{hash}")
}

pub(crate) fn parse_list_record(record: &str) -> Option<(usize, &str)> {
    let (count, hash) = record.split_once(':')?;
    Some((count.parse().ok()?, hash))
}

pub(crate) fn text_hash(text: &str) -> String {
    profile::content_hash(text.as_bytes())
}

/// `"name": value`, the shape a member has in the file.
pub(crate) fn member_text(member: &str, value: &str) -> String {
    format!(
        "{}: {value}",
        serde_json::to_string(member).unwrap_or_else(|_| format!("\"{member}\""))
    )
}

/// One inserted run, framed the way the file frames its own members.
pub(crate) fn piece(entry_indent: &str, close_indent: &str, text: &str) -> String {
    format!("\n{entry_indent}{text}\n{close_indent}")
}

/// Append a rendered run before the closing bracket of a JSON object or array,
/// keeping every byte that is already there.
///
/// Two JSON files written by the same hands indent the same way, which is what
/// makes inserting a dependency's own text produce a correctly laid-out result
/// rather than a guess; and the container's own closing indent is restored from
/// the `pieces`, so a member inserted into a file whose last entry was on its
/// own line still ends on one.
pub(crate) fn append_inside(value: &str, pieces: &str) -> Option<String> {
    let trimmed = value.trim_end();
    let close = trimmed.chars().last()?;
    let open = match close {
        ']' => '[',
        '}' => '{',
        _ => return None,
    };
    let inner = trimmed.get(open.len_utf8()..trimmed.len() - close.len_utf8())?;
    let mut out = String::with_capacity(value.len() + pieces.len() + 2);
    out.push(open);
    if !inner.trim().is_empty() {
        out.push_str(inner.trim_end());
        out.push(',');
    }
    out.push_str(pieces);
    out.push(close);
    Some(out)
}

/// Replace one member's value, keeping every other byte of the file.
pub(crate) fn replace_member(text: &str, member: &str, value: &str) -> Option<String> {
    let entries = crate::audio_pool::scan_entries(text)?;
    let e = entries.iter().find(|e| e.key == member)?;
    Some(format!(
        "{}{}{}",
        &text[..e.value_start],
        value,
        &text[e.value_end..]
    ))
}

/// Drop one member, and its separator, keeping every other byte.
pub(crate) fn remove_member(text: &str, member: &str) -> Option<String> {
    let entries = crate::audio_pool::scan_entries(text)?;
    let at = entries.iter().position(|e| e.key == member)?;
    let start = if at == 0 {
        text.find('{')? + 1
    } else {
        entries[at - 1].value_end
    };
    // Removing the first member has no separator before it to take with it, so
    // it takes the one after it instead — or the file would open with a comma.
    let end = if at == 0 {
        match entries.get(1) {
            Some(next) => text[entries[0].value_end..next.value_start]
                .find(',')
                .map(|o| entries[0].value_end + o + 1)
                .unwrap_or(entries[0].value_end),
            None => entries[0].value_end,
        }
    } else {
        entries[at].value_end
    };
    Some(format!("{}{}", &text[..start], &text[end..]))
}

/// The raw text of a run of JSON values, joined — what a list record hashes, so
/// a comparison is about the entries and not about their layout.
pub(crate) fn entries_text(text: &str) -> Option<String> {
    let spans = crate::audio_pool::scan_array(text)?;
    Some(
        spans
            .iter()
            .map(|(a, b)| &text[*a..*b])
            .collect::<Vec<_>>()
            .join(","),
    )
}
