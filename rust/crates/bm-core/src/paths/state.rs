use super::*;

impl Layout {
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
}
