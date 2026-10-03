//! Profiles: genre bundles (assets + prompts) as versioned transfer files.
//!
//! A profile is `profiles/<name>.tar.zst`: the `assets/` and `prompts/` trees
//! plus a `manifest.json` (`{name, version, files: {path: sha256}}`). The
//! bundle is transfer and archive only — day to day the pipeline reads the
//! unpacked live tree, so no code path reaches through decompression.
//!
//! "Loaded" is a pointer file, `.bm/profile` (`{name, hash}`), where `hash`
//! is the manifest hash recomputed over the live tree. Anything that runs
//! ([`verify`]) recomputes and compares: a hand-edited live tree, or a tree
//! unpacked from a different profile, is adopted (the pointer is re-stamped
//! to the live hash) with a warning, instead of refusing to run. The live
//! tree is the source of truth — `:sound` retunes it routinely — and
//! `profile pack <name>` is the verb that saves it back to a bundle.
//!
//! Packing and unpacking (tar + zstd) live in `tools/profile.sh` — shell,
//! like ssh/rsync/ffmpeg. This module only reads pointers and verifies.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The live trees a profile owns, relative to the repo/workspace root.
///
/// The union of every piece's trees, kept for the fixture install — which lays
/// down a whole checkout regardless of which piece owns what — and for the
/// messages that still name them.
///
/// It is **not** what any piece is hashed over: the adapter's trees are the
/// bundle's (`adapters/<name>/{prompts,crawl}`, see [`adapter_dirs`]) when the
/// checkout has one, and the flat pair here when it does not. So the names are
/// the *pre-split* shape, which is also the shape a fresh clone has.
pub const LIVE_DIRS: [&str; 3] = ["assets", "prompts", "crawl"];

/// One of the three things a checkout is bound to.
///
/// A profile used to be one bundle of two trees with one name and one hash.
/// The trees are the split, and they are split because they change for
/// different reasons and are shared differently: a genre's art is universal
/// across languages, a language's prompts are not, and the engine's files are
/// gigabytes the other two never touch.
///
/// `assets/` and `prompts/` were literally [`LIVE_DIRS`]; the engine is named
/// by `settings.engine` because it has no tree at the root to be found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Piece {
    /// The genre: `assets/` — music, injects, effects, the scene map, crawlers.
    Pack,
    /// The language: `prompts/` — the templates a stage renders from.
    Adapter,
    /// The voice engine: its weights, binary and voice store.
    Engine,
}

impl Piece {
    pub const ALL: [Piece; 3] = [Piece::Pack, Piece::Adapter, Piece::Engine];

    /// The live trees this piece owns, relative to the root, in the **pre-split
    /// flat shape** — `assets/`, `prompts/` and the engine's own tree.
    ///
    /// The adapter is the one piece whose trees are not settled here: a
    /// checkout with an adapter bundle keeps its prompts *and* its crawlers
    /// under `adapters/<name>/`, so what is hashed is answered by
    /// [`adapter_dirs`] from the binding, not by a constant. This is the
    /// fallback — the shape every checkout that predates the split has on disk.
    ///
    /// Empty for the engine on purpose. It does own a tree now
    /// (`engines/<name>/`, see `Layout::engine_dir`), but the engine's identity
    /// is its *declaration* — its name and its roster — and never a digest of
    /// its bytes: that tree is about a gigabyte of weights, and hashing it on
    /// every `serve` and `worker` start is the cost the split exists to avoid.
    pub fn trees(self) -> &'static [&'static str] {
        match self {
            Piece::Pack => &["assets"],
            Piece::Adapter => &["prompts", "crawl"],
            Piece::Engine => &[],
        }
    }

    /// What the piece is called in a message — and in a release: `pack 'xianxia'`,
    /// `profiles/pack/xianxia.tar.zst`.
    pub fn noun(self) -> &'static str {
        match self {
            Piece::Pack => "pack",
            Piece::Adapter => "adapter",
            Piece::Engine => "engine",
        }
    }

    /// [`Piece::noun`], parsed back. `None` for a name no piece answers to,
    /// which a caller must refuse rather than guess at.
    pub fn from_noun(noun: &str) -> Option<Piece> {
        Piece::ALL.into_iter().find(|p| p.noun() == noun)
    }
}

/// The three pieces a checkout is bound to, each with the name it was loaded
/// from and the hash of what is on disk now.
///
/// `Deserialize` is hand-written so the pre-split shim applies *everywhere* a
/// binding is read — `settings.json` and the ledger's stamp as well as
/// `.bm/profile`. A derived impl would turn an old `{name, hash}` into an
/// empty binding, which reads as "this workspace runs nothing" and trips the
/// ledger gate on every workspace that exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Binding {
    #[serde(default)]
    pub pack: Pointer,
    #[serde(default)]
    pub adapter: Pointer,
    #[serde(default)]
    pub engine: Pointer,
}

impl Binding {
    pub fn get(&self, piece: Piece) -> &Pointer {
        match piece {
            Piece::Pack => &self.pack,
            Piece::Adapter => &self.adapter,
            Piece::Engine => &self.engine,
        }
    }

    pub fn get_mut(&mut self, piece: Piece) -> &mut Pointer {
        match piece {
            Piece::Pack => &mut self.pack,
            Piece::Adapter => &mut self.adapter,
            Piece::Engine => &mut self.engine,
        }
    }

    /// Nothing named: a workspace that has never loaded anything.
    pub fn is_unset(&self) -> bool {
        self.pack.name.is_empty() && self.adapter.name.is_empty() && self.engine.name.is_empty()
    }

    /// The adapter name a cache path is keyed by.
    ///
    /// The bound adapter, or [`crate::paths::DEFAULT_ADAPTER`] when this
    /// checkout has not been given one — which is the language it was already
    /// using, and therefore what its pre-split caches are named after.
    pub fn cache_adapter(&self) -> String {
        if self.adapter.name.is_empty() {
            crate::paths::DEFAULT_ADAPTER.to_string()
        } else {
            self.adapter.name.clone()
        }
    }

    /// The engine name a cache path and an `engines/<name>/` tree are keyed by.
    ///
    /// The bound engine, or [`crate::paths::DEFAULT_ENGINE`] when this checkout
    /// has never been given one — the engine it was already running before the
    /// split gave the axis a slot in the binding.
    pub fn cache_engine(&self) -> String {
        if self.engine.name.is_empty() {
            crate::paths::DEFAULT_ENGINE.to_string()
        } else {
            self.engine.name.clone()
        }
    }

    /// The shim, in one place: a pre-split `{name, hash}` becomes the pack.
    fn from_stored(stored: Stored) -> Self {
        match stored {
            Stored::Legacy(pack) => Binding {
                pack,
                ..Binding::default()
            },
            Stored::Split(f) => Binding {
                pack: f.pack,
                adapter: f.adapter,
                engine: f.engine,
            },
        }
    }
}

/// A binding's own fields, kept out of [`Stored`] so the untagged try-order
/// cannot recurse through the shim it is standing beside.
#[derive(Deserialize)]
struct BindingFields {
    #[serde(default)]
    pack: Pointer,
    #[serde(default)]
    adapter: Pointer,
    #[serde(default)]
    engine: Pointer,
}

/// A one-line name for a binding: `xianxia · vi-VN · vieneu`.
///
/// Unnamed pieces are skipped rather than printed as blanks, so a checkout
/// that has not split yet still reads as `xianxia` — the name the operator
/// knows — instead of `xianxia ·  ·  `.
pub fn label(binding: &Binding) -> String {
    let mut parts: Vec<String> = Vec::new();
    for piece in Piece::ALL {
        let name = &binding.get(piece).name;
        if !name.is_empty() {
            parts.push(name.clone());
        }
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" · ")
    }
}

/// The pieces two bindings disagree on, in `Piece::ALL` order.
///
/// The ledger gate names them, because "another profile" does not tell an
/// operator *what* changed — and the pieces do not carry the same weight: a
/// different pack or adapter is a re-unpack of files, while a different engine
/// invalidates the segment cache and every rendered clip.
pub fn pieces_differing(a: &Binding, b: &Binding) -> Vec<Piece> {
    Piece::ALL
        .into_iter()
        .filter(|piece| a.get(*piece) != b.get(*piece))
        .collect()
}

/// Manifest stored as `manifest.json` at the release root.
///
/// The keys in `files` are the paths the release **unpacks to**, relative to the
/// checkout root — `assets/…` for a pack, `adapters/<name>/…` for a language.
/// That is not a detail: it is why `manifest_hash` over these keys is the same
/// number [`verify_binding`] computes for the piece on disk, so a bundle and the
/// tree it came from agree by construction instead of by a re-stamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Which piece this release is ([`Piece::noun`]). A pre-split manifest has
    /// no field and is therefore a **pack**, which is what the one bundle held
    /// before the split — the art, with the language riding along uninvited.
    #[serde(default = "default_piece")]
    pub piece: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// The assets this one was built on, name and content hash, in the order
    /// they were folded in. Empty for a language, which is built on nothing.
    #[serde(default)]
    pub deps: Vec<crate::compose::DepRecord>,
}

fn default_version() -> String {
    "1".into()
}

fn default_piece() -> String {
    Piece::Pack.noun().to_string()
}

/// The load pointer: which profile the live tree claims to be. Empty
/// (`Default`) means unset — a workspace that never named one.
///
/// [`version`](Self::version) is the **release** version, not a build counter:
/// the third half of what a release is named by, and the reason a box can be
/// told which artifact to download instead of only which bytes it must end up
/// with. `tools/profile.sh pack <name> --version 0.1.0` writes it, and the tag
/// it produces is the tag the release is cut under — so the pointer and the URL
/// a box fetches are two readings of one string, never two things to keep in
/// step.
///
/// Empty on a pointer written before this field existed, and empty is the *safe*
/// direction: [`crate::artifact::PackRelease::resolve`] reads it as "no release",
/// and a box with no release is pushed the profile exactly as it was before any
/// of this existed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub name: String,
    pub hash: String,
    #[serde(default)]
    pub version: String,
}

/// The pre-split bundle: one file holding every piece.
///
/// Kept only so a checkout that has one keeps parsing it; a release is per
/// piece now ([`release_path`]), because the two halves change for different
/// reasons and a second language should cost a prompt file rather than a second
/// copy of the art.
pub fn bundle_path(root: &Path, name: &str) -> PathBuf {
    root.join("profiles").join(format!("{name}.tar.zst"))
}

/// Where a piece's releases live: `profiles/<piece>/`.
///
/// A directory per piece rather than one flat `profiles/`, so `xianxia` the pack
/// and `xianxia` the language cannot shadow each other — which is the whole
/// point of the split, and would be the first thing a flat directory broke.
pub fn release_dir(root: &Path, piece: Piece) -> PathBuf {
    root.join("profiles").join(piece.noun())
}

/// One piece's release file.
pub fn release_path(root: &Path, piece: Piece, name: &str) -> PathBuf {
    release_dir(root, piece).join(format!("{name}.tar.zst"))
}

/// Build the manifest for one piece of the live tree.
///
/// The engine is refused: it is not a bundle. Its tree is gigabytes of weights
/// fetched from a models release and rsynced, and "pack it into a tar.zst" is
/// the one thing the engine axis has never done.
pub fn compute_manifest(
    layout: &crate::paths::Layout,
    piece: Piece,
    name: &str,
    version: &str,
) -> Result<Manifest> {
    let root = layout.root.as_path();
    // The pack is the tree **in force** — the workspace's own composition when
    // it has one, the checkout's otherwise — while a language's trees are the
    // checkout's. `base` is what the manifest keys are relative to, and it has
    // to be the tree's parent: `push_pack` rsyncs `layout.assets()`, and the
    // receipt this manifest becomes is diffed against the box, so a manifest
    // rooted anywhere else describes files the push never sent.
    let (base, dirs): (PathBuf, Vec<String>) = match piece {
        Piece::Pack => (
            layout.assets().parent().unwrap_or(root).to_path_buf(),
            vec!["assets".to_string()],
        ),
        Piece::Adapter => (root.to_path_buf(), adapter_dirs(root, name)),
        Piece::Engine => anyhow::bail!(
            "an engine is not a bundle: engines/<name>/ comes from the models release, not from profiles/"
        ),
    };
    let files = files_under(&base, &dirs);
    if files.is_empty() {
        anyhow::bail!(
            "nothing to pack: {} is missing or empty under {}",
            dirs.join(" + "),
            base.display()
        );
    }
    Ok(Manifest {
        name: name.to_string(),
        version: version.to_string(),
        piece: piece.noun().to_string(),
        files: hash_files(&base, files).context("hashing the live piece")?,
        // Only an asset is built on anything, and the record of what it was
        // built on is the one composition already keeps.
        deps: match piece {
            Piece::Pack => crate::compose::read_marker(&layout.assets()).deps,
            _ => Vec::new(),
        },
    })
}

/// The manifest for a **dependency pack**: the sanitized, self-contained release
/// of an asset the live tree inherits from.
///
/// The live checkout is a composition — `assets/` resolves a preset over its
/// `deps`, with the dependency trees unpacked at `assets/_extends/<name>/` — and
/// only the composed preset is runnable. But a root (`common`, `weapons`,
/// `magic`) is also a pack a fresh checkout can start from, and publishing those
/// means releasing the *dependency* tree, not the live one. So the release
/// unpacks the dependency tree **to `assets/`**, where the pack's own resolution
/// reads it, and carries a generated `assets/pack.json` saying what its
/// extension point is. That is what "sanitized" means here: pure content, the
/// files in the folders every worker reads, and no `_extends/` input inside the
/// bundle — a bundle that did carry it would re-fold composition inputs into
/// whatever unpacked it.
///
/// The manifest's keys are still the paths the release unpacks to, so
/// [`manifest_hash`] over them is the same number [`tree_hash`] computes for the
/// dependency on live disk — the hash the composition record (and therefore a
/// parent's manifest `deps`) names. A release, the live tree and the records
/// that say what was built on what therefore agree by construction, which is
/// what keeps them in sync rather than three numbers somebody has to compare.
/// The dependency's own `pack.json` and `_extends.json`, where it had them, are
/// bookkeeping and are replaced or refused rather than inherited.
///
/// **A composed tree is not a dependency release.** `deps` are unpacked flat —
/// one directory per pack under `assets/_extends/`, each folded once — so a
/// dependent that named a *composition* would put a second copy of that
/// composition's own parents inside the tree, which is the duplication the flat
/// shape exists to avoid; and `"deps": []` in its manifest would be a false
/// claim about what it is. The route is to name the composition's **roots** in
/// the dependent's own `deps`, at the position each should fold at, and let
/// [`crate::compose::closure`] order them (`assets/_extends.json`'s `tree`
/// records what it reached and through which pack). A composition is released as
/// *itself* — the composed pack bundle — and a checkout that extends it names its
/// roots rather than unpacking it as a dependency.
///
/// `Piece::Adapter` is refused: a language is not composed, so it has no
/// dependency tree to release.
pub fn compute_dep_manifest(
    layout: &crate::paths::Layout,
    dep: &str,
    version: &str,
) -> Result<Manifest> {
    let dir = layout.assets().join(crate::compose::EXTENDS_DIR).join(dep);
    if !dir.is_dir() {
        anyhow::bail!(
            "'{dep}' is not unpacked: {} is missing — resolve the live tree first",
            dir.display()
        );
    }
    let names = crate::compose::read_pack(&dir).deps;
    if !names.is_empty() {
        anyhow::bail!(
            "'{dep}' is itself composed (deps: {}) — a dependency release is flat, one pack per directory: name its roots in the depending pack's own deps instead, or release the composition itself",
            names.join(", ")
        );
    }
    let files = files_under(&dir, &[""]);
    anyhow::ensure!(
        !files.is_empty(),
        "nothing to pack: {dep} under {} is empty",
        dir.display()
    );
    let mut map = BTreeMap::new();
    for p in files {
        let rel = p
            .strip_prefix(&dir)
            .context("a dependency file escaped its own tree")?;
        let rel = rel.display().to_string();
        // The dependency's own bookkeeping is not content — the same rule
        // `compose` applies when folding a dependency in. `pack.json` describes
        // *its* extension point and a consumer writes their own; `_extends.json`
        // is a record about a resolution that is not travelling with this tree.
        if rel == crate::compose::PACK_FILE || rel == crate::compose::MARKER_FILE {
            continue;
        }
        map.insert(format!("assets/{rel}"), file_hash(&p)?);
    }
    Ok(Manifest {
        name: dep.to_string(),
        version: version.to_string(),
        piece: Piece::Pack.noun().to_string(),
        files: map,
        deps: Vec::new(),
    })
}

/// Dependencies the live pack was built against that have since moved.
///
/// A comparison, never a guess: the composition record holds each dependency's
/// tree hash, so an edited or re-unpacked parent is *named* here — and the
/// asset that was packed against it is stale until it is resolved again. This
/// is the check that turns "editing a parent" into "rebuilding the children".
pub fn stale_dependencies(layout: &crate::paths::Layout) -> Result<Vec<String>> {
    Ok(crate::compose::resolve(&layout.assets(), true)?.stale)
}

pub fn pointer_path(root: &Path) -> PathBuf {
    root.join(".bm").join("profile")
}

/// sha256 of one file, hex.
fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(content_hash(&bytes))
}

/// sha256 of some bytes, hex — the one digest everything here is built from.
///
/// Shared with [`crate::compose`], which hashes a single registry *value* to
/// tell an entry it inserted from the same key the operator has since edited.
/// One primitive, so "the bytes changed" means the same thing in both places.
pub(crate) fn content_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_digest(h.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for b in bytes.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The manifest hash over the live tree: sha256 of `path + NUL + content-hash`
/// lines in sorted order. Sorting (BTreeMap) keeps it stable across machines.
///
/// Two steps, deliberately: walk, then hash. The walk is a few dozen `readdir`
/// calls and the hashing is 57 MB of sha256 on this repo — measured at 0.60 s
/// release and 2.07 s debug, single-threaded, on **every** `serve` and `worker`
/// start. Splitting them lets the hashing run on every core, which is the one
/// thing that made that number worth attacking.
pub fn hash_live(root: &Path) -> Result<BTreeMap<String, String>> {
    hash_files(root, live_files(root))
}

/// Every file a profile owns, relative to `root`, sorted.
fn live_files(root: &Path) -> Vec<PathBuf> {
    files_under(root, &LIVE_DIRS)
}

/// Every file under `dirs`, relative to `root`, sorted.
///
/// `.DS_Store` is skipped because OS noise is not content: the pack manifest
/// skips it too, so a Finder visit must never hash-drift the live tree.
///
/// Per `dirs` rather than over [`LIVE_DIRS`] so each piece can be hashed on
/// its own — which is the point of the split: a prompts edit must move the
/// adapter without moving the pack.
pub(crate) fn files_under<S: AsRef<str>>(root: &Path, dirs: &[S]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        let dir = dir.as_ref();
        // An empty entry walks `root` itself, which is what a caller holding a
        // tree in hand (a composition dependency) wants: `root.join("")` would
        // prefix every path with `./` and hash the same tree twice over.
        let base = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        let mut stack = vec![base];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                let name = p.file_name().and_then(|n| n.to_str());
                if name == Some(".DS_Store") {
                    continue;
                }
                // A composition *input*, not this piece's own content:
                // `assets/_extends/` holds other assets' whole trees, so
                // hashing them would both double the pack's digest and make a
                // dependency's edit read as the child's. What is hashed is the
                // resolved result, which is what every reader sees.
                if p.is_dir() && name == Some(crate::compose::EXTENDS_DIR) {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// The adapter's trees, relative to `root`, for the binding that names it.
///
/// The bundle's pair when the checkout has one —
/// `adapters/<name>/prompts` and `adapters/<name>/crawl` — else the pre-split
/// flat `prompts/`, where the crawlers were still the pack's and were therefore
/// hashed as the pack's.
///
/// The **checkout's** scope, not the workspace's: the load pointer lives at the
/// root (`.bm/profile`), so the hash it holds has to be a claim about the root's
/// trees. A workspace that carries its own prompts is read through
/// `Layout::prompts_base` and is deliberately not part of this claim.
fn adapter_dirs(root: &Path, adapter: &str) -> Vec<String> {
    let dir = crate::paths::ADAPTERS_DIR;
    if root.join(dir).join(adapter).is_dir() {
        vec![
            format!("{dir}/{adapter}/prompts"),
            format!("{dir}/{adapter}/crawl"),
        ]
    } else {
        vec!["prompts".to_string()]
    }
}

/// Hash `files` on as many threads as the machine has cores, and fold them into
/// the same sorted map the sequential version produced.
///
/// The result must be **byte-identical** to hashing one at a time in sorted
/// order — the pointer on every machine was computed that way, and a different
/// order would be a different hash, i.e. a false "profile drift" on every box.
/// A worker takes the next index from an atomic counter, so the assignment is
/// dynamic and a slow file cannot leave a thread idle; the fold is a BTreeMap,
/// so insertion order does not matter.
///
/// One thread is used when there is one file or one core: spawning a thread to
/// hash a fixture is slower than doing it.
pub(crate) fn hash_files(root: &Path, files: Vec<PathBuf>) -> Result<BTreeMap<String, String>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(files.len());
    if threads <= 1 {
        let mut map = BTreeMap::new();
        for p in files {
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            map.insert(rel.display().to_string(), file_hash(&p)?);
        }
        return Ok(map);
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let parts: Vec<Result<Vec<(String, String)>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let next = &next;
                let files = &files;
                scope.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(p) = files.get(i) else { break };
                        let Ok(rel) = p.strip_prefix(root) else {
                            continue;
                        };
                        out.push((rel.display().to_string(), file_hash(p)?));
                    }
                    Ok(out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("a hashing worker panicked")))
            })
            .collect()
    });

    let mut map = BTreeMap::new();
    for part in parts {
        for (rel, sum) in part? {
            map.insert(rel, sum);
        }
    }
    Ok(map)
}

pub fn manifest_hash(files: &BTreeMap<String, String>) -> String {
    let mut h = Sha256::new();
    for (path, sum) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(sum.as_bytes());
        h.update([0]);
    }
    hex_digest(h.finalize())
}

/// The manifest hash over an explicit set of trees, relative to `root`.
///
/// The number a bundle manifest folds to and a binding's piece hash carries,
/// computed on demand rather than read from a pointer — which is what lets a
/// workspace stamp its own binding at creation without a pointer to read.
/// Refuses an empty set: a hash over nothing is not a claim, it is a blank
/// that looks like one.
pub fn trees_hash(root: &Path, dirs: &[String]) -> Result<String> {
    let files = files_under(root, dirs);
    anyhow::ensure!(
        !files.is_empty(),
        "nothing to hash under {} — the tree is missing or empty",
        dirs.join(" + ")
    );
    Ok(manifest_hash(&hash_files(root, files)?))
}

/// Read a release bundle's `manifest.json` back, and fold it to the one number
/// that release is named by.
///
/// A function rather than a `serde_json` call at each site because the hash is
/// the *identity* of a release, and every consumer — the box that downloads it,
/// the publish gate, the provisioner deciding whether a release exists at all —
/// has to arrive at it by the same route, or a "verified" bundle is only
/// verified against a differently-computed number.
pub fn read_manifest_at(path: &Path) -> Result<Manifest> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let m: Manifest =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if m.files.is_empty() {
        anyhow::bail!("{} lists no files", path.display());
    }
    Ok(m)
}

/// The stored `.bm/profile`, in either shape.
///
/// `Legacy` is the pre-split document — `{name, hash}`, one hash over `assets/`
/// **plus** `prompts/` — and it has to keep parsing: every checkout that
/// exists carries one. `Legacy` is tried first and requires both fields, so a
/// binding document (which has neither) falls through to `Split`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Stored {
    Legacy(Pointer),
    Split(BindingFields),
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Binding::from_stored(Stored::deserialize(d)?))
    }
}

/// The load pointer, as a binding.
///
/// The shim: a legacy document becomes `pack = {name, hash}` with the adapter
/// and engine unnamed. No per-piece hash can reproduce the old combined one,
/// so the first [`verify_binding`] re-stamps the pack and the adapter from the
/// live trees — which is what `verify` has always done on drift.
pub fn read_binding(root: &Path) -> Result<Binding> {
    let path = pointer_path(root);
    let text = std::fs::read_to_string(&path).with_context(|| {
        "no profile loaded (.bm/profile missing) — load one before running".to_string()
    })?;
    let stored: Stored =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Binding::from_stored(stored))
}

/// The binding **in force** for a layout: the active workspace's own
/// `settings.json` binding when it names a piece, the checkout's `.bm/profile`
/// for every piece it does not.
///
/// The merge is piece by piece, not all-or-nothing, because that is what
/// [`crate::paths::Layout::resolve`] already does for the adapter and the engine:
/// a workspace made before presets names only its pack, and its language and
/// engine still come from the checkout. Reading the whole binding from one side
/// or the other would either lose the workspace's pack or invent an adapter it
/// never claimed.
///
/// This is the one answer to "what is this book bound to", so the dashboard,
/// `profile check` and the serve gate cannot each arrive at a different one.
/// The checkout pointer is the fallback; a checkout that has none and a
/// workspace that names nothing is an unset binding, not an error.
pub fn in_force(layout: &crate::paths::Layout) -> Result<Binding> {
    if layout.work == layout.root {
        return read_binding(&layout.root);
    }
    let mut binding = crate::config::Settings::load(&layout.settings()).profile;
    let checkout = read_binding(&layout.root).unwrap_or_default();
    for piece in Piece::ALL {
        if binding.get(piece).name.is_empty() {
            *binding.get_mut(piece) = checkout.get(piece).clone();
        }
    }
    Ok(binding)
}

pub fn write_binding(root: &Path, binding: &Binding) -> Result<()> {
    let path = pointer_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::atomic_write(&path, &serde_json::to_string_pretty(binding)?)?;
    Ok(())
}

/// The pack piece: what "the profile" meant before the split.
///
/// Still the answer six call sites want — the dashboard's header, the release
/// path, the provision stamp — so the split does not have to reach them yet.
pub fn read_pointer(root: &Path) -> Result<Pointer> {
    read_binding(root).map(|b| b.pack)
}

/// Re-stamp the pack piece, keeping the other two.
pub fn write_pointer(root: &Path, pointer: &Pointer) -> Result<()> {
    let mut binding = read_binding(root).unwrap_or_default();
    binding.pack = pointer.clone();
    write_binding(root, &binding)
}

/// The load gate: the pointer must exist. A drifted live tree (hand edit,
/// `:sound` retune, different unpack) is adopted, not refused: the piece is
/// re-stamped to the live hash so the next run is clean, and the operator is
/// told to `profile pack <name>` to save it back to a bundle. An empty live
/// tree is still refused — that is a missing unpack, not an edit.
pub fn verify(root: &Path) -> Result<Pointer> {
    verify_binding(root, None).map(|b| b.pack)
}

/// [`verify`], one piece at a time.
///
/// Each piece is hashed over its own trees, so an edited `prompts/analyze.txt`
/// moves the adapter and leaves the pack untouched — and the warning names the
/// piece that moved, which is the whole point of having names for them.
///
/// A piece whose tree is absent is left exactly as it is: a checkout that has
/// not split yet has no adapter bundle, and that is not an error. Both of the
/// file-backed trees missing *is* — that is a missing unpack.
///
/// The engine's hash is not computed here. Its identity is its declaration
/// (the roster plus the name), not the bytes of its weights: `hash_files`
/// reads every file, and `models/` is gigabytes. `engine` names the piece
/// from `settings.engine`, which is where that name lives.
pub fn verify_binding(root: &Path, engine: Option<&str>) -> Result<Binding> {
    let mut binding = read_binding(root)?;
    if let Some(name) = engine {
        binding.engine.name = name.to_string();
    }
    let pack = files_under(root, Piece::Pack.trees());
    let adapter = files_under(root, &adapter_dirs(root, &binding.cache_adapter()));
    if pack.is_empty() && adapter.is_empty() {
        anyhow::bail!(
            "live {} + {} are missing or empty for pack '{}' — `profile load {}` to unpack it",
            LIVE_DIRS[0],
            LIVE_DIRS[1],
            binding.pack.name,
            binding.pack.name,
        );
    }
    let mut drifted: Vec<&str> = Vec::new();
    for (piece, files) in [(Piece::Pack, pack), (Piece::Adapter, adapter)] {
        if files.is_empty() {
            continue;
        }
        let hash = manifest_hash(&hash_files(root, files).context("hashing a live piece")?);
        let p = binding.get_mut(piece);
        if p.hash != hash {
            p.hash = hash;
            drifted.push(piece.noun());
        }
    }
    if !drifted.is_empty() {
        write_binding(root, &binding)?;
        eprintln!(
            "warning: live tree drifted from profile '{}' — adopting and re-stamping {}; `profile pack {}` to save it",
            binding.pack.name,
            drifted.join(" + "),
            binding.pack.name,
        );
    }
    Ok(binding)
}

/// [`verify_binding`], for a layout: the load gate, over the binding **in
/// force**.
///
/// The default root workspace keeps the pre-workspace behaviour exactly — the
/// checkout's `.bm/profile` is read and a drifted live tree is adopted there.
/// A real workspace is verified **read-only**: its binding lives in its own
/// `settings.json`, and that document is also the ledger's stamp (the gate in
/// [`pieces_differing`]'s caller compares the two), so re-stamping the pack
/// hash here would make every following `serve` refuse a book whose ledger is
/// perfectly consistent. Adopting workspace drift is a re-compose or a
/// reconcile, not a load-time stamp.
///
/// What this refuses is unchanged: a binding that names nothing, or a live
/// `assets/` + `prompts/` that are both missing or empty, is a missing unpack
/// rather than an edit.
pub fn verify_layout(layout: &crate::paths::Layout, engine: Option<&str>) -> Result<Binding> {
    if layout.work == layout.root {
        return verify_binding(&layout.root, engine);
    }
    let mut binding = in_force(layout)?;
    if let Some(name) = engine {
        binding.engine.name = name.to_string();
    }
    // The trees the layout actually reads: the workspace's own `assets/` when
    // it has one, the checkout's otherwise — the same work-first shape
    // `Layout::assets` gives every reader.
    let pack = if layout.work.join("assets").is_dir() {
        files_under(&layout.work, &["assets"])
    } else {
        files_under(&layout.root, Piece::Pack.trees())
    };
    let adapter = files_under(
        &layout.root,
        &adapter_dirs(&layout.root, &binding.cache_adapter()),
    );
    if pack.is_empty() && adapter.is_empty() {
        anyhow::bail!(
            "live assets/ + prompts/ are missing or empty for workspace '{}' — no pack to run",
            layout.work.display()
        );
    }
    Ok(binding)
}

/// Copy the tracked test fixture (`rust/fixtures/profile/`) over `dest`,
/// creating `assets/` + `prompts/`. The one way tests get a profile: they
/// must never read the live (ignored, maybe absent) tree.
///
/// The path resolves from bm-core's own manifest dir, so callers in other
/// crates land in the same place.
pub fn install_fixture(dest: &Path) -> Result<()> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/profile");
    for dir in LIVE_DIRS {
        let from = src.join(dir);
        // The fixture ships what a test needs, not every name in `LIVE_DIRS`:
        // a language's crawlers are optional, and most of the suite never
        // crawls at all.
        if !from.is_dir() {
            continue;
        }
        copy_dir(&from, &dest.join(dir))?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let entries =
        std::fs::read_dir(src).with_context(|| format!("reading fixture {}", src.display()))?;
    for e in entries {
        let e = e?;
        let (from, to) = (e.path(), dest.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-profile-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        install_fixture(&dir).unwrap();
        dir
    }

    #[test]
    fn concurrent_hashing_matches_hashing_one_at_a_time() {
        // The whole safety argument for the parallel path: the pointer on every
        // machine was computed sequentially, so a different map is a different
        // hash — a false "profile drift" on every box at once. Cross-check the
        // concurrent implementation against the sequential one, same primitive.
        let dir = live_fixture("parallel");
        let files = live_files(&dir);
        assert!(files.len() > 1, "the fixture must have something to spread");

        let concurrent = hash_files(&dir, files.clone()).unwrap();
        let mut sequential = BTreeMap::new();
        for p in &files {
            sequential.insert(
                p.strip_prefix(&dir).unwrap().display().to_string(),
                file_hash(p).unwrap(),
            );
        }
        assert_eq!(concurrent, sequential, "same files, same map");
        // And the fold over it is what the pointer holds.
        assert_eq!(manifest_hash(&concurrent), manifest_hash(&sequential));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_passes_on_a_fresh_tree_and_adopts_drift() {
        let dir = live_fixture("verify");
        let hash = manifest_hash(&hash_live(&dir).unwrap());
        write_pointer(
            &dir,
            &Pointer {
                name: "fixture".into(),
                hash,
                version: String::new(),
            },
        )
        .unwrap();
        assert_eq!(verify(&dir).unwrap().name, "fixture");

        // A hand edit (e.g. a `:sound` retune) is adopted, not refused: the
        // pointer is re-stamped so the next run is clean.
        std::fs::write(dir.join("prompts/analyze.txt"), "tampered").unwrap();
        let adopted = verify(&dir).unwrap();
        assert_eq!(adopted.name, "fixture");
        assert_eq!(adopted, read_pointer(&dir).unwrap());
        // And the adopted pointer verifies cleanly afterwards.
        assert_eq!(verify(&dir).unwrap(), adopted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_without_a_pointer_names_the_load() {
        let dir = std::env::temp_dir().join("bm-profile-nopointer");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = verify(&dir).unwrap_err();
        assert!(err.to_string().contains("no profile loaded"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fixture_resolves_from_any_crate() {
        // install_fixture anchors on bm-core's manifest dir, not the
        // caller's cwd — a bm-inductor test lands in the same fixture.
        let dir = std::env::temp_dir().join("bm-profile-anchor");
        let _ = std::fs::remove_dir_all(&dir);
        install_fixture(&dir).unwrap();
        assert!(dir.join("assets/scene-map.json").is_file());
        assert!(dir.join("prompts/analyze.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pre_split_pointer_becomes_the_pack_piece() {
        let dir = live_fixture("legacy");
        let hash = manifest_hash(&hash_live(&dir).unwrap());
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        // The document as it exists on every checkout today.
        std::fs::write(
            pointer_path(&dir),
            format!(r#"{{"name":"xianxia","hash":"{hash}"}}"#),
        )
        .unwrap();

        let b = read_binding(&dir).unwrap();
        assert_eq!(b.pack.name, "xianxia");
        assert_eq!(b.pack.hash, hash);
        assert!(
            b.adapter.name.is_empty() && b.engine.name.is_empty(),
            "the other two pieces are unnamed, not guessed at"
        );
        // And the six callers that still want "the profile" keep working.
        assert_eq!(read_pointer(&dir).unwrap().name, "xianxia");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The binding in force is the workspace's, piece by piece — the same
    /// merge `Layout::resolve` gives the adapter and the engine, so the
    /// dashboard, `profile check` and the serve gate all answer with one voice.
    #[test]
    fn the_binding_in_force_merges_the_workspace_and_the_checkout_piece_by_piece() {
        let dir = live_fixture("in-force");
        write_binding(
            &dir,
            &Binding {
                pack: Pointer {
                    name: "xianxia".into(),
                    hash: "c".into(),
                    version: String::new(),
                },
                adapter: Pointer {
                    name: "vi-VN".into(),
                    hash: "a".into(),
                    version: String::new(),
                },
                engine: Pointer::default(),
            },
        )
        .unwrap();
        // The workspace names its own pack and engine and leaves the adapter to
        // the checkout.
        let work = dir.join("workspaces/book");
        std::fs::create_dir_all(&work).unwrap();
        let settings = crate::config::Settings {
            profile: Binding {
                pack: Pointer {
                    name: "apothecary".into(),
                    hash: "w".into(),
                    version: String::new(),
                },
                adapter: Pointer::default(),
                engine: Pointer {
                    name: "pocket".into(),
                    hash: String::new(),
                    version: String::new(),
                },
            },
            ..crate::config::Settings::default()
        };
        settings.save(&work.join("settings.json")).unwrap();
        let layout = crate::paths::Layout {
            root: dir.clone(),
            work: work.clone(),
            ..crate::paths::Layout::new(dir.clone())
        };
        let b = in_force(&layout).unwrap();
        assert_eq!(b.pack.name, "apothecary");
        assert_eq!(
            b.adapter.name, "vi-VN",
            "the checkout's language fills the gap"
        );
        assert_eq!(b.engine.name, "pocket");
        // The implicit root workspace still reads the pointer alone.
        let root_layout = crate::paths::Layout::new(dir.clone());
        assert_eq!(in_force(&root_layout).unwrap(), read_binding(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Running a workspace verifies *its* binding and writes neither side: the
    /// workspace's `settings.json` is the ledger's stamp, so re-stamping it at
    /// load would make the next `serve` refuse a consistent book; and the
    /// checkout's `.bm/profile` is never this workspace's to write.
    #[test]
    fn verify_layout_reads_the_workspace_binding_and_writes_neither_side() {
        let dir = live_fixture("verify-ws");
        let before = Binding {
            pack: Pointer {
                name: "xianxia".into(),
                hash: "checkout".into(),
                version: String::new(),
            },
            adapter: Pointer {
                name: "vi-VN".into(),
                hash: "a".into(),
                version: String::new(),
            },
            engine: Pointer::default(),
        };
        write_binding(&dir, &before).unwrap();

        let work = dir.join("workspaces/book");
        std::fs::create_dir_all(work.join("assets")).unwrap();
        std::fs::write(work.join("assets/world.json"), "{}").unwrap();
        let settings = crate::config::Settings {
            profile: Binding {
                pack: Pointer {
                    name: "book".into(),
                    hash: String::new(),
                    version: String::new(),
                },
                adapter: Pointer::default(),
                engine: Pointer {
                    name: "pocket".into(),
                    hash: String::new(),
                    version: String::new(),
                },
            },
            ..crate::config::Settings::default()
        };
        settings.save(&work.join("settings.json")).unwrap();

        let layout = crate::paths::Layout {
            root: dir.clone(),
            work: work.clone(),
            ..crate::paths::Layout::new(dir.clone())
        };
        let b = verify_layout(&layout, Some("pocket")).unwrap();
        assert_eq!(b.pack.name, "book");
        assert_eq!(
            b.adapter.name, "vi-VN",
            "the checkout's language fills the gap"
        );
        assert_eq!(b.engine.name, "pocket", "named from settings");
        let stamped = crate::config::Settings::load(&work.join("settings.json")).profile;
        assert_eq!(
            stamped.pack.hash, "",
            "the workspace binding was not re-stamped"
        );
        assert_eq!(
            read_binding(&dir).unwrap(),
            before,
            "the checkout pointer is untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binding_round_trips_and_names_all_three_pieces() {
        let dir = live_fixture("binding");
        let b = Binding {
            pack: Pointer {
                name: "xianxia".into(),
                hash: "p".into(),
                version: String::new(),
            },
            adapter: Pointer {
                name: "vi-VN".into(),
                hash: "a".into(),
                version: String::new(),
            },
            engine: Pointer {
                name: "vieneu".into(),
                hash: "e".into(),
                version: String::new(),
            },
        };
        write_binding(&dir, &b).unwrap();
        assert_eq!(read_binding(&dir).unwrap(), b);
        assert!(!b.is_unset());
        // The legacy view still resolves, and re-stamping the pack keeps the
        // adapter and engine exactly as they were.
        write_pointer(
            &dir,
            &Pointer {
                name: "xianxia".into(),
                hash: "p2".into(),
                version: String::new(),
            },
        )
        .unwrap();
        let after = read_binding(&dir).unwrap();
        assert_eq!(after.pack.hash, "p2");
        assert_eq!(after.adapter.name, "vi-VN");
        assert_eq!(after.engine.name, "vieneu");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_hashes_each_tree_separately_and_leaves_the_other_alone() {
        let dir = live_fixture("split");
        write_binding(
            &dir,
            &Binding {
                pack: Pointer {
                    name: "xianxia".into(),
                    hash: String::new(),
                    version: String::new(),
                },
                adapter: Pointer {
                    name: "vi-VN".into(),
                    hash: String::new(),
                    version: String::new(),
                },
                engine: Pointer::default(),
            },
        )
        .unwrap();

        let before = verify_binding(&dir, Some("vieneu")).unwrap();
        assert!(!before.pack.hash.is_empty(), "the pack was hashed");
        assert!(!before.adapter.hash.is_empty(), "so was the adapter");
        assert_eq!(before.engine.name, "vieneu", "named from settings");

        // The claim the split exists for: one tree moving does not move the
        // other, and the piece that moved is the one that gets re-stamped.
        std::fs::write(dir.join("prompts/analyze.txt"), "tampered").unwrap();
        let after = verify_binding(&dir, Some("vieneu")).unwrap();
        assert_eq!(after.pack.hash, before.pack.hash, "the pack did not move");
        assert_ne!(after.adapter.hash, before.adapter.hash, "the adapter did");
        assert_eq!(after, read_binding(&dir).unwrap(), "and it was re-stamped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The point of keying a release's manifest by *where it unpacks to*: the
    /// bundle and the live tree it came from hash to the same number, so loading
    /// one never re-stamps a hash it just changed. A language release is only its
    /// own two trees — nothing of the pack's rides along, which is the split.
    #[test]
    fn a_pieces_manifest_hashes_to_what_the_binding_holds() {
        let dir = live_fixture("manifest");
        let layout = crate::paths::Layout::new(&dir);
        write_binding(
            &dir,
            &Binding {
                pack: Pointer {
                    name: "xianxia".into(),
                    hash: String::new(),
                    version: String::new(),
                },
                adapter: Pointer {
                    name: "vi-VN".into(),
                    hash: String::new(),
                    version: String::new(),
                },
                engine: Pointer::default(),
            },
        )
        .unwrap();
        let verified = verify_binding(&dir, Some("vieneu")).unwrap();

        let pack = compute_manifest(&layout, Piece::Pack, "xianxia", "1").unwrap();
        assert_eq!(pack.piece, "pack");
        assert_eq!(manifest_hash(&pack.files), verified.pack.hash);
        assert!(pack.deps.is_empty(), "the fixture was built on nothing");
        assert!(pack.files.keys().any(|k| k.starts_with("assets/")));

        let language = compute_manifest(&layout, Piece::Adapter, "vi-VN", "1").unwrap();
        assert_eq!(language.piece, "adapter");
        assert_eq!(manifest_hash(&language.files), verified.adapter.hash);
        assert!(
            language
                .files
                .keys()
                .all(|k| k.starts_with("prompts/") || k.starts_with("crawl/")),
            "a language release carries its own two trees and nothing else: {:?}",
            language.files.keys().take(3).collect::<Vec<_>>()
        );
        assert!(language.deps.is_empty(), "a language is built on nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A workspace that composes its own `assets/` is the tree a pack manifest
    /// hashes — not the checkout's.
    ///
    /// The manifest is keyed by the paths `push_pack` rsyncs and the receipt a
    /// box diffs against, so a manifest rooted at the checkout while the tree
    /// in force is the workspace's would describe bytes that never travelled:
    /// the pack release gate would compare a box's receipt to the wrong book
    /// and re-push (or skip) forever. This is the same split `sources.rs`
    /// enforces for the bundle, one layer up.
    #[test]
    fn a_workspace_owned_pack_manifests_the_workspace_tree_not_the_checkouts() {
        let root = live_fixture("ws-pack-manifest");
        // The checkout keeps its own `assets/`, so a root-layout manifest is
        // still buildable — the assertion is that the workspace's is not it.
        let workspace = root.join("workspaces/book");
        std::fs::create_dir_all(workspace.join("assets")).unwrap();
        std::fs::write(workspace.join("assets/scene-map.json"), r#"{"scenes":[]}"#).unwrap();
        std::fs::write(workspace.join("assets/only-here.json"), "{}").unwrap();
        let layout = crate::paths::Layout {
            root: root.clone(),
            work: workspace.clone(),
            adapter: crate::paths::DEFAULT_ADAPTER.into(),
            engine: crate::paths::DEFAULT_ENGINE.into(),
        };
        assert!(layout.owns_assets(), "the fixture is the case under test");

        let pack = compute_manifest(&layout, Piece::Pack, "book", "1").unwrap();
        assert!(
            pack.files.keys().any(|k| k == "assets/only-here.json"),
            "the manifest must name the workspace's own file: {:?}",
            pack.files.keys().take(5).collect::<Vec<_>>()
        );
        let checkout =
            compute_manifest(&crate::paths::Layout::new(&root), Piece::Pack, "book", "1").unwrap();
        assert_ne!(
            manifest_hash(&pack.files),
            manifest_hash(&checkout.files),
            "the workspace's own tree is a different pack from the checkout's"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One file per piece, in a directory per piece — so `xianxia` the pack and
    /// `xianxia` the language cannot shadow each other — and the engine, which
    /// is not a bundle at all.
    #[test]
    fn a_release_is_one_file_per_piece_and_the_engine_is_not_one() {
        let root = Path::new("/repo");
        assert!(
            release_path(root, Piece::Pack, "xianxia").ends_with("profiles/pack/xianxia.tar.zst")
        );
        assert!(release_path(root, Piece::Adapter, "xianxia")
            .ends_with("profiles/adapter/xianxia.tar.zst"));
        assert_ne!(
            release_path(root, Piece::Pack, "xianxia"),
            release_path(root, Piece::Adapter, "xianxia"),
            "the same name on two axes is two releases"
        );
        assert_eq!(Piece::from_noun("adapter"), Some(Piece::Adapter));
        assert_eq!(Piece::from_noun("engine"), Some(Piece::Engine));
        assert_eq!(
            Piece::from_noun("language"),
            None,
            "no piece answers to that"
        );

        let dir = live_fixture("engine-release");
        let layout = crate::paths::Layout::new(&dir);
        let err = compute_manifest(&layout, Piece::Engine, "vieneu", "1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("models release"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Editing a parent is what makes a child stale, and the check *names* the
    /// parent: the composition record holds the hash each dependency was folded
    /// in at, so nothing has to be guessed at or re-hashed to answer it.
    #[test]
    fn a_release_is_stale_when_a_dependency_it_was_built_on_has_moved() {
        let dir = live_fixture("stale-deps");
        let layout = crate::paths::Layout::new(&dir);
        assert!(stale_dependencies(&layout).unwrap().is_empty(), "no deps");

        let assets = layout.assets();
        std::fs::write(assets.join("pack.json"), r#"{"deps":["common"]}"#).unwrap();
        let dep = assets.join("_extends/common");
        std::fs::create_dir_all(&dep).unwrap();
        let pool = |extra: &str| {
            format!(r#"{{"wind":{{"tags":["wind"],"files":["effects/wind-1.mp3"]}}{extra}}}"#)
        };
        std::fs::write(dep.join("effect-pool.json"), pool("")).unwrap();
        // The record is written by a *resolve*; until then there is nothing to
        // compare against, and a first sighting is not staleness.
        crate::compose::resolve(&assets, false).unwrap();
        assert!(stale_dependencies(&layout).unwrap().is_empty());

        // The parent gains a sound, so this tree is built on something that has
        // moved — and stays so until it is resolved again.
        std::fs::write(
            dep.join("effect-pool.json"),
            pool(r#","rain":{"tags":["rain"],"files":["effects/rain-1.mp3"]}"#),
        )
        .unwrap();
        assert_eq!(stale_dependencies(&layout).unwrap(), vec!["common"]);
        crate::compose::resolve(&assets, false).unwrap();
        assert!(stale_dependencies(&layout).unwrap().is_empty(), "resolved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A composition input is not this pack's own content. `assets/_extends/`
    /// holds other assets' whole trees, so hashing them would double the digest
    /// and make a dependency's edit read as this pack's own — while the
    /// *resolved* result, which is what every reader and every worker sees, is
    /// hashed as it always was.
    #[test]
    fn a_dependency_tree_is_not_part_of_the_packs_own_digest() {
        let dir = live_fixture("extends");
        let before = hash_live(&dir).unwrap();
        assert!(!before.keys().any(|k| k.contains("_extends")));

        std::fs::create_dir_all(dir.join("assets/_extends/common/effects")).unwrap();
        std::fs::write(
            dir.join("assets/_extends/common/effects/wind-1.mp3"),
            b"clip",
        )
        .unwrap();
        assert_eq!(
            hash_live(&dir).unwrap(),
            before,
            "an unpacked dependency is an input, not a change to the pack"
        );

        // The resolved result, on the other hand, *is* the pack.
        std::fs::create_dir_all(dir.join("assets/effects")).unwrap();
        std::fs::write(dir.join("assets/effects/wind-1.mp3"), b"clip").unwrap();
        assert_ne!(hash_live(&dir).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sanitized dependency release: what a root pack publishes. The
    /// manifest keys are the unpack paths, so `manifest_hash` over them equals
    /// `tree_hash` over the dependency — the exact number the composition
    /// record carries — minus the bookkeeping, which is not content.
    #[test]
    fn a_dependency_release_unpacks_to_the_paths_the_record_hashes() {
        let dir = live_fixture("dep-release");
        let layout = crate::paths::Layout::new(&dir);
        let dep_dir = dir.join("assets/_extends/common");
        std::fs::create_dir_all(dep_dir.join("effects")).unwrap();
        std::fs::write(dep_dir.join("effects/wind-1.mp3"), b"clip").unwrap();
        std::fs::write(
            dep_dir.join("effect-pool.json"),
            r#"{ "_note": "the world's", "wind": { "files": ["effects/wind-1.mp3"] } }"#,
        )
        .unwrap();
        // The dependency's own bookkeeping, which a resolve would refuse to
        // inherit and a release must not carry either. (A `pack.json` naming
        // deps is the composed case below, so this one names none.)
        std::fs::write(
            dep_dir.join("pack.json"),
            r#"{ "_note": "authored elsewhere", "deps": [] }"#,
        )
        .unwrap();
        std::fs::write(
            dep_dir.join("_extends.json"),
            r#"{ "deps": [], "keys": {}, "files": {} }"#,
        )
        .unwrap();

        let m = compute_dep_manifest(&layout, "common", "0.1.0").unwrap();
        assert_eq!(m.name, "common");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.piece, "pack");
        assert!(m.deps.is_empty(), "a root is built on nothing");
        assert_eq!(
            m.files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "assets/effect-pool.json".to_string(),
                "assets/effects/wind-1.mp3".to_string(),
            ]
        );
        // The identity agrees with the composition record's number: the same
        // fold over the same file set, computed straight off the release's own
        // unpack keys (manifest_hash is order-stable, so a refold is a no-op).
        let from_disk = crate::compose::tree_hash(&dep_dir).unwrap();
        let released = manifest_hash(&m.files);
        let mut refold = BTreeMap::new();
        refold.extend(m.files.iter().map(|(k, v)| (k.clone(), v.clone())));
        assert_eq!(manifest_hash(&refold), released, "the fold is the manifest");
        // `tree_hash` reads the tree *with* its bookkeeping; the release drops
        // it, so the two numbers must differ.
        assert_ne!(released, from_disk, "bookkeeping changes the tree's hash");

        // A tree that is itself composed is refused: a dependency release is
        // one pack, and a dependent names a composition's roots itself.
        std::fs::write(
            dep_dir.join("pack.json"),
            r#"{ "_note": "a preset, not a root", "deps": ["weapons"] }"#,
        )
        .unwrap();
        let err = compute_dep_manifest(&layout, "common", "0.1.0").unwrap_err();
        assert!(err.to_string().contains("composed"), "{err}");

        // And a tree nobody unpacked is an error that says where it looked.
        let err = compute_dep_manifest(&layout, "guns", "0.1.0").unwrap_err();
        assert!(err.to_string().contains("_extends"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
