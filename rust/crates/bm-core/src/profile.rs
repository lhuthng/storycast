//! Profiles: genre bundles (assets + prompts) as versioned transfer files.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The live trees a profile owns, relative to the repo/workspace root.
pub const LIVE_DIRS: [&str; 3] = ["assets", "prompts", "crawl"];

/// One of the three things a checkout is bound to.
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
    pub fn trees(self) -> &'static [&'static str] {
        match self {
            Piece::Pack => &["assets"],
            Piece::Adapter => &["prompts", "crawl"],
            Piece::Engine => &[],
        }
    }

    /// What the piece is called in a message — and in a release: `pack 'xianxia'`,
    pub fn noun(self) -> &'static str {
        match self {
            Piece::Pack => "pack",
            Piece::Adapter => "adapter",
            Piece::Engine => "engine",
        }
    }

    /// [`Piece::noun`], parsed back. `None` for a name no piece answers to,
    pub fn from_noun(noun: &str) -> Option<Piece> {
        Piece::ALL.into_iter().find(|p| p.noun() == noun)
    }
}

/// The three pieces a checkout is bound to, each with the name it was loaded
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
    pub fn cache_adapter(&self) -> String {
        if self.adapter.name.is_empty() {
            crate::paths::DEFAULT_ADAPTER.to_string()
        } else {
            self.adapter.name.clone()
        }
    }

    /// The engine name a cache path and an `engines/<name>/` tree are keyed by.
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
pub fn pieces_differing(a: &Binding, b: &Binding) -> Vec<Piece> {
    Piece::ALL
        .into_iter()
        .filter(|piece| a.get(*piece) != b.get(*piece))
        .collect()
}

/// Manifest stored as `manifest.json` at the release root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Which piece this release is ([`Piece::noun`]). A pre-split manifest has
    #[serde(default = "default_piece")]
    pub piece: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// The assets this one was built on, name and content hash, in the order
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pointer {
    pub name: String,
    pub hash: String,
    #[serde(default)]
    pub version: String,
}

/// The pre-split bundle: one file holding every piece.
pub fn bundle_path(root: &Path, name: &str) -> PathBuf {
    root.join("profiles").join(format!("{name}.tar.zst"))
}

/// Where a piece's releases live: `profiles/<piece>/`.
pub fn release_dir(root: &Path, piece: Piece) -> PathBuf {
    root.join("profiles").join(piece.noun())
}

/// One piece's release file.
pub fn release_path(root: &Path, piece: Piece, name: &str) -> PathBuf {
    release_dir(root, piece).join(format!("{name}.tar.zst"))
}

/// Build the manifest for one piece of the live tree.
pub fn compute_manifest(
    layout: &crate::paths::Layout,
    piece: Piece,
    name: &str,
    version: &str,
) -> Result<Manifest> {
    let root = layout.root.as_path();
    // The pack is the tree **in force** — the workspace's own composition when
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
        deps: match piece {
            Piece::Pack => crate::compose::read_marker(&layout.assets()).deps,
            _ => Vec::new(),
        },
    })
}

/// The manifest for a **dependency pack**: the sanitized, self-contained release
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
pub fn hash_live(root: &Path) -> Result<BTreeMap<String, String>> {
    hash_files(root, live_files(root))
}

/// Every file a profile owns, relative to `root`, sorted.
fn live_files(root: &Path) -> Vec<PathBuf> {
    files_under(root, &LIVE_DIRS)
}

/// Every file under `dirs`, relative to `root`, sorted.
pub(crate) fn files_under<S: AsRef<str>>(root: &Path, dirs: &[S]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in dirs {
        let dir = dir.as_ref();
        // An empty entry walks `root` itself, which is what a caller holding a
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
pub fn verify(root: &Path) -> Result<Pointer> {
    verify_binding(root, None).map(|b| b.pack)
}

/// [`verify`], one piece at a time.
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
pub fn verify_layout(layout: &crate::paths::Layout, engine: Option<&str>) -> Result<Binding> {
    if layout.work == layout.root {
        return verify_binding(&layout.root, engine);
    }
    let mut binding = in_force(layout)?;
    if let Some(name) = engine {
        binding.engine.name = name.to_string();
    }
    // The trees the layout actually reads: the workspace's own `assets/` when
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
pub fn install_fixture(dest: &Path) -> Result<()> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/profile");
    for dir in LIVE_DIRS {
        let from = src.join(dir);
        // The fixture ships what a test needs, not every name in `LIVE_DIRS`:
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
mod tests;
