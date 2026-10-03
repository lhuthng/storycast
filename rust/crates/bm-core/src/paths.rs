//! Where everything lives. Every path the pipeline touches is derived here, so
//! a worker that was told `--root /srv/bm` resolves exactly the same tree the
//! inductor resolved locally.

use crate::util::squeeze_ws;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
    /// The active workspace: `workspaces/<name>/`, holding this book's
    /// settings, ledger, data and output. Equals `root` when no workspace is
    /// selected (the implicit default) — `new()` always says that, and every
    /// test uses it, so production entry points resolve through
    /// [`Layout::resolve`].
    pub work: PathBuf,
    /// The adapter (language) caches are keyed by, taken from the load pointer.
    ///
    /// A property of the checkout like `work`, and for the same reason: the
    /// cast and every segment directory hold text and audio in *one* language,
    /// and a second language is a second workspace. `default` is what a
    /// checkout that has no adapter bundle yet keys under — the language it was
    /// already using, before the split gave it a name. Public because tests in
    /// other crates build a `Layout` by literal.
    pub adapter: String,
    /// The voice engine this checkout runs: the name of its `engines/<name>/`
    /// tree, and the `settings.engine` name every cache path is keyed by.
    ///
    /// A property of the checkout like `adapter`, and read the same way — from
    /// the load pointer, falling back to [`DEFAULT_ENGINE`]. It is what gives
    /// the engine an identity on disk: before this, `models/`, `bm-tts` and
    /// `libonnxruntime.so.1` sat at the root with the engine's name nowhere in
    /// a path, so a second engine had nowhere to live and the dictionary was
    /// hardcoded to VieNeu's.
    pub engine: String,
}

/// What a checkout with no adapter bundle keys its caches under.
pub const DEFAULT_ADAPTER: &str = "default";

/// What a checkout with no engine named runs: the local engine that was there
/// before engines had names.
pub const DEFAULT_ENGINE: &str = "vieneu";

/// The directory every engine's own files hang off, at the root: `engines/`.
///
/// One subdirectory per engine, each with its own `models/`, `bm-tts`,
/// `libonnxruntime.so.1`, `refs/` and `samples/`. Shipped bytes are enormous
/// (about a gigabyte live) and gitignored like `models/` always was.
pub const ENGINES_DIR: &str = "engines";

/// The directory every language's own trees hang off, at the scope root:
/// `adapters/<name>/`, holding that language's `prompts/` and `crawl/`.
///
/// One subdirectory per adapter, so a language is a *thing* rather than a pair
/// of directory names the checkout happens to carry — which is what lets two
/// languages of one book exist, and what a language release unpacks into.
pub const ADAPTERS_DIR: &str = "adapters";

/// The one engine that ever had a *flat* tree at the root.
///
/// History, like [`LEGACY_CACHE_ENGINES`]: before the engine tree, `models/`
/// and `bm-tts` were the root's, and they were VieNeu's — so the one-time
/// rename moves them under `engines/vieneu/` whatever the checkout now runs.
/// A second engine never had a flat tree to migrate.
pub const LEGACY_ENGINE: &str = "vieneu";

/// The one language that ever had a *flat* tree at the root.
///
/// History, the same shape of argument as [`LEGACY_ENGINE`]: before the adapter
/// had a home, a language *was* two directory names the checkout happened to
/// have — the root's `prompts/` and, for its crawlers, the pack's
/// `assets/crawl/` — and they were this project's own: the Vietnamese one, whose
/// sites the bundled templates are written for. So the one-time move puts the
/// prompts under `adapters/vi-VN/` (the crawlers are the global `crawlers/` tree
/// now, so they are not moved into a language's home). A second language never
/// had a flat tree to migrate.
pub const LEGACY_ADAPTER: &str = "vi-VN";

/// The engine's name as it appears in a cache path.
///
/// Derived rather than switched on, so a second engine gets its own space
/// instead of the one the old `else` handed it. `gemini` keeps its historical
/// on-disk spelling because renaming those directories would orphan every
/// cloud-rendered chapter for no benefit; an empty engine would otherwise
/// produce `cast-default-.json`.
fn engine_key(engine: &str) -> &str {
    match engine {
        "" => "unknown",
        "gemini" => "gemini-v2",
        other => other,
    }
}

/// The pre-split spelling of `engine`'s cache, for the one-time rename.
///
/// `None` for an engine that never had a pre-split directory to move — which
/// includes every engine added from here on.
fn legacy_engine_key(engine: &str) -> Option<&'static str> {
    match engine {
        "vieneu" => Some("vieneu"),
        "gemini" => Some("gemini-v2"),
        _ => None,
    }
}

/// The engines that have a pre-split spelling on disk to move.
///
/// Deliberately not "the engines this build knows": this is *history*, so it is
/// a fixed list that stops growing. An engine added later has nothing to
/// migrate, and [`Layout::migrate_cache_keys`] is a no-op for it.
pub const LEGACY_CACHE_ENGINES: [&str; 2] = ["vieneu", "gemini"];

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Layout {
            work: root.clone(),
            root,
            adapter: DEFAULT_ADAPTER.to_string(),
            engine: DEFAULT_ENGINE.to_string(),
        }
    }

    /// The adapter the load pointer names, or [`DEFAULT_ADAPTER`].
    ///
    /// Best-effort on purpose: a fresh clone and a box mid-provision both have
    /// no pointer, and a path lookup is not the place to refuse to work. The
    /// pointer is checked by `profile::verify`, where a missing one matters.
    fn bound_adapter(root: &Path) -> String {
        crate::profile::read_binding(root)
            .map(|b| b.cache_adapter())
            .unwrap_or_else(|_| DEFAULT_ADAPTER.to_string())
    }

    /// The engine the load pointer names, or [`DEFAULT_ENGINE`].
    ///
    /// Best-effort for the same reason `bound_adapter` is: a fresh clone and a
    /// box mid-provision both have no pointer, and a path lookup is not the
    /// place to refuse to work.
    fn bound_engine(root: &Path) -> String {
        crate::profile::read_binding(root)
            .map(|b| b.cache_engine())
            .unwrap_or_else(|_| DEFAULT_ENGINE.to_string())
    }

    /// Resolve the active workspace: `.bm/active-workspace` names a directory
    /// under `workspaces/`. No pointer means this root *is* the workspace
    /// (the implicit default) — a fresh clone just works, and state appears
    /// under it on demand. Only a stale pointer (naming a missing directory)
    /// is an error: silently running at the root would scatter one book's
    /// state where another was expected.
    pub fn resolve(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let adapter = Self::bound_adapter(&root);
        let engine = Self::bound_engine(&root);
        let pointer = Self::active_workspace_file(&root);
        if !pointer.is_file() {
            // No pointer, so this root **is** the workspace (or a bare
            // checkout). Its own `settings.json` still names its adapter and
            // engine, exactly as a pointed-to workspace's do below. Without
            // this a layout rooted at a book reports `default` for both, and
            // every adapter-scoped fact silently degrades — the spoken
            // heading's language most visibly, which is how a `Chapter` title
            // came out as `Chương` for a book whose adapter declares `en-US`.
            let ws = crate::config::Settings::load(&root.join("settings.json"));
            let adapter = if ws.profile.adapter.name.is_empty() {
                adapter
            } else {
                ws.profile.adapter.name.clone()
            };
            let engine = if ws.profile.engine.name.is_empty() {
                engine
            } else {
                ws.profile.engine.name.clone()
            };
            return Ok(Layout {
                adapter,
                engine,
                work: root.clone(),
                root,
            });
        }
        let name = std::fs::read_to_string(&pointer)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let work = root.join("workspaces").join(&name);
        if name.is_empty() || !work.is_dir() {
            anyhow::bail!(
                "workspace pointer {:?} names {:?}, which is not a directory — recreate it (`workspace new {}`) or point elsewhere (`workspace use <name>`)",
                pointer.display(),
                work.display(),
                name,
            );
        }
        // The active workspace's own binding is the authority for what it
        // reads. Prompts, crawlers and every cache path key on the adapter and
        // engine, and those are facts about the BOOK — `workspace new
        // --profile` stamps them into the workspace's settings precisely so a
        // second language can live beside the first. The checkout pointer is
        // the fallback: what this checkout was unpacked with, and what every
        // workspace whose binding does not name the piece keeps following.
        let ws = crate::config::Settings::load(&work.join("settings.json"));
        let adapter = if ws.profile.adapter.name.is_empty() {
            adapter
        } else {
            ws.profile.adapter.name.clone()
        };
        let engine = if ws.profile.engine.name.is_empty() {
            engine
        } else {
            ws.profile.engine.name.clone()
        };
        Ok(Layout {
            root,
            work,
            adapter,
            engine,
        })
    }

    /// This layout, re-bound to the adapter and engine a task arrives with.
    ///
    /// **The offer is the authority for where a task's bytes live.** A worker's
    /// root is a flat mirror — no `.bm/profile` of the inductor's shape, and on
    /// a box provisioned before the split not even a name — so a layout
    /// resolved at startup keys `cast-*` and `segments-*` under `default`
    /// while the inductor that drives it, which has a pointer and a ledger,
    /// keys them under `vi-VN`. Nothing noticed while segment files travelled
    /// by *name* (`RenderUnitSpec.name`), so the cost was a wasted re-render
    /// rather than wrong audio — until a stage reads a cast or a prompt on the
    /// box, and then it is a chapter mixed from another language's cast.
    ///
    /// An empty name is no opinion (an inductor that predates the field sends
    /// nothing), and keeps whatever this box resolved for itself.
    pub fn rebind(&self, adapter: &str, engine: &str) -> Self {
        let mut bound = self.clone();
        if !adapter.trim().is_empty() {
            bound.adapter = adapter.trim().to_string();
        }
        if !engine.trim().is_empty() {
            bound.engine = engine.trim().to_string();
        }
        bound
    }

    /// Every adapter home this checkout carries: `(name, scope)`, the tree
    /// being `scope/adapters/<name>/`.
    ///
    /// **All of them, in every scope**, because the bundle ships every one:
    /// a language is 21 KB of prompts, the artifact it rides is 59 MB of clips,
    /// and the alternative — a per-machine adapter set — needs a field, a
    /// screen and a way to answer "why is this box not offered the book". One
    /// bundle for the whole cluster is also what keeps a box ready for a
    /// language it is not running today.
    ///
    /// The workspace's scope is walked first and a name found in both resolves
    /// to it, the rule [`Self::adapter_home`] already follows for the one in
    /// force. Sorted within each scope, so `Sources::plan` — and therefore the
    /// bundle's digest — is a property of the tree rather than of the order the
    /// filesystem answers in.
    pub fn adapter_homes(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for scope in [&self.work, &self.root] {
            let Ok(entries) = std::fs::read_dir(scope.join(ADAPTERS_DIR)) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            for name in names {
                if seen.insert(name.clone()) {
                    out.push((name, scope.clone()));
                }
            }
        }
        out
    }

    /// Resolve, or fall back to the bare root when the pointer is stale.
    ///
    /// **Management plane only.** The dashboard and `workspace` must open on a
    /// broken pointer — re-pointing is how it gets repaired — so they need the
    /// root even when `resolve` refuses. The error is handed back rather than
    /// swallowed: a stale pointer is a fact the operator has to see. Every
    /// runner uses [`Layout::resolve`], which refuses outright.
    pub fn resolve_or_root(root: impl Into<PathBuf>) -> (Self, Option<String>) {
        let root = root.into();
        match Self::resolve(&root) {
            Ok(layout) => (layout, None),
            Err(e) => (Layout::new(root), Some(e.to_string())),
        }
    }

    /// The workspace pointer file. One line: the workspace name.
    pub fn active_workspace_file(root: &Path) -> PathBuf {
        root.join(".bm").join("active-workspace")
    }

    /// The bm root above the current directory, resolving nothing.
    ///
    /// Split out of [`Layout::discover`] for the management plane, which has to
    /// start with a stale workspace pointer in hand. Two markers, because there
    /// are two kinds of root: a checkout carries the tracked `rust/Cargo.toml`
    /// (the live profile tree is ignored, so a fresh clone has no `prompts/` to
    /// look for), while a provisioned worker is a flat mirror — prompts,
    /// assets, models, binaries — with no Rust tree at all, and the
    /// `.bm/profile` pointer provision writes is what says so. Matching only
    /// the repo marker made every remote worker die at startup with "no repo
    /// root found".
    pub fn find_root() -> Result<PathBuf> {
        let cwd = std::env::current_dir().context("reading current dir")?;
        let mut cur: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cur {
            if dir.join("rust/Cargo.toml").is_file() || dir.join(".bm/profile").is_file() {
                return Ok(dir.to_path_buf());
            }
            cur = dir.parent();
        }
        anyhow::bail!(
            "no bm root found above {} (expected rust/Cargo.toml in a checkout, .bm/profile on a provisioned worker)",
            cwd.display()
        )
    }

    /// Walk up from the current directory looking for a bm root, and resolve
    /// the workspace inside it.
    pub fn discover() -> Result<Self> {
        Self::resolve(Self::find_root()?)
    }
}
mod assets;
mod book;
mod migrate;
mod state;
mod workspaces;
pub use workspaces::{chapter_of, workspaces, WorkspaceConfig, WorkspaceEntry};

#[cfg(test)]
mod tests;
