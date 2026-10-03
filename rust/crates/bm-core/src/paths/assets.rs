use super::*;

impl Layout {
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
}
