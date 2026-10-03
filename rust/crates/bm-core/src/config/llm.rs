use super::settings::Settings;
use super::*;
use crate::util::{atomic_write, read_json};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

impl Settings {
    pub fn load(path: &Path) -> Settings {
        read_json::<Settings>(path).unwrap_or_default()
    }

    /// Whether the digest's own `title` is this chapter's title.
    ///
    /// See [`Settings::title_mode`]. Only `default` opts out; anything else,
    /// including an empty or misspelled value, keeps the digest's title so an
    /// existing workspace never loses its heading by accident.
    pub fn auto_title(&self) -> bool {
        !self.title_mode.trim().eq_ignore_ascii_case("default")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        atomic_write(path, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Expand the chapter URL template: `{n}` and the padded `{n:03}`.
    ///
    /// **Every occurrence**, not just the first — `…/chuong-{n}?page={n}` is a
    /// real template shape and this behaviour is asserted below. The padded form
    /// lives here rather than in a script because `chapter-{n:03}` is 80% of the
    /// cases that would otherwise need one.
    pub fn chapter_url(&self, n: u32) -> String {
        crate::crawl::expand_template(&self.url_template, n)
    }

    /// The analyzer values the digest lane reads, as the block that travels
    /// with a task offer.
    pub fn analyzer_settings(&self) -> bm_proto::AnalyzerSettings {
        bm_proto::AnalyzerSettings {
            analyze_models: Some(self.analyze_models.clone()),
            gemini_url: self.gemini_url.clone(),
            openrouter_model: self.openrouter_model.clone(),
            openrouter_url: self.openrouter_url.clone(),
            local_model: self.local_model.clone(),
            ollama_url: self.ollama_url.clone(),
            backend: self.analyzer_backend.clone(),
        }
    }

    /// Overlay an offer's analyzer block: the inductor's values win, and
    /// anything it did not send leaves this box's own value alone.
    ///
    /// The inductor is the single source of truth for what the analyzer runs,
    /// because the alternative is what actually happened: a provisioned worker
    /// has **no `.bm/settings.json`** — provisioning copies the sources bundle
    /// and never `.bm/`, which is the
    /// inductor's state — so `Settings::load` falls back to
    /// `Settings::default()` and the *compiled-in* `analyze_models` ran instead
    /// of the operator's. That showed up as a 503 naming a model the operator
    /// had stopped using.
    ///
    /// Only the analyzer's own values are taken. `url_template`, the chapter
    /// range, `control_port`, `advertise` and `ssh` are the inductor's
    /// business and stay where they are.
    pub fn with_analyzer_settings(&self, a: &bm_proto::AnalyzerSettings) -> Settings {
        let mut s = self.clone();
        // `Some` replaces outright — including `Some([])`, which is a
        // deliberate "no chain". `None` is silence, not an instruction to
        // clear.
        if let Some(models) = &a.analyze_models {
            s.analyze_models = models.clone();
        }
        if !a.gemini_url.is_empty() {
            s.gemini_url = a.gemini_url.clone();
        }
        if !a.openrouter_model.is_empty() {
            s.openrouter_model = a.openrouter_model.clone();
        }
        if !a.openrouter_url.is_empty() {
            s.openrouter_url = a.openrouter_url.clone();
        }
        if !a.local_model.is_empty() {
            s.local_model = a.local_model.clone();
        }
        if !a.ollama_url.is_empty() {
            s.ollama_url = a.ollama_url.clone();
        }
        if !a.backend.is_empty() {
            s.analyzer_backend = a.backend.clone();
        }
        s
    }

    /// The address to hand workers, or `None` when the operator has not set one.
    ///
    /// `127.0.0.1` is the sentinel for *unset*, not an address to advertise:
    /// nobody outside this machine can reach an inductor that is only on
    /// loopback, so treating the default as a real value would silently hand
    /// every worker an unreachable URL. Unset falls back to the routing-table
    /// guess, which is correct on a LAN and wrong behind NAT.
    pub fn advertised_host(&self) -> Option<&str> {
        let a = self.advertise.trim();
        if a.is_empty() || matches!(a, "127.0.0.1" | "localhost" | "::1") {
            None
        } else {
            Some(a)
        }
    }

    /// The batch size the scheduler actually uses: `render_batch`, floored at
    /// one and capped at [`MAX_RENDER_BATCH`].
    ///
    /// **Never zero.** `0` would be the honest reading of "batch nothing", and
    /// it is a deadlock: the offer loop would take an empty slice, assign no
    /// row, and the chapter would sit `Pending` for ever with no error anywhere.
    /// A value the operator did not mean is therefore clamped into range rather
    /// than obeyed, and the clamp lives here — one place — so no call site can
    /// forget it.
    pub fn render_batch(&self) -> usize {
        self.render_batch.clamp(1, MAX_RENDER_BATCH) as usize
    }
}

/// One LLM provider: which protocol it speaks, where it lives, the key that
/// activates it, and the model to run. An empty `api_key` means the provider
/// is off — that is the default for all of them, so a fresh machine calls
/// nothing until the operator adds a key (TUI: `L`, or `:llm`).
///
/// Providers are data, not code: the ids and their `kind`s come from
/// `llm.default.json` (copied to `.bm/llm.json` on first run), and a
/// hand-added entry with just a `base_url` rides the OpenAI-compatible path.
/// Nothing outside this file names a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderEntry {
    /// `gemini` (native REST) | `openai` (`POST {base}/chat/completions`) |
    /// `ollama` (`POST {base}/api/chat`). Anything else means `openai`.
    #[serde(default)]
    pub kind: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl ProviderEntry {
    /// Set when the operator gave it a key (Ollama needs none — see
    /// [`LlmConfig::resolve`]).
    pub fn has_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }
}

/// Every LLM provider this machine knows, in one machine-global file.
///
/// `.bm/llm.json`, next to `machines.json`: a key is this machine's access,
/// not a book's, so it does not live in the workspace's `settings.json` and
/// there is no `.env` to keep in sync — the inductor sends the active
/// provider's key with each task offer (see `Credentials::for_stage`), which
/// is the whole sync: switching the model takes effect on the next offer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LlmConfig {
    /// Active provider id (`google` | `openrouter` | `tokenharbor` |
    /// `ollama` | any custom id). Empty means none — the digest refuses
    /// rather than calling a provider the operator never chose.
    pub active: String,
    /// By id. Unknown ids are OpenAI-compatible (`POST {base}/chat/completions`).
    pub providers: BTreeMap<String, ProviderEntry>,
}

/// The wire protocol one provider speaks. Parsed from the entry's `kind`,
/// never from its id: ids are the operator's (`llm.default.json` plus
/// anything added by hand), protocols are the three this program implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmKind {
    /// Native Gemini REST (`:generateContent`).
    Gemini,
    /// OpenAI-compatible (`POST {base}/chat/completions`).
    Openai,
    /// Ollama (`POST {base}/api/chat`).
    Ollama,
}

impl LlmKind {
    /// Anything unrecognised (including absent) is OpenAI-compatible, so a
    /// hand-added gateway needs only a `base_url` to work.
    pub fn parse(kind: &str) -> LlmKind {
        match kind.trim() {
            "gemini" => LlmKind::Gemini,
            "ollama" => LlmKind::Ollama,
            _ => LlmKind::Openai,
        }
    }

    /// The backend slot: the vocabulary the wire (`AnalyzerSettings::backend`,
    /// `Credentials::for_stage`, `generate`) routes on.
    pub fn as_backend(self) -> &'static str {
        match self {
            LlmKind::Gemini => "gemini",
            LlmKind::Openai => "openai",
            LlmKind::Ollama => "ollama",
        }
    }
}

/// A provider ready to call: what the offer carries to the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLlm {
    pub provider: String,
    pub kind: LlmKind,
    /// The provider id itself (`tokenharbor`, not the backend slot) — this
    /// is the `analyzer` wire value, so every progress line and screen names
    /// what the operator picked. Routing reads the entry's `kind`, never
    /// this label.
    pub analyzer: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// The env var the worker installs it as (`GEMINI_API_KEY` |
    /// `OPENROUTER_API_KEY`), so the wire stays one narrowed key per stage.
    pub key_var: &'static str,
}

impl LlmConfig {
    /// Machine-global path: `.bm/llm.json` at the root.
    pub fn path(root: &Path) -> PathBuf {
        root.join(".bm").join("llm.json")
    }

    pub fn load(root: &Path) -> LlmConfig {
        // `.bm/llm.json` wins; the shipped `llm.default.json` is the fallback
        // (tracked, like `voices.default.json`); otherwise empty. What the
        // file names is what exists — no compiled-in providers to merge.
        let root_layout = crate::Layout::new(root);
        let mut cfg: LlmConfig = read_json(&Self::path(root))
            .or_else(|_| read_json(&root_layout.llm_default()))
            .unwrap_or_default();
        // Kinds arrived after keys did: a file written before them names no
        // `kind`, so backfill from the shipped file — same id first, same
        // endpoint when the operator renamed it. Anything still blank rides
        // the OpenAI-compatible path (`LlmKind::parse`), so a hand-added
        // gateway needs only a `base_url` to work.
        let shipped: LlmConfig = read_json(&root_layout.llm_default()).unwrap_or_default();
        fn norm(u: &str) -> String {
            u.trim().trim_end_matches('/').to_string()
        }
        for (id, e) in cfg.providers.iter_mut() {
            if !e.kind.trim().is_empty() {
                continue;
            }
            e.kind = shipped
                .providers
                .get(id)
                .map(|s| s.kind.clone())
                .filter(|k| !k.trim().is_empty())
                .or_else(|| {
                    shipped
                        .providers
                        .values()
                        .find(|s| {
                            !s.base_url.trim().is_empty() && norm(&s.base_url) == norm(&e.base_url)
                        })
                        .map(|s| s.kind.clone())
                })
                .unwrap_or_default();
        }
        cfg
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let p = Self::path(root);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        atomic_write(&p, &serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Load, seeding once from the legacy `settings.json` + environment so an
    /// upgraded machine keeps digesting with what it already used. `opencode`
    /// maps to nothing — that backend is gone, and an operator who relied on
    /// it picks a keyed provider instead.
    ///
    /// The legacy `.env` file is read here, once, for the same reason: it is
    /// retired (nothing loads it at startup anymore), but its keys are still
    /// the operator's, so the seed carries them over rather than stranding
    /// them. Existing process env wins over the file.
    pub fn load_or_seed(root: &Path, settings: &Settings) -> LlmConfig {
        let p = Self::path(root);
        if p.is_file() {
            return Self::load(root);
        }
        // Missing: copy the shipped `llm.default.json` (via `load`'s own
        // fallback), then overlay what the upgraded machine already used.
        let mut cfg = Self::load(root);
        let legacy = read_legacy_env(&root.join(".env"));
        let legacy_var = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    legacy
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|(_, v)| v.clone())
                        .filter(|v| !v.is_empty())
                })
        };
        let gemini_key = legacy_var("GEMINI_API_KEY").unwrap_or_default();
        let or_key = legacy_var("OPENROUTER_API_KEY").unwrap_or_default();
        if !gemini_key.is_empty() {
            let e = cfg.entry_for_kind(LlmKind::Gemini);
            e.api_key = gemini_key;
            if let Some(m) = settings.analyze_models.first() {
                if !m.trim().is_empty() {
                    e.model = m.trim().to_string();
                }
            }
        }
        if !or_key.is_empty() {
            let e = cfg.entry_for_kind(LlmKind::Openai);
            e.api_key = or_key;
            if !settings.openrouter_model.trim().is_empty() {
                e.model = settings.openrouter_model.clone();
            }
            if !settings.openrouter_url.trim().is_empty() {
                e.base_url = settings.openrouter_url.clone();
            }
        }
        if !settings.local_model.trim().is_empty() {
            let e = cfg.entry_for_kind(LlmKind::Ollama);
            e.model = settings.local_model.clone();
            if !settings.ollama_url.trim().is_empty() {
                e.base_url = settings.ollama_url.clone();
            }
        }
        // The legacy backend name follows the first entry that speaks its
        // kind; an id already names its own entry. Anything else stays
        // unactivated — an explicit `a` in the `L` screen, not a guess.
        cfg.active = match settings.analyzer.as_str() {
            id if cfg.providers.contains_key(id) => id.into(),
            "gemini" => cfg.first_of_kind(LlmKind::Gemini),
            "openrouter" => cfg.first_of_kind(LlmKind::Openai),
            "local" => cfg.first_of_kind(LlmKind::Ollama),
            _ => String::new(),
        };
        // …but only when the entry is usable: a keyless non-Ollama entry (or
        // a modelless one) resolves to nothing, and an active value that
        // resolves to nothing is a lie about the state.
        if cfg.resolve().is_none() {
            cfg.active.clear();
        }
        // ponytail: best-effort seed; a failed write surfaces on the next save.
        let _ = cfg.save(root);
        cfg
    }

    /// The protocol one provider id speaks, read from its entry's `kind`.
    /// Missing ids are OpenAI-compatible, so a hand-added gateway needs only
    /// a `base_url` to work.
    pub fn kind_of(&self, id: &str) -> LlmKind {
        self.providers
            .get(id)
            .map(|e| LlmKind::parse(&e.kind))
            .unwrap_or(LlmKind::Openai)
    }

    /// The backend slot for an id or a legacy backend name, or `None` when it
    /// names nothing usable (empty, or a name no provider and no legacy slot
    /// owns). `gemini | local | openrouter` are the retired wire values the
    /// previous release sent — mapped here so old offers still route, not
    /// because any provider is called that.
    pub fn backend_for(&self, analyzer: &str) -> Option<String> {
        if let Some(e) = self.providers.get(analyzer) {
            return Some(LlmKind::parse(&e.kind).as_backend().to_string());
        }
        match analyzer {
            "gemini" => Some("gemini".into()),
            "local" => Some("ollama".into()),
            "openrouter" => Some("openai".into()),
            _ => None,
        }
    }

    /// First provider id speaking a kind, or empty. File order would be
    /// nicest, but the map is sorted — deterministic beats curated here.
    fn first_of_kind(&self, kind: LlmKind) -> String {
        self.providers
            .iter()
            .find(|(_, e)| LlmKind::parse(&e.kind) == kind)
            .map(|(id, _)| id.clone())
            .unwrap_or_default()
    }

    /// The entry to seed a legacy key/model into: the first of its kind, or
    /// a new kind-named slot when the file names none. The id is a protocol
    /// tag, not a provider — routing reads `kind`, never this label.
    fn entry_for_kind(&mut self, kind: LlmKind) -> &mut ProviderEntry {
        let id = self
            .providers
            .iter()
            .find(|(_, e)| LlmKind::parse(&e.kind) == kind)
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| kind.as_backend().to_string());
        self.providers.entry(id).or_insert_with(|| ProviderEntry {
            kind: kind.as_backend().to_string(),
            ..Default::default()
        })
    }

    /// The active provider, or `None` when none is usable. Ollama needs no
    /// key (local URL); every other provider needs a key *and* a model.
    pub fn resolve(&self) -> Option<ResolvedLlm> {
        let id = self.active.trim();
        if id.is_empty() {
            return None;
        }
        let e = self.providers.get(id)?;
        let kind = self.kind_of(id);
        let key_var = match kind {
            LlmKind::Gemini => "GEMINI_API_KEY",
            // Ollama takes no key; every other service shares the one
            // OpenAI-compatible wire slot, with the endpoint riding alongside.
            _ => "OPENROUTER_API_KEY",
        };
        if e.model.trim().is_empty() {
            return None;
        }
        if !matches!(kind, LlmKind::Ollama) && !e.has_key() {
            return None;
        }
        Some(ResolvedLlm {
            provider: id.to_string(),
            kind,
            analyzer: id.to_string(),
            base_url: e.base_url.trim().to_string(),
            model: e.model.trim().to_string(),
            api_key: e.api_key.clone(),
            key_var,
        })
    }

    /// What a digest offer carries: the provider id plus the model/URL block.
    /// The worker has no `llm.json` (provisioning never copies `.bm/`), so
    /// without this it would digest with the compiled-in default.
    pub fn offer_analyzer(&self, fallback: &Settings) -> (String, bm_proto::AnalyzerSettings) {
        match self.resolve() {
            Some(r) => {
                let mut a = bm_proto::AnalyzerSettings {
                    backend: r.kind.as_backend().to_string(),
                    ..Default::default()
                };
                match r.kind {
                    LlmKind::Gemini => {
                        a.analyze_models = Some(vec![r.model.clone()]);
                        a.gemini_url = r.base_url.clone();
                    }
                    LlmKind::Ollama => {
                        a.local_model = r.model.clone();
                        a.ollama_url = r.base_url.clone();
                    }
                    LlmKind::Openai => {
                        a.openrouter_model = r.model.clone();
                        a.openrouter_url = r.base_url.clone();
                    }
                }
                (r.analyzer, a)
            }
            None => {
                // No usable provider: the workspace settings stand in, with
                // the backend resolved the same way — an id the file knows, a
                // retired wire value, or nothing (the worker then refuses).
                let mut a = fallback.analyzer_settings();
                a.backend = self.backend_for(&fallback.analyzer).unwrap_or_default();
                (fallback.analyzer.clone(), a)
            }
        }
    }

    /// Both provider keys this machine holds, so `for_stage` can narrow to
    /// the one the offered stage reads — a digest carries the analyzer's key,
    /// a gemini render the TTS key, a crawl nothing at all. Slots are filled
    /// by kind, never by id.
    pub fn credentials(&self) -> bm_proto::Credentials {
        let mut c = bm_proto::Credentials::default();
        let key_of_kind = |kind: LlmKind| {
            self.providers
                .iter()
                .find(|(_, e)| LlmKind::parse(&e.kind) == kind && e.has_key())
                .map(|(_, e)| e.api_key.clone())
                .unwrap_or_default()
        };
        // The ACTIVE provider's key travels under its kind's slot — never a
        // same-kind neighbour's. Sending the alphabetically-first OpenAI key
        // while TokenHarbor is active is exactly a 401 from the wrong issuer.
        if let Some(e) = self.providers.get(self.active.trim()) {
            match LlmKind::parse(&e.kind) {
                LlmKind::Gemini => c.gemini_api_key = e.api_key.clone(),
                LlmKind::Ollama => {}
                LlmKind::Openai => c.openrouter_api_key = e.api_key.clone(),
            }
        }
        // Legacy-fallback offers (no usable active provider) and the gemini
        // TTS sidecar still need a key: first of the slot. Configured keys
        // above win; nothing overwrites them.
        if c.gemini_api_key.is_empty() {
            c.gemini_api_key = key_of_kind(LlmKind::Gemini);
        }
        if c.openrouter_api_key.is_empty() {
            c.openrouter_api_key = key_of_kind(LlmKind::Openai);
        }
        // Legacy env still counts when llm.json has no key (TTS sidecar and
        // old setups): env never overrides a configured key.
        if c.gemini_api_key.is_empty() {
            c.gemini_api_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
        }
        if c.openrouter_api_key.is_empty() {
            c.openrouter_api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
        }
        c
    }

    /// Mirror the active provider back into the workspace settings so the run
    /// screen, the API preview and the headless CLI commands read the same
    /// model the offers carry. One direction only: `llm.json` wins.
    pub fn sync_settings(&self, s: &mut Settings) {
        let Some(r) = self.resolve() else { return };
        s.analyzer = r.analyzer.clone();
        s.analyzer_backend = r.kind.as_backend().to_string();
        match r.kind {
            LlmKind::Gemini => {
                s.analyze_models = vec![r.model.clone()];
                if !r.base_url.is_empty() {
                    s.gemini_url = r.base_url.clone();
                }
            }
            LlmKind::Ollama => {
                s.local_model = r.model.clone();
                if !r.base_url.is_empty() {
                    s.ollama_url = r.base_url.clone();
                }
            }
            LlmKind::Openai => {
                s.openrouter_model = r.model.clone();
                if !r.base_url.is_empty() {
                    s.openrouter_url = r.base_url.clone();
                }
            }
        }
    }
}

/// Read `KEY=value` lines from a legacy `.env` file: `#` comments and
/// optional quotes. Used once, by [`LlmConfig::load_or_seed`], to carry keys
/// over from the retired file — process env wins, and nothing else reads it.
fn read_legacy_env(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        out.push((key.to_string(), value.to_string()));
    }
    out
}
