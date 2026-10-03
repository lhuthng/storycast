use super::crawl::CrawlSettings;
use super::*;

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
