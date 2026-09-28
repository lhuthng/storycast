//! Runtime settings and LLM provider config.
//!
//! Settings live in the workspace's `settings.json` so the inductor can be
//! reconfigured from the TUI and survive restarts. LLM providers live in
//! `.bm/llm.json` (see [`LlmConfig`]) — machine-global like `machines.json`,
//! because a key is this machine's access, not a book's. The SSH key is a
//! *path*, which is config, not a secret: it lives here (per-machine in
//! `machines.json`, app-wide default below) and never with the keys.

use crate::util::{atomic_write, read_json};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How many render takes one offer carries when the workspace does not say.
///
/// A render is scheduled per **take** (one `render:<ch>:<pos>` row each, so a
/// local edit costs one segment rather than a chapter), but a worker pays for
/// every offer: a process-to-process round trip, a heartbeat, a completion
/// report and a unit collection per take. Batching five takes into one offer
/// amortises that without changing what the ledger records — the batch is an
/// *assignment* detail, and each take still settles on its own row.
///
/// Five is a size that keeps an offer's JSON small (a take is text plus a few
/// numbers) and its lease meaningful, while being a large enough slice that
/// the per-offer overhead stops mattering — and a small enough one that a
/// worker cycles back to the scheduler quickly, where a ready merge outranks
/// its next render batch.
///
/// **The batch is also the unit of sharing.** It is the slice one box claims,
/// and takes are never pinned, so the rest of the chapter stays claimable
/// and the next worker to ask deepens this chapter instead of opening
/// another one. A smaller batch therefore does not give one box less work
/// overall — it lets more boxes work on the *same* chapter at once, which is
/// what makes the first artifact appear sooner.
pub const DEFAULT_RENDER_BATCH: u32 = 5;

/// The largest batch a workspace may ask for.
///
/// A batch is a **lease**, not a queue: the whole batch is `Assigned` to one
/// box for the render lease, so an absurd value would hold a chapter's worth of
/// work hostage on one machine for hours. The cap is what keeps a typo in
/// `settings.json` from being an outage; anything above it is clamped, not
/// refused, because a workspace that cannot run at all is a worse failure than
/// one that runs slower than it asked.
pub const MAX_RENDER_BATCH: u32 = 64;

/// How this workspace crawls, and what a crawl script may spend.
///
/// `mode` is the only switch that changes *where the text comes from*:
///
/// * `script` (the default) — the crawler named by `script` decides. An empty
///   `script` means the built-in fetcher, which is `url_template` plus the
///   element `params.extract` names and nothing else: it knows no site, so it
///   is the fallback for a workspace whose script cannot be read;
/// * `manual` (the default) — nothing is fetched. Chapters are adopted from
///   files the operator supplies (`:import`), one at a time. A workspace that
///   wants the automatic path says `mode: "script"`.
/// * `script` — the crawler named by `script` decides. An empty `script` means
///   the built-in fetcher, which is `url_template` plus the element
///   `params.extract` names and nothing else: it knows no site.
///
/// **Why manual is the default.** Every workspace starts with no novel in it —
/// the operator has not chosen a site yet, and the wrong default fetches:
/// `mode: script` + a template would have a fresh workspace crawl the bundled
/// crawler's home site the first time `:translate` ran. Starting closed makes
/// the first crawl a decision the operator makes about *their* site, and the
/// import path needs no crawler at all.
///
/// **Migration is still the identity for old workspaces.** The bundled default
/// is what a settings file written before `crawl` existed *named implicitly*,
/// so deserializing a file without a `crawl` block keeps it: such a workspace
/// loads `mode: script` with the bundled Storya crawler, and its book crawls
/// byte-identically on the first run after upgrading. The manual default
/// applies only to a workspace created *after* this change — whose settings
/// say `manual` explicitly, or nothing yet at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlSettings {
    /// `script` | `manual`. **`manual` is the default** (see the type docs); a
    /// settings file that predates the whole `crawl` block keeps `script` —
    /// see [`CrawlSettings::legacy_default`].
    pub mode: String,
    /// The crawler. A relative name is resolved against the active workspace
    /// first (`crawl/mysite.lua` there is this book's own crawler, synced to
    /// every worker with the sources), then the root, then `assets/` — which
    /// is why a profile can ship one. `.js`/`.mjs` picks the JavaScript
    /// engine, anything else Lua. Empty means the built-in fetcher.
    pub script: String,
    /// Passed to the script verbatim and never validated: a new site should be
    /// zero changes to Rust.
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Extra headers on every fetch: a `Referer` some sites require, or a
    /// session cookie the operator pasted in. Secrets here live in the
    /// workspace's `settings.json`, not with the provider keys — a deliberate
    /// trade, since a crawl header is per-book configuration.
    pub headers: std::collections::BTreeMap<String, String>,
    /// Empty means the built-in browser-ish default.
    pub user_agent: String,
    /// Minimum gap between two fetches of the same host, in milliseconds.
    ///
    /// **On by default, and that is the point.** A cluster pointed at one site
    /// is the thing that gets a novel scraper banned: ten workers leasing crawl
    /// tasks all arrive at once. Rotating an address is not the fix — the new one
    /// is throttled the same, because the pacing was the problem. `0` disables
    /// it for a local fixture server.
    pub pace_ms: u64,
    /// Per-request timeout.
    pub timeout_secs: u64,
    /// Wall-clock budget for one chapter, engine time included: what stops an
    /// infinite `next`-link loop from owning a worker.
    pub max_seconds: u64,
    /// Network round trips one chapter may make. A listing walk spends several.
    pub max_fetches: u32,
}

impl Default for CrawlSettings {
    /// The default for **`Settings::default()`** — a workspace being created
    /// now, which has named no site yet: `manual`, so nothing is fetched until
    /// the operator says how chapters arrive.
    ///
    /// This is deliberately **not** the default a *deserialized* settings file
    /// gets for an absent `crawl` block — see [`Self::legacy_default`].
    fn default() -> Self {
        CrawlSettings {
            mode: "manual".into(),
            script: String::new(),
            params: serde_json::Map::new(),
            headers: std::collections::BTreeMap::new(),
            user_agent: String::new(),
            pace_ms: 750,
            timeout_secs: 60,
            max_seconds: 180,
            max_fetches: 64,
        }
    }
}

impl CrawlSettings {
    /// What a settings file written before `crawl` existed deserializes to:
    /// **the bundled scripted crawler**, byte-identically to what the old Rust
    /// path fetched for it. That file named a `url_template` and nothing else,
    /// and the whole point of the crawl-script rework is that the same book
    /// crawls the same way after upgrading — a manual default here would turn
    /// every existing workspace off the moment the binary was replaced.
    ///
    /// Reached only when the block is *missing*: a file that says `"crawl":
    // {"mode": "manual"}` said so and is honoured, like any explicit value.
    pub fn legacy_default() -> Self {
        CrawlSettings {
            mode: "script".into(),
            script: crate::crawl::DEFAULT_SCRIPT.into(),
            ..Default::default()
        }
    }

    /// Whether anything is fetched at all.
    pub fn is_manual(&self) -> bool {
        self.mode.eq_ignore_ascii_case("manual")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where chapters come from, e.g. `https://site/truyen/x/chuong-{n}`.
    ///
    /// The built-in mapping: `{n}` is substituted (and `{n:03}` zero-pads) once
    /// per chapter, and it is what fills the chapter index when a crawl script
    /// has no `discover()`. A site whose URLs are slugs needs the script's
    /// `discover` (or a hand-authored index) instead — see [`CrawlSettings`].
    pub url_template: String,
    /// How chapters are crawled. See [`CrawlSettings`].
    ///
    /// The `#[serde(default)]` on this field (via the type's own `default`
    /// attribute) resolves through [`CrawlSettings::legacy_default`]: a file
    /// with no `crawl` block at all is an *old* workspace, and old workspaces
    /// keep crawling the bundled script. `Settings::default()` — a workspace
    /// being created now — gets `manual`, because a fresh workspace has named
    /// no site and must not fetch one.
    #[serde(default = "CrawlSettings::legacy_default")]
    pub crawl: CrawlSettings,
    /// `vieneu` (local, unlimited) or `gemini` (cloud, quota-limited).
    pub engine: String,
    /// Chapter range the cluster is currently working on.
    pub start: u32,
    pub count: u32,
    /// Final-mix tempo and inter-line silence.
    pub speed: f64,
    pub gap_ms: u32,
    /// Per-scene sound design under the voice mix: the effect layer's beds and
    /// stingers. Off means no effect windows at all; room reverb is part of
    /// this layer's scene treatment and goes with it.
    pub ambience: bool,
    /// The background-music layer, independently switchable: a book can want
    /// effects and no music, and the mix should not have to be edited to say so.
    /// Scenes may also opt out individually (`music_off` in the scene map).
    pub music: bool,
    /// Master gains for the three layers, 1.0 = as authored, 0.0 = muted.
    /// They multiply the scene map's own levels (`layers.effect.trim`,
    /// `layers.music.level`, `layers.inject.level`), so retuning the whole mix
    /// is three numbers in the run config instead of an edit per rule.
    /// Range 0.0–2.0.
    pub effect_volume: f64,
    pub music_volume: f64,
    pub inject_volume: f64,
    /// Digest backend, mirrored from [`LlmConfig::active`] by the TUI so the
    /// run screen and the API preview stay truthful: the provider id
    /// (`tokenharbor`) or a legacy backend name (`gemini` | `openrouter` |
    /// `local`). Empty means no provider is active — the digest refuses with
    /// "press L" rather than calling anything.
    pub analyzer: String,
    /// The backend slot the active provider speaks (`gemini` | `openai` |
    /// `ollama`), mirrored with [`Settings::analyzer`]. Routing reads this,
    /// labels read the id — so renaming a provider never reroutes it.
    #[serde(default)]
    pub analyzer_backend: String,
    pub openrouter_model: String,
    /// The model service's base URL. Settings, not a constant, because the
    /// endpoint a key talks to is a deployment fact: the public one by default,
    /// a gateway or a proxy in a locked-down network, and an operator pointing
    /// the digest at a different service should not have to rebuild.
    pub openrouter_url: String,
    pub local_model: String,
    pub ollama_url: String,
    /// Gemini fallback chain, first tried first.
    pub analyze_models: Vec<String>,
    /// Gemini TTS fallback chain, newest first.
    pub model_order: Vec<String>,
    /// Port the inductor's control API listens on.
    pub control_port: u16,
    /// Where the inductor is reachable from workers: a hostname or address
    /// (optionally `host:port`), as seen *from the workers*.
    ///
    /// Unset by default, and read through [`Settings::advertised_host`]. Left
    /// unset the launcher asks the routing table which of this machine's
    /// addresses reaches each box — right on a LAN, and useless from a cloud
    /// worker, which is what this field is for.
    pub advertise: String,
    /// `owner/name` of the GitHub Releases that host the baked model artifact.
    ///
    /// The weights are identical on every box and change only when the
    /// operator re-bakes them, so they are the one payload worth naming:
    /// `tools/models.sh publish` cuts a release tagged by the manifest hash and
    /// a provisioned box downloads that bundle and verifies it itself, instead
    /// of 668 MB arriving over this machine's uplink once per box.
    ///
    /// Empty means the push, which is what every workspace had before this
    /// field existed and what a box behind a firewall that blocks GitHub
    /// still gets. Not a secret and not per-box: it names a public release.
    #[serde(default)]
    pub models_release: String,
    /// Minutes with nothing left to do before the cluster shuts itself down.
    ///
    /// The inductor arms it once the queue has been empty this long, and then
    /// tells each worker to exit. A worker also exits on its own after the same
    /// silence plus a margin, so an inductor that died without saying goodbye
    /// does not leave boxes holding ports and a TTS sidecar. `0` disables the
    /// timer entirely — for a long-lived inductor an operator keeps open.
    pub idle_mins: u32,
    /// How many of one chapter's render takes a single offer carries.
    ///
    /// **Per workspace**, like the rest of this file: `.bm/settings.json` in the
    /// active workspace, so two books can be batched differently (a chapter of
    /// two-hundred-word lines wants a different slice from one of paragraphs).
    /// `Settings::load` fills it from [`DEFAULT_RENDER_BATCH`] when the file
    /// omits it, and [`Settings::render_batch`] clamps it — so an old
    /// `settings.json` needs no edit, and a bad value degrades instead of
    /// stopping the cluster.
    ///
    /// Read by the **inductor**, not the worker: the batch decides how many
    /// ledger rows one offer assigns, which is scheduling, not synthesis. A
    /// worker that predates this simply receives several `render_units` in one
    /// offer, which it already loops over.
    ///
    /// **Smaller means more sharing, not less work per box.** Only the batch is
    /// pinned to the box that takes it, so a small batch is how several workers
    /// end up on the *same* chapter — see [`DEFAULT_RENDER_BATCH`]. Turning it
    /// down to "make the scope smaller" without that pin being batch-scoped is
    /// what once put every worker on a different chapter.
    #[serde(default = "default_render_batch")]
    pub render_batch: u32,
    /// App-wide ssh defaults for binding machines: user, port, key path.
    /// `None` key means ssh decides (agent, `~/.ssh/config`, default keys).
    /// `#[serde(default)]` keeps every existing `settings.json` parsing —
    /// the same trick `analyze_models` below relies on.
    #[serde(default)]
    pub ssh: SshDefaults,
    /// The pieces this workspace runs under, stamped from the load pointer
    /// when the workspace is created. A ledger holding another binding's tasks
    /// refuses to run here rather than mixing two genres' or two languages'
    /// output.
    ///
    /// A binding rather than one pointer: the pack (the genre's art), the
    /// adapter (the language's prompts) and the engine (the voices) travel
    /// together but change independently. A pre-split `settings.json` still
    /// loads: `Binding`'s own `Deserialize` carries the shim, so an old
    /// `{name, hash}` lands as the pack rather than as an empty binding.
    #[serde(default)]
    pub profile: crate::profile::Binding,
}

/// App-wide ssh defaults. The per-machine value in `machines.json` wins;
/// this saves retyping the same key across boxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SshDefaults {
    pub user: String,
    pub port: u16,
    pub key: Option<String>,
}

/// The serde default for [`Settings::render_batch`] — named rather than inlined
/// so the "omitted means ten" rule is one place, and so a `settings.json`
/// written before the field existed loads without an edit.
fn default_render_batch() -> u32 {
    DEFAULT_RENDER_BATCH
}

impl Default for SshDefaults {
    fn default() -> Self {
        SshDefaults {
            user: "thang".into(),
            port: 22,
            key: None,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            url_template: "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}".into(),
            crawl: CrawlSettings::default(),
            engine: "vieneu".into(),
            start: 1,
            count: 1,
            speed: 1.25,
            gap_ms: 300,
            ambience: true,
            music: true,
            effect_volume: 1.0,
            music_volume: 1.0,
            inject_volume: 1.0,
            analyzer: String::new(),
            analyzer_backend: String::new(),
            openrouter_model: "google/gemma-4-31b-it:free".into(),
            openrouter_url: "https://openrouter.ai/api/v1".into(),
            local_model: "gemma-4-12b".into(),
            ollama_url: "http://localhost:11434".into(),
            analyze_models: vec!["gemini-3.5-flash".into()],
            models_release: String::new(),
            model_order: vec![
                "gemini-3.1-flash-tts-preview".into(),
                "gemini-2.5-pro-preview-tts".into(),
                "gemini-2.5-flash-preview-tts".into(),
            ],
            control_port: 8901,
            advertise: "127.0.0.1".into(),
            idle_mins: 5,
            render_batch: DEFAULT_RENDER_BATCH,
            ssh: SshDefaults::default(),
            profile: crate::profile::Binding::default(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        read_json::<Settings>(path).unwrap_or_default()
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
                    LlmKind::Gemini => a.analyze_models = Some(vec![r.model.clone()]),
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
            LlmKind::Gemini => s.analyze_models = vec![r.model.clone()],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_without_ssh_parses_as_defaults_and_roundtrips() {
        // A pre-ssh settings.json has no `ssh` key: it must load as defaults.
        let v: Settings = serde_json::from_str(r#"{"engine":"gemini"}"#).unwrap();
        assert_eq!(v.engine, "gemini");
        assert_eq!(v.ssh.user, "thang");
        assert_eq!(v.ssh.port, 22);
        assert_eq!(v.ssh.key, None);

        let dir = std::env::temp_dir().join("bm-settings-ssh");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        let mut s = Settings::default();
        s.ssh.key = Some("~/.ssh/k".into());
        s.save(&p).unwrap();
        let back = Settings::load(&p);
        assert_eq!(back.ssh.key.as_deref(), Some("~/.ssh/k"));
        assert_eq!(back.ssh.user, "thang");
    }

    #[test]
    fn an_unset_advertise_is_not_an_address_to_hand_out() {
        // The default is the sentinel for "unset". Read as a value it would
        // hand every worker `http://127.0.0.1:8901` — a URL that works on the
        // inductor and nowhere else, which is the silent failure this field
        // exists to prevent.
        let mut s = Settings::default();
        assert_eq!(s.advertise, "127.0.0.1");
        assert!(s.advertised_host().is_none());
        for unset in ["", "  ", "localhost", "::1", "127.0.0.1"] {
            s.advertise = unset.into();
            assert!(s.advertised_host().is_none(), "{unset:?} is not an address");
        }
        for set in ["box.example.com", "203.0.113.9", "box.example.com:8901"] {
            s.advertise = set.into();
            assert_eq!(s.advertised_host(), Some(set));
        }
    }

    #[test]
    fn analyzer_models_default_only_when_omitted() {
        let omitted: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(omitted.analyze_models, vec!["gemini-3.5-flash"]);
        let explicit_empty = bm_proto::AnalyzerSettings {
            analyze_models: Some(vec![]),
            ..Default::default()
        };
        let effective = omitted.with_analyzer_settings(&explicit_empty);
        assert!(effective.analyze_models.is_empty());
        assert_eq!(effective.analyzer_settings().analyze_models, Some(vec![]));
        for models in [vec![], vec!["first", "second"]] {
            let settings: Settings =
                serde_json::from_value(serde_json::json!({"analyze_models": models})).unwrap();
            assert_eq!(settings.analyze_models, models);
            let saved = serde_json::to_value(&settings).unwrap();
            assert_eq!(saved["analyze_models"], serde_json::json!(models));
        }
    }

    #[test]
    fn chapter_url_substitutes_every_n() {
        let s = Settings {
            url_template: "https://x/chuong-{n}?page={n}".into(),
            ..Default::default()
        };
        assert_eq!(s.chapter_url(12), "https://x/chuong-12?page=12");
        // The padded form, which is the whole reason this is not a bare
        // `replace`: a site numbering `chapter-001` needs no script.
        let padded = Settings {
            url_template: "https://x/chapter-{n:03}".into(),
            ..Default::default()
        };
        assert_eq!(padded.chapter_url(7), "https://x/chapter-007");
    }

    #[test]
    fn a_workspace_written_before_scripted_crawls_still_crawls_the_same_way() {
        // The migration promise: a settings.json that only ever named a
        // url_template loads as `mode: script` with the bundled crawler, and
        // that crawler is handed the same URL the old Rust path expanded.
        // The absent `crawl` block means *old workspace*, so it keeps the
        // scripted default — the manual default is for workspaces created now.
        let old: Settings =
            serde_json::from_str(r#"{"url_template":"https://storya.click/truyen/x/chuong-{n}"}"#)
                .unwrap();
        assert_eq!(old.crawl.mode, "script");
        assert_eq!(old.crawl.script, crate::crawl::DEFAULT_SCRIPT);
        assert!(!old.crawl.is_manual(), "the old workspace still fetches");
        assert_eq!(
            old.chapter_url(34),
            "https://storya.click/truyen/x/chuong-34"
        );
        // A workspace created now defaults to manual: nothing fetches until
        // the operator says how chapters arrive.
        let fresh = Settings::default();
        assert!(fresh.crawl.is_manual(), "the fresh default does not fetch");
        assert!(fresh.crawl.script.is_empty());
        // …and an explicit value is honoured. A block that names no script is
        // the built-in fetcher (`script` fills from the per-field default, which
        // is empty — the operator named no crawler); a block naming one gets it.
        let plain: Settings = serde_json::from_str(r#"{"crawl":{"script":""}}"#).unwrap();
        assert_eq!(plain.crawl.script, "");
        assert!(plain.crawl.params.is_empty());
        let named: Settings =
            serde_json::from_str(r#"{"crawl":{"mode":"script","script":"assets/crawl/site.lua"}}"#)
                .unwrap();
        assert_eq!(named.crawl.script, "assets/crawl/site.lua");
        assert!(!named.crawl.is_manual());
    }

    #[test]
    fn settings_roundtrip_and_default_on_missing() {
        let dir = std::env::temp_dir().join("bm-settings-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        assert_eq!(Settings::load(&p).engine, "vieneu");

        let s = Settings {
            engine: "gemini".into(),
            count: 42,
            ..Default::default()
        };
        s.save(&p).unwrap();
        let back = Settings::load(&p);
        assert_eq!(back.engine, "gemini");
        assert_eq!(back.count, 42);
    }

    #[test]
    fn llm_config_defaults_to_nothing_at_all() {
        // Default is none twice over: no active provider AND no providers —
        // slots come from `llm.default.json` (or `.bm/llm.json`), never code.
        let cfg = LlmConfig::default();
        assert!(cfg.active.is_empty());
        assert!(cfg.providers.is_empty());
        assert!(cfg.resolve().is_none());
    }

    /// One test provider: `kind` is what routes, the id is just a label.
    fn entry(kind: &str, key: &str, model: &str) -> ProviderEntry {
        ProviderEntry {
            kind: kind.into(),
            base_url: "https://example/v1".into(),
            api_key: key.into(),
            model: model.into(),
        }
    }

    fn llm_cfg(active: &str, providers: &[(&str, &str, &str, &str)]) -> LlmConfig {
        LlmConfig {
            active: active.into(),
            providers: providers
                .iter()
                .map(|(id, kind, key, model)| (id.to_string(), entry(kind, key, model)))
                .collect(),
        }
    }

    #[test]
    fn llm_resolve_needs_a_key_and_a_model() {
        let mut cfg = llm_cfg("openrouter", &[("openrouter", "openai", "", "")]);
        assert!(cfg.resolve().is_none(), "no key, no model: not usable");
        cfg.providers.get_mut("openrouter").unwrap().api_key = "sk-or-x".into();
        assert!(cfg.resolve().is_none(), "key but no model: not usable");
        cfg.providers.get_mut("openrouter").unwrap().model = "x/y".into();
        let r = cfg.resolve().expect("key + model resolves");
        assert_eq!(r.analyzer, "openrouter");
        assert_eq!(r.key_var, "OPENROUTER_API_KEY");
        // An id the file invented still routes by its kind, not its name.
        cfg.providers
            .insert("my-gateway".into(), entry("openai", "k", "m"));
        cfg.providers.get_mut("my-gateway").unwrap().base_url = "https://gw.example/v1".into();
        cfg.active = "my-gateway".into();
        let r = cfg.resolve().expect("custom provider resolves");
        assert_eq!(r.analyzer, "my-gateway");
        assert_eq!(r.base_url, "https://gw.example/v1");
        // The ollama kind needs a model but no key.
        cfg.providers
            .insert("ollama".into(), entry("ollama", "", ""));
        cfg.active = "ollama".into();
        assert!(cfg.resolve().is_none(), "ollama still model-less");
        cfg.providers.get_mut("ollama").unwrap().model = "gemma-4-12b".into();
        let r = cfg.resolve().expect("ollama resolves keyless");
        assert_eq!(r.analyzer, "ollama");
    }

    #[test]
    fn backend_slots_come_from_kinds_and_legacy_names() {
        // Labels travel, slots decide. Routing reads the entry's `kind`;
        // the retired wire values still map, so old offers keep working.
        let cfg = llm_cfg(
            "",
            &[
                ("google", "gemini", "", ""),
                ("tokenharbor", "openai", "", ""),
                ("my-gateway", "weird-kind", "", ""),
                ("ollama", "ollama", "", ""),
            ],
        );
        for (id, slot) in [
            ("google", "gemini"),
            ("tokenharbor", "openai"),
            ("my-gateway", "openai"),
            ("ollama", "ollama"),
            ("gemini", "gemini"),
            ("local", "ollama"),
            ("openrouter", "openai"),
        ] {
            assert_eq!(cfg.backend_for(id).as_deref(), Some(slot), "{id}");
        }
        for id in ["", "watson", "opencode"] {
            assert_eq!(cfg.backend_for(id), None, "{id} names nothing usable");
        }
    }

    #[test]
    fn the_active_key_travels_never_a_neighbour() {
        // The outage: active TokenHarbor plus a stocked OpenRouter entry sent
        // OpenRouter's key to tokenharbor.ai — a 401 from the wrong issuer
        // that reads exactly like a revoked key.
        let cfg = llm_cfg(
            "tokenharbor",
            &[
                ("google", "gemini", "g-key", "gem"),
                ("openrouter", "openai", "o-key", "o-model"),
                ("tokenharbor", "openai", "t-key", "th-model"),
            ],
        );
        let creds = cfg.credentials().for_stage(
            bm_proto::Stage::Digest,
            &cfg.offer_analyzer(&Settings::default()).1.backend,
            "vieneu",
        );
        assert_eq!(creds.pairs(), vec![("OPENROUTER_API_KEY", "t-key")]);
        // …and the Gemini slot still finds its own key for a gemini render.
        let render = cfg
            .credentials()
            .for_stage(bm_proto::Stage::Render, "gemini", "gemini");
        assert_eq!(render.pairs(), vec![("GEMINI_API_KEY", "g-key")]);
    }

    #[test]
    fn llm_offer_carries_only_the_active_provider() {
        let mut cfg = llm_cfg(
            "google",
            &[
                ("google", "gemini", "g-key", "gemini-3.5-flash"),
                ("tokenharbor", "openai", "", ""),
            ],
        );
        let (analyzer, a) = cfg.offer_analyzer(&Settings::default());
        assert_eq!(analyzer, "google");
        assert_eq!(a.analyze_models, Some(vec!["gemini-3.5-flash".into()]));
        assert_eq!(a.backend, "gemini");
        let creds = cfg
            .credentials()
            .for_stage(bm_proto::Stage::Digest, &a.backend, "vieneu");
        assert_eq!(
            creds.pairs(),
            vec![("GEMINI_API_KEY", "g-key")],
            "the digest offer carries its key and nothing else"
        );
        // Switching provider switches the next offer — that is the whole
        // sync: the id, key, model and slot travel per task.
        cfg.active = "tokenharbor".into();
        cfg.providers.get_mut("tokenharbor").unwrap().api_key = "t-key".into();
        cfg.providers.get_mut("tokenharbor").unwrap().model = "th-model".into();
        cfg.providers.get_mut("tokenharbor").unwrap().base_url = "https://th.example/v1".into();
        let (analyzer, a) = cfg.offer_analyzer(&Settings::default());
        assert_eq!(analyzer, "tokenharbor");
        assert_eq!(a.backend, "openai");
        assert_eq!(a.openrouter_model, "th-model");
        assert_eq!(a.openrouter_url, "https://th.example/v1");
        let creds = cfg
            .credentials()
            .for_stage(bm_proto::Stage::Digest, &a.backend, "vieneu");
        assert_eq!(creds.pairs(), vec![("OPENROUTER_API_KEY", "t-key")]);
    }

    #[test]
    fn llm_seed_migrates_legacy_settings_once() {
        let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("bm-llm-seed{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        let settings = Settings {
            analyzer: "gemini".into(),
            analyze_models: vec!["gemini-3.5-flash-lite".into()],
            ..Settings::default()
        };
        std::env::set_var("BM_LLM_SEED_TEST_G", "seed-key");
        let saved_g = std::env::var("GEMINI_API_KEY").ok();
        std::env::set_var("GEMINI_API_KEY", "seed-key");
        let cfg = LlmConfig::load_or_seed(&dir, &settings);
        assert_eq!(cfg.active, "gemini");
        assert_eq!(cfg.providers["gemini"].model, "gemini-3.5-flash-lite");
        assert!(LlmConfig::path(&dir).is_file(), "the seed is persisted");
        std::env::remove_var("BM_LLM_SEED_TEST_G");
        if let Some(k) = saved_g {
            std::env::set_var("GEMINI_API_KEY", k);
        } else {
            std::env::remove_var("GEMINI_API_KEY");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn llm_seed_reads_the_retired_env_file_once() {
        // The file is retired — nothing loads it at startup — but its keys
        // are still the operator's, so the one-time seed carries them over.
        // Process env wins over the file.
        let dir = std::env::temp_dir().join(format!("bm-llm-env{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::write(
            dir.join(".env"),
            "# legacy\nGEMINI_API_KEY=\"file-key\"\nOPENROUTER_API_KEY=file-or-key\n",
        )
        .unwrap();
        let _g = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_g = std::env::var("GEMINI_API_KEY").ok();
        let saved_or = std::env::var("OPENROUTER_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        std::env::remove_var("OPENROUTER_API_KEY");
        let cfg = LlmConfig::load_or_seed(&dir, &Settings::default());
        assert_eq!(cfg.providers["gemini"].api_key, "file-key");
        assert_eq!(cfg.providers["openai"].api_key, "file-or-key");
        // Second load reads the seeded file, not the legacy one.
        std::fs::remove_file(dir.join(".env")).unwrap();
        let again = LlmConfig::load_or_seed(&dir, &Settings::default());
        assert_eq!(again.providers["gemini"].api_key, "file-key");
        if let Some(k) = saved_g {
            std::env::set_var("GEMINI_API_KEY", k);
        }
        if let Some(k) = saved_or {
            std::env::set_var("OPENROUTER_API_KEY", k);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shipped_default_names_no_key_and_no_model() {
        // The template a fresh clone copies: endpoints only. A default key
        // would be a leaked secret and a default model a choice the operator
        // never made — both are set with `L`, never shipped.
        let root =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../llm.default.json");
        let cfg: LlmConfig =
            read_json(&root).expect("llm.default.json parses — if you moved it, move this test");
        assert!(cfg.active.is_empty());
        for (id, e) in &cfg.providers {
            assert!(e.api_key.is_empty(), "{id} ships a key");
            assert!(e.model.is_empty(), "{id} ships a model");
            assert!(!e.base_url.trim().is_empty(), "{id} has no endpoint");
            assert!(
                ["gemini", "openai", "ollama"].contains(&e.kind.as_str()),
                "{id} ships kind {:?}, which routes nowhere",
                e.kind
            );
        }
        assert_eq!(
            cfg.providers["tokenharbor"].base_url, "https://tokenharbor.ai/v1",
            "the OpenAI-compatible base, not the full /chat/completions path"
        );
    }

    #[test]
    fn llm_load_backfills_kinds_from_the_shipped_file() {
        // Files written before `kind` existed carry keys and models but no
        // routing info. Loading restores it from the shipped data — same id,
        // else same endpoint — instead of stranding them on the default path.
        let dir = std::env::temp_dir().join(format!("bm-llm-kind{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".bm")).unwrap();
        std::fs::write(
            dir.join("llm.default.json"),
            r#"{"active":"","providers":{"google":{"kind":"gemini","base_url":"https://g.example","api_key":"","model":""}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".bm/llm.json"),
            r#"{"active":"google","providers":{"google":{"base_url":"https://g.example","api_key":"k","model":"m"}}}"#,
        )
        .unwrap();
        let cfg = LlmConfig::load(&dir);
        assert_eq!(cfg.kind_of("google"), LlmKind::Gemini);
        assert_eq!(cfg.backend_for("google").as_deref(), Some("gemini"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn llm_load_falls_back_to_the_shipped_default() {
        // No `.bm/llm.json` and no tracked file in this temp root: empty.
        // With a `llm.default.json` beside it: that file's content, and
        // nothing else — the file is the whole roster.
        let dir = std::env::temp_dir().join(format!("bm-llm-fallback{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bare = LlmConfig::load(&dir);
        assert!(bare.active.is_empty());
        assert!(bare.providers.is_empty());
        std::fs::write(
            dir.join("llm.default.json"),
            r#"{"active":"","providers":{"mybox":{"kind":"ollama","base_url":"http://x:11434","api_key":"","model":""}}}"#,
        )
        .unwrap();
        let cfg = LlmConfig::load(&dir);
        assert_eq!(cfg.providers["mybox"].base_url, "http://x:11434");
        assert_eq!(cfg.providers.len(), 1, "no compiled-in ids are merged in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inductors_analyzer_settings_win_over_the_boxes_own() {
        // The outage: a provisioned worker has no `.bm/settings.json` —
        // provisioning copies the sources bundle and never `.bm/` — so
        // `Settings::load` hands back the compiled default.
        let remote_box = Settings::default();
        let inductor = Settings {
            analyze_models: vec!["gemini-3.5-flash-lite".into()],
            openrouter_model: "someone/else".into(),
            ..Settings::default()
        };
        let effective = remote_box.with_analyzer_settings(&inductor.analyzer_settings());
        assert_eq!(effective.analyze_models, vec!["gemini-3.5-flash-lite"]);
        assert_eq!(effective.openrouter_model, "someone/else");
    }

    #[test]
    fn an_inductor_with_no_opinion_leaves_the_boxes_own_analyzer_alone() {
        // An older inductor sends no block at all. Every local value survives,
        // which is what keeps either side upgradable on its own.
        let boxed = Settings {
            analyze_models: vec!["mine-1".into(), "mine-2".into()],
            ollama_url: "http://elsewhere:11434".into(),
            ..Settings::default()
        };
        let same = boxed.with_analyzer_settings(&bm_proto::AnalyzerSettings::default());
        assert_eq!(same.analyze_models, vec!["mine-1", "mine-2"]);
        assert_eq!(same.ollama_url, "http://elsewhere:11434");
    }

    #[test]
    fn a_deliberately_empty_chain_clears_the_boxes_own() {
        let boxed = Settings {
            analyze_models: vec!["stale-1".into()],
            ..Settings::default()
        };
        let cleared = boxed.with_analyzer_settings(&bm_proto::AnalyzerSettings {
            analyze_models: Some(vec![]),
            ..Default::default()
        });
        assert!(
            cleared.analyze_models.is_empty(),
            "{:?}",
            cleared.analyze_models
        );
    }

    #[test]
    fn the_render_batch_defaults_to_five_and_a_saved_value_wins() {
        // Three ways the setting can arrive, and the rule for each:
        //   * absent from settings.json  → five (the compiled default)
        //   * present                    → that value, not the default
        //   * nonsense                   → clamped, never obeyed and never fatal
        let omitted: Settings = serde_json::from_str(r#"{"engine":"vieneu"}"#).unwrap();
        assert_eq!(omitted.render_batch, DEFAULT_RENDER_BATCH);
        assert_eq!(omitted.render_batch(), 5, "and the scheduler sees five");

        let chosen: Settings = serde_json::from_str(r#"{"render_batch":3}"#).unwrap();
        assert_eq!(
            chosen.render_batch(),
            3,
            "a workspace value overrides the default"
        );

        // Zero is the deadlock the clamp exists for: an offer of no takes
        // assigns no row, so the chapter would never leave Pending and nothing
        // anywhere would say why.
        let zero: Settings = serde_json::from_str(r#"{"render_batch":0}"#).unwrap();
        assert_eq!(zero.render_batch(), 1, "zero would offer nothing at all");

        let absurd: Settings = serde_json::from_str(r#"{"render_batch":100000}"#).unwrap();
        assert_eq!(
            absurd.render_batch(),
            MAX_RENDER_BATCH as usize,
            "an absurd batch is a lease held on one box for hours"
        );
    }

    #[test]
    fn a_saved_render_batch_round_trips_through_the_file() {
        // The value has to survive `save`/`load`, because that file is the
        // single source the run screen previews and the next backend boots with.
        let dir = std::env::temp_dir().join("bm-settings-batch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        let s = Settings {
            render_batch: 4,
            ..Default::default()
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p).render_batch(), 4);
        // And a workspace that never mentions it still gets the default.
        std::fs::write(&p, r#"{"engine":"gemini"}"#).unwrap();
        assert_eq!(
            Settings::load(&p).render_batch(),
            DEFAULT_RENDER_BATCH as usize
        );
    }

    #[test]
    fn the_overlay_carries_only_the_analyzer() {
        // `url_template`, the chapter range, the control port and the ssh
        // defaults are the inductor's business. A task offer is not a channel
        // for them, and this path must not become one.
        let boxed = Settings {
            url_template: "https://mine/{n}".into(),
            count: 7,
            ..Settings::default()
        };
        let inductor = Settings {
            url_template: "https://theirs/{n}".into(),
            count: 99,
            ..Settings::default()
        };
        let effective = boxed.with_analyzer_settings(&inductor.analyzer_settings());
        assert_eq!(effective.url_template, "https://mine/{n}");
        assert_eq!(effective.count, 7);
    }
}
