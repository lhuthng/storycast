use super::crawl::CrawlSettings;
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where chapters come from, e.g. `https://site/truyen/x/chuong-{n}`.
    pub url_template: String,
    /// How chapters are crawled. See [`CrawlSettings`].
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
    pub ambience: bool,
    /// The background-music layer, independently switchable: a book can want
    pub music: bool,
    /// Master gains for the three layers, 1.0 = as authored, 0.0 = muted.
    pub effect_volume: f64,
    pub music_volume: f64,
    pub inject_volume: f64,
    /// Digest backend, mirrored from [`LlmConfig::active`] by the TUI so the
    pub analyzer: String,
    /// The backend slot the active provider speaks (`gemini` | `openai` |
    #[serde(default)]
    pub analyzer_backend: String,
    pub openrouter_model: String,
    /// The model service's base URL. Settings, not a constant, because the
    pub openrouter_url: String,
    pub local_model: String,
    pub ollama_url: String,
    /// Gemini fallback chain, first tried first.
    pub analyze_models: Vec<String>,
    /// The `kind: gemini` slot's endpoint root. Settings for the same reason
    #[serde(default = "default_gemini_url")]
    pub gemini_url: String,
    /// Gemini TTS fallback chain, newest first.
    pub model_order: Vec<String>,
    /// Port the inductor's control API listens on.
    pub control_port: u16,
    /// Where the inductor is reachable from workers: a hostname or address
    pub advertise: String,
    /// `owner/name` of the GitHub Releases that host the baked model artifact.
    #[serde(default)]
    pub models_release: String,
    /// `owner/name` of the GitHub Releases that hold the **profile pack** — the
    #[serde(default)]
    pub packs_release: String,
    /// Minutes with nothing left to do before the cluster shuts itself down.
    pub idle_mins: u32,
    /// How many of one chapter's render takes a single offer carries.
    #[serde(default = "default_render_batch")]
    pub render_batch: u32,
    /// How a chapter too long for one digest answer is split. See
    #[serde(default)]
    pub digest: DigestSettings,
    /// App-wide ssh defaults for binding machines: user, port, key path.
    #[serde(default)]
    pub ssh: SshDefaults,
    /// The pieces this workspace runs under, stamped from the load pointer
    #[serde(default)]
    pub profile: crate::profile::Binding,
    /// How many previous chapters' excerpts the attribution pass sees as
    #[serde(default = "default_excerpt_window")]
    pub excerpt_window: u32,
    /// How a rendered take is stored: `raw` keeps the sidecar's PCM wav
    #[serde(default = "default_take_quality")]
    pub take_quality: String,
    /// Where a chapter's title comes from: `auto` or `default`.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DigestSettings {
    /// Sentences per window. `0` means "decide from the budget alone".
    pub chunk_sentences: u32,
    /// Characters of chapter text per window. `0` means "decide from the budget".
    pub chunk_chars: u32,
    /// The answer budget for one call, in tokens. **`0` means never split.**
    pub answer_tokens: u32,
}

impl Default for DigestSettings {
    fn default() -> Self {
        DigestSettings {
            chunk_sentences: 0,
            chunk_chars: 0,
            // Under the 16384 every backend enforces, with room for a model
            answer_tokens: DEFAULT_ANSWER_TOKENS,
        }
    }
}

/// The answer budget a workspace that has not said otherwise gets, in tokens.
pub const DEFAULT_ANSWER_TOKENS: u32 = 12000;

/// App-wide ssh defaults. The per-machine value in `machines.json` wins;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SshDefaults {
    pub user: String,
    pub port: u16,
    pub key: Option<String>,
}

/// The serde default for [`Settings::render_batch`] — named rather than inlined
fn default_render_batch() -> u32 {
    DEFAULT_RENDER_BATCH
}

/// The public Gemini endpoint: the `kind: gemini` slot's default base, and the
pub const DEFAULT_GEMINI_URL: &str = "https://generativelanguage.googleapis.com";

/// The serde default for [`Settings::gemini_url`]: an existing `settings.json`
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
