//! Wire types shared by the inductor (orchestrator) and the worker agents.
//!
//! Everything here is plain data plus a few pure helpers, so both binaries can
//! depend on it without dragging in a runtime.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in whole seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The four pipeline stages. TTS is deliberately *not* a stage of its own: it is
/// a service the `Render` stage calls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Fetch a chapter URL and clean it into plain text.
    Crawl,
    /// Digest chapter text into `script-NN.json` and merge the shared bible.
    Digest,
    /// Render per-segment audio through the TTS sidecar.
    Render,
    /// Assemble cached segments into the final `Ch.N - Title.mp3`.
    Merge,
}

impl Stage {
    pub const ALL: [Stage; 4] = [Stage::Crawl, Stage::Digest, Stage::Render, Stage::Merge];

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Crawl => "crawl",
            Stage::Digest => "digest",
            Stage::Render => "render",
            Stage::Merge => "merge",
        }
    }

    pub fn parse(s: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// Stages that must be `Done` before this one may be assigned.
    pub fn upstream(self) -> &'static [Stage] {
        match self {
            Stage::Crawl => &[],
            Stage::Digest => &[Stage::Crawl],
            Stage::Render => &[Stage::Crawl, Stage::Digest],
            Stage::Merge => &[Stage::Crawl, Stage::Digest, Stage::Render],
        }
    }

    /// Whether a stage needs the Python TTS sidecar to be reachable.
    pub fn needs_tts(self) -> bool {
        matches!(self, Stage::Render)
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Pending,
    Assigned,
    Running,
    Done,
    Failed,
    /// Too many strikes: parked so it stops starving healthy chapters.
    Shelved,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Shelved)
    }

    /// Lowercase name, matching the JSON wire form. The TUI filters and colours
    /// by this string, so it must stay in step with `serde`'s rename.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Pending => "pending",
            TaskState::Assigned => "assigned",
            TaskState::Running => "running",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Shelved => "shelved",
        }
    }

    /// Every state, in pipeline order — the filter's vocabulary, printed in the
    /// Tasks screen's hint line so nobody has to guess a spelling.
    pub const ALL: [TaskState; 6] = [
        TaskState::Pending,
        TaskState::Assigned,
        TaskState::Running,
        TaskState::Done,
        TaskState::Failed,
        TaskState::Shelved,
    ];
}

/// One (chapter, stage) unit of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub chapter: u32,
    pub stage: Stage,
    pub state: TaskState,
    pub attempts: u32,
    pub assigned_to: Option<String>,
    /// Unix seconds. When the lease expires the task returns to the pool
    /// *without* a strike — silence is not failure.
    pub lease_until: Option<u64>,
    pub detail: String,
    pub updated: u64,
    /// Machine that must run this task. Set on `Merge` so the segments never
    /// cross the network: merge runs wherever the render happened.
    #[serde(default)]
    pub affinity: Option<String>,
}

impl Task {
    pub fn id(&self) -> String {
        format!("{}:{}", self.stage, self.chapter)
    }

    pub fn new(chapter: u32, stage: Stage) -> Self {
        Task {
            chapter,
            stage,
            state: TaskState::Pending,
            attempts: 0,
            assigned_to: None,
            lease_until: None,
            detail: String::new(),
            updated: now_secs(),
            affinity: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineState {
    #[default]
    Unknown,
    Probing,
    /// Probed and found already provisioned — nothing to distribute.
    Configured,
    Provisioning,
    Online,
    Offline,
    Error,
}

impl MachineState {
    pub fn as_str(self) -> &'static str {
        match self {
            MachineState::Unknown => "unknown",
            MachineState::Probing => "probing",
            MachineState::Configured => "configured",
            MachineState::Provisioning => "provisioning",
            MachineState::Online => "online",
            MachineState::Offline => "offline",
            MachineState::Error => "error",
        }
    }
}

/// A machine the inductor knows how to reach. Machines are added by address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub addr: String,
    pub ssh_user: String,
    pub ssh_port: u16,
    #[serde(default)]
    pub ssh_key: Option<String>,
    /// `worker`, `tts`, or `both`.
    pub role: String,
    pub state: MachineState,
    pub last_seen: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tts_url: Option<String>,
    /// Human-readable note: probe output, error, provision result.
    pub note: String,
}

impl Machine {
    pub fn new(
        addr: &str,
        ssh_user: &str,
        ssh_port: u16,
        ssh_key: Option<String>,
        role: &str,
    ) -> Self {
        Machine {
            id: addr.to_string(),
            addr: addr.to_string(),
            ssh_user: ssh_user.to_string(),
            ssh_port,
            ssh_key,
            role: role.to_string(),
            state: MachineState::Unknown,
            last_seen: 0,
            capabilities: Vec::new(),
            tts_url: None,
            note: String::new(),
        }
    }

    /// `user@host` for ssh/rsync.
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.ssh_user, self.addr)
    }
}

/// Sent by an agent once on startup (and again on reconnect).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Register {
    pub worker_id: String,
    pub addr: String,
    pub hostname: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tts_url: Option<String>,
    pub version: String,
}

/// Sent every few seconds while an agent is alive. This is the real-time
/// visibility channel the TUI renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub worker_id: String,
    pub addr: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub stage: Option<Stage>,
    #[serde(default)]
    pub chapter: Option<u32>,
    /// 0.0..=1.0 within the current task.
    pub progress: f32,
    /// What the worker is doing right now, e.g. "digest ch42 via opencode".
    pub activity: String,
    #[serde(default)]
    pub eta_secs: Option<u64>,
    pub ts: u64,
    /// Machine identity reported by the agent, for the TUI's machine column.
    #[serde(default)]
    pub hostname: String,
    /// Stable display name chosen by the worker at startup and kept in its
    /// root (`worker.alias`). Empty from older agents — the TUI falls back to
    /// hashing the worker id, which churns on every restart.
    #[serde(default)]
    pub alias: String,
}

/// Sent when a task finishes (successfully or not).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Complete {
    pub worker_id: String,
    pub task_id: String,
    pub ok: bool,
    #[serde(default)]
    pub detail: String,
    pub duration_secs: f64,
    /// Digest stage: `{new_characters, new_aliases, roster, speakers}` for the
    /// inductor to merge as the single bible writer.
    #[serde(default)]
    pub bible_delta: Option<serde_json::Value>,
    /// Render stage: how many TTS units actually ran (cache hits excluded).
    #[serde(default)]
    pub units: u64,
    /// Digest stage: the full script, so the inductor holds the artifact and
    /// can hand it to whichever machine renders.
    #[serde(default)]
    pub script: Option<serde_json::Value>,
    /// Crawl stage: the cleaned chapter text, for the same reason.
    #[serde(default)]
    pub text: Option<String>,
    /// Merge stage: the final mp3, base64. Small enough for LAN; this is how
    /// a remote merge's product comes home without shared storage.
    #[serde(default)]
    pub mp3_b64: Option<String>,
}

/// Provider credentials the offered stage will read, sourced from the
/// inductor's own environment.
///
/// Workers are provisioned by *copying files* — `prompts/`, `python/`,
/// `assets/`, `refs/` — and `.env` is deliberately not among them: it is
/// personal and git-ignored, so a remote box has no key of its own. Before
/// this, a digest offered to such a box died on `GEMINI_API_KEY missing` no
/// matter how carefully the operator had set the inductor up, because the
/// key never left the machine that held it.
///
/// The field names are the environment variables the generation backends
/// already read (`bm-core/src/digest/llm.rs`, and `python/tts_router.py` for
/// the TTS sidecar). That is the whole point: installing them on the worker is
/// a loop over [`Credentials::pairs`], not a mapping table that can drift from
/// the code that consumes them.
///
/// An empty string means "not configured on the inductor" and is never
/// installed, so a worker with its own `.env` keeps working; an offer from an
/// inductor that predates this field carries nothing at all and behaves
/// exactly as before.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub gemini_api_key: String,
    #[serde(default)]
    pub openrouter_api_key: String,
}

/// Redacted on purpose. `TaskOffer` derives `Debug` and every task offer is a
/// candidate for a log line or a panic message; a key that reaches a log file
/// is a key that has been leaked, and nothing here needs to print one.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn state(v: &str) -> &'static str {
            if v.is_empty() {
                "unset"
            } else {
                "set"
            }
        }
        f.debug_struct("Credentials")
            .field("gemini_api_key", &state(&self.gemini_api_key))
            .field("openrouter_api_key", &state(&self.openrouter_api_key))
            .finish()
    }
}

impl Credentials {
    /// The provider keys this process holds.
    pub fn from_env() -> Credentials {
        Credentials {
            gemini_api_key: std::env::var("GEMINI_API_KEY").unwrap_or_default(),
            openrouter_api_key: std::env::var("OPENROUTER_API_KEY").unwrap_or_default(),
        }
    }

    /// Narrow to the keys this stage will actually read.
    ///
    /// A crawl offer carries no secret at all; a digest carries the analyzer's
    /// key and nothing else. Sending the whole set on every offer would hand
    /// every worker every credential on the cluster for no reason — the point
    /// of narrowing is that the wire only ever carries what the receiving
    /// stage is about to need.
    pub fn for_stage(self, stage: Stage, analyzer: &str, engine: &str) -> Credentials {
        // The digest backend picks its key from `analyzer`; the backends that
        // need none (`opencode` authenticates itself, `local` is Ollama) get an
        // empty block rather than a gratuitous secret.
        let wanted = match stage {
            Stage::Digest => match analyzer {
                "gemini" => Some("GEMINI_API_KEY"),
                "openrouter" => Some("OPENROUTER_API_KEY"),
                _ => None,
            },
            // The TTS sidecar reads `GEMINI_API_KEY` from its own environment
            // and is spawned by the worker, so it inherits whatever is
            // installed. The local engine needs nothing.
            Stage::Render if engine == "gemini" => Some("GEMINI_API_KEY"),
            _ => None,
        };
        match wanted {
            Some("GEMINI_API_KEY") => Credentials {
                openrouter_api_key: String::new(),
                ..self
            },
            Some("OPENROUTER_API_KEY") => Credentials {
                gemini_api_key: String::new(),
                ..self
            },
            _ => Credentials::default(),
        }
    }

    /// `(environment variable, value)` for everything actually set — the one
    /// place the wire field names meet the env names.
    pub fn pairs(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        if !self.gemini_api_key.is_empty() {
            out.push(("GEMINI_API_KEY", self.gemini_api_key.as_str()));
        }
        if !self.openrouter_api_key.is_empty() {
            out.push(("OPENROUTER_API_KEY", self.openrouter_api_key.as_str()));
        }
        out
    }

    /// Just the variable names, for a log line that must never carry a value.
    pub fn names(&self) -> Vec<&'static str> {
        self.pairs().into_iter().map(|(name, _)| name).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.gemini_api_key.is_empty() && self.openrouter_api_key.is_empty()
    }
}

/// The analyzer's model configuration, exactly as the inductor holds it.
///
/// [`TaskOffer::analyzer`] names the *backend*; this names what that backend
/// runs. Both have to travel, because the worker's copy of `Settings` is not
/// the operator's: provisioning copies `prompts/`, `python/`, `assets/` and
/// `refs/` and never `.bm/` (that is the inductor's state), so a remote box has
/// **no `.bm/settings.json` at all** and `Settings::load` silently returns
/// `Settings::default()`.
///
/// That default is compiled in and names `gemini-3.5-flash`. A box the
/// operator had configured for `gemini-3.5-flash-lite` therefore ran
/// `gemini-3.5-flash` instead, and the only evidence was the model name inside
/// a 503 — which is exactly the outage this block closes.
///
/// Empty strings mean "the inductor said nothing" and leave the worker's own
/// value alone, so an older inductor's offer still behaves as before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzerSettings {
    /// The model the chain stands on when there is no chain.
    #[serde(default)]
    pub analyze_model: String,
    /// The fallback chain, tried in order.
    ///
    /// `None` means "the inductor said nothing" (an older inductor); `Some([])`
    /// means "no chain — `analyze_model` stands alone", which is a real
    /// configuration and the default in `Settings::default()`. The `Option` is
    /// what keeps those two apart, for the same reason
    /// [`TaskOffer::render_units`] is one.
    #[serde(default)]
    pub analyze_models: Option<Vec<String>>,
    #[serde(default)]
    pub opencode_model: String,
    #[serde(default)]
    pub openrouter_model: String,
    #[serde(default)]
    pub local_model: String,
    #[serde(default)]
    pub ollama_url: String,
}

/// A worker asking for work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub worker_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// The inductor's answer: either a task or "nothing for you".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOffer {
    pub task_id: String,
    pub chapter: u32,
    pub stage: Stage,
    /// Absolute path to the repo root the worker should operate in.
    pub root: String,
    /// Where to fetch the chapter from (crawl stage).
    #[serde(default)]
    pub url: Option<String>,
    /// TTS sidecar base URL (render stage).
    #[serde(default)]
    pub tts_url: Option<String>,
    pub engine: String,
    /// Gemini TTS fallback chain, newest first.
    ///
    /// **Written but not yet read.** The inductor fills it from
    /// `Settings::model_order` and no worker consumes it: the sidecar an agent
    /// spawns is `python/tts_server.py`, which is VieNeu-only and ignores
    /// `engine` on `/infer`, and `python/tts_router.py` — the only code that
    /// would ever resolve a chain, from `GEMINI_MODEL_ORDER` — is imported by
    /// nothing (it has been orphaned since the first commit). So a render does
    /// **not** honour this today. It is left on the wire deliberately: removing
    /// it would break older offers, and the field is the right shape for the
    /// day the Gemini TTS path is finished.
    #[serde(default)]
    pub model_order: Vec<String>,
    /// Digest backend: `opencode` | `openrouter` | `local` | `gemini`.
    /// Defaults to `opencode` so old inductors' offers still parse.
    #[serde(default = "default_analyzer")]
    pub analyzer: String,
    /// What that backend runs — the model chain and the per-backend model
    /// names. Without it a provisioned worker, which has no `.bm/settings.json`
    /// to read, digests with the compiled-in `Settings::default()` instead of
    /// the operator's choice. Empty means "the inductor said nothing".
    #[serde(default)]
    pub analyzer_settings: AnalyzerSettings,
    /// The provider keys this stage will read, from the inductor's `.env`.
    ///
    /// Empty when the inductor has nothing configured — the worker then uses
    /// whatever its own environment holds, exactly as before this field
    /// existed. An old inductor sends nothing and an old worker ignores it, so
    /// either side may be upgraded first.
    #[serde(default)]
    pub credentials: Credentials,
    /// Current bible snapshot. The worker uses it to build the prompt and
    /// mirrors it locally so the cast assigner can read voice hints; it never
    /// writes the authoritative copy back.
    #[serde(default)]
    pub bible: Option<serde_json::Value>,
    /// Render/merge stages: the script JSON inline, so any machine can run
    /// them without shared storage.
    #[serde(default)]
    pub script: Option<serde_json::Value>,
    /// Digest stage: the chapter text inline, for the same reason.
    #[serde(default)]
    pub text: Option<String>,
    /// Final-mix settings for the merge stage.
    #[serde(default)]
    pub gap_ms: u32,
    #[serde(default = "default_speed")]
    pub speed: f64,
    #[serde(default)]
    pub ambience: bool,
    /// The background-music layer, independently switchable. Absent means no
    /// music — an inductor that predates this field never offered any, so the
    /// two sides agree without a version check.
    #[serde(default)]
    pub music: bool,
    /// Render stage: exactly the units the inductor's store lacks — the
    /// worker speaks these and nothing else. `None` (old inductor) means
    /// "plan from your own script as before"; `Some([])` means the store is
    /// already complete, so report `ok` with `units: 0` at once. The Option
    /// (not a bare Vec) is what keeps those two apart.
    #[serde(default)]
    pub render_units: Option<Vec<RenderUnitSpec>>,
    /// This worker shares the inductor's root: its seg-dir writes land in the
    /// authoritative store directly, so it neither uploads nor discards.
    /// The inductor decides — the worker never guesses from paths.
    #[serde(default)]
    pub local_node: bool,
}

/// One TTS call the inductor planned: everything the worker needs to speak
/// exactly one file, and nothing it doesn't (no script, no cast, no bible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderUnitSpec {
    /// "0004-0011" | "title" — for progress lines, not for paths.
    pub tag: String,
    /// "0004-0011_Narrator.wav" — the file it must produce.
    pub name: String,
    pub speaker: String,
    pub voice: String,
    pub text: String,
    pub temperature: f64,
    pub silence_p: f64,
}

fn default_speed() -> f64 {
    1.0
}

fn default_analyzer() -> String {
    "opencode".into()
}

/// One selectable voice plus the metadata an operator needs to choose it.
///
/// `gender`/`accent`/`style` come from the sidecar's SDK labels when it
/// answers; `bm-core::voices` carries a smaller offline table for when it does
/// not. `language` is the language the pipeline *speaks*, which is the novel's
/// language — not the voice's full multilingual capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceInfo {
    /// ASCII slug identity, `^[a-z0-9][a-z0-9-]*$`. Stable across a rename,
    /// unlike `name` — which is why the cast file stores this rather than the
    /// display name. Empty when the catalogue does not declare the voice: an
    /// enrolled clone before `roster add` gives it a key, or a payload from an
    /// older inductor that never learned about keys at all.
    #[serde(default)]
    pub key: String,
    pub name: String,
    /// `male` | `female` | `neutral` | `unknown`.
    pub gender: String,
    /// `Northern` | `Central` | `South` | `Central/South` | `unknown`.
    pub accent: String,
    /// BCP-47-ish tag, e.g. `vi-VN`.
    pub language: String,
    /// Free text from the roster label, e.g. `kể chuyện`.
    pub style: String,
    /// An operator-enrolled clone rather than a shipped preset.
    #[serde(default)]
    pub enrolled: bool,
    /// Passes the engine's accent policy (clones are vetted at enrolment).
    #[serde(default)]
    pub allowed: bool,
}

/// Everything the voice picker needs in one round trip: the roster, the
/// current cast, and the speakers the inductor knows about.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Roster {
    pub engine: String,
    /// `live` when the TTS sidecar answered, `offline` when this is the
    /// bundled fallback table. The UI shows this so nobody trusts a guess.
    pub source: String,
    pub voices: Vec<VoiceInfo>,
    /// character -> voice, exactly as the cast file holds it.
    pub cast: std::collections::BTreeMap<String, String>,
    /// Every speaker seen in the cast, the bible or any script — the picker's
    /// first step. Sorted, `Narrator` first.
    pub characters: Vec<String>,
    /// One line describing the active accent policy, for the picker header.
    pub policy_note: String,
}

/// The operator-facing operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum Op {
    /// Enqueue crawl + digest tasks for a chapter range.
    Translate,
    /// Persist the URL template and probe one crawl — "set up link crawling".
    CrawlSetup,
    /// Read the sidecar roster, apply the accent policy, write the cast.
    Voices,
    /// Repoint one character's voice and invalidate only its cached segments.
    SwapVoice,
    /// Render a short sample of one voice so it can be auditioned before it is
    /// assigned. Writes nothing: the wav comes back in `OpResult::audio_b64`
    /// and the client decides where (and whether) it lands on disk.
    PreviewVoice,
    /// Serve one already-rendered segment for a voice: no synthesis, just
    /// bytes from the local segment cache. The sentence it speaks comes back
    /// in `OpResult::line_text` so the client can show which line it heard
    /// and hold it for A/B comparison.
    Segment,
    /// Estimate wall-clock time for the remaining range.
    #[default]
    Eta,
    /// Requeue tasks stranded on dead workers (no live beat). Unsticks
    /// chapters after a kill without waiting out leases.
    Requeue,
    /// Retry shelved tasks (too many strikes) after fixing the cause.
    /// Resets strikes so the next failure gets a full 3 attempts again.
    Retry,
    /// Retry an individual task by stage/chapter, optionally forcing re-run.
    RetryTask,
    /// Fold duplicate characters into one (title/case/description variants
    /// of the same person), rewrite cast + scripts, re-render the losers.
    /// Only deterministic same-key folds apply; ambiguous pairs are listed
    /// for a human and never auto-merged.
    Reconcile,
    /// Rewrite written-out non-verbal sounds into engine tags across every
    /// script (`Ha ha ha!` → `[cười]`), and requeue the chapters it touches.
    Retag,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Translate => "translate",
            Op::CrawlSetup => "crawl-setup",
            Op::Voices => "voices",
            Op::SwapVoice => "swap-voice",
            Op::PreviewVoice => "preview-voice",
            Op::Segment => "segment",
            Op::Eta => "eta",
            Op::Requeue => "requeue",
            Op::Retry => "retry",
            Op::RetryTask => "retry-task",
            Op::Reconcile => "reconcile",
            Op::Retag => "retag",
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        [
            Op::Translate,
            Op::CrawlSetup,
            Op::Voices,
            Op::SwapVoice,
            Op::PreviewVoice,
            Op::Segment,
            Op::Eta,
            Op::Requeue,
            Op::Retry,
            Op::RetryTask,
            Op::Reconcile,
            Op::Retag,
        ]
        .into_iter()
        .find(|o| o.as_str() == s)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpRequest {
    pub op: Op,
    #[serde(default)]
    pub start: Option<u32>,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub url_template: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub character: Option<String>,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub stage: Option<Stage>,
    #[serde(default)]
    pub chapter: Option<u32>,
    #[serde(default)]
    pub force: Option<bool>,
    /// Literal text to speak, for the ops that render speech. `None` means "the
    /// op's own default", which for `PreviewVoice` is the sidecar's fixed
    /// audition line — the thing that makes two voice samples comparable.
    /// Sending text is how an operator hears a *real* line instead of a sample.
    #[serde(default)]
    pub text: Option<String>,
    /// Report-only mode for the ops that rewrite state (`retag`): show what
    /// would change and write nothing.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpResult {
    pub ok: bool,
    pub message: String,
    /// The wav this op rendered, base64-encoded, when it rendered one.
    ///
    /// **Bytes, not a path**, and that is the whole point. The inductor renders
    /// and the client plays, and those are different machines: a path is only
    /// meaningful to a client that happens to share the inductor's filesystem,
    /// which is exactly the assumption that breaks the moment the TUI is
    /// pointed at a remote `--api`. Shipping the bytes lets the client write
    /// the file where the *speaker* is, and leaves the inductor's disk
    /// untouched — an audition is not a pipeline artifact and has no business
    /// accumulating in `data/`.
    ///
    /// Base64 for the same reason merge reports carry base64 mp3s: this is
    /// JSON. A voice sample is ~240 KB, so ~320 KB on the wire — nothing on a
    /// LAN, and bounded by one sample in flight at a time.
    #[serde(default)]
    pub audio_b64: Option<String>,
    /// A book line the returned audio speaks, when the op served audio it did
    /// not render (segment audition). Lets the client show which sentence it
    /// just heard and hold it for A/B instead of another random pick.
    #[serde(default)]
    pub line_text: Option<String>,
    /// Whose line `line_text` is — the speaker of the served segment, which
    /// is not necessarily the character the operator asked about.
    #[serde(default)]
    pub line_speaker: Option<String>,
}

impl OpResult {
    pub fn ok(message: impl Into<String>) -> Self {
        OpResult {
            ok: true,
            message: message.into(),
            audio_b64: None,
            line_text: None,
            line_speaker: None,
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        OpResult {
            ok: false,
            message: message.into(),
            audio_b64: None,
            line_text: None,
            line_speaker: None,
        }
    }

    /// Attach the rendered wav, base64, so the caller can play it.
    pub fn with_audio_b64(mut self, b64: impl Into<String>) -> Self {
        self.audio_b64 = Some(b64.into());
        self
    }

    /// Attach the book line the audio speaks, so the caller can show which
    /// sentence it just heard instead of another random pick.
    pub fn with_line(mut self, speaker: impl Into<String>, text: impl Into<String>) -> Self {
        self.line_speaker = Some(speaker.into());
        self.line_text = Some(text.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_roundtrips_through_strings() {
        for st in Stage::ALL {
            assert_eq!(Stage::parse(st.as_str()), Some(st));
        }
        assert_eq!(Stage::parse("nope"), None);
    }

    #[test]
    fn upstream_chain_is_a_prefix_of_all() {
        for st in Stage::ALL {
            let idx = Stage::ALL.iter().position(|s| *s == st).unwrap();
            assert_eq!(st.upstream(), &Stage::ALL[..idx]);
        }
    }

    #[test]
    fn only_render_needs_tts() {
        assert!(Stage::Render.needs_tts());
        assert!(!Stage::Crawl.needs_tts());
        assert!(!Stage::Merge.needs_tts());
    }

    #[test]
    fn task_ids_are_stable_and_unique_per_stage() {
        let a = Task::new(7, Stage::Render);
        let b = Task::new(7, Stage::Merge);
        assert_eq!(a.id(), "render:7");
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn ssh_target_joins_user_and_addr() {
        let m = Machine::new("10.0.0.5", "pi", 22, None, "worker");
        assert_eq!(m.ssh_target(), "pi@10.0.0.5");
    }

    #[test]
    fn ops_roundtrip_through_kebab_case() {
        for op in [
            Op::Translate,
            Op::CrawlSetup,
            Op::Voices,
            Op::SwapVoice,
            Op::PreviewVoice,
            Op::Segment,
            Op::Eta,
            Op::Requeue,
            Op::Retry,
            Op::RetryTask,
            Op::Reconcile,
            Op::Retag,
        ] {
            assert_eq!(Op::parse(op.as_str()), Some(op));
        }
        assert_eq!(Op::parse("nope"), None);
    }

    #[test]
    fn audition_carries_text_in_and_audio_bytes_out() {
        // The audition path: literal text in, rendered wav out.
        let req: OpRequest =
            serde_json::from_str(r#"{"op":"preview-voice","voice":"Đức Trí","text":"Ừm!"}"#)
                .unwrap();
        assert_eq!(req.text.as_deref(), Some("Ừm!"));
        assert_eq!(req.voice.as_deref(), Some("Đức Trí"));

        // Absent text is not an error: it means the sidecar's fixed sample, which
        // is what makes two voices comparable. An older caller never sends it.
        let bare: OpRequest =
            serde_json::from_str(r#"{"op":"preview-voice","voice":"X"}"#).unwrap();
        assert_eq!(bare.text, None);

        // An op that rendered no audio still parses — which is what an *older
        // inductor* looks like on the wire, since it has no such field at all.
        // The client must be able to tell that apart from audio it failed to
        // decode, because the two have different next moves (restart the
        // inductor vs. report a bug).
        let res: OpResult = serde_json::from_str(r#"{"ok":true,"message":"m"}"#).unwrap();
        assert_eq!(res.audio_b64, None);
        let with: OpResult =
            serde_json::from_str(r#"{"ok":true,"message":"m","audio_b64":"UklGRg=="}"#).unwrap();
        assert_eq!(with.audio_b64.as_deref(), Some("UklGRg=="));

        // The constructors are the single place `ok` and `audio_b64` are paired,
        // so a new op cannot forget the field and still compile.
        assert!(OpResult::ok("m").ok && OpResult::ok("m").audio_b64.is_none());
        assert!(!OpResult::fail("m").ok);
        assert_eq!(
            OpResult::ok("m")
                .with_audio_b64("UklGRg==")
                .audio_b64
                .as_deref(),
            Some("UklGRg==")
        );
    }

    #[test]
    fn offer_without_analyzer_means_opencode() {
        // An old inductor never sent `analyzer`; its offers still digest.
        let o: TaskOffer = serde_json::from_str(
            r#"{"task_id":"digest:1","chapter":1,"stage":"digest","root":"/r",
                "engine":"vieneu","gap_ms":300,"speed":1.25,"ambience":true}"#,
        )
        .unwrap();
        assert_eq!(o.analyzer, "opencode");
    }

    #[test]
    fn an_offer_without_music_means_no_music_layer() {
        // The rollout: an inductor that predates the music layer sends no
        // `music`, and the worker must read that as "mix what I always mixed"
        // rather than as a malformed offer or as music-on. Absent is off, so
        // neither side needs to know the other's version.
        let o: TaskOffer = serde_json::from_str(
            r#"{"task_id":"merge:1","chapter":1,"stage":"merge","root":"/r",
                "engine":"vieneu","gap_ms":300,"speed":1.25,"ambience":true}"#,
        )
        .unwrap();
        assert!(o.ambience, "the old field still means what it always did");
        assert!(!o.music);
    }

    #[test]
    fn roster_deserialises_with_optional_flags_absent() {
        // The picker must survive a payload from an older inductor that never
        // learned about `enrolled`/`allowed`.
        let r: Roster = serde_json::from_str(
            r#"{"engine":"vieneu","source":"offline","voices":[
                 {"name":"Đức Trí","gender":"male","accent":"Central/South",
                  "language":"vi-VN","style":"đọc truyện"}],
               "cast":{"Narrator":"Đức Trí"},"characters":["Narrator"],
               "policy_note":"Central/South only"}"#,
        )
        .unwrap();
        assert_eq!(r.voices[0].name, "Đức Trí");
        assert!(!r.voices[0].enrolled);
        // A payload from an inductor that predates `key` must still parse, and
        // must not invent one — an empty key means "not a catalogue voice".
        assert_eq!(r.voices[0].key, "");
        assert_eq!(r.cast["Narrator"], "Đức Trí");
    }

    #[test]
    fn task_state_names_match_the_wire_form() {
        // The TUI filters and colours tasks by `as_str`; a divergence from
        // serde's lowercase rename would make a filter match nothing.
        for st in TaskState::ALL {
            let wire = serde_json::to_value(st).unwrap();
            assert_eq!(wire.as_str(), Some(st.as_str()), "{st:?}");
        }
        assert_eq!(TaskState::Shelved.as_str(), "shelved");
        assert_eq!(TaskState::Running.as_str(), "running");
    }

    #[test]
    fn retry_task_requests_carry_stage_chapter_and_force() {
        let req: OpRequest = serde_json::from_str(
            r#"{"op":"retry-task","stage":"digest","chapter":7,"force":true}"#,
        )
        .unwrap();
        assert_eq!(req.op, Op::RetryTask);
        assert_eq!(req.op.as_str(), "retry-task");
        assert_eq!(req.stage, Some(Stage::Digest));
        assert_eq!(req.chapter, Some(7));
        assert_eq!(req.force, Some(true));
        // Everything else stays None: a retry must not smuggle a voice or range.
        assert!(req.start.is_none() && req.voice.is_none() && req.character.is_none());

        // An old caller that knows nothing of the new fields still parses, and
        // the missing keys read as "not specified" rather than as an error.
        let bare: OpRequest = serde_json::from_str(r#"{"op":"retry-task"}"#).unwrap();
        assert_eq!(bare.stage, None);
        assert_eq!(bare.chapter, None);
        assert_eq!(bare.force, None);

        // `retry` with a chapter is the narrow form of the blanket retry.
        let narrow: OpRequest =
            serde_json::from_str(r#"{"op":"retry","stage":"render","chapter":3}"#).unwrap();
        assert_eq!(narrow.op, Op::Retry);
        assert_eq!(narrow.stage, Some(Stage::Render));
        assert_eq!(narrow.chapter, Some(3));
        assert_eq!(narrow.force, None, "absent force means plain retry");
    }

    fn both_keys() -> Credentials {
        Credentials {
            gemini_api_key: "g-key".into(),
            openrouter_api_key: "o-key".into(),
        }
    }

    #[test]
    fn credentials_travel_only_to_the_stage_that_reads_them() {
        // The digest lane takes the analyzer's key — and only that one.
        assert_eq!(
            both_keys().for_stage(Stage::Digest, "gemini", "vieneu"),
            Credentials {
                gemini_api_key: "g-key".into(),
                openrouter_api_key: String::new(),
            }
        );
        assert_eq!(
            both_keys().for_stage(Stage::Digest, "openrouter", "vieneu"),
            Credentials {
                gemini_api_key: String::new(),
                openrouter_api_key: "o-key".into(),
            }
        );
        // opencode authenticates itself and Ollama is a local URL: neither
        // reads a key, so neither gets one.
        for analyzer in ["opencode", "local"] {
            assert!(
                both_keys()
                    .for_stage(Stage::Digest, analyzer, "vieneu")
                    .is_empty(),
                "{analyzer} reads no key"
            );
        }
        // A gemini TTS render hands the key to the sidecar the worker spawns.
        assert_eq!(
            both_keys()
                .for_stage(Stage::Render, "gemini", "gemini")
                .gemini_api_key,
            "g-key"
        );
        assert!(both_keys()
            .for_stage(Stage::Render, "gemini", "vieneu")
            .is_empty());
        // Crawl and merge touch no provider at all — a crawl offer that
        // carried a key would be shipping a secret to a box that fetches a URL.
        for stage in [Stage::Crawl, Stage::Merge] {
            assert!(
                both_keys().for_stage(stage, "gemini", "gemini").is_empty(),
                "{stage} reads no key"
            );
        }
    }

    #[test]
    fn credential_pairs_name_the_variables_the_backends_read() {
        // These two strings are the contract with `bm-core/src/digest/llm.rs`
        // and `python/tts_router.py`: they read the environment by exactly
        // these names, so a rename here would install nothing.
        assert_eq!(
            both_keys().pairs(),
            vec![("GEMINI_API_KEY", "g-key"), ("OPENROUTER_API_KEY", "o-key")]
        );
        assert_eq!(
            both_keys().names(),
            vec!["GEMINI_API_KEY", "OPENROUTER_API_KEY"]
        );
        // An unset key is absent, never an empty assignment: the worker must
        // be able to leave a box's own `.env` alone.
        let half = Credentials {
            gemini_api_key: String::new(),
            openrouter_api_key: "o-key".into(),
        };
        assert_eq!(half.pairs(), vec![("OPENROUTER_API_KEY", "o-key")]);
        assert!(!half.is_empty());
        assert!(Credentials::default().pairs().is_empty());
    }

    #[test]
    fn debug_never_prints_a_key() {
        // `TaskOffer` is `Debug` and every offer is a candidate for a log
        // line. A redaction that is not tested is a redaction that gets
        // dropped in a refactor.
        let shown = format!("{:?}", both_keys());
        assert!(!shown.contains("g-key"), "{shown}");
        assert!(!shown.contains("o-key"), "{shown}");
        assert!(shown.contains("set"), "{shown}");
        assert!(format!("{:?}", Credentials::default()).contains("unset"));
        // And through the struct that actually gets printed.
        let offer = TaskOffer {
            task_id: "digest:1".into(),
            chapter: 1,
            stage: Stage::Digest,
            root: "/r".into(),
            url: None,
            tts_url: None,
            engine: "vieneu".into(),
            model_order: vec![],
            analyzer: "gemini".into(),
            analyzer_settings: AnalyzerSettings::default(),
            credentials: both_keys(),
            bible: None,
            script: None,
            text: None,
            gap_ms: 300,
            speed: 1.0,
            ambience: false,
            music: false,
            render_units: None,
            local_node: false,
        };
        assert!(!format!("{offer:?}").contains("g-key"));
    }

    #[test]
    fn an_old_inductors_offer_carries_no_credentials() {
        // The staged rollout: an inductor that predates this field sends none,
        // and the worker must read that as "use your own environment" rather
        // than as a malformed offer.
        let o: TaskOffer = serde_json::from_str(
            r#"{"task_id":"digest:1","chapter":1,"stage":"digest","root":"/r",
                "engine":"vieneu","gap_ms":300,"speed":1.25,"ambience":true}"#,
        )
        .unwrap();
        assert!(o.credentials.is_empty());
        assert!(o.credentials.pairs().is_empty());
        // And it carries no analyzer configuration either — the worker's own
        // settings stand, exactly as before this block existed.
        assert!(o.analyzer_settings.analyze_models.is_none());
        assert_eq!(o.analyzer_settings.analyze_model, "");
        // And an old *worker* ignores the fields entirely — the serializer
        // emits them, the parser above proves absence is tolerated.
        let round: TaskOffer = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(round.credentials, o.credentials);
        assert_eq!(round.analyzer_settings, o.analyzer_settings);
    }

    #[test]
    fn an_empty_chain_is_not_the_same_as_saying_nothing() {
        // `Some([])` is a real configuration — "no chain, `analyze_model`
        // stands alone" — and it is `Settings::default()`. `None` is an older
        // inductor that has no opinion. Collapsing the two would let a worker
        // keep a chain the operator had deliberately removed.
        let stated: AnalyzerSettings =
            serde_json::from_str(r#"{"analyze_model":"m","analyze_models":[]}"#).unwrap();
        assert_eq!(stated.analyze_models, Some(vec![]));
        let silent: AnalyzerSettings = serde_json::from_str(r#"{"analyze_model":"m"}"#).unwrap();
        assert_eq!(silent.analyze_models, None);
        // And both survive a round trip, which is what the worker sees.
        for block in [stated, silent] {
            let back: AnalyzerSettings =
                serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
            assert_eq!(back, block);
        }
    }
}
