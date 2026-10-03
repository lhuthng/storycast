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

/// ONNX intra-op threads the TTS sidecar should open its sessions with.
///
/// A property of the **box**, not of the book, and it rides the environment for
/// the same reason `bm-agent`'s `BM_TTS_*` memory guard does: a provisioned
/// worker has no `settings.json` at all. `BM_TTS_THREADS=0` (or unset) keeps the
/// sidecar's own default — **half the cores, capped at 8**, the reference's
/// choice — and a positive value is the count it opens with.
///
/// Raising it is the one lever that makes a *single* render use more of a box's
/// cores; it cannot buy parallelism on one model, because the sidecar serialises
/// inferences behind its `synth` mutex. Set it to `nproc` on a render box whose
/// cores sit idle, and leave it alone where the box also merges (a merge stops
/// the sidecar before ffmpeg, so there is no overlap to arbitrate).
pub fn tts_threads() -> usize {
    std::env::var("BM_TTS_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

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
    /// The `kind: gemini` slot's endpoint root. Settings for the same reason
    /// [`Self::openrouter_url`] is: the address a key talks to is a deployment
    /// fact. `gemini` names the *protocol* (`kind`, native `:generateContent`
    /// REST), not a host — a compatible gateway is that same slot at a
    /// different address, which is why this field exists rather than a constant
    /// at the call site.
    #[serde(default = "default_gemini_url")]
    pub gemini_url: String,
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
    /// `owner/name` of the GitHub Releases that hold the **profile pack** — the
    /// registries, the clips they register, the attribution, ~60 MB of `assets/`
    /// that is byte-identical on every box.
    ///
    /// Separate from [`models_release`](Self::models_release) on purpose, and
    /// for the reason the two artifacts are published separately: a checkout
    /// with a released pack and an unreleased bake of the weights is the
    /// ordinary case, and a single setting that could only say both or neither
    /// would make the operator choose a 668 MB push to save a 60 MB one.
    ///
    /// Which release is meant is **not** configured here. It is read off the
    /// load pointer's `version` — the one `tools/profile.sh pack <name>
    /// --version 0.1.0` stamps — so the tag is a fact about the profile rather
    /// than a string that can drift from it. A pointer with no version (every
    /// checkout from before versions existed) resolves to no release, which is
    /// the push: the behaviour every box already has.
    #[serde(default)]
    pub packs_release: String,
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
    /// How a chapter too long for one digest answer is split. See
    /// [`DigestSettings`].
    ///
    /// `#[serde(default)]` for the usual reason: a `settings.json` written
    /// before this block existed has to keep loading, and what it means is
    /// "the default", which is the question of whether a chapter splits at all.
    /// A chapter under the budget is one window, and a one-window chapter is
    /// the pre-window digest byte for byte — so an old workspace that never
    /// touches this block behaves exactly as it did, right up to the point
    /// where the alternative was a truncated answer.
    #[serde(default)]
    pub digest: DigestSettings,
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
    /// How many previous chapters' excerpts the attribution pass sees as
    /// `---PREVIOUSLY---` context. 1 (the default) is chapter *n−1* only;
    /// 0 turns the excerpt chain off entirely, and the prompt is then the
    /// pre-excerpt prompt byte for byte. Larger values concatenate older
    /// chapters at their weight — depth 1 is the design; the knob exists so
    /// an operator can zero it or widen it, not so the prompt grows by
    /// default.
    #[serde(default = "default_excerpt_window")]
    pub excerpt_window: u32,
    /// How a rendered take is stored: `raw` keeps the sidecar's PCM wav
    /// (~5.8 MB a minute), the mp3 tiers trade bytes for a re-encode the
    /// final 64k mp3 makes inaudible. `balanced` (96k mono) is the default —
    /// a casual listener cannot hear it against raw, and the store shrinks
    /// about tenfold. A tier change re-speaks: the extension rides the
    /// content-addressed take name, so the plan names new files and the old
    /// ones go stale.
    #[serde(default = "default_take_quality")]
    pub take_quality: String,
    /// Where a chapter's title comes from: `auto` or `default`.
    ///
    /// `auto` (the default) uses the digest's own `title` — the model reads the
    /// chapter and names it — which is why `beyond-myriads` runs this way.
    /// `default` uses the crawled headline instead.
    ///
    /// The distinction is about **stability**. A digest is a fresh model call:
    /// re-running it for any reason (a new model, a repair round, a requeue)
    /// can answer a different `title`, and under `auto` that moves the spoken
    /// headline *and* the `Ch.N - ….mp3` filename with it. A book whose
    /// chapter titles must not drift pins `default`.
    ///
    /// An unrecognised value reads as `auto`: this names a preference, and a
    /// typo should not silently pin a book to its crawled heading.
    #[serde(default = "default_title_mode")]
    pub title_mode: String,
}

fn default_excerpt_window() -> u32 {
    1
}

fn default_take_quality() -> String {
    "balanced".into()
}

fn default_title_mode() -> String {
    "auto".into()
}

/// How the digest splits a chapter that cannot be answered in one call.
///
/// The digest is two rounds — attribution, then staging — and the staging answer
/// carries the chapter's own words (every segment echoes the `source_id` it
/// answers, and any segment that splits or fixes a line also carries its `text`).
/// Every backend caps that answer at **16384 tokens** (`max_tokens` on the
/// OpenAI-compatible slot, `maxOutputTokens` on Gemini, `num_ctx` on Ollama,
/// where the prompt shares the same 16384), and a truncated answer is not a
/// smaller answer: it is JSON that fails to parse, then a repair call that fails
/// the same way, then a chapter shelved. A real novel chapter three times the
/// length of this corpus's longest therefore cannot be digested at all today.
///
/// So the digest runs in **windows** — contiguous runs of the chapter's prepared
/// events, each with its own attribution and staging round, folded back into one
/// script and one bible delta. `plan_windows` decides where they fall; this block
/// is the budget that decides whether they fall at all, plus the two explicit
/// knobs for an operator who wants them smaller for their own reasons.
///
/// The one field that is *not* a budget is the tie between windows: each
/// attribution answer returns a `summary` of its own window, and window *k* is
/// handed the summaries of 1..k as a `PLOT SO FAR` block. Without it a later
/// window is staged by a model that has never been told what the earlier ones
/// established, which on ch1 of a real chapter means the second half is voiced
/// against a cast list that no longer matches who is speaking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DigestSettings {
    /// Sentences per window. `0` means "decide from the budget alone".
    ///
    /// A window closes at the first sentence-final event at or after this many
    /// sentences, so it never ends mid-sentence — and never inside an event,
    /// because a segment's `source_id` names a whole event and the source gate
    /// would refuse a window that cut one in half.
    pub chunk_sentences: u32,
    /// Characters of chapter text per window. `0` means "decide from the budget".
    ///
    /// Characters of *chapter text*, in the currency `window::weight` counts:
    /// an event's own text plus the JSON overhead a segment answering it costs.
    /// It is a ceiling, not a target — where this and the answer budget disagree
    /// the smaller one wins, so a `chunk_chars` set to make windows small is not
    /// undone by a budget that would have allowed one large one.
    pub chunk_chars: u32,
    /// The answer budget for one call, in tokens. **`0` means never split.**
    ///
    /// A soft ceiling under the hard 16384 every backend imposes, because the
    /// estimate that plans a window is an estimate: it assumes the worst case
    /// (a model that echoes every event's text) and converts characters to
    /// tokens at `window::CHARS_PER_TOKEN`. `0` is the escape hatch back to
    /// single-call behaviour — the exact digest of every release before this
    /// existed — which is what makes it usable as a bisect: a chapter that
    /// behaves differently windowed and unwindowed can be compared by changing
    /// one number rather than by editing code.
    pub answer_tokens: u32,
}

impl Default for DigestSettings {
    fn default() -> Self {
        DigestSettings {
            chunk_sentences: 0,
            chunk_chars: 0,
            // Under the 16384 every backend enforces, with room for a model
            // that writes more than it was asked to. Deliberately not the cap:
            // the failure this whole block exists to prevent is the answer that
            // runs off the end, and headroom is cheaper than a repair round.
            answer_tokens: DEFAULT_ANSWER_TOKENS,
        }
    }
}

/// The answer budget a workspace that has not said otherwise gets, in tokens.
///
/// A named constant because three places have to agree about it: this default,
/// the docs that quote it, and the tests that pin a chapter's split. Chosen as
/// ~73% of the 16384 output cap every backend imposes — the estimate below it
/// is conservative, but headroom on a 16384 ceiling is cheap and a truncated
/// answer costs a repair round and then shelves the chapter.
pub const DEFAULT_ANSWER_TOKENS: u32 = 12000;

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

/// The public Gemini endpoint: the `kind: gemini` slot's default base, and the
/// fallback for a settings file that predates [`Settings::gemini_url`].
///
/// `pub` because the one caller that must agree with it is the request builder
/// in `digest/llm.rs`, which appends the protocol's own path to whichever base
/// it is handed.
pub const DEFAULT_GEMINI_URL: &str = "https://generativelanguage.googleapis.com";

/// The serde default for [`Settings::gemini_url`]: an existing `settings.json`
/// has no such key, and a file that omits it must still reach the public
/// service rather than an empty string (which would build a relative URL).
fn default_gemini_url() -> String {
    DEFAULT_GEMINI_URL.to_string()
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
            // **No book by default.** A fresh workspace has named no source, and
            // the wrong default fetches: this used to be the `beyond-myriads`
            // URL, so every new workspace silently pointed at that one book —
            // the digest then produced a cast and a bible for the wrong novel.
            // `crawl` is `manual` for the same reason, and an empty template is
            // the template that agrees with it.
            url_template: String::new(),
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
            gemini_url: default_gemini_url(),
            models_release: String::new(),
            packs_release: String::new(),
            model_order: vec![
                "gemini-3.1-flash-tts-preview".into(),
                "gemini-2.5-pro-preview-tts".into(),
                "gemini-2.5-flash-preview-tts".into(),
            ],
            control_port: 8901,
            advertise: "127.0.0.1".into(),
            idle_mins: 5,
            render_batch: DEFAULT_RENDER_BATCH,
            digest: DigestSettings::default(),
            ssh: SshDefaults::default(),
            profile: crate::profile::Binding::default(),
            excerpt_window: default_excerpt_window(),
            take_quality: default_take_quality(),
            title_mode: default_title_mode(),
        }
    }
}

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

#[cfg(test)]
mod tests;
