use super::*;
use crate::audio_pool::PoolKind;
use crate::audio_pool::Sound;
use anyhow::Result;
/// What a resolve did — or, in a dry run, would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every **direct** dependency folded in, in order, with the hash it was
    pub deps: Vec<DepRecord>,
    /// How many packs the closure held — the same count `deps` gives when the
    pub tree: usize,
    /// Dependencies whose tree has moved since the last resolve. Empty is the
    pub stale: Vec<String>,
    /// Entries and files removed because this resolve owned them.
    pub withdrawn: usize,
    /// Keys and files added (or overridden by a later dependency).
    pub added: usize,
    /// Entries an operator had edited, left alone and dropped from the record.
    pub adopted: usize,
    /// Layered files whose bytes a resolve would rewrite while the record stays
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
pub(crate) fn value_hash(sound: &Sound) -> String {
    profile::content_hash(serde_json::to_string(sound).unwrap_or_default().as_bytes())
}

/// A content hash of a whole dependency tree.
pub(crate) fn tree_hash(dir: &Path) -> Result<String> {
    let files = profile::files_under(dir, &[""]);
    Ok(profile::manifest_hash(&profile::hash_files(dir, files)?))
}

// ---------------------------------------------------------------------------

/// How one member of a layered file merges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// The file's own member wins whole; a dependency's is added only when the
    Whole,
    /// A map of name -> value: the file's names win, the dependency's missing
    ByKey,
    /// An ordered list: the file's entries stay first and each dependency's
    Concat,
}

/// A file whose members layer, and how each one merges.
pub(crate) struct Layered {
    pub(crate) file: &'static str,
    /// Members that do not take the `fallback` mode. A `_`-prefixed name is
    pub(crate) members: &'static [(&'static str, Mode)],
    pub(crate) fallback: Mode,
}

/// The layered files. Everything not named here is still copied whole, and a
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
