use super::*;

impl Layout {
    /// Inductor-private state (cluster registry, settings, stats).
    pub fn bm_state(&self) -> PathBuf {
        self.root.join(".bm")
    }

    /// The workspace's own state: settings, ledger, stats. Per book, so two
    pub fn stats(&self) -> PathBuf {
        self.state_file("stats.jsonl")
    }

    pub fn settings(&self) -> PathBuf {
        self.state_file("settings.json")
    }

    /// The task ledger: which chapter/stage is in which state. Per workspace,
    pub fn ledger(&self) -> PathBuf {
        self.state_file("ledger.json")
    }

    /// Workspace state file: under the workspace, except in legacy mode
    fn state_file(&self, name: &str) -> PathBuf {
        if self.work == self.root && !self.root.join("settings.json").is_file() {
            self.root.join(".bm").join(name)
        } else {
            self.work.join(name)
        }
    }

    /// The committed voice catalogue: every preset, its metadata, the accent
    pub fn roster_default(&self) -> PathBuf {
        self.root.join("voices.default.json")
    }

    /// Linked machines: the per-machine connection config (addr/user/port/key),
    pub fn machines(&self) -> PathBuf {
        self.bm_state().join("machines.json")
    }

    /// LLM providers (keys, endpoints, active model): machine-global like
    pub fn llm_config(&self) -> PathBuf {
        self.bm_state().join("llm.json")
    }

    /// The shipped provider template: four keyless, modelless slots and the
    pub fn llm_default(&self) -> PathBuf {
        self.root.join("llm.default.json")
    }

    /// The AWS worker pool definition: region, type, subnet, security group,
    pub fn aws_config(&self) -> PathBuf {
        self.bm_state().join("aws.json")
    }

    /// The tracked AWS template: the *shape* of the pool, which travels with
    pub fn aws_default(&self) -> PathBuf {
        self.root.join(crate::provision::DEFAULT_FILE)
    }

    /// Reference clips for enrolled clones — supplied by the operator and the
    pub fn voice_refs(&self) -> PathBuf {
        self.engine_dir().join("refs")
    }

    /// Audition clips for the picker — generated, disposable, and deliberately
    pub fn voice_samples(&self) -> PathBuf {
        self.engine_dir().join("samples")
    }

    /// Transient working space for the merge stage.
    pub fn scratch(&self) -> PathBuf {
        if self.work == self.root {
            self.root.join(".bm").join("tmp")
        } else {
            self.work.join("tmp")
        }
    }

    /// One chapter's scratch directory. Everything the merge writes — the
    pub fn scratch_ch(&self, n: u32) -> PathBuf {
        self.scratch().join(format!("ch{n:02}"))
    }
}
