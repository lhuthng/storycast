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
/// messages that still name the pair.
pub const LIVE_DIRS: [&str; 2] = ["assets", "prompts"];

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

    /// The live trees this piece owns, relative to the root. Empty for the
    /// engine, whose files `Layout` names one path at a time.
    pub fn trees(self) -> &'static [&'static str] {
        match self {
            Piece::Pack => &["assets"],
            Piece::Adapter => &["prompts"],
            Piece::Engine => &[],
        }
    }

    /// What the piece is called in a message: `pack 'xianxia'`.
    pub fn noun(self) -> &'static str {
        match self {
            Piece::Pack => "pack",
            Piece::Adapter => "adapter",
            Piece::Engine => "engine",
        }
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

/// Manifest stored as `manifest.json` at the bundle root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

fn default_version() -> String {
    "1".into()
}

/// The load pointer: which profile the live tree claims to be. Empty
/// (`Default`) means unset — a workspace that never named one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub name: String,
    pub hash: String,
}

pub fn bundle_path(root: &Path, name: &str) -> PathBuf {
    root.join("profiles").join(format!("{name}.tar.zst"))
}

pub fn pointer_path(root: &Path) -> PathBuf {
    root.join(".bm").join("profile")
}

/// sha256 of one file, hex.
fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(hex_digest(h.finalize()))
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
fn files_under(root: &Path, dirs: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        let mut stack = vec![root.join(dir)];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.file_name().and_then(|n| n.to_str()) == Some(".DS_Store") {
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
    let adapter = files_under(root, Piece::Adapter.trees());
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

/// Copy the tracked test fixture (`rust/fixtures/profile/`) over `dest`,
/// creating `assets/` + `prompts/`. The one way tests get a profile: they
/// must never read the live (ignored, maybe absent) tree.
///
/// The path resolves from bm-core's own manifest dir, so callers in other
/// crates land in the same place.
pub fn install_fixture(dest: &Path) -> Result<()> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/profile");
    for dir in LIVE_DIRS {
        copy_dir(&src.join(dir), &dest.join(dir))?;
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

    #[test]
    fn a_binding_round_trips_and_names_all_three_pieces() {
        let dir = live_fixture("binding");
        let b = Binding {
            pack: Pointer {
                name: "xianxia".into(),
                hash: "p".into(),
            },
            adapter: Pointer {
                name: "vi-VN".into(),
                hash: "a".into(),
            },
            engine: Pointer {
                name: "vieneu".into(),
                hash: "e".into(),
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
                },
                adapter: Pointer {
                    name: "vi-VN".into(),
                    hash: String::new(),
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
}
