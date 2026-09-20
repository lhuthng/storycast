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
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Layout {
            work: root.clone(),
            root,
        }
    }

    /// Resolve the active workspace: `.bm/active-workspace` names a directory
    /// under `workspaces/`. No pointer means this root *is* the workspace
    /// (the implicit default) — a fresh clone just works, and state appears
    /// under it on demand. Only a stale pointer (naming a missing directory)
    /// is an error: silently running at the root would scatter one book's
    /// state where another was expected.
    pub fn resolve(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let pointer = Self::active_workspace_file(&root);
        if !pointer.is_file() {
            return Ok(Layout::new(root));
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
        Ok(Layout { root, work })
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

    pub fn script(&self, n: u32) -> PathBuf {
        self.data().join(format!("script-{n:02}.json"))
    }

    pub fn bible(&self) -> PathBuf {
        self.data().join("bible.json")
    }

    /// Cast file is per-engine so swapping voices never poisons another
    /// engine's segment cache.
    pub fn cast(&self, engine: &str) -> PathBuf {
        if engine == "vieneu" {
            self.data().join("cast-vieneu.json")
        } else {
            self.data().join("cast.json")
        }
    }

    /// Per-chapter segment cache, also per-engine.
    pub fn seg_dir(&self, engine: &str, n: u32) -> PathBuf {
        if engine == "vieneu" {
            self.data().join(format!("audio/segments-vieneu-{n:02}"))
        } else {
            self.data().join(format!("audio/segments-gemini-v2-{n:02}"))
        }
    }

    pub fn audio(&self) -> PathBuf {
        self.data().join("audio")
    }

    pub fn output(&self) -> PathBuf {
        self.work.join("output")
    }

    /// The digest's first pass: read the chapter, report the cast and the story.
    pub fn prompt(&self) -> PathBuf {
        self.root.join("prompts/analyze.txt")
    }

    /// The digest's second pass: the cast is already resolved, so this one only
    /// has to split the chapter and tag it.
    pub fn script_prompt(&self) -> PathBuf {
        self.root.join("prompts/script.txt")
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

    /// The TTS sidecar binary, `bm-tts`, at the worker root.
    ///
    /// The same spelling on the inductor and on a worker: `root` is the repo
    /// locally and `~/bm-worker` remotely, so one method serves both. This is
    /// what `provision::Probe` looks for and what the agent spawns.
    pub fn tts_binary(&self) -> PathBuf {
        self.root.join("bm-tts")
    }

    /// The baked model directory: one flat directory, codec included.
    ///
    /// Deliberately *not* the Hugging Face cache layout — `bake-models.py`
    /// flattens it so provisioning can rsync bytes and a worker needs no
    /// `huggingface_hub` and no internet.
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Where `libonnxruntime.so.1` sits — beside the binary, at the root.
    ///
    /// This is the directory `LD_LIBRARY_PATH` has to name. The SONAME matters:
    /// the file must be reachable as `libonnxruntime.so.1`, not only under its
    /// versioned filename, or the binary dies at startup with "error while
    /// loading shared libraries".
    pub fn tts_lib_dir(&self) -> PathBuf {
        self.root.clone()
    }

    /// The G2P dictionary inside the model directory.
    pub fn tts_dict(&self) -> PathBuf {
        self.models_dir().join("sea_g2p.bin")
    }

    /// The voice store the Rust server reads: the shipped presets *and* every
    /// enrolled clone, which is why the bake copies it rather than shipping a
    /// separate roster.
    pub fn tts_voices(&self) -> PathBuf {
        self.models_dir().join("voices.json")
    }

    /// The sidecar binary to spawn: the provisioned copy at the worker root
    /// first, then the workspace's own debug/release builds beside it.
    ///
    /// The local worker runs from the repo, where no provision ever installs
    /// `bm-tts` — but `cargo build --workspace` keeps `target/debug/bm-tts`
    /// fresh. Without the fallback a dead sidecar is fatal locally even
    /// though a working binary sits one directory over. Order matters only
    /// in that the provisioned copy wins where it exists, so remote
    /// behaviour is unchanged.
    pub fn sidecar_binary(&self) -> PathBuf {
        [
            self.root.join("bm-tts"),
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
        (
            self.sidecar_binary(),
            vec![
                "--models".into(),
                models.display().to_string(),
                // One directory, codec included — see `tools/bake-models.py`.
                "--codec".into(),
                models.display().to_string(),
                "--dict".into(),
                self.tts_dict().display().to_string(),
                "--voices".into(),
                self.tts_voices().display().to_string(),
                "--port".into(),
                port.to_string(),
                "--bind".into(),
                "127.0.0.1".into(),
            ],
        )
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

    /// The operator's roster: a machine-local delta over the catalogue —
    /// enabled flags, enrolled clones, policy overrides.
    ///
    /// Inside `.bm/`, which `.gitignore` already covers, so keeping personal
    /// voices out of git needs no `.gitignore` change at all.
    pub fn roster(&self) -> PathBuf {
        self.bm_state().join("voices.json")
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

    /// The AWS worker pool definition: region, type, subnet, security group,
    /// instance profile, keypair names, bucket, caps.
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
    pub fn voice_refs(&self) -> PathBuf {
        self.bm_state().join("voices/refs")
    }

    /// Audition clips for the picker — generated, disposable, and deliberately
    /// never synced to a worker, which needs only `voice_refs()` to enrol.
    pub fn voice_samples(&self) -> PathBuf {
        self.bm_state().join("voices/samples")
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
            root.join("bm-tts"),
            "absent everywhere reports the canonical path"
        );
        let debug = root.join("rust/target/debug/bm-tts");
        std::fs::create_dir_all(debug.parent().unwrap()).unwrap();
        std::fs::write(&debug, b"fake").unwrap();
        assert_eq!(layout.sidecar_binary(), debug);
        let provisioned = root.join("bm-tts");
        std::fs::write(&provisioned, b"fake").unwrap();
        assert_eq!(layout.sidecar_binary(), provisioned);
        let (bin, args) = layout.sidecar_command(8818);
        assert_eq!(bin, provisioned);
        assert!(args.windows(2).any(|w| w[0] == "--port" && w[1] == "8818"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_engine_scoped() {
        let l = Layout::new("/repo");
        assert!(l.cast("vieneu").ends_with("cast-vieneu.json"));
        assert!(l.cast("gemini").ends_with("cast.json"));
        assert!(l
            .seg_dir("vieneu", 7)
            .ends_with("data/audio/segments-vieneu-07"));
        assert!(l
            .seg_dir("gemini", 7)
            .ends_with("data/audio/segments-gemini-v2-07"));
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
    fn the_catalogue_is_tracked_but_the_operator_roster_is_not() {
        let l = Layout::new("/repo");
        // The catalogue is repo content: a fresh clone has to render with no
        // local config, so this one is committed at the root.
        assert_eq!(l.roster_default(), Path::new("/repo/voices.default.json"));
        // Everything personal lives under `.bm/`, which `.gitignore` already
        // covers — which is the whole reason the split needs no ignore churn.
        for p in [l.roster(), l.voice_refs(), l.voice_samples()] {
            assert!(p.starts_with(l.bm_state()), "{} escaped .bm/", p.display());
        }
        assert!(l.roster().ends_with(".bm/voices.json"));
        assert!(l.voice_refs().ends_with(".bm/voices/refs"));
        assert!(l.voice_samples().ends_with(".bm/voices/samples"));
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
