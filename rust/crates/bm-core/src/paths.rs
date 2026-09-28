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
/// sites the bundled templates are written for. So the one-time move puts both
/// under `adapters/vi-VN/`. A second language never had a flat tree to migrate.
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

    pub fn data(&self) -> PathBuf {
        self.work.join("data")
    }

    pub fn chapters(&self) -> PathBuf {
        self.data().join("chapters")
    }

    pub fn chapter_txt(&self, n: u32) -> PathBuf {
        self.chapters().join(format!("ch{n:02}.txt"))
    }

    /// The chapter index: the frozen `n -> url` mapping a crawl run works from.
    ///
    /// An artifact, not state, and deliberately a file an operator can read and
    /// hand-edit — for a book whose URLs are arbitrary slugs, authoring this by
    /// hand is more reliable than any script that re-derives it.
    pub fn crawl_index(&self) -> PathBuf {
        self.data().join("crawl-index.json")
    }

    /// The crawlers the **adapter** ships: `<adapter home>/crawl/`.
    ///
    /// The language's, not the pack's. A crawler is one site read in one
    /// language, and the adapter's language is both the source's and the
    /// target's — so a Vietnamese site's crawler belongs beside the Vietnamese
    /// prompts, and the same genre crawled in English is a different site
    /// rather than a different genre. Keeping them in `assets/` made the pack
    /// carry a tree that no pack value ever reads and that changes for a
    /// reason (the site) the art never changes for.
    ///
    /// **Pre-split they were the pack's**, at `assets/crawl/`, which is what a
    /// checkout with no adapter bundle still reads — so nothing on disk changes
    /// meaning, and the fallback is a tree that is already there.
    pub fn crawl_scripts(&self) -> PathBuf {
        match self.adapter_home() {
            Some(home) => home.join("crawl"),
            None => self.assets().join("crawl"),
        }
    }

    /// The active workspace's own crawlers: `workspaces/<name>/crawl/`.
    ///
    /// The adapter's crawlers are shared by every workspace on this root and
    /// replaced wholesale by `:profile load`; a book whose site needs its own
    /// crawler therefore lives here, where `:profile load` cannot reach it and a
    /// second workspace never sees it. Searched **first** by
    /// `crawl::resolve_script`, so a same-named file shadows the profile's —
    /// the workspace's answer wins over the profile's.
    ///
    /// Provisioning rsyncs this directory to every worker (see
    /// `provision::steps::install_sources`) and the stamp hashes it, so an edit
    /// here reaches the cluster with the next `:prov`.
    pub fn crawl_workspace(&self) -> PathBuf {
        self.work.join("crawl")
    }

    /// The chapter's script: `data/script/NN.json`.
    pub fn script(&self, n: u32) -> PathBuf {
        self.script_dir().join(format!("{n:02}.json"))
    }

    /// Every script in the workspace, in chapter order.
    ///
    /// **One definition, because eight places ask it** — the audition index, the
    /// cast refill, the inject screen's usage map, the speaker index, the
    /// character's-lines test, the two planners that walk a range, and the
    /// reconciler — and because they cannot be allowed to disagree. A `read_dir`
    /// order is arbitrary, so an unsorted answer makes a "random" pick differ
    /// between two runs of the same session for no reason anyone could see.
    pub fn scripts(&self) -> Vec<PathBuf> {
        let mut out = chapter_files(&self.script_dir());
        out.sort();
        out
    }

    /// Chapter numbers that have a script, ascending.
    pub fn script_chapters(&self) -> Vec<u32> {
        self.scripts().iter().filter_map(|p| chapter_of(p)).collect()
    }

    /// The scripts, one directory: `data/script/`.
    ///
    /// A directory, because a book of five hundred chapters wrote five hundred
    /// `script-NN.json` and five hundred `render-NN.json` in one folder beside
    /// the cast and the bible — so *finding* the scripts meant filtering a name
    /// prefix, which every reader spelled out for itself. The name now lives in
    /// the folder and the file is the chapter, which is the shape
    /// `chapters/chNN.txt` beside it already had.
    pub fn script_dir(&self) -> PathBuf {
        self.data().join("script")
    }

    /// Whether this chapter has been digested.
    ///
    /// **One definition, because three places ask it** — the digest manager's
    /// list, its filter and its painter — and they have to agree: the cursor
    /// indexes the *filtered* rows, so a predicate that answered differently in
    /// the draw than in the filter would highlight one chapter while acting on
    /// another. A test found exactly that divergence when each site spelled the
    /// question out for itself.
    ///
    /// The script file is the whole answer: it is what the digest stage produces
    /// and what every downstream stage reads.
    pub fn digested(&self, n: u32) -> bool {
        self.script(n).is_file()
    }

    /// The recorded render plan: the single namer for a chapter's audio. See
    /// [`crate::assemble::RenderPlan`].
    pub fn plan(&self, n: u32) -> PathBuf {
        self.render_dir().join(format!("{n:02}.json"))
    }

    /// The plans, one directory: `data/render/` — the sibling of
    /// [`script_dir`](Self::script_dir), and for the same reason.
    pub fn render_dir(&self) -> PathBuf {
        self.data().join("render")
    }

    /// The workspace a chapter's script lives in, and the chapter, both read
    /// back off the script's own path.
    ///
    /// The merge path is handed one file and nothing else — `data/script/NN.json`
    /// — and must answer for the layout behind it. The workspace is the parent
    /// of the `data` that holds the script folder, and the folder is recognised
    /// **by name** rather than by counting levels: a count is silent when it is
    /// wrong, and being one level short of the workspace does not fail, it hands
    /// back a layout whose `chapters/` is somewhere else entirely. A path that
    /// is not in a script folder is `None` rather than a guess.
    pub fn of_script(script_path: &Path) -> Option<(Self, u32)> {
        let chapter = chapter_of(script_path)?;
        let script_dir = script_path.parent()?;
        if script_dir.file_name()? != std::ffi::OsStr::new("script") {
            return None;
        }
        Some((Self::new(script_dir.parent()?.parent()?), chapter))
    }

    pub fn bible(&self) -> PathBuf {
        self.data().join("bible.json")
    }

    /// Cast file, keyed by adapter **and** engine: swapping either one must
    /// never poison the other's segment cache.
    pub fn cast(&self, engine: &str) -> PathBuf {
        self.data()
            .join(format!("cast-{}-{}.json", self.adapter, engine_key(engine)))
    }

    /// Per-chapter segment cache, keyed the same way.
    ///
    /// The engine half is what the old `if engine == "vieneu" … else
    /// "gemini-v2"` got wrong: the `else` named one engine for *every* engine
    /// that was not VieNeu, so a third engine would have read and written
    /// Gemini's segments — one engine's audio served under another's name.
    /// Deriving the key is what makes a second engine possible at all.
    pub fn seg_dir(&self, engine: &str, n: u32) -> PathBuf {
        self.data().join(format!(
            "audio/segments-{}-{}-{n:02}",
            self.adapter,
            engine_key(engine)
        ))
    }

    /// Bring a pre-adapter-home checkout into the `adapters/<name>/` shape.
    ///
    /// Before the adapter was a home, a language was two directory names the
    /// checkout happened to have: `prompts/` at the root and, for its crawlers,
    /// the pack's `assets/crawl/`. Neither said which language it was — which is
    /// why a second one could not exist, and why the crawlers ended up in the
    /// pack, where nothing about them is a genre fact.
    ///
    /// So the trees are **moved**, never rebuilt, into one directory that names
    /// the language: `adapters/<name>/{prompts,crawl}/`. The name is the
    /// checkout's own when it already names one, and [`LEGACY_ADAPTER`] when it
    /// does not. Rename-only, never overwriting, idempotent — and it does
    /// nothing at all until a pointer exists, because stamping a name is a claim
    /// about a checkout that has loaded something.
    pub fn migrate_adapter_tree(&self) -> Result<Option<String>> {
        if self.adapter_home().is_some() {
            return Ok(None); // already has one; nothing to move again
        }
        if crate::profile::read_binding(&self.root).is_err() {
            return Ok(None); // a fresh clone: no pointer, so nothing to name
        }
        let name = if self.adapter == DEFAULT_ADAPTER {
            LEGACY_ADAPTER.to_string()
        } else {
            self.adapter.clone()
        };
        let home = self.root.join(ADAPTERS_DIR).join(&name);
        let mut moved = false;
        for (from, to) in [
            (self.root.join("prompts"), home.join("prompts")),
            (self.assets().join("crawl"), home.join("crawl")),
        ] {
            if !from.exists() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved = true;
        }
        if !moved {
            return Ok(None);
        }
        // The name is what every path below resolves through, so the pointer
        // carries it. `assets/` lost its crawlers in the same breath, so the
        // pack's hash has moved — re-stamping here keeps the next start from
        // warning about a drift this migration caused.
        let mut binding = crate::profile::read_binding(&self.root)?;
        binding.adapter.name = name.clone();
        crate::profile::write_binding(&self.root, &binding)?;
        let _ = crate::profile::verify_binding(&self.root, None);
        Ok(Some(name))
    }

    /// Bring a pre-split cache into the `(adapter, engine)` shape.
    ///
    /// Before the adapter reached the path, the cast was `cast-<engine>.json`
    /// (and plain `cast.json` for anything but VieNeu) and the segment
    /// directories were `segments-<engine>-NN`. Those bytes are already
    /// correct — they were produced by this adapter and this engine; only the
    /// *name* was missing a component — so this renames rather than rebuilds.
    /// Leaving the old names behind would re-render every chapter already
    /// spoken, which is hours of synthesis for a path string.
    ///
    /// Idempotent, and it never overwrites: a target that already exists wins.
    /// Returns what it moved, so a caller can say so once instead of silently
    /// rewriting the operator's data directory.
    pub fn migrate_cache_keys(&self, engine: &str) -> Result<Vec<PathBuf>> {
        let mut moved = Vec::new();
        let data = self.data();

        // The adapter half first, when this checkout was migrated *out of*
        // `default`: those bytes are in the language that now has a name, and
        // leaving them keyed by the name-less default would re-render every
        // chapter already spoken — the same waste, and the same fix, as the
        // engine half below.
        if self.adapter != DEFAULT_ADAPTER {
            moved.extend(self.rename_default_caches(&data)?);
        }

        let legacy_cast = match engine {
            "vieneu" => Some(data.join("cast-vieneu.json")),
            _ => Some(data.join("cast.json")),
        };
        if let Some(legacy) = legacy_cast {
            let target = self.cast(engine);
            if legacy.is_file() && !target.exists() {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&legacy, &target).with_context(|| {
                    format!("renaming {} to {}", legacy.display(), target.display())
                })?;
                moved.push(target);
            }
        }

        let Some(legacy_engine) = legacy_engine_key(engine) else {
            return Ok(moved);
        };
        let prefix = format!("segments-{legacy_engine}-");
        let audio = data.join("audio");
        let Ok(entries) = std::fs::read_dir(&audio) else {
            return Ok(moved);
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with(&prefix))
            .collect();
        names.sort();
        for name in names {
            let Ok(n) = name[prefix.len()..].parse::<u32>() else {
                continue;
            };
            let (from, to) = (audio.join(&name), self.seg_dir(engine, n));
            if to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved.push(to);
        }
        Ok(moved)
    }

    /// Rename `default`-keyed caches to this adapter's name.
    ///
    /// `cast-default-<engine>.json` and `segments-default-<engine>-NN` were
    /// correct content under a name that said nothing, written before the
    /// language had one. The engine half is taken from each filename rather
    /// than assumed, so `gemini-v2` (which carries its own `-`) renames as
    /// faithfully as `vieneu` does.
    fn rename_default_caches(&self, data: &Path) -> Result<Vec<PathBuf>> {
        let mut moved = Vec::new();
        let cast_prefix = format!("cast-{DEFAULT_ADAPTER}-");
        if let Ok(entries) = std::fs::read_dir(data) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(&cast_prefix) && n.ends_with(".json"))
                .collect();
            names.sort();
            for name in names {
                let engine = &name[cast_prefix.len()..name.len() - ".json".len()];
                if engine.is_empty() {
                    continue;
                }
                let (from, to) = (data.join(&name), self.cast(engine));
                if to.exists() {
                    continue;
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&from, &to)
                    .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
                moved.push(to);
            }
        }

        let seg_prefix = format!("segments-{DEFAULT_ADAPTER}-");
        let audio = data.join("audio");
        let Ok(entries) = std::fs::read_dir(&audio) else {
            return Ok(moved);
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.starts_with(&seg_prefix))
            .collect();
        names.sort();
        for name in names {
            // `segments-default-<engine>-NN`: the chapter is the last field, so
            // an engine that carries a `-` of its own still splits right.
            let Some((rest, chapter)) = name[seg_prefix.len()..].rsplit_once('-') else {
                continue;
            };
            let (Ok(n), false) = (chapter.parse::<u32>(), rest.is_empty()) else {
                continue;
            };
            let (from, to) = (audio.join(&name), self.seg_dir(rest, n));
            if to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved.push(to);
        }
        Ok(moved)
    }

    /// Bring a pre-engine-tree checkout into the `engines/<name>/` shape.
    ///
    /// Before engines had trees, `models/`, `bm-tts`, `libonnxruntime.so.1`
    /// and the `.bm/voices/` pair sat at the root — and they were **VieNeu's**,
    /// because it was the only local engine. So the files are *moved*, never
    /// rebuilt, into [`LEGACY_ENGINE`]'s tree whatever this checkout now runs:
    /// the bytes have a fixed owner even though `settings.engine` is a setting
    /// somebody can switch.
    ///
    /// Rename-only, never overwriting, and idempotent. Everything it moves sits
    /// on one filesystem, so this is a handful of renames rather than a
    /// gigabyte of copying — and the alternative, leaving the old tree where no
    /// new path points, would make the sidecar unspawnable and the weights
    /// unreachable rather than merely misnamed.
    pub fn migrate_engine_tree(&self) -> Result<Vec<PathBuf>> {
        let target = self.root.join(ENGINES_DIR).join(LEGACY_ENGINE);
        let bm = self.bm_state();
        let mut moved = Vec::new();
        for (from, to) in [
            (self.root.join("models"), target.join("models")),
            (self.root.join("bm-tts"), target.join("bm-tts")),
            (
                self.root.join("libonnxruntime.so"),
                target.join("libonnxruntime.so"),
            ),
            (
                self.root.join("libonnxruntime.so.1"),
                target.join("libonnxruntime.so.1"),
            ),
            (bm.join("voices/refs"), target.join("refs")),
            (bm.join("voices/samples"), target.join("samples")),
        ] {
            if !from.exists() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
            moved.push(to);
        }
        // `.bm/voices/` only ever held the pair above, so an empty one is
        // leftover scaffolding rather than state.
        let _ = std::fs::remove_dir(bm.join("voices"));
        Ok(moved)
    }

    pub fn audio(&self) -> PathBuf {
        self.data().join("audio")
    }

    pub fn output(&self) -> PathBuf {
        self.work.join("output")
    }

    /// The directory the live `prompts/` tree hangs off: the workspace when it
    /// carries one, else the checkout.
    ///
    /// `work/prompts/` is the adapter's home for the same reason `work/crawl/`
    /// is a book's: `:profile load` replaces `assets/` + `prompts/` for the
    /// whole checkout, so prompts at the root are prompts every workspace on
    /// this root must share — one language per checkout, which is the limit the
    /// adapter exists to remove. A workspace that carries its own tree speaks
    /// its own language.
    ///
    /// Returning the *base* rather than the directory is what lets a caller
    /// both read the tree and ship it: a bundle member is a path relative to
    /// its base, so `prompts/analyze.txt` travels from either tree to the same
    /// place on a box.
    pub fn prompts_base(&self) -> PathBuf {
        if let Some(home) = self.adapter_home() {
            return home;
        }
        if self.work.join("prompts").is_dir() {
            self.work.clone()
        } else {
            self.root.clone()
        }
    }

    /// The directory an adapter's own trees hang off, if this checkout carries
    /// one: `adapters/<adapter>/`, in the nearest scope that has it.
    ///
    /// A **scope** is the workspace and then the checkout, and inside a scope
    /// the bundle wins over the pre-split flat tree (`prompts/` at the scope
    /// root), because the bundle is the shape a language release unpacks into
    /// and it is the one that can also carry the language's `crawl/`. `None`
    /// means this checkout has not been given an adapter bundle at all — which
    /// is every checkout that predates the split, and what the flat fallbacks
    /// in [`Layout::prompts_base`] and [`Layout::crawl_scripts`] exist for.
    ///
    /// It is a scope *root*, not a tree, so callers that resolve a name
    /// relative to a scope (the crawler resolver) and callers that want one
    /// directory (the prompts) both get what they need from it.
    pub fn adapter_home(&self) -> Option<PathBuf> {
        [
            self.work.join(ADAPTERS_DIR).join(&self.adapter),
            self.root.join(ADAPTERS_DIR).join(&self.adapter),
        ]
        .into_iter()
        .find(|p| p.is_dir())
    }

    /// The live `prompts/` tree in force, whole. The fallback is not silence:
    /// it is the checkout's tree, which is what every workspace read before the
    /// adapter split. A workspace whose own tree is incomplete fails on the
    /// missing template, and that error names the file it wanted.
    pub fn prompts_dir(&self) -> PathBuf {
        self.prompts_base().join("prompts")
    }

    /// The chapter attribution template. The automatic worker adds its prepared
    /// events and immutable-speaker contract; the manual manager also uses the
    /// legacy raw-chapter rendering of this file.
    pub fn prompt(&self) -> PathBuf {
        self.prompts_dir().join("analyze.txt")
    }

    /// The audio-staging contract. The automatic builder appends the immutable
    /// speaker map and prepared-source obligations; the manual manager renders
    /// the legacy full script contract directly.
    pub fn script_prompt(&self) -> PathBuf {
        self.prompts_dir().join("script.txt")
    }

    pub fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }

    /// The scene map: the rules, the palette and the layer knobs.
    pub fn scene_map(&self) -> PathBuf {
        self.assets().join("scene-map.json")
    }

    /// One layer's clip registry.
    ///
    /// The three registries used to be spelled here one method at a time, which
    /// meant a fourth layer was an edit in every file that named a pool. The
    /// spelling now lives in [`crate::audio_pool::PoolKind`] alone — this just
    /// joins it to `assets/`, which is also what provisioning ships, so a clip
    /// and its registry travel together.
    pub fn pool(&self, kind: crate::audio_pool::PoolKind) -> PathBuf {
        self.assets().join(kind.registry())
    }

    /// The directory one layer's clips live in, under `assets/`.
    pub fn pool_dir(&self, kind: crate::audio_pool::PoolKind) -> PathBuf {
        self.assets().join(kind.dir())
    }

    pub fn refs(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn python_dir(&self) -> PathBuf {
        self.root.join("python")
    }

    /// One engine's own tree: `engines/<name>/`.
    ///
    /// Every file that *is* the engine lives under here — the binary, its
    /// runtime library, the weights, the lexicon and the voice store — so the
    /// engine name is in the path rather than only in `settings.engine`. It is
    /// what makes a second engine possible at all: before this, two engines
    /// would have shared one `models/`, one `bm-tts` and one dictionary.
    pub fn engine_dir(&self) -> PathBuf {
        self.root.join(ENGINES_DIR).join(&self.engine)
    }

    /// The TTS sidecar binary: `engines/<name>/bm-tts`, at the worker root.
    ///
    /// The same spelling on the inductor and on a worker: `root` is the repo
    /// locally and `~/bm-worker` remotely, so one method serves both. This is
    /// what `provision::Probe` looks for and what the agent spawns.
    ///
    /// Per engine rather than per root, deliberately: `bm-tts` is not a generic
    /// sidecar that any engine plugs into — it *is* VieNeu, and a second engine
    /// ships its own binary beside its own weights.
    pub fn tts_binary(&self) -> PathBuf {
        self.engine_dir().join("bm-tts")
    }

    /// The baked model directory: one flat directory, codec included.
    ///
    /// Deliberately *not* the Hugging Face cache layout — `bake-models.py`
    /// flattens it so provisioning can rsync bytes and a worker needs no
    /// `huggingface_hub` and no internet.
    pub fn models_dir(&self) -> PathBuf {
        self.engine_dir().join("models")
    }

    /// Where `libonnxruntime.so.1` sits — beside the engine's binary.
    ///
    /// This is the directory `LD_LIBRARY_PATH` has to name. The SONAME matters:
    /// the file must be reachable as `libonnxruntime.so.1`, not only under its
    /// versioned filename, or the binary dies at startup with "error while
    /// loading shared libraries".
    pub fn tts_lib_dir(&self) -> PathBuf {
        self.engine_dir()
    }

    /// The G2P dictionary the engine's front end reads, if it has one.
    ///
    /// The file name comes from the engine's own declaration rather than being
    /// spelled here: it used to hardcode `sea_g2p.bin`, VieNeu's Southeast-Asian
    /// lexicon, so a second engine would have loaded the wrong dictionary and
    /// *mispronounced* — worse than a missing file, which at least fails.
    /// `None` means this engine needs no lexicon, and the sidecar is not handed
    /// a `--dict` it has nothing to read.
    pub fn tts_dict(&self) -> Option<PathBuf> {
        crate::voices::dictionary(&self.engine).map(|name| self.models_dir().join(name))
    }

    /// The voice store the Rust server reads: the shipped presets *and* every
    /// enrolled clone, which is why the bake copies it rather than shipping a
    /// separate roster.
    pub fn tts_voices(&self) -> PathBuf {
        self.models_dir().join("voices.json")
    }

    /// The sidecar binary to spawn: the engine's provisioned copy first, then
    /// the workspace's own debug/release builds beside it.
    ///
    /// The local worker runs from the repo, where no provision ever installs
    /// `bm-tts` — but `cargo build --workspace` keeps `target/debug/bm-tts`
    /// fresh. Without the fallback a dead sidecar is fatal locally even
    /// though a working binary sits one directory over. Order matters only
    /// in that the provisioned copy wins where it exists, so remote
    /// behaviour is unchanged.
    ///
    /// The repo-build fallbacks stay at their historical paths: the build tree
    /// is a build artifact, not an engine's own file, and `cargo` is the one
    /// that decides where it goes.
    pub fn sidecar_binary(&self) -> PathBuf {
        [
            self.tts_binary(),
            self.root.join("rust/target/debug/bm-tts"),
            self.root.join("rust/target/release/bm-tts"),
        ]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| self.tts_binary())
    }

    /// The binary and argv for the sidecar on this machine.
    ///
    /// Everything is derived from `Layout`, so the inductor and a worker
    /// resolve the same tree — `root` is the repo locally and `~/bm-worker`
    /// remotely.
    pub fn sidecar_command(&self, port: u16) -> (PathBuf, Vec<String>) {
        let models = self.models_dir();
        let mut args: Vec<String> = vec![
            "--models".into(),
            models.display().to_string(),
            // One directory, codec included — see `tools/bake-models.py`.
            "--codec".into(),
            models.display().to_string(),
        ];
        // Only an engine with a declared lexicon is handed one. Passing
        // VieNeu's `sea_g2p.bin` to an engine that does not read it is the bug
        // this optional argument removes.
        if let Some(dict) = self.tts_dict() {
            args.push("--dict".into());
            args.push(dict.display().to_string());
        }
        args.extend([
            "--voices".into(),
            self.tts_voices().display().to_string(),
            "--port".into(),
            port.to_string(),
            "--bind".into(),
            "127.0.0.1".into(),
        ]);
        (self.sidecar_binary(), args)
    }

    /// Interpreter for local voice work (enroll now, preview offline): the
    /// provision-managed `python/.venv` first, a repo-root `.venv` second.
    /// One order everywhere — enrollment and serving can never aim at two
    /// different voice stores, which is exactly how a fresh voice 500s.
    ///
    /// **Being retired.** Serving no longer uses this; only voice enrollment
    /// still does, and that is the last Python dependency in the project. It
    /// goes when enrollment is ported to Rust.
    pub fn venv_python(&self) -> Option<PathBuf> {
        [
            self.python_dir().join(".venv/bin/python"),
            self.root.join(".venv/bin/python"),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }

    /// Inductor-private state (cluster registry, settings, stats).
    pub fn bm_state(&self) -> PathBuf {
        self.root.join(".bm")
    }

    /// The workspace's own state: settings, ledger, stats. Per book, so two
    /// workspaces never share a ledger; machine-global files (machines,
    /// roster, profile pointer) stay in [`Layout::bm_state`]. The files
    /// themselves land directly in `work` — see [`Layout::state_file`].
    pub fn stats(&self) -> PathBuf {
        self.state_file("stats.jsonl")
    }

    pub fn settings(&self) -> PathBuf {
        self.state_file("settings.json")
    }

    /// The task ledger: which chapter/stage is in which state. Per workspace,
    /// bound to its profile (see `profile` in settings) — running a workspace
    /// under another profile is refused rather than mixed.
    pub fn ledger(&self) -> PathBuf {
        self.state_file("ledger.json")
    }

    /// Workspace state file: under the workspace, except in legacy mode
    /// (`work == root`), where state still lives in `.bm/` from before
    /// workspaces existed. Migration shim — remove once no checkout predates
    /// it; every legacy root migrates by moving `.bm/{settings,ledger}.json`
    /// and `stats.jsonl` into `workspaces/<name>/`.
    fn state_file(&self, name: &str) -> PathBuf {
        if self.work == self.root {
            self.root.join(".bm").join(name)
        } else {
            self.work.join(name)
        }
    }

    /// The committed voice catalogue: every preset, its metadata, the accent
    /// policy and the default cast, for both engines.
    ///
    /// Tracked, and read at compile time by `voices::CATALOGUE_JSON`, so this
    /// path exists for tooling (`roster init`, a diff against a fresh clone)
    /// rather than for the render path.
    pub fn roster_default(&self) -> PathBuf {
        self.root.join("voices.default.json")
    }

    /// Linked machines: the per-machine connection config (addr/user/port/key),
    /// keyed by address. The inductor's join of this file with the ledger's
    /// `machine_state` is the `Machine` the API serves.
    ///
    /// Same deal as `roster()`: inside `.bm/`, so SSH users, addresses and key
    /// paths stay on the machine and out of git with no extra ignore rules.
    pub fn machines(&self) -> PathBuf {
        self.bm_state().join("machines.json")
    }

    /// LLM providers (keys, endpoints, active model): machine-global like
    /// `machines()`, for the same reason — a key is this machine's access,
    /// not a book's. The single file the `L` screen edits; the inductor sends
    /// the active key with each task offer, so workers never read this.
    pub fn llm_config(&self) -> PathBuf {
        self.bm_state().join("llm.json")
    }

    /// The shipped provider template: four keyless, modelless slots and the
    /// default endpoints, for `.bm/llm.json` to be copied from.
    ///
    /// Tracked, like [`Layout::roster_default`]: a fresh clone has to show
    /// the same `L` screen with no local config, so this one is committed at
    /// the root.
    pub fn llm_default(&self) -> PathBuf {
        self.root.join("llm.default.json")
    }

    /// The AWS worker pool definition: region, type, subnet, security group,
    /// instance profile, keypair names, caps.
    ///
    /// At the root rather than in the workspace, for the same reason as
    /// `machines()`: it describes *this machine's access to AWS*, not a book.
    /// The profile a box is built for is the machine-global `.bm/profile`, so
    /// two workspaces share one pool without either one's settings leaking into
    /// it.
    pub fn aws_config(&self) -> PathBuf {
        self.bm_state().join("aws.json")
    }

    /// The tracked AWS template: the *shape* of the pool, which travels with
    /// the repo so a clone knows what to fill in. Values are personal and live
    /// in [`Layout::aws_config`]; see `AwsConfig::load_layered`.
    ///
    /// Tracked at the root beside `voices.default.json`, the same split the
    /// voice catalogue uses: the shipped half is content, the local half is
    /// machine state.
    pub fn aws_default(&self) -> PathBuf {
        self.root.join(crate::provision::DEFAULT_FILE)
    }

    /// Reference clips for enrolled clones — supplied by the operator and the
    /// input to enrolment. Ignored, and the only voice asset that reaches a
    /// worker.
    ///
    /// Under the engine, because a reference clip is only meaningful to the
    /// engine that clones from it: enrolling VieNeu from a clip says nothing
    /// about any other engine, and a clip with no engine beside it would have to
    /// be paired up again by whoever reads it.
    ///
    /// **Not** [`Layout::refs`], which is the sample pool's own `root/refs/`
    /// and a different thing entirely — see `pool::add_sample`.
    pub fn voice_refs(&self) -> PathBuf {
        self.engine_dir().join("refs")
    }

    /// Audition clips for the picker — generated, disposable, and deliberately
    /// never synced to a worker, which needs only `voice_refs()` to enrol.
    pub fn voice_samples(&self) -> PathBuf {
        self.engine_dir().join("samples")
    }

    /// Transient working space for the merge stage.
    ///
    /// Deliberately next to the workspace's output rather than in the system
    /// temp dir. `publish` moves the finished mp3 into `output()` with
    /// `fs::rename`, which fails across filesystems (`EXDEV`); scratch must
    /// share a device with the output, and the workspace always does because
    /// it hangs off the same directory. Legacy mode keeps the old `.bm/tmp`.
    pub fn scratch(&self) -> PathBuf {
        if self.work == self.root {
            self.root.join(".bm").join("tmp")
        } else {
            self.work.join("tmp")
        }
    }

    /// One chapter's scratch directory. Everything the merge writes — the
    /// concat, the ambience pass, the tempo pass and their intermediates —
    /// lands here and is removed once the chapter is published.
    pub fn scratch_ch(&self, n: u32) -> PathBuf {
        self.scratch().join(format!("ch{n:02}"))
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [
            self.data(),
            self.chapters(),
            self.script_dir(),
            self.render_dir(),
            self.audio(),
            self.output(),
            self.bm_state(),
            self.scratch(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    /// Chapter title for the output filename, ported from `main._chapter_title`.
    ///
    /// **The script's own `title` wins.** The crawled headline is the site's
    /// auto-excerpt of the chapter — `Chương 9: Tê! Thật là khủng khiếp dao
    /// phay`, `Chương 10: Tiền bối đối với dao phay yêu cầu đều cao như vậy?` —
    /// a sentence out of the prose with the punctuation still on it, which then
    /// lands on the cover of the mp3. It is a title only in the sense that the
    /// site put it on the first line. The digest has read the chapter and can
    /// name it, so its `title` is the one used; the headline stays as the
    /// fallback for a chapter that was digested before the field existed.
    ///
    /// Both routes end in the same scrub, so a title from either source is a
    /// legal filename and the spoken headline and the file agree.
    pub fn chapter_title(&self, n: u32) -> String {
        let from_script = crate::read_json::<serde_json::Value>(&self.script(n))
            .ok()
            .and_then(|d| {
                d.get("title")
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(String::from)
            });
        let raw = from_script.unwrap_or_else(|| {
            let raw = std::fs::read_to_string(self.chapter_txt(n)).unwrap_or_default();
            let first = raw.lines().next().unwrap_or("").trim().to_string();
            match first.split_once(':') {
                Some((_, rest)) => rest.to_string(),
                None => first,
            }
        });
        let mut title = squeeze_ws(&raw);
        // trailing dot-runs: ". . ." / "..." / "…"
        title = title.trim_end_matches([' ', '.', '…']).to_string();
        // windows-illegal filename characters
        title = title
            .chars()
            .filter(|c| !matches!(c, '?' | ':' | '"' | '*' | '<' | '>' | '|'))
            .collect();
        let title = squeeze_ws(&title);
        if title.is_empty() {
            format!("Chapter {n}")
        } else {
            title
        }
    }

    /// Final per-chapter deliverable: `output/Ch.N - Title.mp3`.
    pub fn final_mp3(&self, n: u32) -> PathBuf {
        self.output()
            .join(format!("Ch.{n} - {}.mp3", self.chapter_title(n)))
    }
}

/// The chapter a `NN.json` in a chapter directory names. `None` for anything
/// else, so a stray file a reader drops in is skipped rather than read as a
/// chapter.
pub fn chapter_of(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// The `NN.json` files in a chapter directory, in whatever order the
/// filesystem hands them over. Callers that order the answer ask
/// [`Layout::scripts`].
fn chapter_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|x| x.path()))
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                .filter(|p| chapter_of(p).is_some())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bm-layout-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        // The repo marker discover() walks up to (tracked, always present).
        std::fs::create_dir_all(dir.join("rust")).unwrap();
        std::fs::write(dir.join("rust/Cargo.toml"), "[workspace]").unwrap();
        dir
    }

    #[test]
    fn the_sidecar_prefers_the_provisioned_copy_then_the_workspace_build() {
        // Moved with the fallback itself: the local worker runs from the
        // repo, where no provision ever installs `bm-tts` — but `cargo
        // build` keeps the debug binary fresh, so a dead sidecar must fall
        // back to it, not fail the render. The provisioned copy still wins
        // where it exists, so remote behaviour is unchanged.
        let root = std::env::temp_dir().join(format!("bm-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layout = Layout::new(&root);
        assert_eq!(
            layout.sidecar_binary(),
            layout.tts_binary(),
            "absent everywhere reports the canonical path"
        );
        assert_eq!(
            layout.tts_binary(),
            root.join("engines/vieneu/bm-tts"),
            "the sidecar is the engine's, inside its own tree"
        );
        let debug = root.join("rust/target/debug/bm-tts");
        std::fs::create_dir_all(debug.parent().unwrap()).unwrap();
        std::fs::write(&debug, b"fake").unwrap();
        assert_eq!(layout.sidecar_binary(), debug);
        let provisioned = layout.tts_binary();
        std::fs::create_dir_all(provisioned.parent().unwrap()).unwrap();
        std::fs::write(&provisioned, b"fake").unwrap();
        assert_eq!(layout.sidecar_binary(), provisioned);
        let (bin, args) = layout.sidecar_command(8818);
        assert_eq!(bin, provisioned);
        assert!(args.windows(2).any(|w| w[0] == "--port" && w[1] == "8818"));
        // VieNeu declares a lexicon, so it is handed one — from its own tree.
        let dict_at = args.iter().position(|a| a == "--dict").expect("--dict");
        assert_eq!(
            args[dict_at + 1],
            layout.tts_dict().unwrap().display().to_string()
        );
        assert!(args[dict_at + 1].contains("engines/vieneu/models/sea_g2p.bin"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_adapter_and_engine_scoped() {
        let l = Layout::new("/repo");
        // A checkout with no adapter bundle keys under `default`.
        assert!(l.cast("vieneu").ends_with("cast-default-vieneu.json"));
        assert!(l
            .seg_dir("vieneu", 7)
            .ends_with("data/audio/segments-default-vieneu-07"));
        // `gemini` keeps its historical on-disk spelling, so renaming nothing
        // orphans the cloud-rendered chapters. Anything else gets its own.
        assert!(l
            .seg_dir("gemini", 7)
            .ends_with("data/audio/segments-default-gemini-v2-07"));
        assert!(l
            .seg_dir("neutts-air", 7)
            .ends_with("data/audio/segments-default-neutts-air-07"));
        assert_ne!(
            l.seg_dir("neutts-air", 7),
            l.seg_dir("gemini", 7),
            "a third engine must not land in another engine's cache"
        );
        // A named adapter is its own namespace: vi-VN and en-US of one book are
        // two workspaces, and must not share a segment directory even if they
        // somehow shared a `data/`.
        let named = Layout {
            adapter: "en-US".into(),
            ..Layout::new("/repo")
        };
        assert!(named.cast("vieneu").ends_with("cast-en-US-vieneu.json"));
        assert_ne!(named.seg_dir("vieneu", 7), l.seg_dir("vieneu", 7));
    }

    #[test]
    fn a_pre_split_cache_is_renamed_into_the_new_shape() {
        let root = fixture_root("cache-migrate");
        let l = Layout::new(&root);
        let data = l.data();
        std::fs::create_dir_all(data.join("audio/segments-vieneu-07")).unwrap();
        std::fs::write(data.join("cast-vieneu.json"), "{\"Narrator\":\"Adam\"}").unwrap();

        let moved = l.migrate_cache_keys("vieneu").unwrap();
        assert_eq!(moved.len(), 2, "the cast and one chapter: {moved:?}");
        assert!(l.cast("vieneu").is_file(), "the cast was carried over");
        assert!(l.seg_dir("vieneu", 7).is_dir(), "and the segments with it");
        assert!(
            !data.join("cast-vieneu.json").exists(),
            "the old name is gone, not duplicated"
        );
        // Idempotent: nothing left to move, and nothing overwritten.
        assert!(l.migrate_cache_keys("vieneu").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A language that was flat at the root takes its own home, and the pointer
    /// is stamped with the name every path below then resolves through —
    /// rename-only, never overwriting, idempotent.
    #[test]
    fn a_pre_adapter_home_checkout_moves_both_trees_into_the_languages_home() {
        let root = fixture_root("adapter-migrate");
        // The flat language, as it sat before adapters were a directory: the
        // root's prompts, and the crawlers still inside the pack.
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/analyze.txt"), "vi-VN").unwrap();
        std::fs::create_dir_all(root.join("assets/crawl/templates")).unwrap();
        std::fs::write(root.join("assets/crawl/templates/storya.lua"), "-- crawl").unwrap();
        std::fs::create_dir_all(root.join("assets/music")).unwrap();
        std::fs::write(root.join("assets/music/day-1.mp3"), b"bed").unwrap();
        std::fs::create_dir_all(root.join(".bm")).unwrap();
        std::fs::write(
            root.join(".bm/profile"),
            r#"{"name":"xianxia","hash":"deadbeef"}"#,
        )
        .unwrap();

        let l = Layout::new(&root);
        assert_eq!(l.adapter, DEFAULT_ADAPTER, "nothing has named the language");
        assert_eq!(l.migrate_adapter_tree().unwrap().as_deref(), Some("vi-VN"));

        let home = root.join("adapters/vi-VN");
        assert!(home.join("prompts/analyze.txt").is_file());
        assert!(home.join("crawl/templates/storya.lua").is_file());
        assert!(!root.join("prompts").exists(), "moved, not copied");
        assert!(!root.join("assets/crawl").exists());
        assert!(
            root.join("assets/music/day-1.mp3").is_file(),
            "the art stays"
        );

        // The name is the pointer's, and it is what the layout now resolves
        // through: both of the language's trees come from one directory.
        let after = Layout::resolve(&root).unwrap();
        assert_eq!(after.adapter, "vi-VN");
        assert_eq!(after.adapter_home(), Some(home.clone()));
        assert_eq!(after.prompts_base(), home);
        assert_eq!(after.crawl_scripts(), home.join("crawl"));

        // Idempotent: a bundle exists, so there is nothing left to move.
        assert_eq!(after.migrate_adapter_tree().unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cache written before the language had a name holds the right bytes
    /// under a name that says nothing. It is *renamed*, because the alternative
    /// is re-synthesising every chapter already spoken for a path string — and
    /// the engine half is read out of each filename rather than assumed, so an
    /// engine whose key carries its own `-` renames as faithfully as any other.
    #[test]
    fn default_keyed_caches_are_re_keyed_for_the_language_that_now_has_a_name() {
        let root = fixture_root("adapter-cache");
        let l = Layout {
            adapter: "vi-VN".into(),
            ..Layout::new(&root)
        };
        let data = l.data();
        std::fs::create_dir_all(data.join("audio/segments-default-vieneu-07")).unwrap();
        std::fs::create_dir_all(data.join("audio/segments-default-gemini-v2-07")).unwrap();
        std::fs::write(data.join("cast-default-vieneu.json"), "{}").unwrap();
        std::fs::write(data.join("cast-default-gemini-v2.json"), "{}").unwrap();

        assert_eq!(l.migrate_cache_keys("vieneu").unwrap().len(), 4);
        assert!(data.join("cast-vi-VN-vieneu.json").is_file());
        assert!(data.join("cast-vi-VN-gemini-v2.json").is_file());
        assert!(data.join("audio/segments-vi-VN-vieneu-07").is_dir());
        assert!(data.join("audio/segments-vi-VN-gemini-v2-07").is_dir());
        assert!(
            !data.join("cast-default-vieneu.json").exists(),
            "the name-less key is gone, not duplicated"
        );
        // Idempotent, and the `default` spelling is left alone once named.
        assert!(l.migrate_cache_keys("vieneu").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The engine's own files have an identity now: one tree per engine, and the
    /// name is in every path rather than only in `settings.engine`.
    #[test]
    fn engine_files_live_under_a_named_tree_and_the_dictionary_is_declared() {
        let l = Layout::new("/repo");
        assert_eq!(l.engine, "vieneu", "no binding means the local engine");
        assert_eq!(l.engine_dir(), Path::new("/repo/engines/vieneu"));
        assert!(l.models_dir().ends_with("engines/vieneu/models"));
        assert!(l
            .tts_voices()
            .ends_with("engines/vieneu/models/voices.json"));
        assert_eq!(
            l.tts_lib_dir(),
            l.engine_dir(),
            "the SONAME sits by the binary"
        );
        assert!(l
            .tts_dict()
            .unwrap()
            .ends_with("engines/vieneu/models/sea_g2p.bin"));

        // A second engine gets its own space and its own answer to the question
        // that used to be hardcoded: Gemini is cloud and has no lexicon, so it
        // must not be handed VieNeu's — which would \*mispronounce*.
        let cloud = Layout {
            engine: "gemini".into(),
            ..Layout::new("/repo")
        };
        assert_eq!(cloud.tts_dict(), None);
        assert_ne!(cloud.models_dir(), l.models_dir());
        assert_ne!(cloud.tts_binary(), l.tts_binary());
        // And a name nobody declared has no dictionary either, rather than
        // inheriting the local engine's.
        let unknown = Layout {
            engine: "neutts-air".into(),
            ..Layout::new("/repo")
        };
        assert_eq!(unknown.tts_dict(), None);
    }

    /// The one-time move into the engine tree: rename-only, never overwriting,
    /// idempotent. Leaving the old tree where no path points would make the
    /// sidecar unspawnable and the weights unreachable rather than misnamed.
    #[test]
    fn a_pre_engine_tree_checkout_is_renamed_into_the_engine_tree() {
        let root = fixture_root("engine-migrate");
        let l = Layout::new(&root);
        let bm = l.bm_state();
        // The flat tree, as it sat before engines had one — VieNeu's, always.
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(root.join("models/manifest.json"), "{}").unwrap();
        std::fs::write(root.join("models/sea_g2p.bin"), b"lexicon").unwrap();
        std::fs::write(root.join("bm-tts"), b"binary").unwrap();
        std::fs::write(root.join("libonnxruntime.so.1"), b"soname").unwrap();
        std::fs::create_dir_all(bm.join("voices/refs")).unwrap();
        std::fs::write(bm.join("voices/refs/narrator.mp3"), b"clip").unwrap();

        let moved = l.migrate_engine_tree().unwrap();
        assert_eq!(moved.len(), 4, "weights, binary, lib and refs: {moved:?}");
        assert!(l.models_dir().join("sea_g2p.bin").is_file());
        assert!(l.tts_binary().is_file());
        assert!(l.tts_lib_dir().join("libonnxruntime.so.1").is_file());
        assert!(l.voice_refs().join("narrator.mp3").is_file());
        assert!(
            !root.join("models").exists(),
            "the old name is gone, not duplicated"
        );
        assert!(!root.join("bm-tts").exists());
        assert!(
            !bm.join("voices").exists(),
            "the emptied scaffolding goes too"
        );

        // Idempotent: nothing left to move, so a second start is silent.
        assert!(l.migrate_engine_tree().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The migration never overwrites: a target that already exists wins, so a
    /// checkout that has already moved on cannot have its tree clobbered by a
    /// stray flat file.
    #[test]
    fn the_engine_tree_migration_never_overwrites_what_is_already_there() {
        let root = fixture_root("engine-migrate-keep");
        let l = Layout::new(&root);
        std::fs::write(root.join("bm-tts"), b"the old flat binary").unwrap();
        std::fs::create_dir_all(l.engine_dir()).unwrap();
        std::fs::write(l.tts_binary(), b"the engine's own binary").unwrap();

        assert!(l.migrate_engine_tree().unwrap().is_empty());
        assert_eq!(
            std::fs::read(l.tts_binary()).unwrap(),
            b"the engine's own binary"
        );
        // …and the loser is left exactly where it was rather than deleted.
        assert!(root.join("bm-tts").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The prompts come from the adapter: a workspace that carries a `prompts/`
    /// tree speaks its own language, and a checkout that carries none keeps
    /// reading the root's — which is what every workspace read before the
    /// split, so no existing checkout changes behaviour.
    #[test]
    fn prompts_come_from_the_workspace_adapter_and_fall_back_to_the_checkout() {
        let root = fixture_root("adapter-prompts");
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/analyze.txt"), "checkout").unwrap();

        // No tree of its own: the checkout answers, exactly as before.
        let bare = Layout::new(&root);
        assert_eq!(bare.prompts_base(), root);
        assert_eq!(bare.prompt(), root.join("prompts/analyze.txt"));

        // Now the workspace carries one, and *both* prompts move with it: a
        // workspace reading its own `analyze.txt` beside the checkout's
        // `script.txt` would be half one language and half another.
        let book = root.join("workspaces/book");
        std::fs::create_dir_all(book.join("prompts")).unwrap();
        std::fs::write(book.join("prompts/analyze.txt"), "xianxia-en-US").unwrap();
        std::fs::write(book.join("prompts/script.txt"), "xianxia-en-US").unwrap();
        let l = Layout {
            root: root.clone(),
            work: book.clone(),
            adapter: "xianxia-en-US".into(),
            engine: DEFAULT_ENGINE.into(),
        };
        assert_eq!(l.prompts_base(), book, "and the bundle is cut from there");
        assert_eq!(l.prompt(), book.join("prompts/analyze.txt"));
        assert_eq!(l.script_prompt(), book.join("prompts/script.txt"));
        assert_ne!(l.prompt(), bare.prompt());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The adapter is a *home* now, not a pair of directory names: a checkout
    /// carrying `adapters/<name>/` reads its prompts **and** its crawlers from
    /// there, and one carrying no bundle keeps reading the flat trees it always
    /// read — including the pack's `assets/crawl/`, which is where the crawlers
    /// were before they were the language's.
    #[test]
    fn an_adapter_bundle_owns_both_its_prompts_and_its_crawlers() {
        let root = fixture_root("adapter-home");
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/analyze.txt"), "flat").unwrap();

        let flat = Layout {
            adapter: "vi-VN".into(),
            ..Layout::new(&root)
        };
        assert_eq!(flat.adapter_home(), None, "no bundle, no home");
        assert_eq!(
            flat.prompts_base(),
            root,
            "so the prompts are the flat ones"
        );
        assert_eq!(
            flat.crawl_scripts(),
            root.join("assets/crawl"),
            "and the crawlers are still the pack's"
        );

        // With the bundle, both trees answer from one directory.
        let home = root.join("adapters/vi-VN");
        std::fs::create_dir_all(home.join("prompts")).unwrap();
        std::fs::create_dir_all(home.join("crawl")).unwrap();
        std::fs::write(home.join("prompts/analyze.txt"), "vi-VN").unwrap();
        let l = Layout {
            adapter: "vi-VN".into(),
            ..Layout::new(&root)
        };
        assert_eq!(l.adapter_home(), Some(home.clone()));
        assert_eq!(l.prompts_base(), home);
        assert_eq!(l.prompt(), home.join("prompts/analyze.txt"));
        assert_eq!(l.crawl_scripts(), home.join("crawl"));

        // A workspace's own bundle is nearer than the checkout's — the same
        // rule the flat trees already followed.
        let book = root.join("workspaces/book");
        std::fs::create_dir_all(book.join("adapters/vi-VN/crawl")).unwrap();
        let scoped = Layout {
            root: root.clone(),
            work: book.clone(),
            adapter: "vi-VN".into(),
            engine: DEFAULT_ENGINE.into(),
        };
        assert_eq!(scoped.prompts_base(), book.join("adapters/vi-VN"));
        assert_eq!(scoped.crawl_scripts(), book.join("adapters/vi-VN/crawl"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn chapter_title_prefers_subtitle_and_scrubs_illegal_chars() {
        let root = fixture_root("title");
        let l = Layout::new(&root);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::write(
            l.chapter_txt(3),
            "Chương 3: Kiếm khí xung thiên...\n\nbody\n",
        )
        .unwrap();
        assert_eq!(l.chapter_title(3), "Kiếm khí xung thiên");
    }

    /// The crawled headline is a word-for-word machine translation of the
    /// Chinese title, and it is what the mp3 is named after. The digest reads
    /// the chapter and names it, so its `title` is the one that wins — for the
    /// filename *and* for the spoken headline, which is the whole point of
    /// routing both through this one function.
    #[test]
    fn chapter_title_prefers_the_digests_own_title_over_the_mt_headline() {
        let root = fixture_root("title-script");
        let l = Layout::new(&root);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::create_dir_all(l.script_dir()).unwrap();
        std::fs::write(
            l.chapter_txt(9),
            "Chương 9: Tê! Thật là khủng khiếp dao phay\n\nbody\n",
        )
        .unwrap();
        // No script yet: the headline is all there is.
        assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
        // A script with a title: it wins, and the headline is not consulted.
        std::fs::write(
            l.script(9),
            r#"{"title":"Bí Ẩn Dao Phay Trong Phòng Bếp","segments":[]}"#,
        )
        .unwrap();
        assert_eq!(l.chapter_title(9), "Bí Ẩn Dao Phay Trong Phòng Bếp");
        // An empty or whitespace title falls back rather than naming the file "".
        std::fs::write(l.script(9), r#"{"title":"   ","segments":[]}"#).unwrap();
        assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
        // And a script that predates the field does too.
        std::fs::write(l.script(9), r#"{"segments":[]}"#).unwrap();
        assert_eq!(l.chapter_title(9), "Tê! Thật là khủng khiếp dao phay");
    }

    #[test]
    fn chapter_title_falls_back_when_no_text() {
        let root = fixture_root("missing");
        let l = Layout::new(&root);
        assert_eq!(l.chapter_title(9), "Chapter 9");
    }

    #[test]
    fn scratch_shares_the_output_root_so_publish_can_rename() {
        let l = Layout::new("/repo");
        assert!(l.scratch().ends_with("tmp"));
        assert!(l.scratch_ch(7).ends_with("tmp/ch07"));
        assert_eq!(l.scratch_ch(7).parent().unwrap(), l.scratch());
        // `publish` renames scratch -> output. Both must hang off the same
        // workspace or the rename fails with EXDEV.
        assert!(l.scratch().starts_with(&l.work));
        assert!(l.output().starts_with(&l.work));
    }

    #[test]
    fn resolve_pins_the_active_workspace_and_new_stays_legacy() {
        // No pointer: this root is its own workspace, exactly `new()` —
        // a fresh clone just works.
        let l = Layout::resolve("/repo").unwrap();
        assert_eq!(l.work, Path::new("/repo"));
        // ...through the old `.bm/` state paths.
        assert_eq!(
            l.settings(),
            Path::new("/repo/.bm/settings.json"),
            "default workspace keeps its paths"
        );
        assert!(l.scratch().ends_with(".bm/tmp"));
        // A pointer names a directory under workspaces/.
        let dir = std::env::temp_dir().join(format!("bm-resolve{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::create_dir_all(dir.join("workspaces/beyond-myriads")).unwrap();
        std::fs::write(dir.join(".bm/active-workspace"), "beyond-myriads\n").unwrap();
        let l = Layout::resolve(&dir).unwrap();
        assert_eq!(l.work, dir.join("workspaces/beyond-myriads"));
        assert_eq!(
            l.settings(),
            dir.join("workspaces/beyond-myriads/settings.json")
        );
        assert_eq!(
            l.ledger(),
            dir.join("workspaces/beyond-myriads/ledger.json")
        );
        // Machine-global files stay at the root.
        assert_eq!(l.machines(), dir.join(".bm/machines.json"));
        // A pointer at a missing directory is stale, not a fallback.
        std::fs::write(dir.join(".bm/active-workspace"), "gone\n").unwrap();
        let err = Layout::resolve(&dir).unwrap_err();
        assert!(err.to_string().contains("gone"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_catalogue_is_tracked() {
        let l = Layout::new("/repo");
        // The catalogue is repo content: a fresh clone has to render with no
        // local config, so this one is committed at the root.
        assert_eq!(l.roster_default(), Path::new("/repo/voices.default.json"));
        // Everything engine-owned lives under the engine's own tree, which
        // `/engines/` ignores the way `/models/` used to.
        for p in [l.voice_refs(), l.voice_samples()] {
            assert!(
                p.starts_with(l.engine_dir()),
                "{} escaped the engine tree",
                p.display()
            );
        }
        assert!(l.voice_refs().ends_with("engines/vieneu/refs"));
        assert!(l.voice_samples().ends_with("engines/vieneu/samples"));
        // refs and samples are different things and stay separable, so
        // `roster ls` can tell "no sample rendered yet" from "no ref provided".
        assert_ne!(l.voice_refs(), l.voice_samples());
    }

    /// `discover()` reads the *process* cwd, so tests that move it have to take
    /// turns: two of them in parallel race, and the loser resolves the other's
    /// fixture. Poisoning is tolerated — one failing test should not fail its
    /// neighbour for an unrelated reason.
    fn in_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let out = f();
        std::env::set_current_dir(prev).unwrap();
        out
    }

    #[test]
    fn resolve_or_root_hands_back_the_pointer_it_could_not_follow() {
        // The management plane opens on a broken pointer; the error rides
        // along so it can be reported instead of swallowed.
        let dir = std::env::temp_dir().join(format!("bm-lenient{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::write(dir.join(".bm/active-workspace"), "gone\n").unwrap();
        let (layout, err) = Layout::resolve_or_root(&dir);
        assert_eq!(layout.work, dir, "falls back to the root");
        assert!(err.unwrap().contains("gone"));
        // And with no pointer at all there is nothing to report.
        std::fs::remove_file(dir.join(".bm/active-workspace")).unwrap();
        let (layout, err) = Layout::resolve_or_root(&dir);
        assert_eq!(layout.work, dir);
        assert!(err.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_finds_repo_root_from_a_subdir() {
        let root = fixture_root("discover");
        let sub = root.join("data/audio");
        std::fs::create_dir_all(&sub).unwrap();
        let found = in_dir(&sub, Layout::discover).unwrap();
        assert_eq!(
            found.root.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    /// A worker root is a flat mirror: prompts, assets, models, binaries — and
    /// no `rust/` tree, so the repo marker alone would never match. Before this
    /// the launch script's `cd ~/bm-worker && ./bm-agent worker` died at startup
    /// with "no repo root found", which is every remote worker.
    #[test]
    fn discover_finds_a_provisioned_worker_root() {
        let root = std::env::temp_dir().join(format!("bm-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".bm")).unwrap();
        std::fs::write(
            root.join(".bm/profile"),
            r#"{"name":"fixture","hash":"00"}"#,
        )
        .unwrap();
        let found = in_dir(&root, Layout::discover).unwrap();
        // Canonicalize both sides: macOS reports `/private/var/...` for the
        // cwd and `/var/...` for `temp_dir()`, and they are the same place.
        assert_eq!(
            found.root.canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
        // No workspace pointer means the worker root *is* the workspace.
        assert_eq!(found.work, found.root);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The divergence the offer's binding closes.** A worker's root is a flat
    /// mirror: nothing on it names an adapter, so a layout resolved there keys
    /// every cache under `default` while the inductor — which has a pointer and
    /// a ledger — keys them under `vi-VN`. One chapter's takes under two names,
    /// which is a re-render nobody asked for today and would be an empty store
    /// the day a merge looks for what the render wrote.
    #[test]
    fn rebind_moves_the_caches_to_the_adapter_the_offer_is_for() {
        let root = fixture_root("rebind");
        let worker = Layout::resolve(&root).unwrap();
        assert_eq!(worker.adapter, DEFAULT_ADAPTER, "no pointer, no language");
        let audio = worker.data().join("audio");
        assert_eq!(
            worker.seg_dir(DEFAULT_ENGINE, 7),
            audio.join("segments-default-vieneu-07")
        );

        let bound = worker.rebind("vi-VN", "gemini");
        assert_eq!(bound.adapter, "vi-VN");
        assert_eq!(
            bound.seg_dir("gemini", 7),
            audio.join("segments-vi-VN-gemini-v2-07"),
            "the engine is the argument, the adapter is the binding"
        );
        assert_eq!(
            bound.cast("gemini"),
            worker.data().join("cast-vi-VN-gemini-v2.json"),
            "and the cast moves with it, under its historical spelling"
        );
        // A name, not a tree: the root and the workspace are untouched.
        assert_eq!(&bound.root, &worker.root);
        assert_eq!(&bound.work, &worker.work);

        // An empty name is no opinion — an inductor from before the binding
        // rode the offer — and keeps what this box resolved for itself.
        let kept = worker.rebind("", "   ");
        assert_eq!(kept.adapter, DEFAULT_ADAPTER);
        assert_eq!(kept.engine, DEFAULT_ENGINE);
    }

    /// Every home in every scope, sorted, with a name found in both resolving
    /// to the workspace's — the rule `adapter_home()` already follows for the
    /// one in force, so "the tree the bundle ships" and "the tree this run
    /// reads" cannot disagree about which scope an adapter lives in.
    #[test]
    fn adapter_homes_walks_both_scopes_and_the_workspace_wins() {
        let root = fixture_root("homes");
        let book = root.join("workspaces/book");
        std::fs::create_dir_all(&book).unwrap();
        for (scope, name) in [
            (&root, "vi-VN"),
            (&root, "en-US"),
            (&book, "vi-VN"),
            (&book, "ja-JP"),
        ] {
            std::fs::create_dir_all(scope.join(ADAPTERS_DIR).join(name).join("prompts")).unwrap();
        }
        let layout = Layout {
            root: root.clone(),
            work: book.clone(),
            adapter: "vi-VN".into(),
            engine: DEFAULT_ENGINE.into(),
        };
        assert_eq!(
            layout.adapter_homes(),
            vec![
                ("ja-JP".to_string(), book.clone()),
                ("vi-VN".to_string(), book.clone()),
                ("en-US".to_string(), root.clone()),
            ],
            "workspace scope first, each scope sorted, a shadowed name once"
        );
        assert_eq!(
            layout.adapter_home(),
            Some(book.join(ADAPTERS_DIR).join("vi-VN")),
            "and the one in force resolves in that same scope"
        );

        // No `adapters/` anywhere is the pre-split shape, and it is an empty
        // list rather than an error: `Sources::plan` falls back to the flat
        // `prompts/` for exactly this case.
        let bare = Layout::new(fixture_root("homes-bare"));
        assert!(bare.adapter_homes().is_empty());
    }

    /// A script knows its chapter; recovering the workspace from the script's
    /// own path is how the merge path gets a layout at all, and the depth is
    /// easy to get wrong in a way that does not fail — a layout rooted at
    /// `data/` has a perfectly good `chapters()`, just somewhere else, so the
    /// chapter text is silently not found and the chapter loses its title. So
    /// the assertion is on the *resolved* path, not on the return value.
    #[test]
    fn a_script_path_resolves_the_workspace_behind_it() {
        let l = Layout::new(fixture_root("of-script"));
        l.ensure().unwrap();
        std::fs::write(l.chapter_txt(9), "Chương 9: Tiêu đề\n\nbody\n").unwrap();
        std::fs::write(l.script(9), r#"{"segments":[]}"#).unwrap();

        let (back, chapter) = Layout::of_script(&l.script(9)).expect("the script is in a script dir");
        assert_eq!(chapter, 9, "the chapter is the file's own name");
        assert_eq!(
            back.chapter_txt(9),
            l.chapter_txt(9),
            "and the layout is rooted at the workspace, not at data/"
        );
        assert_eq!(back.script(9), l.script(9), "so it also agrees about the script");

        // A path that is not in a script folder is refused rather than guessed
        // at: a wrong guess is a layout that resolves to nothing and says so
        // nowhere.
        assert!(Layout::of_script(&l.chapter_txt(9)).is_none());
        assert!(Layout::of_script(&l.data().join("bible.json")).is_none());
    }
}
