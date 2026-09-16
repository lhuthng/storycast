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
    pub fn new(addr: &str, ssh_user: &str, ssh_port: u16, ssh_key: Option<String>, role: &str) -> Self {
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
    #[serde(default)]
    pub model_order: Vec<String>,
    /// Digest backend: `opencode` | `openrouter` | `local` | `gemini`.
    /// Defaults to `opencode` so old inductors' offers still parse.
    #[serde(default = "default_analyzer")]
    pub analyzer: String,
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
    /// assigned. Writes `data/previews/<voice>.wav` and reports the path.
    PreviewVoice,
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
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Translate => "translate",
            Op::CrawlSetup => "crawl-setup",
            Op::Voices => "voices",
            Op::SwapVoice => "swap-voice",
            Op::PreviewVoice => "preview-voice",
            Op::Eta => "eta",
            Op::Requeue => "requeue",
            Op::Retry => "retry",
            Op::RetryTask => "retry-task",
            Op::Reconcile => "reconcile",
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        [
            Op::Translate,
            Op::CrawlSetup,
            Op::Voices,
            Op::SwapVoice,
            Op::PreviewVoice,
            Op::Eta,
            Op::Requeue,
            Op::Retry,
            Op::RetryTask,
            Op::Reconcile,
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
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpResult {
    pub ok: bool,
    pub message: String,
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
            Op::Eta,
            Op::Requeue,
            Op::Retry,
            Op::RetryTask,
            Op::Reconcile,
        ] {
            assert_eq!(Op::parse(op.as_str()), Some(op));
        }
        assert_eq!(Op::parse("nope"), None);
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
}
