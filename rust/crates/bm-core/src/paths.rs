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

    /// The **global** crawler tree: `crawlers/`, at the checkout root.
    ///
    /// Not a language's and not a workspace's. A crawler is one site read in one
    /// language, but the *set* of crawlers this project has written is a fact
    /// about the project, and keeping it in one place is what lets a preset name
    /// a known site (`crawlers/known/storya.lua`) or the example EPUB crawler
    /// (`crawlers/examples/epub.lua`) without a copy per workspace. One tree, so
    /// an edit reaches every book and a new known site is one file and one
    /// registry row.
    ///
    /// The registry that names these files is `crawlers/knownsites.json`, read
    /// through [`crate::crawl::known_sites`].
    pub fn crawlers_dir(&self) -> PathBuf {
        self.root.join("crawlers")
    }

    /// The crawlers on this machine, for the screens that list them.
    ///
    /// Kept as a method because two callers ask it (the TUI's crawl view and the
    /// provision planner) and answers have to agree: the global tree. A book's
    /// own crawlers are [`Self::crawl_workspace`].
    pub fn crawl_scripts(&self) -> PathBuf {
        self.crawlers_dir()
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
    ///
    /// **Sorted by chapter number, not by path.** `PathBuf`'s own `Ord` is
    /// lexical over the file name, and `NN.json` is only zero-padded to two
    /// digits — so chapter 100 sorted between 10 and 11, and a book past 99 came
    /// out as `1 … 10, 100 … 109, 11, 110 …`. Every consumer walked chapters
    /// backwards and the script window listed them that way. Comparing the
    /// parsed number is the only order that means "chapter order" past 99.
    pub fn scripts(&self) -> Vec<PathBuf> {
        let mut out = chapter_files(&self.script_dir());
        out.sort_by_key(|p| chapter_of(p).unwrap_or(u32::MAX));
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
    /// The layout a script at this path belongs to, and its chapter number.
    ///
    /// **Resolved, not defaulted.** This used to hand back [`Layout::new`],
    /// whose adapter is the hardcoded `"default"` — so every adapter-scoped
    /// fact read through a script path silently degraded, and the most visible
    /// was the spoken chapter heading: `title_speech_for_script` asks the
    /// layout's adapter for its language, found no `adapters/default/`, and
    /// announced `Chương` for an English book whose adapter declares `en-US`.
    /// A checkout resolves its binding, a book-rooted workspace its own
    /// `settings.json`, and a provisioned box its pushed `.bm/profile`, so all
    /// three say which language they write. `new` remains the fallback for a
    /// root that cannot resolve at all.
    pub fn of_script(script_path: &Path) -> Option<(Self, u32)> {
        let chapter = chapter_of(script_path)?;
        let script_dir = script_path.parent()?;
        if script_dir.file_name()? != std::ffi::OsStr::new("script") {
            return None;
        }
        let root = script_dir.parent()?.parent()?;
        Some((Self::resolve(root).unwrap_or_else(|_| Self::new(root)), chapter))
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
    /// So the prompts are **moved**, never rebuilt, into one directory that
    /// names the language: `adapters/<name>/prompts/`. (The crawlers are the
    /// global `crawlers/` tree now, tracked in the repo, so a checkout's old
    /// `assets/crawl/` is left where it is rather than moved into a home the
    /// resolver no longer reads crawlers from.) The name is the checkout's own
    /// when it already names one, and [`LEGACY_ADAPTER`] when it does not.
    /// Rename-only, never overwriting, idempotent — and it does nothing at all
    /// until a pointer exists, because stamping a name is a claim about a
    /// checkout that has loaded something.
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
        // Only the prompts. The crawlers that a pre-split checkout kept in
        // `assets/crawl/` are global now (the tracked `crawlers/` tree), so
        // moving one checkout's old copy into a per-language home would put a
        // second, stale tree where nothing resolves it.
        for (from, to) in [(self.root.join("prompts"), home.join("prompts"))] {
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

    /// The quote-repair template, asked only when the pre-digest gate finds
    /// unbalanced quotation marks.
    ///
    /// A prompt file like the other two, not a string in the digest: an
    /// operator reworking how a chapter is proofread should edit text, and a
    /// language whose prose does not read Vietnamese gets its own wording
    /// without a recompile.
    pub fn repair_prompt(&self) -> PathBuf {
        self.prompts_dir().join("repair.txt")
    }

    /// The pack tree in force: the active workspace's own `assets/` when it
    /// has one, the checkout's when it does not.
    ///
    /// Prompts have been work-scoped since the adapter split, for the same
    /// reason this now is: `:profile load` replaces the checkout's trees
    /// wholesale, and a score every book on the root must share is a score
    /// none of them owns. A workspace that carries its own composition —
    /// `workspaces/<name>/assets/`, `pack.json` and a resolve written at
    /// creation (see `preset::compose_workspace_pack`) — reads its own music,
    /// its own beds, its own scene map; the checkout's tree is the fallback,
    /// which is what every existing workspace still reads, so nothing on disk
    /// changes meaning and no migration is needed.
    ///
    /// This is ROADMAP §3's "what I'd do first", one line of it: a workspace
    /// releasing its own composition and a binding that names it follow from
    /// this, and mostly already have.
    pub fn assets(&self) -> PathBuf {
        if self.owns_assets() {
            self.work.join("assets")
        } else {
            self.root.join("assets")
        }
    }

    /// Whether the `assets/` tree in force is the **workspace's own** rather
    /// than the checkout's.
    ///
    /// The distinction is not cosmetic. A released profile pack describes the
    /// *checkout's* tree: its manifest, its receipt and the fetch that lands it
    /// are all about that one directory, so a release only names what is on
    /// disk when this is false. A workspace that composes its own pack is not
    /// the checkout a release was cut from — its tree travels in the sources
    /// bundle, and a pack release pointed at it would land the wrong book's
    /// `assets/` on the box.
    pub fn owns_assets(&self) -> bool {
        self.work != self.root && self.work.join("assets").is_dir()
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

    /// The reference clips a book owns: `workspaces/<name>/refs/`.
    ///
    /// **Not shared, and not the checkout's.** Voices are not a preset yet
    /// (they arrive as bundles), so `workspace new` puts none in a workspace —
    /// and this resolves the workspace's own tree, finding none, rather than
    /// reaching back to the checkout's. That tree is beyond-myriads': reading
    /// it from another book is how `the-apothecary-diaries` cast from a roster
    /// that was never its own. A checkout root (`work == root`) owns everything
    /// by definition and reads its own `refs/`.
    pub fn refs(&self) -> PathBuf {
        self.work.join("refs")
    }

    /// The clone manifest a book owns: `workspaces/<name>/voices.json`.
    /// `name -> refs/clip` for every enrolled clone. Missing reads as none —
    /// never as the checkout's.
    pub fn voices_manifest(&self) -> PathBuf {
        self.work.join("voices.json")
    }

    /// The sample pool a book owns: `workspaces/<name>/voice-pool.json`. The
    /// registry the cast assigner rolls from. Missing reads as none.
    pub fn voice_pool(&self) -> PathBuf {
        self.work.join("voice-pool.json")
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
    /// the workspace's own **release** build, then its debug one.
    ///
    /// The local worker runs from the repo, where no provision ever installs
    /// `bm-tts`, so a checkout that has only `cargo build`-ed needs a fallback
    /// or a dead sidecar is fatal locally even though a working binary sits one
    /// directory over. The provisioned copy still wins where it exists, so
    /// remote behaviour is unchanged.
    ///
    /// **Release before debug, and that order is load-bearing.** An engine
    /// whose support is a *default-off* cargo feature — `pocket` — cannot be
    /// served by a plain `cargo build --workspace` binary: it exits at startup
    /// ("built without it"). The workspace build produces exactly that binary
    /// at `target/debug/bm-tts`, so preferring debug hands the worker a sidecar
    /// that can never serve the tree it was given, while the working release
    /// build sits unused beside it. The release sidecar is also what the
    /// Makefile requires for rendering at all (a debug one decodes an order of
    /// magnitude slower). Debug remains the last resort for a checkout that has
    /// never been built for release.
    ///
    /// The repo-build fallbacks stay at their historical paths: the build tree
    /// is a build artifact, not an engine's own file, and `cargo` is the one
    /// that decides where it goes.
    pub fn sidecar_binary(&self) -> PathBuf {
        [
            self.tts_binary(),
            self.root.join("rust/target/release/bm-tts"),
            self.root.join("rust/target/debug/bm-tts"),
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
    ///
    /// `threads` is the ONNX intra-op count the sessions open with; `0` is
    /// omitted so the sidecar keeps its own default (half the cores, capped at
    /// 8). See [`crate::config::tts_threads`] for the per-box source.
    pub fn sidecar_command(&self, port: u16, threads: usize) -> (PathBuf, Vec<String>) {
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
        // Only when the box asked: `0` is the sidecar's own default, and
        // spelling it out would freeze the reference's half-core heuristic
        // out of future `bm-tts` builds.
        if threads > 0 {
            args.push("--threads".into());
            args.push(threads.to_string());
        }
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
    ///
    /// **`work == root` alone does not mean legacy.** A layout resolved from a
    /// *book's* directory has `work == root` too, and its state sits in that
    /// directory, not in a `.bm/` nobody wrote. The test is the file itself: a
    /// root that carries `settings.json` is a workspace, and the shim applies
    /// only to a checkout that has never had one. Without this, the same book
    /// reads its settings depending on which directory the layout was resolved
    /// from — which is how `title_mode` (and `speed`, `gap_ms`) went missing
    /// on one path and not the other.
    fn state_file(&self, name: &str) -> PathBuf {
        if self.work == self.root && !self.root.join("settings.json").is_file() {
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
    /// **The script's own `title` wins — unless `title_mode` says `default`.**
    /// The crawled headline is the site's
    /// auto-excerpt of the chapter — `Chương 9: Tê! Thật là khủng khiếp dao
    /// phay`, `Chương 10: Tiền bối đối với dao phay yêu cầu đều cao như vậy?` —
    /// a sentence out of the prose with the punctuation still on it, which then
    /// lands on the cover of the mp3. It is a title only in the sense that the
    /// site put it on the first line. The digest has read the chapter and can
    /// name it, so its `title` is the one used; the headline stays as the
    /// fallback for a chapter that was digested before the field existed.
    ///
    /// [`crate::config::Settings::title_mode`] = `default` inverts that and
    /// takes the headline only. A digest is a fresh model call, so under
    /// `auto` a re-digest can answer a different `title` — and that moves both
    /// the spoken headline and this filename. A book that must not drift pins
    /// the headline instead.
    ///
    /// Both routes end in the same scrub, so a title from either source is a
    /// legal filename and the spoken headline and the file agree.
    pub fn chapter_title(&self, n: u32) -> String {
        let auto = crate::config::Settings::load(&self.settings()).auto_title();
        let from_script = if auto {
            crate::read_json::<serde_json::Value>(&self.script(n))
                .ok()
                .and_then(|d| {
                    d.get("title")
                        .and_then(|t| t.as_str())
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(String::from)
                })
        } else {
            None
        };
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

/// What a directory under `workspaces/` carries: the config that makes it a
/// book, or the reason it is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceConfig {
    /// `settings.json` is there and parses — the file `workspace new` stamps
    /// and the first thing every command reads, so this directory is a book.
    Valid,
    /// No `settings.json`: a directory somebody left here, not a workspace.
    Missing,
    /// The file is there and does not parse. Worse than missing, because
    /// `Settings::load` falls back to defaults rather than refusing — so the
    /// commands would run against settings nobody wrote.
    Broken,
}

/// One directory under `workspaces/`: what it is called, whether the pointer
/// names it, what config it carries, and how far the book has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// The directory name — what `:ws <name>` and `workspace use <name>` take.
    pub name: String,
    /// The pointer names this one: the book every command runs on right now.
    pub active: bool,
    pub config: WorkspaceConfig,
    /// Chapters crawled and scripts written: the two numbers that say whether a
    /// switch is worth making.
    pub chapters: usize,
    pub scripts: usize,
}

/// Every directory under `workspaces/`, by name, with the pointer marked.
///
/// For a *list*, never for a switch — switching is a pointer write, and what a
/// list has to answer is which directories are books at all. An unusable one is
/// listed and marked rather than hidden: the usual way to find one is to have
/// made it by accident, and a row that quietly vanished is how a stale pointer
/// turns into a mystery.
pub fn workspaces(root: &Path) -> Vec<WorkspaceEntry> {
    let active = std::fs::read_to_string(Layout::active_workspace_file(root))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mut out: Vec<WorkspaceEntry> = std::fs::read_dir(root.join("workspaces"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    WorkspaceEntry {
                        active: name == active,
                        config: workspace_config(&e.path()),
                        chapters: files_with_extension(&e.path().join("data").join("chapters"), "txt"),
                        scripts: files_with_extension(&e.path().join("data").join("script"), "json"),
                        name,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A workspace's own `settings.json`, or the reason it is not a book. Read
/// rather than loaded: [`crate::config::Settings::load`] treats a missing file
/// as defaults, which is right for a command that may legitimately run at the
/// repo root and wrong for a question about whether a directory is a workspace.
fn workspace_config(dir: &Path) -> WorkspaceConfig {
    let path = dir.join("settings.json");
    if !path.is_file() {
        return WorkspaceConfig::Missing;
    }
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::config::Settings>(&raw).ok());
    match parsed {
        Some(_) => WorkspaceConfig::Valid,
        None => WorkspaceConfig::Broken,
    }
}

/// How many `*.<ext>` files sit in `dir`. Zero for a directory that is not
/// there, which is an ordinary state: a book nobody has crawled yet.
fn files_with_extension(dir: &Path, ext: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(ext))
                .count()
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
    fn the_workspace_list_says_which_directory_is_a_book() {
        // A list that offers a directory with no settings.json — or with one
        // that does not parse — is offering a switch that fails a second later.
        // So the config is read here and marked, and the switch is refused on
        // the row, instead of the failure arriving as a job error.
        let root = fixture_root("ws-inventory");
        let book = root.join("workspaces/book-a");
        std::fs::create_dir_all(book.join("data/chapters")).unwrap();
        std::fs::create_dir_all(book.join("data/script")).unwrap();
        // Exactly what `workspace new` stamps.
        crate::config::Settings::default()
            .save(&book.join("settings.json"))
            .unwrap();
        std::fs::write(book.join("data/chapters/ch01.txt"), "x").unwrap();
        std::fs::write(book.join("data/script/01.json"), "{}").unwrap();
        // A directory whose settings do not parse, and a file under
        // `workspaces/` that is not a directory at all.
        std::fs::create_dir_all(root.join("workspaces/book-b/data/chapters")).unwrap();
        std::fs::write(root.join("workspaces/book-b/data/chapters/ch01.txt"), "x").unwrap();
        std::fs::write(root.join("workspaces/book-b/settings.json"), "{ not json").unwrap();
        std::fs::write(root.join("workspaces/notes.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join(".bm")).unwrap();
        std::fs::write(Layout::active_workspace_file(&root), "book-a\n").unwrap();

        let found = workspaces(&root);
        assert_eq!(
            found.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
            vec!["book-a", "book-b"],
            "a plain file under workspaces/ is not a workspace"
        );
        assert_eq!(found[0].config, WorkspaceConfig::Valid);
        assert!(found[0].active, "the pointer marks the row it names");
        assert_eq!((found[0].chapters, found[0].scripts), (1, 1));
        assert_eq!(found[1].config, WorkspaceConfig::Broken);
        assert_eq!(
            (found[1].chapters, found[1].scripts),
            (1, 0),
            "the counts come from the tree whatever the config says"
        );
        assert!(!found[1].active);

        // No pointer is not an error: the root is the implicit default, and a
        // list with nothing marked is exactly right for it.
        std::fs::remove_file(Layout::active_workspace_file(&root)).unwrap();
        assert!(workspaces(&root).iter().all(|w| !w.active));
    }

    #[test]
    fn title_mode_default_pins_the_crawled_headline() {
        // A digest is a fresh model call, so its `title` can differ every time
        // it runs; `default` pins the book to the headline the source shipped.
        let root = fixture_root("title-mode");
        let l = Layout::new(&root);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::write(l.chapter_txt(3), "Chapter 3: Maomao\n\nbody\n").unwrap();
        std::fs::create_dir_all(l.script(3).parent().unwrap()).unwrap();
        std::fs::write(
            l.script(3),
            r#"{"title":"The Digest Renamed This","segments":[]}"#,
        )
        .unwrap();
        assert_eq!(
            l.chapter_title(3),
            "The Digest Renamed This",
            "auto is the default and prefers the digest's title"
        );
        let mut s = crate::config::Settings::load(&l.settings());
        s.title_mode = "default".into();
        std::fs::create_dir_all(l.settings().parent().unwrap()).unwrap();
        s.save(&l.settings()).unwrap();
        assert_eq!(
            l.chapter_title(3),
            "Maomao",
            "default takes the headline, so a re-digest cannot move the title"
        );
    }

    #[test]
    fn a_workspace_rooted_layout_keeps_its_adapter_and_its_settings() {
        // Resolved from the *book*, this layout used to report the adapter as
        // `default` — with no manifest to read, so the content language with
        // it — and to look for its settings in a `.bm/` the book does not
        // have. That is how an adapter declaring `en-US` still produced a
        // Vietnamese spoken heading, and how one book read two different
        // settings files depending on which directory resolved it.
        let root = fixture_root("ws-rooted");
        let book = root.join("workspaces/book");
        std::fs::create_dir_all(book.join("adapters/jnovel-en-US")).unwrap();
        std::fs::write(
            book.join("adapters/jnovel-en-US/adapter.json"),
            r#"{"pack":"","language":"en-US","engine":""}"#,
        )
        .unwrap();
        let mut s = crate::config::Settings::default();
        s.profile.adapter.name = "jnovel-en-US".into();
        s.profile.engine.name = "pocket".into();
        s.save(&book.join("settings.json")).unwrap();

        let l = Layout::resolve(&book).unwrap();
        assert_eq!(l.adapter, "jnovel-en-US");
        assert_eq!(l.engine, "pocket");
        assert_eq!(
            l.settings(),
            book.join("settings.json"),
            "a root carrying settings.json is a workspace, not a legacy .bm/"
        );
        assert_eq!(
            crate::adapter::in_force(&l).unwrap().unwrap().language,
            "en-US"
        );
    }

    #[test]
    fn the_sidecar_prefers_the_provisioned_copy_then_the_workspace_build() {
        // Moved with the fallback itself: the local worker runs from the
        // repo, where no provision ever installs `bm-tts`, so a dead sidecar
        // must fall back to a repo build rather than fail the render. The
        // provisioned copy still wins where it exists, so remote behaviour is
        // unchanged.
        //
        // Release wins over debug: a plain `cargo build --workspace` produces a
        // pocket-less debug binary that exits at startup, so preferring it
        // would hand the worker a sidecar that can never serve a pocket tree.
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
        assert_eq!(layout.sidecar_binary(), debug, "debug is the last resort");
        let release = root.join("rust/target/release/bm-tts");
        std::fs::create_dir_all(release.parent().unwrap()).unwrap();
        std::fs::write(&release, b"fake").unwrap();
        assert_eq!(layout.sidecar_binary(), release, "release beats debug");
        let provisioned = layout.tts_binary();
        std::fs::create_dir_all(provisioned.parent().unwrap()).unwrap();
        std::fs::write(&provisioned, b"fake").unwrap();
        assert_eq!(layout.sidecar_binary(), provisioned);
        let (bin, args) = layout.sidecar_command(8818, 0);
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
    /// rename-only, never overwriting, idempotent. The old pack's `assets/crawl/`
    /// stays where it is: crawlers are the global `crawlers/` tree now.
    #[test]
    fn a_pre_adapter_home_checkout_moves_the_prompts_into_the_languages_home() {
        let root = fixture_root("adapter-migrate");
        // The flat language, as it sat before adapters were a directory: the
        // root's prompts, and the old pack crawlers.
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
        assert!(!root.join("prompts").exists(), "moved, not copied");
        assert!(
            root.join("assets/crawl/templates/storya.lua").is_file(),
            "the old pack crawlers are left where they are — the global tree is the live one"
        );
        assert!(
            root.join("assets/music/day-1.mp3").is_file(),
            "the art stays"
        );

        // The name is the pointer's, and it is what the layout now resolves
        // through: the prompts come from the home, the crawlers from the global
        // tree.
        let after = Layout::resolve(&root).unwrap();
        assert_eq!(after.adapter, "vi-VN");
        assert_eq!(after.adapter_home(), Some(home.clone()));
        assert_eq!(after.prompts_base(), home);
        assert_eq!(after.crawl_scripts(), root.join("crawlers"));

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

    /// The pack is work-scoped the way the prompts are: a workspace with its
    /// own `assets/` scores its own book, and a workspace without one keeps
    /// reading the checkout's tree — which is every workspace that exists
    /// today, so nothing changes meaning underneath them.
    #[test]
    fn the_pack_comes_from_the_workspace_and_falls_back_to_the_checkout() {
        let root = fixture_root("workspace-assets");
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/scene-map.json"), "{}").unwrap();

        // No tree of its own: the checkout answers, exactly as before.
        let bare = Layout::new(&root);
        assert_eq!(bare.assets(), root.join("assets"));
        assert_eq!(bare.scene_map(), root.join("assets/scene-map.json"));

        // The workspace's own composition wins whole — scene map, pools,
        // everything a pack owns, because a score that is half the checkout's
        // is a score neither book can trust.
        let book = root.join("workspaces/book");
        std::fs::create_dir_all(book.join("assets")).unwrap();
        std::fs::write(book.join("assets/scene-map.json"), "{}").unwrap();
        std::fs::write(book.join("assets/music-pool.json"), "{}").unwrap();
        let l = Layout {
            root: root.clone(),
            work: book.clone(),
            adapter: DEFAULT_ADAPTER.into(),
            engine: DEFAULT_ENGINE.into(),
        };
        assert_eq!(l.assets(), book.join("assets"));
        assert_eq!(l.scene_map(), book.join("assets/scene-map.json"));
        assert_ne!(l.assets(), bare.assets());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Voice material is the workspace's own and is **not** the checkout's:
    /// a book with no `voices.json` reads none, not beyond-myriads'.
    ///
    /// This is the voice half of the `the-apothecary-diaries` bug. Voices are
    /// not a preset yet (they arrive as bundles), so `workspace new` puts none
    /// in a workspace — and the resolution has to find none too, or the second
    /// book silently casts from the first one's roster. A checkout root
    /// (`work == root`) still reads its own, because those *are* its voices.
    #[test]
    fn voice_material_is_the_workspaces_own_and_not_the_checkouts() {
        let root = fixture_root("workspace-voices");
        std::fs::create_dir_all(root.join("refs")).unwrap();
        std::fs::write(root.join("voices.json"), r#"{"Narrator":"refs/n.wav"}"#).unwrap();
        std::fs::write(
            root.join("voice-pool.json"),
            r#"{"a":{"file":"refs/a.wav","tags":[]}}"#,
        )
        .unwrap();

        // The checkout root owns its own material by definition.
        let bare = Layout::new(&root);
        assert_eq!(bare.voices_manifest(), root.join("voices.json"));
        assert_eq!(bare.voice_pool(), root.join("voice-pool.json"));
        assert_eq!(bare.refs(), root.join("refs"));

        // A workspace with none of its own reads none — never the root's.
        let book = root.join("workspaces/book");
        let l = Layout {
            root: root.clone(),
            work: book.clone(),
            adapter: DEFAULT_ADAPTER.into(),
            engine: DEFAULT_ENGINE.into(),
        };
        assert_eq!(l.voices_manifest(), book.join("voices.json"));
        assert_eq!(l.voice_pool(), book.join("voice-pool.json"));
        assert_eq!(l.refs(), book.join("refs"));
        assert!(
            !l.voices_manifest().exists() && !l.voice_pool().exists(),
            "the checkout's manifests must not answer for the workspace"
        );
        assert!(
            crate::pool::load_manifest(&l.voices_manifest()).is_empty(),
            "a book with no voices casts from none, not from another book's"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The adapter is a *home* now for **prompts**: a checkout carrying
    /// `adapters/<name>/` reads its prompts from there, and one carrying no
    /// bundle keeps reading the flat tree it always read. The crawlers are
    /// neither — they are the global `crawlers/` tree, the same for every
    /// adapter and every workspace.
    #[test]
    fn an_adapter_bundle_owns_its_prompts_and_the_crawlers_are_global() {
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
        assert_eq!(flat.crawl_scripts(), root.join("crawlers"));

        // With the bundle, the prompts answer from one directory.
        let home = root.join("adapters/vi-VN");
        std::fs::create_dir_all(home.join("prompts")).unwrap();
        std::fs::write(home.join("prompts/analyze.txt"), "vi-VN").unwrap();
        let l = Layout {
            adapter: "vi-VN".into(),
            ..Layout::new(&root)
        };
        assert_eq!(l.adapter_home(), Some(home.clone()));
        assert_eq!(l.prompts_base(), home);
        assert_eq!(l.prompt(), home.join("prompts/analyze.txt"));
        // …while the crawlers do not move: they are the global tree either way.
        assert_eq!(l.crawl_scripts(), root.join("crawlers"));

        // A workspace's own prompts are nearer than the checkout's — the same
        // rule the flat trees already followed; the crawlers stay global.
        let book = root.join("workspaces/book");
        std::fs::create_dir_all(book.join("adapters/vi-VN/prompts")).unwrap();
        let scoped = Layout {
            root: root.clone(),
            work: book.clone(),
            adapter: "vi-VN".into(),
            engine: DEFAULT_ENGINE.into(),
        };
        assert_eq!(scoped.prompts_base(), book.join("adapters/vi-VN"));
        assert_eq!(scoped.crawl_scripts(), root.join("crawlers"));
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

    /// Scripts come back in chapter order, and **chapter order is the number**,
    /// not the file name.
    ///
    /// `NN.json` is padded to two digits, so a lexical sort of the paths put
    /// chapter 100 between 10 and 11 — and past 99 the script window, the
    /// audition index and the reconciler all walked the book backwards. This is
    /// the one place the order is defined, so this is where it is pinned.
    #[test]
    fn scripts_come_back_in_chapter_order_past_ninety_nine() {
        let dir = std::env::temp_dir().join(format!("bm-script-order{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        std::fs::create_dir_all(layout.script_dir()).unwrap();
        for n in [1u32, 2, 9, 10, 11, 99, 100, 101, 132] {
            std::fs::write(layout.script(n), "{}").unwrap();
        }
        assert_eq!(
            layout.script_chapters(),
            vec![1, 2, 9, 10, 11, 99, 100, 101, 132],
            "100 must sit after 99, not between 10 and 11"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The workspace's own binding is what it reads: a book stamped with
    /// `--profile` follows its adapter and engine even when the checkout's
    /// pointer names another language — two languages on one checkout is the
    /// preset's whole point. A workspace whose binding does not name the
    /// piece (every one made before presets) keeps following the pointer,
    /// which is the pre-preset behaviour unchanged.
    #[test]
    fn the_active_workspaces_binding_chooses_its_adapter_and_engine() {
        let dir = std::env::temp_dir().join(format!("bm-resolve-bind{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::create_dir_all(dir.join("workspaces/book")).unwrap();
        std::fs::write(dir.join(".bm/active-workspace"), "book\n").unwrap();
        std::fs::write(
            dir.join(".bm/profile"),
            r#"{"pack":{"name":"xianxia","hash":"p"},"adapter":{"name":"vi-VN","hash":"a"},"engine":{"name":"vieneu","hash":""}}"#,
        )
        .unwrap();

        // No workspace settings: the pointer answers, as always.
        let l = Layout::resolve(&dir).unwrap();
        assert_eq!(l.adapter, "vi-VN");
        assert_eq!(l.engine, "vieneu");

        // The workspace stamps its own triple: it wins, piece by piece.
        std::fs::write(
            dir.join("workspaces/book/settings.json"),
            r#"{"profile":{"pack":{"name":"apothecary","hash":"q"},"adapter":{"name":"jnovel-en-US","hash":"b"},"engine":{"name":"pocket","hash":""}}}"#,
        )
        .unwrap();
        let l = Layout::resolve(&dir).unwrap();
        assert_eq!(l.adapter, "jnovel-en-US", "the book's adapter");
        assert_eq!(l.engine, "pocket", "the book's engine");

        // A binding that names only some pieces: the named ones win, the
        // unnamed ones stay the pointer's — a stamp is a claim, not a wipe.
        std::fs::write(
            dir.join("workspaces/book/settings.json"),
            r#"{"profile":{"pack":{"name":"xianxia","hash":"p"},"adapter":{"name":"","hash":""},"engine":{"name":"gemini","hash":""}}}"#,
        )
        .unwrap();
        let l = Layout::resolve(&dir).unwrap();
        assert_eq!(l.adapter, "vi-VN", "unnamed stays the pointer's");
        assert_eq!(l.engine, "gemini", "named wins");
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

    /// A layout reached through a script path keeps the adapter that names the
    /// book's language, on both shapes a book is read from.
    ///
    /// The regression: `of_script` handed back `Layout::new`, whose adapter is
    /// the hardcoded `"default"`, so `title_speech_for_script` found no
    /// `adapters/default/` and announced `Chương` for a book whose adapter
    /// declares `en-US`. The word is inaudible in an English chapter and the
    /// chapter simply loses its heading, so nothing looks wrong but the audio.
    #[test]
    fn a_script_path_keeps_the_adapter_that_declares_the_language() {
        // A book-rooted workspace: its own `settings.json` is the authority.
        let book = Layout::new(fixture_root("of-script-book"));
        book.ensure().unwrap();
        std::fs::create_dir_all(book.root.join("adapters/en-US")).unwrap();
        std::fs::write(
            book.root.join("adapters/en-US/adapter.json"),
            r#"{"pack":"","language":"en-US","engine":""}"#,
        )
        .unwrap();
        let mut settings = crate::config::Settings::default();
        settings.profile.adapter.name = "en-US".into();
        settings.save(&book.root.join("settings.json")).unwrap();
        std::fs::write(book.script(3), r#"{"segments":[]}"#).unwrap();

        let (from_book, chapter) = Layout::of_script(&book.script(3)).expect("a script resolves");
        assert_eq!(chapter, 3);
        assert_eq!(from_book.adapter, "en-US", "the book's own settings name it");
        assert_eq!(
            crate::adapter::in_force(&from_book)
                .expect("readable")
                .expect("a manifest")
                .language,
            "en-US",
            "so the language the heading word is chosen from is the real one"
        );

        // A provisioned box: no `settings.json` and no workspace pointer, so
        // the pushed `.bm/profile` binding is what says which adapter it is.
        let boxy = Layout::new(fixture_root("of-script-box"));
        boxy.ensure().unwrap();
        std::fs::create_dir_all(boxy.root.join("adapters/en-US")).unwrap();
        std::fs::write(
            boxy.root.join("adapters/en-US/adapter.json"),
            r#"{"pack":"","language":"en-US","engine":""}"#,
        )
        .unwrap();
        std::fs::create_dir_all(boxy.root.join(".bm")).unwrap();
        std::fs::write(
            boxy.root.join(".bm/profile"),
            r#"{"pack":{"name":"b","hash":"","version":""},"adapter":{"name":"en-US","hash":"","version":""},"engine":{"name":"pocket","hash":"","version":""}}"#,
        )
        .unwrap();
        std::fs::write(boxy.script(4), r#"{"segments":[]}"#).unwrap();

        let (from_box, chapter) = Layout::of_script(&boxy.script(4)).expect("a script resolves");
        assert_eq!(chapter, 4);
        assert_eq!(
            from_box.adapter, "en-US",
            "a box's pushed binding names its adapter, not `default`"
        );
    }
}
