//! Asset composition: a genre's art, built from the assets it depends on.
//!
//! A pack used to be one flat tree, so a sound every genre wants — a door slam,
//! wind, a body hitting the floor — had to exist once per genre, and a
//! correction had to be made once per copy. An asset now names its
//! dependencies in `assets/pack.json`:
//!
//! ```json
//! { "deps": ["common", "xianxia-base"] }
//! ```
//!
//! and this module folds them into the live `assets/` tree, **weakest first**,
//! so `xianxia-base` overrides `common` on a shared key and the asset's own
//! entries override both. A single-parent chain is the one-element case; the
//! list is ordered rather than a set so the precedence is written down rather
//! than inferred.
//!
//! **The dependencies are unpacked, not referenced.** `assets/_extends/<name>/`
//! holds each one's tree — an unpacked release, put there by whoever cut it —
//! so the whole composition lives inside `assets/`, which is the tree the
//! binding hashes and the tree provisioning ships. Nothing resolves through a
//! reference at run time: a reader sees one tree, exactly as it always did.
//!
//! **Composition is fill-in, and whole entry by key.** A sound is a key in a
//! pool registry, so a dependency's `wind` is inherited only if the asset ships
//! no `wind` — tags, files, `mode`, `hold` and `level` together. Fields are not
//! merged: `serde`'s defaults cannot tell "unset" from "set to the default", and
//! a half-inherited entry is a clip whose `mode` came from a sound it is not.
//! Everything that is not a pool registry (a clip, `scene-map.json`,
//! `tag-aliases.json`) is inherited per file, on the same missing-wins rule.
//!
//! **And the resolution remembers what it did.** Fill-in alone is not enough to
//! be regenerable: without a record, a second resolve cannot tell an entry *it*
//! inserted from an entry the asset has always had, so it could never withdraw
//! one a dependency has since dropped. [`Inherited`] (`assets/_extends.json`) is
//! that record — per registry, the keys inserted and a hash of the value
//! inserted; per file, the paths copied in and their content hashes. On the next
//! resolve an inherited entry whose hash *still matches* is withdrawn and
//! re-filled from the dependency, so a parent's change propagates; one whose
//! hash **differs** has been edited by the operator, who has adopted it — it
//! stays, and it stops being tracked. Nothing is ever silently thrown away,
//! which is what makes the merge both idempotent (no change in, no change out)
//! and safe to run at any time.
//!
//! Nothing here runs by itself. `asset resolve` is the verb, and a checkout with
//! no `pack.json` has no dependencies, so a resolve leaves it byte for byte as
//! it was.

use crate::audio_pool::{load_pool, save_pool, ClipPool, PoolKind, Sound};
use crate::profile;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The authored dependency list, at `assets/pack.json`.
pub const PACK_FILE: &str = "pack.json";

/// Where the dependencies are unpacked: `assets/_extends/<name>/`.
///
/// Inside `assets/` on purpose. A composition that lived beside the pack would
/// be two trees to hash and two to ship, and the pack hash would then be a
/// claim about only half of what a reader resolves.
pub const EXTENDS_DIR: &str = "_extends";

/// The resolution record, at `assets/_extends.json`. Generated, never authored.
pub const MARKER_FILE: &str = "_extends.json";

pub fn pack_path(assets: &Path) -> PathBuf {
    assets.join(PACK_FILE)
}

pub fn extends_dir(assets: &Path) -> PathBuf {
    assets.join(EXTENDS_DIR)
}

pub fn marker_path(assets: &Path) -> PathBuf {
    assets.join(MARKER_FILE)
}

/// `assets/pack.json`: the other assets this one is built on, weakest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pack {
    #[serde(default)]
    pub deps: Vec<String>,
}

/// One dependency, as it stood when the last resolve folded it in.
///
/// The hash is the point: it is what lets a child say "I was built against
/// `common` at *this* content" and therefore be told, cheaply and exactly, that
/// it is stale. A dependency that never moved leaves the child alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRecord {
    pub name: String,
    /// A content hash of the dependency's whole tree.
    pub hash: String,
}

/// `assets/_extends.json`: what a resolve put in the live tree, and from where.
///
/// Generated. Read back by the next resolve so it can withdraw what it owns,
/// and by the packer so a release can name the dependencies it was built
/// against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inherited {
    /// The **direct** dependencies folded in, in `pack.json` order — what a
    /// release names, and what a staleness check was written against before the
    /// closure was recorded at all.
    #[serde(default)]
    pub deps: Vec<DepRecord>,
    /// The whole **closure**, weakest first: every pack the graph reaches, once,
    /// with the hash it was folded in at. This is the map — see [`PackNode`].
    #[serde(default)]
    pub tree: Vec<PackNode>,
    /// Per registry filename, `key -> hash of the value inserted`.
    #[serde(default)]
    pub keys: BTreeMap<String, BTreeMap<String, String>>,
    /// Per file copied in, relative to `assets/`, its content hash.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

/// One pack in the composition's **closure**, weakest first, and how the graph
/// reached it.
///
/// `pack.json` is what an asset *declares*; this is what its tree contains once
/// the walk has followed every dependency's own `pack.json`. The difference is
/// the diamond: two dependencies that both name `E` are **one** `E`, folded once
/// at the weakest position that keeps every parent behind it, and [`via`] is the
/// only place that fact survives — a tree that folded per direct dependency
/// would hold `E` twice and name neither.
///
/// It is also the map a provisioning step flattens from: one directory per
/// `name` under `_extends/`, and `order` (this vector's order) is the order to
/// fold them in.
///
/// [`via`]: PackNode::via
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackNode {
    pub name: String,
    /// A content hash of the pack's own tree — the number a release, a child's
    /// `deps` and the staleness check all name.
    pub hash: String,
    /// Whether the live `pack.json` names it directly.
    #[serde(default)]
    pub direct: bool,
    /// The packs that name it, in the order they were visited. Empty when it is
    /// only a direct dependency.
    #[serde(default)]
    pub via: Vec<String>,
}

impl Inherited {
    pub fn is_empty(&self) -> bool {
        self.deps.is_empty() && self.keys.is_empty() && self.files.is_empty()
    }
}

/// The composition graph: every pack the live tree reaches, **weakest first**,
/// each exactly once.
///
/// Depth-first over each pack's own `pack.json`: a pack's parents are visited
/// before it, and the direct list's order is kept at every level. That is the
/// flat weakest-first rule (a later name wins a shared key) carried into a
/// graph, and it is what makes a diamond fold once — `A` naming `B` and `C`,
/// both of which name `E`, resolves to `[E, F, B, G, C]` for `B = [E, F]` and
/// `C = [E, G]`: `E` is folded at its weakest position, `F` still overrides it,
/// `B` still beats both, and `C` still beats `B`. Folding per *direct*
/// dependency instead is how one pack ends up inside two trees at once, with no
/// record of either copy.
///
/// A pack's own `pack.json` **is** the edge, which is what lets a release be a
/// node with edges instead of a copy of everything underneath it: a dependency
/// carrying no `deps` is a leaf.
///
/// The closure is the **map**: [`PackNode::via`] remembers a pack reached from
/// two parents (the only place that fact exists), and [`Inherited::tree`] stores
/// the whole of it, so whoever provisions a checkout can place one directory per
/// name under `_extends/` and fold them in this order.
pub fn closure(assets: &Path) -> Result<Vec<PackNode>> {
    let mut order: Vec<String> = Vec::new();
    let mut nodes: BTreeMap<String, PackNode> = BTreeMap::new();
    let mut path: Vec<String> = Vec::new();
    for dep in read_pack(assets).deps {
        visit(assets, &dep, true, None, &mut order, &mut nodes, &mut path)?;
    }
    Ok(order
        .into_iter()
        .filter_map(|name| nodes.remove(&name))
        .collect())
}

/// One step of [`closure`]: a pack's parents first, then the pack, and a name
/// already placed is only annotated — which is what keeps a diamond one node.
#[allow(clippy::too_many_arguments)]
fn visit(
    assets: &Path,
    name: &str,
    direct: bool,
    via: Option<&str>,
    order: &mut Vec<String>,
    nodes: &mut BTreeMap<String, PackNode>,
    path: &mut Vec<String>,
) -> Result<()> {
    if let Some(node) = nodes.get_mut(name) {
        node.direct |= direct;
        if let Some(via) = via {
            if !node.via.iter().any(|seen| seen == via) {
                node.via.push(via.to_string());
            }
        }
        return Ok(());
    }
    if path.iter().any(|seen| seen == name) {
        bail!(
            "pack '{name}' depends on itself: {} -> {name} — a composition is a graph, not a loop",
            path.join(" -> ")
        );
    }
    let dir = extends_dir(assets).join(name);
    if !dir.is_dir() {
        bail!(
            "asset '{name}' is not unpacked: {} is missing — unpack its release under {}/ before resolving",
            dir.display(),
            extends_dir(assets).display(),
        );
    }
    path.push(name.to_string());
    for parent in read_pack(&dir).deps {
        visit(assets, &parent, false, Some(name), order, nodes, path)?;
    }
    path.pop();
    nodes.insert(
        name.to_string(),
        PackNode {
            name: name.to_string(),
            hash: tree_hash(&dir)?,
            direct,
            via: via.map(|v| vec![v.to_string()]).unwrap_or_default(),
        },
    );
    order.push(name.to_string());
    Ok(())
}

/// Read `assets/pack.json`. Absent or unreadable is *no dependencies*, not an
/// error: a plain pack is the shape every checkout has today, and a
/// bookkeeping file must never stop a render.
pub fn read_pack(assets: &Path) -> Pack {
    std::fs::read_to_string(pack_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Read `assets/_extends.json`. A missing or broken record degrades to "nothing
/// was inherited", which is the safe direction: no withdrawal happens and every
/// entry already in the tree reads as the asset's own, so nothing is ever
/// deleted or overwritten on the strength of a file that did not parse.
pub fn read_marker(assets: &Path) -> Inherited {
    std::fs::read_to_string(marker_path(assets))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn write_marker(assets: &Path, marker: &Inherited) -> Result<()> {
    let path = marker_path(assets);
    let text = serde_json::to_string_pretty(marker)?;
    crate::atomic_write(&path, &format!("{text}\n"))?;
    Ok(())
}

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
fn kind_of(registry: &str) -> Option<PoolKind> {
    PoolKind::ALL.into_iter().find(|k| k.registry() == registry)
}

/// A hash of one registry value, so a resolve can tell an entry it inserted
/// from the same key the operator has since edited.
fn value_hash(sound: &Sound) -> String {
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
enum Mode {
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
struct Layered {
    file: &'static str,
    /// Members that do not take the `fallback` mode. A `_`-prefixed name is
    /// always [`Mode::Whole`] whatever this says.
    members: &'static [(&'static str, Mode)],
    fallback: Mode,
}

/// The layered files. Everything not named here is still copied whole, and a
/// file that is one of these never appears in the marker's `files`.
const LAYERED: &[Layered] = &[
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

fn layered(file: &str) -> Option<&'static Layered> {
    LAYERED.iter().find(|l| l.file == file)
}

impl Layered {
    fn mode(&self, member: &str) -> Mode {
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

fn unescape(part: &str) -> String {
    part.replace("%2B", "+")
        .replace("%2F", "/")
        .replace("%25", "%")
}

/// The marker key for a whole member's value.
fn whole_key(member: &str) -> String {
    escape(member)
}

/// The marker key for one key of a keyed member.
fn keyed_key(member: &str, key: &str) -> String {
    format!("{}/{}", escape(member), escape(key))
}

/// The marker key for the entries one dependency appended to a list.
fn concat_key(member: &str, dep: &str) -> String {
    format!("{}+{}", escape(member), escape(dep))
}

/// A list record is `<count>:<hash>`, because a list cannot be withdrawn by
/// value alone: the record has to say how many entries to drop.
fn list_record(count: usize, hash: &str) -> String {
    format!("{count}:{hash}")
}

fn parse_list_record(record: &str) -> Option<(usize, &str)> {
    let (count, hash) = record.split_once(':')?;
    Some((count.parse().ok()?, hash))
}

fn text_hash(text: &str) -> String {
    profile::content_hash(text.as_bytes())
}

/// `"name": value`, the shape a member has in the file.
fn member_text(member: &str, value: &str) -> String {
    format!(
        "{}: {value}",
        serde_json::to_string(member).unwrap_or_else(|_| format!("\"{member}\""))
    )
}

/// One inserted run, framed the way the file frames its own members.
fn piece(entry_indent: &str, close_indent: &str, text: &str) -> String {
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
fn append_inside(value: &str, pieces: &str) -> Option<String> {
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
fn replace_member(text: &str, member: &str, value: &str) -> Option<String> {
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
fn remove_member(text: &str, member: &str) -> Option<String> {
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
fn entries_text(text: &str) -> Option<String> {
    let spans = crate::audio_pool::scan_array(text)?;
    Some(
        spans
            .iter()
            .map(|(a, b)| &text[*a..*b])
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// One layered file's live text, and what a resolve has put into it.
struct Layer {
    file: &'static str,
    text: String,
    /// Members whose value came from a dependency, so a later one may override
    /// an earlier one while the file's own is never touched.
    filled: BTreeSet<String>,
    filled_keys: BTreeSet<(String, String)>,
    records: BTreeMap<String, String>,
    original: String,
}

impl Layer {
    /// Open `assets/<file>`, or start one empty.
    fn open(assets: &Path, file: &'static str) -> Result<Self> {
        let path = assets.join(file);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) if !t.trim().is_empty() => t,
            Ok(_) => bail!("{} is empty — refusing to merge into it", path.display()),
            Err(_) => "{}".to_string(),
        };
        if crate::audio_pool::scan_entries(&text).is_none() {
            bail!(
                "{} is not a JSON object this merge can read — refusing to rewrite it",
                path.display()
            );
        }
        Ok(Layer {
            file,
            original: text.clone(),
            text,
            filled: BTreeSet::new(),
            filled_keys: BTreeSet::new(),
            records: BTreeMap::new(),
        })
    }

    fn policy(&self) -> &'static Layered {
        layered(self.file).expect("only a layered file is ever opened as one")
    }

    fn member_value(&self, member: &str) -> Option<String> {
        let entries = crate::audio_pool::scan_entries(&self.text)?;
        let e = entries.iter().find(|e| e.key == member)?;
        Some(self.text[e.value_start..e.value_end].to_string())
    }

    fn has_member(&self, member: &str) -> bool {
        self.member_value(member).is_some()
    }

    /// Forget a member an earlier dependency contributed, so a later one wins
    /// it outright — the pool rule, one level down.
    fn override_member(&mut self, member: &str) {
        if !self.filled.remove(member) {
            return;
        }
        if let Some(next) = remove_member(&self.text, member) {
            self.text = next;
        }
        self.records
            .retain(|k, _| k != member && !k.starts_with(&format!("{member}/")));
    }

    fn fill_whole(&mut self, member: &str, value: &str) {
        if self.has_member(member) {
            // The file's own member wins. A member a *dependency* put there does
            // not: this pass runs weakest first, so a stronger one has to be able
            // to take it, the way it takes a keyed name.
            if !self.filled.contains(member) {
                return;
            }
            self.override_member(member);
        }
        let run = piece("  ", "", &member_text(member, value));
        let Some(next) = append_inside(&self.text, &run) else {
            return;
        };
        self.text = next;
        self.filled.insert(member.to_string());
        self.records.insert(whole_key(member), text_hash(value));
    }

    /// A member whose value is an object merges key by key; anything else (a
    /// licence line is a string) is the member itself, the member's name being
    /// the key.
    fn fill_keyed(&mut self, member: &str, value: &str) {
        let Some(parent_keys) = crate::audio_pool::scan_entries(value) else {
            self.override_member(member);
            self.fill_whole(member, value);
            return;
        };
        // No `override_member` here, deliberately. A keyed member *accumulates*:
        // the file's names win, every dependency's are added, and "a later
        // dependency overrides an earlier one" is per key — see `filled_keys`
        // below. Replacing the member instead would mean the second dependency to
        // state `sound` in `tag-aliases.json` silently erased the first one's
        // whole synonym table, which is what a two-dependency asset did the first
        // time this ran for real.
        let Some(current) = self.member_value(member) else {
            let run = piece("  ", "", &member_text(member, value));
            let Some(next) = append_inside(&self.text, &run) else {
                return;
            };
            self.text = next;
            self.filled.insert(member.to_string());
            for pk in &parent_keys {
                // Marked as well as recorded: the member arriving created these
                // names, so a stronger dependency is allowed to take them — and
                // only one that arrived later is, which is what makes the pass
                // weakest-first mean anything.
                self.filled_keys
                    .insert((member.to_string(), pk.key.clone()));
                self.records.insert(
                    keyed_key(member, &pk.key),
                    text_hash(&value[pk.value_start..pk.value_end]),
                );
            }
            return;
        };
        let Some(mine) = crate::audio_pool::scan_entries(&current) else {
            return; // the file's member is not an object; it wins as it stands
        };
        let mut merged = current.clone();
        let mut touched = false;
        for pk in &parent_keys {
            let own = mine.iter().any(|m| m.key == pk.key);
            let filled = self
                .filled_keys
                .contains(&(member.to_string(), pk.key.clone()));
            if own && !filled {
                continue; // the file's own name wins
            }
            let one = &value[pk.value_start..pk.value_end];
            // The names the file lacks are *added* to what is already here, never
            // swapped for it: this member accumulates across dependencies, which
            // is the whole point of a keyed member in a layered file.
            if filled {
                merged = remove_member(&merged, &pk.key).unwrap_or(merged);
            }
            let run = piece("    ", "  ", &member_text(&pk.key, one));
            match append_inside(&merged, &run) {
                Some(next) => merged = next,
                None => continue,
            }
            touched = true;
            self.filled_keys
                .insert((member.to_string(), pk.key.clone()));
            self.records
                .insert(keyed_key(member, &pk.key), text_hash(one));
        }
        if touched {
            if let Some(next) = replace_member(&self.text, member, &merged) {
                self.text = next;
            }
        }
    }

    /// An ordered list: the file's entries first, then this dependency's.
    fn fill_list(&mut self, member: &str, value: &str, dep: &str) {
        let Some(entries) = entries_text(value) else {
            return;
        };
        let count = crate::audio_pool::scan_array(value)
            .map(|v| v.len())
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        // A list never overrides — it accumulates, and each dependency's
        // entries are its own record (`member+dep`). Which order they arrive in
        // is [`Layer::fill_lists`]'s business.
        match self.member_value(member) {
            None => {
                let run = piece("  ", "", &member_text(member, value));
                let Some(next) = append_inside(&self.text, &run) else {
                    return;
                };
                self.text = next;
                self.filled.insert(member.to_string());
            }
            Some(current) => {
                // The dependency's own inner text, verbatim: its entries, its
                // layout, its closing indent.
                let trimmed = value.trim_end();
                let inner = match trimmed.get(1..trimmed.len().saturating_sub(1)) {
                    Some(i) => i,
                    None => return,
                };
                let Some(merged) = append_inside(&current, inner) else {
                    return;
                };
                if let Some(next) = replace_member(&self.text, member, &merged) {
                    self.text = next;
                }
            }
        }
        self.records.insert(
            concat_key(member, dep),
            list_record(count, &text_hash(&entries)),
        );
    }

    /// Fold one dependency into this file.
    ///
    /// No dependency name: every record this writes is keyed by the member it
    /// filled, because a member is only ever filled once — the file's own first,
    /// then the strongest dependency's, and a weaker one is refused. The lists
    /// are the exception, and they are the ones that carry the name.
    fn fill(&mut self, dir: &Path) {
        let path = dir.join(self.file);
        let Ok(parent) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(entries) = crate::audio_pool::scan_entries(&parent) else {
            return; // a dependency's file this cannot read contributes nothing
        };
        let policy = self.policy();
        for e in &entries {
            let value = &parent[e.value_start..e.value_end];
            match policy.mode(&e.key) {
                Mode::Whole => {
                    self.override_member(&e.key);
                    self.fill_whole(&e.key, value);
                }
                Mode::ByKey => self.fill_keyed(&e.key, value),
                // Folded in by `fill_lists`, after the scalars and strongest
                // dependency first.
                Mode::Concat => {}
            }
        }
    }

    /// Fold in the members that are *lists*, after the scalars and **strongest
    /// dependency first**.
    ///
    /// A keyed member can say "later in `deps` wins" by replacing a value. A list
    /// has no key to replace: the only way a stronger dependency can win is to be
    /// *seen* earlier, because the scene map's rules are matched in order and the
    /// first match takes the scene. So the lists are appended in reverse `deps`
    /// order, which leaves the strongest nearest the file's own entries — the
    /// ones that are already matched first.
    fn fill_lists(&mut self, dir: &Path, dep: &str) {
        let path = dir.join(self.file);
        let Ok(parent) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(entries) = crate::audio_pool::scan_entries(&parent) else {
            return;
        };
        let policy = self.policy();
        for e in &entries {
            if policy.mode(&e.key) == Mode::Concat {
                let value = &parent[e.value_start..e.value_end];
                self.fill_list(&e.key, value, dep);
            }
        }
    }

    /// Take back what a previous resolve put here, and only where nobody has
    /// edited it since. The marker is what makes a re-resolve regenerate rather
    /// than duplicate, so this runs before any fill.
    fn withdraw(
        &mut self,
        records: &BTreeMap<String, String>,
        deps: &[DepRecord],
        adopted: &mut BTreeSet<(String, String)>,
    ) {
        // Lists first, in `deps` order — which is the reverse of the order they
        // were appended in. The fill adds them strongest-first, so the *weakest*
        // dependency's entries are the tail, and the tail is what has to come
        // off first. Taking them off in the wrong order does not fail loudly:
        // the one whose block is not at the tail reads as "edited here", is
        // adopted, and its rules are then appended a second time.
        for dep in deps {
            for (member, _) in self
                .policy()
                .members
                .iter()
                .copied()
                .filter(|(_, m)| *m == Mode::Concat)
            {
                let key = concat_key(member, &dep.name);
                let Some(record) = records.get(&key) else {
                    continue;
                };
                let Some((count, hash)) = parse_list_record(record) else {
                    continue;
                };
                let Some(current) = self.member_value(member) else {
                    continue;
                };
                let Some(spans) = crate::audio_pool::scan_array(&current) else {
                    continue;
                };
                if count == 0 || spans.len() < count {
                    continue;
                }
                let at = spans.len() - count;
                let tail: String = spans[at..]
                    .iter()
                    .map(|(a, b)| &current[*a..*b])
                    .collect::<Vec<_>>()
                    .join(",");
                if text_hash(&tail) != hash {
                    // Edited here: the operator has adopted it, so it stays and
                    // it stops being tracked.
                    adopted.insert((self.file.to_string(), key));
                    continue;
                }
                // Everything the dependency appended was the tail, so what is
                // left is the file's own — and it keeps its own layout.
                let head = current[..spans[at].0]
                    .trim_end_matches([',', ' ', '\n'])
                    .to_string();
                let next = if at == 0 {
                    "[]".to_string()
                } else if head.contains('\n') {
                    format!("{head}\n  ]")
                } else {
                    format!("{head}]")
                };
                if let Some(text) = replace_member(&self.text, member, &next) {
                    self.text = text;
                }
                self.filled.remove(member);
            }
        }
        // Then whole members and keyed names, whose hash is of the value.
        for (key, hash) in records {
            if key.contains('+') {
                continue; // a list, above
            }
            match key.split_once('/') {
                Some((member, name)) => {
                    let (member, name) = (&unescape(member), &unescape(name));
                    let Some(current) = self.member_value(member) else {
                        continue;
                    };
                    let Some(mine) = crate::audio_pool::scan_entries(&current) else {
                        continue;
                    };
                    let Some(m) = mine.iter().find(|m| m.key == *name) else {
                        continue;
                    };
                    if text_hash(&current[m.value_start..m.value_end]) != *hash {
                        adopted.insert((self.file.to_string(), key.clone()));
                        continue;
                    }
                    let Some(merged) = remove_member(&current, name) else {
                        continue;
                    };
                    let next = if crate::audio_pool::scan_entries(&merged)
                        .map(|e| e.is_empty())
                        .unwrap_or(false)
                    {
                        // Nothing left of it: the member itself came from the
                        // dependency, so it goes too.
                        remove_member(&self.text, member).unwrap_or(merged)
                    } else {
                        replace_member(&self.text, member, &merged).unwrap_or(merged)
                    };
                    self.text = next;
                    self.filled_keys
                        .remove(&(member.to_string(), name.to_string()));
                }
                None => {
                    let member = unescape(key);
                    let Some(current) = self.member_value(&member) else {
                        continue;
                    };
                    if text_hash(&current) != *hash {
                        adopted.insert((self.file.to_string(), key.clone()));
                        continue;
                    }
                    if let Some(text) = remove_member(&self.text, &member) {
                        self.text = text;
                    }
                    self.filled.remove(&member);
                }
            }
        }
    }

    /// Write the merged file, and only when it differs.
    fn finish(&self, assets: &Path) -> Result<()> {
        if self.text == self.original {
            return Ok(());
        }
        crate::atomic_write(&assets.join(self.file), &self.text)?;
        Ok(())
    }
}

/// Fold this asset's dependencies into the live tree.
///
/// Withdraw, then fill, then record — in that order, because a re-resolve has
/// to undo its own last answer before it can give a new one. Everything that
/// is not a pool registry is copied whole; every pool registry is merged key by
/// key through [`save_pool`], which keeps the bytes of every entry it was not
/// asked to change — so a resolve that changes one sound diffs as one sound.
///
/// `dry_run` computes the same answer and writes nothing, including the file
/// deletions and copies, so it can be run on a live tree by anyone asking
/// "what would this do".
pub fn resolve(assets: &Path, dry_run: bool) -> Result<Report> {
    let old = read_marker(assets);
    let mut report = Report {
        dry_run,
        ..Report::default()
    };

    // The live registries, read once. Only the ones that exist are kept, so a
    // layer a dependency fills is created by the fill rather than by an empty
    // file appearing out of nowhere.
    let mut pools: BTreeMap<&'static str, ClipPool> = BTreeMap::new();
    for kind in PoolKind::ALL {
        let path = assets.join(kind.registry());
        if path.is_file() {
            pools.insert(kind.registry(), load_pool(&path));
        }
    }
    let mut dirty: BTreeSet<&'static str> = BTreeSet::new();
    // What the tree looked like before this resolve touched it, so a resolve
    // that reaches the same answer can leave the files alone entirely.
    let original = pools.clone();
    let mut adopted_keys: BTreeSet<(String, String)> = BTreeSet::new();
    let mut adopted_files: BTreeSet<String> = BTreeSet::new();
    // The layered files, opened once: their members merge by name rather than
    // whole-file, which is what lets a root asset own the world's rules.
    let mut layers: Vec<Layer> = Vec::new();
    for l in LAYERED {
        layers.push(Layer::open(assets, l.file)?);
    }

    // 1. Withdraw what the last resolve put here and nobody has edited since.
    for (registry, keys) in &old.keys {
        let Some(kind) = kind_of(registry) else {
            continue;
        };
        let pool = pools.entry(kind.registry()).or_default();
        for (key, hash) in keys {
            match pool.get(key) {
                Some(sound) if value_hash(sound) == *hash => {
                    pool.remove(key);
                    dirty.insert(kind.registry());
                }
                // Edited since it was inherited: the operator has adopted it,
                // so it is theirs now and it leaves the record.
                Some(_) => {
                    adopted_keys.insert((kind.registry().to_string(), key.clone()));
                }
                None => {}
            }
        }
    }
    // An inherited file is *marked*, not deleted, here: whether it comes back is
    // a question for the fill, and deleting it first is how a resolve came to
    // rewrite every clip it had already put there — 58 MB of churn for a change
    // of nothing. It also made a dry run lie, because a dry run cannot delete,
    // so its fill found every file already present and reported them all as
    // withdrawn.
    let mut withdrawable: BTreeSet<String> = BTreeSet::new();
    for (rel, hash) in &old.files {
        // A file that layers now is the layers' business, whatever a marker
        // written before it did says.
        if layered(rel).is_some() {
            continue;
        }
        let path = assets.join(rel);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if profile::content_hash(&bytes) == *hash {
            withdrawable.insert(rel.clone());
        } else {
            adopted_files.insert(rel.clone());
        }
    }

    // Then the layered members, whose record is what lets a re-resolve withdraw
    // its own last answer instead of stacking another on top of it.
    for layer in layers.iter_mut() {
        if let Some(records) = old.keys.get(layer.file) {
            layer.withdraw(records, &old.deps, &mut adopted_keys);
        }
    }

    // 2. Fold the dependencies in, weakest first. A later dependency overrides
    //    an earlier one on a shared key, but never the asset's own — which is
    //    why "already present" has to be told apart from "filled in by a
    //    dependency", and not simply tested with `contains_key`.
    // The **closure**, weakest first, each pack once: the graph is what is folded
    // in, not the direct list, so a pack two dependencies share is one tree here
    // and one fold — see [`closure`].
    let tree = closure(assets)?;
    report.tree = tree.len();
    let mut marker = Inherited {
        tree: tree.clone(),
        ..Inherited::default()
    };
    let mut filled_keys: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    let mut filled_files: BTreeSet<String> = BTreeSet::new();
    // What the last resolve folded, to compare against. The closure when the
    // record has one, the direct list otherwise — a marker written before the
    // tree was recorded still says what the child was built against.
    let mut previous: BTreeMap<&str, &str> = old
        .deps
        .iter()
        .map(|d| (d.name.as_str(), d.hash.as_str()))
        .collect();
    for node in &old.tree {
        previous.insert(node.name.as_str(), node.hash.as_str());
    }

    for node in &tree {
        let dep = &node.name;
        let dir = extends_dir(assets).join(dep);
        let record = DepRecord {
            name: dep.clone(),
            hash: node.hash.clone(),
        };
        // Every pack in the closure is compared, so a parent that moved under a
        // dependency the child never named directly is caught here too.
        if previous.get(dep.as_str()).is_some_and(|was| *was != record.hash) {
            report.stale.push(dep.clone());
        }
        // Only the direct ones are "the dependencies" — what a release of this
        // asset names, and what the flat fold used to mean.
        if node.direct {
            marker.deps.push(record.clone());
            report.deps.push(record);
        }

        for kind in PoolKind::ALL {
            let parent = load_pool(&dir.join(kind.registry()));
            if parent.is_empty() {
                continue;
            }
            let pool = pools.entry(kind.registry()).or_default();
            let slot = filled_keys.entry(kind.registry()).or_default();
            for (key, sound) in &parent {
                if pool.contains_key(key) && !slot.contains(key) {
                    continue; // the asset's own entry wins over every dependency
                }
                pool.insert(key.clone(), sound.clone());
                slot.insert(key.clone());
                marker
                    .keys
                    .entry(kind.registry().to_string())
                    .or_default()
                    .insert(key.clone(), value_hash(sound));
                dirty.insert(kind.registry());
            }
        }

        for layer in layers.iter_mut() {
            layer.fill(&dir);
        }

        for path in profile::files_under(&dir, &[""]) {
            let Ok(rel) = path.strip_prefix(&dir) else {
                continue;
            };
            let rel = rel.display().to_string();
            // The dependency's own manifests are not content: `pack.json` and
            // the marker describe *it*, and inheriting them would make this
            // asset claim a dependency list it never wrote.
            //
            // And a pool registry is **merged, never copied**: it went through
            // the key-by-key pass above, so copying it whole would both bypass
            // that merge and record the same content twice — once as a file and
            // once as its keys.
            if rel == PACK_FILE
                || rel == MARKER_FILE
                || kind_of(&rel).is_some()
                || layered(&rel).is_some()
            {
                continue;
            }
            let to = assets.join(&rel);
            // `to.exists()` alone cannot tell this asset's own file from one a
            // previous resolve put here; the record can, and that is what keeps
            // a resolve from rewriting a clip that is already right.
            if to.exists() && !filled_files.contains(&rel) && !withdrawable.contains(&rel) {
                continue; // the asset's own file wins
            }
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let same = std::fs::read(&to).map(|now| now == bytes).unwrap_or(false);
            if !dry_run && !same {
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(&to, &bytes).with_context(|| format!("writing {}", to.display()))?;
            }
            filled_files.insert(rel.clone());
            marker.files.insert(rel, profile::content_hash(&bytes));
        }
    }

    // And now the files a dependency no longer provides: marked withdrawable
    // before the fill, still unclaimed after it. Deleting them here rather than
    // before the fill is what makes a dry run and a real one agree.
    if !dry_run {
        for rel in &withdrawable {
            if marker.files.contains_key(rel) {
                continue;
            }
            let path = assets.join(rel);
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("withdrawing {}", path.display()))?;
            }
        }
    }

    // The lists last, strongest pack first, over the whole closure: a diamond's
    // shared parent has one entry here, so its rules are appended once, at its
    // own (weakest) position — and the record names the pack that shipped them
    // rather than whichever dependency happened to contain a copy.
    for node in tree.iter().rev() {
        let dir = extends_dir(assets).join(&node.name);
        for layer in layers.iter_mut() {
            layer.fill_lists(&dir, &node.name);
        }
    }

    // What the layered files put in is recorded in the same map the pools use,
    // keyed by filename — a whole member by its name, a keyed member by
    // `member/name`, an appended list by `member+dep`.
    for layer in &layers {
        if !layer.records.is_empty() {
            marker
                .keys
                .insert(layer.file.to_string(), layer.records.clone());
        }
    }

    // 3. Count, then record. The counters describe the *tree*, not the
    //    algorithm: withdrawing and refilling is how the merge works, so a
    //    resolve that reaches the same answer must report no change rather than
    //    narrate its own two steps. A key is withdrawn when it is no longer
    //    inherited at all; a key is added when it is inherited *as something
    //    else* — which is why the withdrawal compares names and the addition
    //    compares names and values.
    // A list record is left out of both sets and counted by its own number
    // below: its *name* is the member and the dependency, which does not change
    // when the list does, so a name comparison cannot see a root gaining or
    // dropping a rule — and the honest unit for a list is entries.
    let names = |m: &Inherited| -> BTreeSet<(String, String)> {
        m.keys
            .iter()
            .flat_map(|(r, ks)| {
                ks.keys()
                    .filter(|k| !k.contains('+'))
                    .map(move |k| (r.clone(), k.clone()))
            })
            .collect()
    };
    let named_values = |m: &Inherited| -> BTreeSet<(String, String, String)> {
        m.keys
            .iter()
            .flat_map(|(r, ks)| {
                ks.iter()
                    .filter(|(k, _)| !k.contains('+'))
                    .map(move |(k, h)| (r.clone(), k.clone(), h.clone()))
            })
            .collect()
    };
    let named_files = |m: &Inherited| -> BTreeSet<(String, String)> {
        m.files
            .iter()
            .map(|(r, h)| (r.clone(), h.clone()))
            .collect()
    };
    report.withdrawn = names(&old)
        .difference(&names(&marker))
        .filter(|(r, k)| !adopted_keys.contains(&(r.clone(), k.clone())))
        .count()
        + old
            .files
            .keys()
            .filter(|rel| !marker.files.contains_key(*rel) && !adopted_files.contains(*rel))
            .count();
    report.added = named_values(&marker)
        .difference(&named_values(&old))
        .count()
        + named_files(&marker).difference(&named_files(&old)).count();
    // Then the lists, by entries: a dependency's rule that is gone is a rule
    // withdrawn, one it gained is a rule filled in, and an edit to one of its
    // rules is one filled in and none withdrawn.
    for (file, records) in &old.keys {
        for (key, record) in records.iter().filter(|(k, _)| k.contains('+')) {
            let Some((was, hash)) = parse_list_record(record) else {
                continue;
            };
            match marker
                .keys
                .get(file)
                .and_then(|r| r.get(key))
                .and_then(|r| parse_list_record(r))
            {
                Some((now, now_hash)) => {
                    if now > was {
                        report.added += now - was;
                    }
                    if was > now {
                        report.withdrawn += was - now;
                    }
                    if now == was && now_hash != hash {
                        report.added += 1;
                    }
                }
                None => report.withdrawn += was,
            }
        }
    }
    report.adopted = adopted_keys.len() + adopted_files.len();

    if !dry_run {
        for registry in &dirty {
            let kind = kind_of(registry).expect("only a known registry is ever marked dirty");
            // Only a registry that actually differs is rewritten, so a resolve
            // that changes nothing leaves even the mtimes alone.
            if pools[registry] != original.get(registry).cloned().unwrap_or_default() {
                save_pool(&assets.join(registry), kind, &pools[registry])?;
            }
        }
        for layer in &layers {
            layer.finish(assets)?;
        }
        // A record that would say nothing is not written, and an empty one left
        // over from a dependency that has since been dropped is removed —
        // otherwise a tree with no dependencies would carry a file claiming it
        // inherited things.
        if marker != old {
            if marker.is_empty() {
                let _ = std::fs::remove_file(marker_path(assets));
            } else {
                write_marker(assets, &marker)?;
            }
        }
    }
    // A layered file is a change the marker cannot always see, so it is counted
    // from the text: a list that duplicated itself keeps its records, keeps its
    // counts, and still rewrites the file.
    report.rewritten = layers.iter().filter(|l| l.text != l.original).count();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `assets/` tree, no pack and no dependencies.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-compose-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        assets
    }

    fn dep_tree(assets: &Path, name: &str) -> PathBuf {
        let dir = extends_dir(assets).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sound(tag: &str) -> String {
        format!(r#"{{"tags":["{tag}"],"files":["effects/{tag}-1.mp3"]}}"#)
    }

    fn write_pool(dir: &Path, registry: &str, entries: &[(&str, &str)]) {
        let body: Vec<String> = entries
            .iter()
            .map(|(k, t)| format!(r#"  "{k}": {}"#, sound(t)))
            .collect();
        std::fs::write(
            dir.join(registry),
            format!("{{\n{}\n}}\n", body.join(",\n")),
        )
        .unwrap();
    }

    /// **The diamond, folded once.** `B` and `C` both name `E`, which is the
    /// shape a preset reaches the moment two of its libraries share a parent:
    /// folding per *direct* dependency puts a copy of `E` inside each of them and
    /// records neither, so a fix to `E` is a fix in two places and the tree has
    /// no way to say so.
    ///
    /// The closure is the answer: `E` is one node at its weakest position, every
    /// parent still sits behind it, and `via` is where the shared reach is
    /// written down — which is the map a provisioning step flattens from.
    #[test]
    fn a_shared_parent_is_folded_once_and_the_map_says_how_it_was_reached() {
        let assets = scratch("diamond");
        std::fs::write(pack_path(&assets), r#"{"deps":["B","C"]}"#).unwrap();
        // E is the shared parent; F overrides one of its keys (the "some" case).
        write_pool(
            &dep_tree(&assets, "E"),
            "effect-pool.json",
            &[("wind", "E-wind"), ("rain", "E-rain")],
        );
        write_pool(
            &dep_tree(&assets, "F"),
            "effect-pool.json",
            &[("wind", "F-wind")],
        );
        let b = dep_tree(&assets, "B");
        std::fs::write(pack_path(&b), r#"{"deps":["E","F"]}"#).unwrap();
        write_pool(&b, "effect-pool.json", &[("night", "B-night")]);
        write_pool(
            &dep_tree(&assets, "G"),
            "effect-pool.json",
            &[("snow", "G-snow")],
        );
        let c = dep_tree(&assets, "C");
        std::fs::write(pack_path(&c), r#"{"deps":["E","G"]}"#).unwrap();
        // C's own rain, so the strongest dependency visibly wins a shared key.
        write_pool(&c, "effect-pool.json", &[("rain", "C-rain")]);

        let tree = closure(&assets).unwrap();
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["E", "F", "B", "G", "C"],
            "weakest first, parents before children, each pack once"
        );
        assert_eq!(
            tree.iter().filter(|n| n.name == "E").count(),
            1,
            "E is one node, not one per parent"
        );
        let shared = tree.iter().find(|n| n.name == "E").unwrap();
        assert!(!shared.direct, "E is reached through its children");
        assert_eq!(shared.via, ["B", "C"], "and the map remembers both paths");
        assert_eq!(shared.hash.len(), 64, "a node carries the hash it folded at");
        let direct = tree.iter().find(|n| n.name == "B").unwrap();
        assert!(direct.direct && direct.via.is_empty(), "B is named here");

        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.tree, 5, "the report counts the closure: {r:?}");
        assert_eq!(r.deps.len(), 2, "but `deps` stays the direct list");

        // The record is the map, and it says which node is a dependency and
        // which was only reached through one.
        let marker = read_marker(&assets);
        let recorded: Vec<&str> = marker.tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(recorded, names, "the record is the closure, in fold order");
        let deps: Vec<&str> = marker.deps.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(deps, ["B", "C"], "a release names the direct ones");
        assert_eq!(
            marker.tree.iter().find(|n| n.name == "E").unwrap().via,
            ["B", "C"],
            "the shared parent survives a re-resolve with both paths"
        );

        // And the order is the precedence: E first, F overrides it, C beats B.
        let pool = load_pool(&assets.join("effect-pool.json"));
        assert_eq!(pool["wind"].tags, vec!["F-wind"], "F overrides E");
        assert_eq!(pool["rain"].tags, vec!["C-rain"], "C beats B and E");
        assert_eq!(pool["night"].tags, vec!["B-night"]);
        assert_eq!(pool["snow"].tags, vec!["G-snow"]);
        assert!(marker.keys["effect-pool.json"].contains_key("snow"), "G's own key is recorded as inherited");

        // A second resolve reaches the same answer, E still once.
        let again = resolve(&assets, false).unwrap();
        assert!(!again.changed(), "{again:?}");
        assert_eq!(
            read_marker(&assets).tree.iter().filter(|n| n.name == "E").count(),
            1
        );
    }

    /// A parent that moves *under* a dependency the child never named directly
    /// is the staleness a flat fold could not see: the child's own `deps` list
    /// does not mention it, so comparing that list alone reads as up to date.
    #[test]
    fn a_parent_that_moves_below_a_dependency_makes_the_child_stale() {
        let assets = scratch("deep-stale");
        std::fs::write(pack_path(&assets), r#"{"deps":["B"]}"#).unwrap();
        let b = dep_tree(&assets, "B");
        std::fs::write(pack_path(&b), r#"{"deps":["E"]}"#).unwrap();
        let e = dep_tree(&assets, "E");
        write_pool(&e, "effect-pool.json", &[("wind", "E-wind")]);

        assert!(resolve(&assets, false).unwrap().stale.is_empty());

        // E moves — one more clip — and only the closure can say so.
        std::fs::create_dir_all(e.join("effects")).unwrap();
        std::fs::write(e.join("effects/wind-2.mp3"), b"clip").unwrap();
        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.stale, ["E"], "the grandparent moved: {r:?}");
        assert!(
            assets.join("effects/wind-2.mp3").is_file(),
            "and its content still arrives"
        );
        assert!(resolve(&assets, false).unwrap().stale.is_empty());
    }

    /// A graph that loops is a mistake to name, not to follow: the walk says
    /// which packs form the loop rather than spinning or blowing the stack.
    #[test]
    fn a_graph_that_loops_is_refused_with_the_path() {
        let assets = scratch("cycle");
        std::fs::write(pack_path(&assets), r#"{"deps":["B"]}"#).unwrap();
        let b = dep_tree(&assets, "B");
        std::fs::write(pack_path(&b), r#"{"deps":["B2"]}"#).unwrap();
        let b2 = dep_tree(&assets, "B2");
        std::fs::write(pack_path(&b2), r#"{"deps":["B"]}"#).unwrap();

        let err = closure(&assets).unwrap_err().to_string();
        assert!(err.contains("depends on itself"), "{err}");
        assert!(err.contains("B -> B2 -> B"), "it names the loop: {err}");
    }

    /// A dependency that is not on disk is named before anything is folded in,
    /// so a half-unpacked closure never half-resolves.
    #[test]
    fn a_missing_pack_in_the_closure_is_refused_by_name() {
        let assets = scratch("missing-in-closure");
        std::fs::write(pack_path(&assets), r#"{"deps":["B"]}"#).unwrap();
        let b = dep_tree(&assets, "B");
        std::fs::write(pack_path(&b), r#"{"deps":["E"]}"#).unwrap();

        let err = resolve(&assets, false).unwrap_err().to_string();
        assert!(err.contains("'E' is not unpacked"), "{err}");
        assert!(
            !marker_path(&assets).exists(),
            "nothing was recorded for a tree that never resolved"
        );
    }

    #[test]
    fn a_pack_with_no_dependencies_resolves_to_nothing() {
        let assets = scratch("no-deps");
        std::fs::write(assets.join("scene-map.json"), "{}").unwrap();
        let before = std::fs::read_dir(&assets).unwrap().count();

        let r = resolve(&assets, false).unwrap();
        assert!(!r.changed(), "{r:?}");
        assert!(r.deps.is_empty() && r.stale.is_empty());
        assert!(
            !marker_path(&assets).exists(),
            "nothing inherited, no record"
        );
        assert_eq!(std::fs::read_dir(&assets).unwrap().count(), before);
        // And it stays a no-op however often it runs.
        assert!(!resolve(&assets, false).unwrap().changed());
    }

    #[test]
    fn a_dependency_fills_in_only_what_the_asset_lacks() {
        let assets = scratch("fill-in");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        // The asset's own `door`, spelled differently so a wrong winner shows.
        write_pool(&assets, "effect-pool.json", &[("door", "my-own-door")]);
        let dep = dep_tree(&assets, "common");
        write_pool(
            &dep,
            "effect-pool.json",
            &[("door", "inherited"), ("wind", "wind")],
        );
        std::fs::create_dir_all(dep.join("effects")).unwrap();
        std::fs::write(dep.join("effects/wind-1.mp3"), b"clip").unwrap();

        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.added, 2, "wind's key and wind's clip: {r:?}");
        assert_eq!(r.withdrawn, 0);

        let pool = load_pool(&assets.join("effect-pool.json"));
        assert_eq!(
            pool["door"].tags,
            vec!["my-own-door"],
            "the asset's own won"
        );
        assert_eq!(
            pool["wind"].tags,
            vec!["wind"],
            "and the missing one filled in"
        );
        assert!(
            assets.join("effects/wind-1.mp3").is_file(),
            "the clip came with its registry entry"
        );

        // The record names the dependency and what it contributed.
        let marker = read_marker(&assets);
        assert_eq!(marker.deps.len(), 1);
        assert_eq!(marker.deps[0].name, "common");
        assert!(marker.keys["effect-pool.json"].contains_key("wind"));
        assert!(!marker.keys["effect-pool.json"].contains_key("door"));
        assert!(marker.files.contains_key("effects/wind-1.mp3"));

        // Idempotent: nothing in, nothing out — and the asset's own entry is
        // still its own.
        let again = resolve(&assets, false).unwrap();
        assert_eq!(again.added, 0, "{again:?}");
        assert_eq!(again.withdrawn, 0);
        assert_eq!(
            load_pool(&assets.join("effect-pool.json"))["door"].tags,
            vec!["my-own-door"]
        );
    }

    #[test]
    fn later_dependencies_override_earlier_ones() {
        let assets = scratch("order");
        std::fs::write(pack_path(&assets), r#"{"deps":["common","xianxia-base"]}"#).unwrap();
        write_pool(
            &dep_tree(&assets, "common"),
            "effect-pool.json",
            &[("wind", "common-wind")],
        );
        write_pool(
            &dep_tree(&assets, "xianxia-base"),
            "effect-pool.json",
            &[("wind", "genre-wind")],
        );

        resolve(&assets, false).unwrap();
        let pool = load_pool(&assets.join("effect-pool.json"));
        assert_eq!(pool["wind"].tags, vec!["genre-wind"], "the later one wins");
        // The record follows the winner, so the withdrawal takes the right one.
        let marker = read_marker(&assets);
        assert_eq!(
            marker.keys["effect-pool.json"]["wind"],
            value_hash(&pool["wind"])
        );
    }

    #[test]
    fn a_re_resolve_withdraws_a_key_the_parent_has_dropped() {
        let assets = scratch("withdraw");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        write_pool(
            &dep,
            "effect-pool.json",
            &[("wind", "wind"), ("rain", "rain")],
        );
        std::fs::create_dir_all(dep.join("effects")).unwrap();
        std::fs::write(dep.join("effects/rain-1.mp3"), b"clip").unwrap();

        assert_eq!(resolve(&assets, false).unwrap().added, 3);
        assert!(assets.join("effects/rain-1.mp3").is_file());

        // The parent drops `rain` and its clip. A fill-in-only merge would leave
        // both behind for good; the record is what makes them withdrawable.
        std::fs::remove_file(dep.join("effects/rain-1.mp3")).unwrap();
        write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.withdrawn, 2, "rain's key and rain's clip: {r:?}");

        let pool = load_pool(&assets.join("effect-pool.json"));
        assert!(!pool.contains_key("rain"), "the dropped key is gone");
        assert!(pool.contains_key("wind"), "and the survivor stayed");
        assert!(!assets.join("effects/rain-1.mp3").exists());
        assert!(!read_marker(&assets).keys["effect-pool.json"].contains_key("rain"));
    }

    #[test]
    fn an_edited_inherited_entry_is_adopted_rather_than_overwritten() {
        let assets = scratch("adopt");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
        resolve(&assets, false).unwrap();

        // The operator retunes it — the reason the live tree is the source of
        // truth. The next resolve must not throw that away.
        write_pool(&assets, "effect-pool.json", &[("wind", "retuned")]);
        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.adopted, 1, "{r:?}");
        assert_eq!(r.added, 0, "an adopted entry is not re-filled");
        assert_eq!(r.withdrawn, 0, "and never withdrawn");
        assert_eq!(
            load_pool(&assets.join("effect-pool.json"))["wind"].tags,
            vec!["retuned"]
        );
        let marker = read_marker(&assets);
        assert!(
            !marker
                .keys
                .get("effect-pool.json")
                .is_some_and(|ks| ks.contains_key("wind")),
            "it is the asset's own now, and the record says so: {marker:?}"
        );
    }

    #[test]
    fn a_dry_run_reports_what_a_real_one_would_do_and_writes_nothing() {
        let assets = scratch("dry-run");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
        std::fs::create_dir_all(dep.join("effects")).unwrap();
        std::fs::write(dep.join("effects/wind-1.mp3"), b"clip").unwrap();

        let dry = resolve(&assets, true).unwrap();
        assert!(dry.dry_run && dry.changed());
        assert_eq!(dry.added, 2);
        assert!(!marker_path(&assets).exists(), "no record written");
        assert!(
            !assets.join("effect-pool.json").exists(),
            "no registry written"
        );
        assert!(
            !assets.join("effects/wind-1.mp3").exists(),
            "no clip copied"
        );

        // The real run does exactly what the dry one said.
        let real = resolve(&assets, false).unwrap();
        assert_eq!(
            (real.added, real.withdrawn),
            (dry.added, dry.withdrawn),
            "a dry run that disagrees with the run is worse than none"
        );
        assert!(assets.join("effects/wind-1.mp3").is_file());
    }

    #[test]
    fn a_second_resolve_leaves_the_files_already_in_place_alone() {
        // Delete-then-refill rewrites every inherited clip on every resolve —
        // and a dry run, which cannot delete, would then find each file already
        // present and report it as withdrawn.
        let assets = scratch("file-churn");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
        std::fs::create_dir_all(dep.join("effects")).unwrap();
        std::fs::write(dep.join("effects/wind-1.mp3"), b"clip").unwrap();

        resolve(&assets, false).unwrap();
        let clip = assets.join("effects/wind-1.mp3");
        let before = std::fs::metadata(&clip).unwrap().modified().unwrap();

        let dry = resolve(&assets, true).unwrap();
        assert!(
            !dry.changed(),
            "a dry run over a settled tree changes nothing: {dry:?}"
        );
        let real = resolve(&assets, false).unwrap();
        assert!(!real.changed(), "{real:?}");
        assert_eq!(
            std::fs::metadata(&clip).unwrap().modified().unwrap(),
            before,
            "and the clip was never rewritten"
        );
    }

    #[test]
    fn a_missing_dependency_tree_refuses_and_names_the_path() {
        let assets = scratch("missing-dep");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let err = resolve(&assets, false).unwrap_err().to_string();
        assert!(err.contains("common"), "{err}");
        assert!(err.contains("_extends"), "it says where it looked: {err}");
    }

    #[test]
    fn a_moving_dependency_is_reported_stale() {
        let assets = scratch("stale");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
        assert!(resolve(&assets, false).unwrap().stale.is_empty());

        // The parent gains a sound. Until this tree is re-packed against it, it
        // is built on something that has moved — a comparison, not a guess.
        write_pool(
            &dep,
            "effect-pool.json",
            &[("wind", "wind"), ("rain", "rain")],
        );
        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.stale, vec!["common"]);
        assert!(r.summary().contains("STALE"), "{}", r.summary());
        assert!(
            resolve(&assets, false).unwrap().stale.is_empty(),
            "then clean"
        );
    }

    #[test]
    fn dropping_the_last_dependency_withdraws_everything_it_gave() {
        let assets = scratch("drop-all");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        write_pool(&dep, "effect-pool.json", &[("wind", "wind")]);
        resolve(&assets, false).unwrap();
        assert!(marker_path(&assets).exists());

        std::fs::write(pack_path(&assets), r#"{"deps":[]}"#).unwrap();
        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.withdrawn, 1);
        assert!(
            !assets.join("effect-pool.json").exists()
                || load_pool(&assets.join("effect-pool.json")).is_empty()
        );
        assert!(
            !marker_path(&assets).exists(),
            "a tree with no dependencies carries no record claiming otherwise"
        );
    }

    /// A scene map on disk, laid out the way the shipped one is.
    fn write_map(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let text = format!("{{\n  {body}\n}}\n");
        std::fs::write(dir.join("scene-map.json"), text).unwrap();
    }

    fn read(assets: &Path, file: &str) -> String {
        std::fs::read_to_string(assets.join(file)).unwrap()
    }

    /// Every rule's first `match`, in order — the one thing about a rule list
    /// that order decides.
    fn matches_of(assets: &Path) -> Vec<String> {
        let text = read(assets, "scene-map.json");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        v["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["match"][0].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_root_asset_owns_the_rules_and_a_genres_own_are_seen_first() {
        let assets = scratch("layered-rules");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        write_map(
            &assets,
            r#""_note": "xianxia only", "rules": [{"match": ["sect"], "effect": ["hall"]}]"#,
        );
        write_map(
            &dep_tree(&assets, "common"),
            r#""_note": "the world", "rules": [{"match": ["rain"]}, {"match": ["night"]}], "layers": {"effect": {"trim": 1.0}}"#,
        );

        resolve(&assets, false).unwrap();
        assert_eq!(
            matches_of(&assets),
            vec!["sect", "rain", "night"],
            "the genre's rule is seen first and the world's sit behind it"
        );
        let text = read(&assets, "scene-map.json");
        assert!(
            text.contains(r#""match": ["sect"]"#),
            "the genre's own rule is still its own bytes: {text}"
        );
        assert!(
            text.contains("\"layers\""),
            "a member it never stated came in: {text}"
        );
        assert!(
            text.contains("xianxia only") && !text.contains("the world"),
            "its own note is the one that stays: {text}"
        );

        // Idempotent: the same answer, and the file is left alone.
        let before = read(&assets, "scene-map.json");
        let again = resolve(&assets, false).unwrap();
        assert!(!again.changed(), "{again:?}");
        assert_eq!(read(&assets, "scene-map.json"), before);
    }

    #[test]
    fn a_rule_the_root_drops_is_withdrawn_from_the_genre() {
        let assets = scratch("layer-withdraw");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        write_map(&assets, r#""rules": [{"match": ["sect"]}]"#);
        let dep = dep_tree(&assets, "common");
        write_map(
            &dep,
            r#""rules": [{"match": ["rain"]}, {"match": ["night"]}]"#,
        );
        resolve(&assets, false).unwrap();
        assert_eq!(matches_of(&assets), vec!["sect", "rain", "night"]);

        // The root drops `night`. A fill-in-only merge would leave it here for
        // good, which is the reason the marker exists.
        write_map(&dep, r#""rules": [{"match": ["rain"]}]"#);
        let r = resolve(&assets, false).unwrap();
        assert_eq!(r.withdrawn, 1, "{r:?}");
        assert_eq!(matches_of(&assets), vec!["sect", "rain"]);
        assert!(!resolve(&assets, false).unwrap().changed());
    }

    #[test]
    fn a_stronger_dependency_has_its_rules_seen_before_a_weaker_one() {
        // `deps` is weakest first, and a keyed name wins by replacing a value —
        // but a rule list has no key to replace, so a stronger dependency can
        // only win a scene by being matched *first*. Hence the lists fold in
        // reverse `deps` order.
        let assets = scratch("rules-order");
        std::fs::write(
            pack_path(&assets),
            r#"{"deps":["common","weapons","magic"]}"#,
        )
        .unwrap();
        write_map(&assets, r#""rules": [{"match": ["mine"]}]"#);
        write_map(
            &dep_tree(&assets, "common"),
            r#""rules": [{"match": ["common"]}]"#,
        );
        write_map(
            &dep_tree(&assets, "weapons"),
            r#""rules": [{"match": ["weapons"]}]"#,
        );
        write_map(
            &dep_tree(&assets, "magic"),
            r#""rules": [{"match": ["magic"]}]"#,
        );

        resolve(&assets, false).unwrap();
        assert_eq!(
            matches_of(&assets),
            vec!["mine", "magic", "weapons", "common"],
            "the file's own first, then the strongest dependency"
        );
        // And it settles. Asserting the list rather than only the report: a
        // withdrawal that takes the wrong block off reads as an operator edit,
        // is adopted, and the rules arrive a second time — while the record is
        // unchanged, so the counters alone would call that "up to date".
        let again = resolve(&assets, false).unwrap();
        assert!(!again.changed(), "{again:?}");
        assert_eq!(
            matches_of(&assets),
            vec!["mine", "magic", "weapons", "common"]
        );
    }

    #[test]
    fn a_genre_that_states_a_knob_keeps_its_own_and_inherits_none_of_it() {
        let assets = scratch("layer-whole");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        write_map(&assets, r#""layers": {"effect": {"trim": 0.8}}"#);
        write_map(
            &dep_tree(&assets, "common"),
            r#""layers": {"effect": {"trim": 1.0}, "music": {"level": 0.16}}"#,
        );

        resolve(&assets, false).unwrap();
        let text = read(&assets, "scene-map.json");
        assert!(
            text.contains("0.8"),
            "the genre's block is the one in force: {text}"
        );
        assert!(
            !text.contains("0.16"),
            "and none of the dependency's is half-merged into it: {text}"
        );
    }

    #[test]
    fn a_licence_line_from_the_root_survives_a_genre_stating_its_own() {
        let assets = scratch("layer-licences");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        std::fs::write(
            assets.join("LICENSES.json"),
            r#"{"_note": "ours", "background music (music/)": "Suno"}"#,
        )
        .unwrap();
        let dep = dep_tree(&assets, "common");
        std::fs::write(
            dep.join("LICENSES.json"),
            r#"{"_note": "theirs", "sound effects (effects/, injects/)": "Pixabay"}"#,
        )
        .unwrap();

        resolve(&assets, false).unwrap();
        let text = read(&assets, "LICENSES.json");
        assert!(
            text.contains("Pixabay"),
            "the line that came with the clips is still here: {text}"
        );
        assert!(text.contains("Suno"), "and the genre's own is untouched");
        assert!(
            text.contains("ours") && !text.contains("theirs"),
            "the nearer note wins: {text}"
        );
    }

    #[test]
    fn a_member_name_that_looks_like_a_record_key_is_still_withdrawn() {
        // `LICENSES.json`'s categories are prose, and a record's two separators
        // can appear inside one — so the encoding has to be injective or a
        // withdrawal reads the record as a different kind, silently drops it,
        // and the line arrives again on every resolve after that.
        let assets = scratch("layer-separators");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        std::fs::write(assets.join("LICENSES.json"), r#"{"_note": "ours"}"#).unwrap();
        let dep = dep_tree(&assets, "common");
        std::fs::write(
            dep.join("LICENSES.json"),
            r#"{"sound effects (effects/, injects/)": "Pixabay", "ns/+odd": "x"}"#,
        )
        .unwrap();

        resolve(&assets, false).unwrap();
        let text = read(&assets, "LICENSES.json");
        assert!(text.contains("Pixabay") && text.contains("odd"), "{text}");
        let again = resolve(&assets, false).unwrap();
        assert!(!again.changed(), "and it settles: {again:?}");
        assert_eq!(read(&assets, "LICENSES.json"), text);
    }

    #[test]
    fn a_palette_gains_a_mood_the_root_has_and_keeps_the_one_it_had() {
        let assets = scratch("layer-palette");
        std::fs::write(pack_path(&assets), r#"{"deps":["common"]}"#).unwrap();
        write_map(&assets, r#""music_palette": {"quiet": {"tags": ["soft"]}}"#);
        write_map(
            &dep_tree(&assets, "common"),
            r#""music_palette": {"quiet": {"tags": ["quiet", "calm"]}, "tense": {"tags": ["tense"]}}"#,
        );

        resolve(&assets, false).unwrap();
        let text = read(&assets, "scene-map.json");
        assert!(text.contains("tense"), "the mood it lacked came in: {text}");
        assert!(
            text.contains(r#""tags": ["soft"]"#),
            "and the name it had is its own, not the root's: {text}"
        );
    }

    #[test]
    fn a_later_dependency_wins_a_keyed_name_over_an_earlier_one() {
        let assets = scratch("layer-order");
        std::fs::write(pack_path(&assets), r#"{"deps":["common","xianxia-base"]}"#).unwrap();
        write_map(
            &dep_tree(&assets, "common"),
            r#""music_palette": {"tense": {"tags": ["common-tense"]}}"#,
        );
        write_map(
            &dep_tree(&assets, "xianxia-base"),
            r#""music_palette": {"tense": {"tags": ["genre-tense"]}}"#,
        );

        resolve(&assets, false).unwrap();
        let text = read(&assets, "scene-map.json");
        assert!(text.contains("genre-tense"), "the later one wins: {text}");
        assert!(
            !text.contains("common-tense"),
            "and the earlier one is gone: {text}"
        );
    }

    #[test]
    fn a_keyed_member_accumulates_across_dependencies() {
        // The shape a real asset now has: `common` states the world's words,
        // `weapons` the ones for its own clips. Every dependency's names are
        // *added* to the member. Losing a weaker dependency's whole table
        // because a stronger one states the same member is the one outcome that
        // would make the vocabulary unusable — and the quietest, because the
        // file still parses and the names are simply gone.
        let assets = scratch("layer-keyed");
        std::fs::write(pack_path(&assets), r#"{"deps":["common","weapons"]}"#).unwrap();
        std::fs::create_dir_all(dep_tree(&assets, "common")).unwrap();
        std::fs::write(
            dep_tree(&assets, "common").join("tag-aliases.json"),
            r#"{"sound": {"knock": "door-knock", "clang": "metal-hit"}}"#,
        )
        .unwrap();
        std::fs::write(
            dep_tree(&assets, "weapons").join("tag-aliases.json"),
            r#"{"sound": {"slash": "sword-slash"}}"#,
        )
        .unwrap();

        resolve(&assets, false).unwrap();
        let text = read(&assets, "tag-aliases.json");
        for name in ["knock", "clang", "slash"] {
            assert!(text.contains(name), "{name} survived: {text}");
        }
    }

    #[test]
    fn a_stronger_dependency_takes_a_whole_member_a_weaker_one_filled() {
        // `Whole` means the *file's* member wins, not the first dependency's to
        // arrive: the fill runs weakest first, and a knob a weak dependency set
        // would otherwise be impossible for a strong one to correct.
        let assets = scratch("layer-whole-order");
        std::fs::write(pack_path(&assets), r#"{"deps":["common","xianxia-base"]}"#).unwrap();
        write_map(&dep_tree(&assets, "common"), r#""pause": {"min_s": 1.0}"#);
        write_map(
            &dep_tree(&assets, "xianxia-base"),
            r#""pause": {"min_s": 3.0}"#,
        );

        resolve(&assets, false).unwrap();
        let text = read(&assets, "scene-map.json");
        assert!(
            text.contains("3.0"),
            "the stronger dependency's knob: {text}"
        );
        assert!(!text.contains("1.0"), "and not the weaker one's: {text}");
    }
}
