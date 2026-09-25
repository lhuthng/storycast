//! Runtime settings and a tiny `.env` loader.
//!
//! Settings live in `.bm/settings.json` so the inductor can be reconfigured
//! from the TUI and survive restarts. API keys stay in `.env` — never here.
//! The SSH key is a *path*, which is config, not a secret: it lives here
//! (per-machine in `machines.json`, app-wide default below) and never in
//! `.env`.

use crate::util::{atomic_write, read_json};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    /// workspace's `settings.json`, not in `.env` — a deliberate trade, since a
    /// crawl header is per-book configuration and not a provider key.
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
    /// `opencode` | `openrouter` | `local` | `gemini`.
    pub analyzer: String,
    pub opencode_model: String,
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
    /// The profile this workspace runs under, stamped from the load pointer
    /// when the workspace is created. A ledger holding another profile's
    /// tasks refuses to run here rather than mixing two genres' output.
    #[serde(default)]
    pub profile: crate::profile::Pointer,
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
            analyzer: "opencode".into(),
            opencode_model: "opencode/muse-spark-1.3-contributor-free".into(),
            openrouter_model: "google/gemma-4-31b-it:free".into(),
            openrouter_url: "https://openrouter.ai/api/v1".into(),
            local_model: "gemma-4-12b".into(),
            ollama_url: "http://localhost:11434".into(),
            analyze_models: vec!["gemini-3.5-flash".into()],
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
            profile: crate::profile::Pointer::default(),
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
            opencode_model: self.opencode_model.clone(),
            openrouter_model: self.openrouter_model.clone(),
            openrouter_url: self.openrouter_url.clone(),
            local_model: self.local_model.clone(),
            ollama_url: self.ollama_url.clone(),
        }
    }

    /// Overlay an offer's analyzer block: the inductor's values win, and
    /// anything it did not send leaves this box's own value alone.
    ///
    /// The inductor is the single source of truth for what the analyzer runs,
    /// because the alternative is what actually happened: a provisioned worker
    /// has **no `.bm/settings.json`** — provisioning copies `prompts/`,
    /// `python/`, `assets/` and `refs/` and never `.bm/`, which is the
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
        if !a.opencode_model.is_empty() {
            s.opencode_model = a.opencode_model.clone();
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

/// Minimal `.env` reader: `KEY=value`, `#` comments, optional quotes.
///
/// Deliberately not a dependency — the format is trivial and we only ever read
/// a handful of keys. Existing process env always wins, so `FOO=bar cargo run`
/// still overrides the file.
pub fn load_dotenv(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        std::env::set_var(key, value);
    }
}

/// Read an env var, falling back to a default.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
    fn dotenv_does_not_clobber_existing_env() {
        let dir = std::env::temp_dir().join("bm-dotenv-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(".env");
        std::fs::write(
            &p,
            "# comment\nBM_TEST_KEY=\"from-file\"\nBM_TEST_QUOTED='quoted'\n",
        )
        .unwrap();
        std::env::set_var("BM_TEST_KEY", "from-env");
        load_dotenv(&p);
        assert_eq!(std::env::var("BM_TEST_KEY").unwrap(), "from-env");
        assert_eq!(std::env::var("BM_TEST_QUOTED").unwrap(), "quoted");
    }

    #[test]
    fn missing_dotenv_is_not_an_error() {
        load_dotenv(Path::new("/nonexistent/.env"));
    }

    #[test]
    fn the_inductors_analyzer_settings_win_over_the_boxes_own() {
        // The outage: a provisioned worker has no `.bm/settings.json` —
        // provisioning copies `prompts/`, `python/`, `assets/` and `refs/` and
        // never `.bm/` — so `Settings::load` hands back the compiled default.
        let remote_box = Settings::default();
        let inductor = Settings {
            analyze_models: vec!["gemini-3.5-flash-lite".into()],
            opencode_model: "opencode/other".into(),
            ..Settings::default()
        };
        let effective = remote_box.with_analyzer_settings(&inductor.analyzer_settings());
        assert_eq!(effective.analyze_models, vec!["gemini-3.5-flash-lite"]);
        assert_eq!(effective.opencode_model, "opencode/other");
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
