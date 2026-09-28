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

/// The one engine that ever had a *flat* tree at the root.
///
/// History, like [`LEGACY_CACHE_ENGINES`]: before the engine tree, `models/`
/// and `bm-tts` were the root's, and they were VieNeu's — so the one-time
/// rename moves them under `engines/vieneu/` whatever the checkout now runs.
/// A second engine never had a flat tree to migrate.
pub const LEGACY_ENGINE: &str = "vieneu";

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

    /// The crawl scripts a profile ships: `assets/crawl/`.
    ///
    /// Under `assets/` because that is what a profile *is* (see
    /// `profile::LIVE_DIRS`) and what provisioning rsyncs to every worker, so a
    /// script and the assets it needs travel together.
    pub fn crawl_scripts(&self) -> PathBuf {
        self.assets().join("crawl")
    }

    /// The active workspace's own crawlers: `workspaces/<name>/crawl/`.
    ///
    /// Profile crawlers (`assets/crawl/`) are shared by every workspace on this
    /// root and replaced wholesale by `:profile load`; a book whose site needs
    /// its own crawler therefore lives here, where `:profile load` cannot reach
    /// it and a second workspace never sees it. Searched **first** by
    /// `crawl::resolve_script`, so a same-named file shadows the profile's —
    /// the workspace's answer wins over the profile's.
    ///
    /// Provisioning rsyncs this directory to every worker (see
    /// `provision::steps::install_sources`) and the stamp hashes it, so an edit
    /// here reaches the cluster with the next `:prov`.
    pub fn crawl_workspace(&self) -> PathBuf {
        self.work.join("crawl")
    }

    pub fn script(&self, n: u32) -> PathBuf {
        self.data().join(format!("script-{n:02}.json"))
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
        self.data().join(format!("render-{n:02}.json"))
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
        if self.work.join("prompts").is_dir() {
            self.work.clone()
        } else {
            self.root.clone()
        }
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
}
