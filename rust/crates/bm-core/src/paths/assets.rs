use super::*;

impl Layout {
    pub fn audio(&self) -> PathBuf {
        self.data().join("audio")
    }

    pub fn output(&self) -> PathBuf {
        self.work.join("output")
    }

    /// The directory the live `prompts/` tree hangs off: the workspace when it
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
    pub fn adapter_home(&self) -> Option<PathBuf> {
        [
            self.work.join(ADAPTERS_DIR).join(&self.adapter),
            self.root.join(ADAPTERS_DIR).join(&self.adapter),
        ]
        .into_iter()
        .find(|p| p.is_dir())
    }

    /// The live `prompts/` tree in force, whole. The fallback is not silence:
    pub fn prompts_dir(&self) -> PathBuf {
        self.prompts_base().join("prompts")
    }

    /// The chapter attribution template. The automatic worker adds its prepared
    pub fn prompt(&self) -> PathBuf {
        self.prompts_dir().join("analyze.txt")
    }

    /// The audio-staging contract. The automatic builder appends the immutable
    pub fn script_prompt(&self) -> PathBuf {
        self.prompts_dir().join("script.txt")
    }

    /// The quote-repair template, asked only when the pre-digest gate finds
    pub fn repair_prompt(&self) -> PathBuf {
        self.prompts_dir().join("repair.txt")
    }

    /// The pack tree in force: the active workspace's own `assets/` when it
    pub fn assets(&self) -> PathBuf {
        if self.owns_assets() {
            self.work.join("assets")
        } else {
            self.root.join("assets")
        }
    }

    /// Whether the `assets/` tree in force is the **workspace's own** rather
    pub fn owns_assets(&self) -> bool {
        self.work != self.root && self.work.join("assets").is_dir()
    }

    /// The scene map: the rules, the palette and the layer knobs.
    pub fn scene_map(&self) -> PathBuf {
        self.assets().join("scene-map.json")
    }

    /// One layer's clip registry.
    pub fn pool(&self, kind: crate::audio_pool::PoolKind) -> PathBuf {
        self.assets().join(kind.registry())
    }

    /// The directory one layer's clips live in, under `assets/`.
    pub fn pool_dir(&self, kind: crate::audio_pool::PoolKind) -> PathBuf {
        self.assets().join(kind.dir())
    }

    /// The reference clips a book owns: `workspaces/<name>/refs/`.
    pub fn refs(&self) -> PathBuf {
        self.work.join("refs")
    }

    /// The clone manifest a book owns: `workspaces/<name>/voices.json`.
    pub fn voices_manifest(&self) -> PathBuf {
        self.work.join("voices.json")
    }

    /// The sample pool a book owns: `workspaces/<name>/voice-pool.json`. The
    pub fn voice_pool(&self) -> PathBuf {
        self.work.join("voice-pool.json")
    }

    pub fn python_dir(&self) -> PathBuf {
        self.root.join("python")
    }

    /// One engine's own tree: `engines/<name>/`.
    pub fn engine_dir(&self) -> PathBuf {
        self.root.join(ENGINES_DIR).join(&self.engine)
    }

    /// The TTS sidecar binary: `engines/<name>/bm-tts`, at the worker root.
    pub fn tts_binary(&self) -> PathBuf {
        self.engine_dir().join("bm-tts")
    }

    /// The baked model directory: one flat directory, codec included.
    pub fn models_dir(&self) -> PathBuf {
        self.engine_dir().join("models")
    }

    /// Where `libonnxruntime.so.1` sits — beside the engine's binary.
    /// versioned filename, or the binary dies at startup with "error while
    /// loading shared libraries".
    pub fn tts_lib_dir(&self) -> PathBuf {
        self.engine_dir()
    }

    /// The G2P dictionary the engine's front end reads, if it has one.
    pub fn tts_dict(&self) -> Option<PathBuf> {
        crate::voices::dictionary(&self.engine).map(|name| self.models_dir().join(name))
    }

    /// The voice store the Rust server reads: the shipped presets *and* every
    pub fn tts_voices(&self) -> PathBuf {
        self.models_dir().join("voices.json")
    }

    /// The sidecar binary to spawn: the engine's provisioned copy first, then
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
        if threads > 0 {
            args.push("--threads".into());
            args.push(threads.to_string());
        }
        (self.sidecar_binary(), args)
    }

    /// Interpreter for local voice work (enroll now, preview offline): the
    pub fn venv_python(&self) -> Option<PathBuf> {
        [
            self.python_dir().join(".venv/bin/python"),
            self.root.join(".venv/bin/python"),
        ]
        .into_iter()
        .find(|p| p.is_file())
    }
}
