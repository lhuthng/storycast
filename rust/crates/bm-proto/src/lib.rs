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

    /// The order the scheduler prefers when a machine has no explicit policy:
    /// finish chapters before starting new ones, so `merge` leads and `crawl`
    /// trails. See [`TaskPref`].
    pub const DEFAULT_PRIORITY: [Stage; 4] =
        [Stage::Merge, Stage::Render, Stage::Digest, Stage::Crawl];

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
    /// Digest racing: the extra workers grinding the same digest row besides
    /// `assigned_to`. Only the digest stage ever fills this — every other
    /// stage assigns a row to exactly one box. The first holder to report
    /// `ok` wins; every later report finds a row it no longer owns and is
    /// dropped as stale, strike-free. Empty on rows written before racing
    /// existed (`#[serde(default)]`), and cleared wherever `assigned_to` is.
    #[serde(default)]
    pub racers: Vec<String>,
    /// Unix seconds. When the lease expires the task returns to the pool
    /// *without* a strike — silence is not failure.
    pub lease_until: Option<u64>,
    pub detail: String,
    pub updated: u64,
    /// Unused: no row is ever pinned. Kept for ledger compatibility so old
    /// files still parse — the loader releases every stored pin.
    #[serde(default)]
    pub affinity: Option<String>,
    /// `Merge` only: the sound design this chapter's artifact was mixed under —
    /// a `bm_core::design` fingerprint over the registries and knobs that
    /// chapter's own script reaches.
    ///
    /// The mix reads a scene map, three clip pools and seven settings, and all
    /// of them can be edited after a chapter is published. Without this the only
    /// record of "which design produced this mp3" was the operator's memory, so
    /// a retuned effect left every mp3 that used it looking current. A merge
    /// whose stamp no longer matches the design on disk is not done.
    ///
    /// **`None` means the task predates this field**, and is adopted rather than
    /// invalidated: the first pass after this ships writes the current stamp
    /// without touching the task, so a library merged before the field existed
    /// is not re-merged wholesale. Any *later* change is caught. Never treat
    /// absent as stale — that is a whole-library re-merge on upgrade.
    ///
    /// Written when the merge is applied as done, not when it is offered: a
    /// merge that failed left no artifact to make a claim about.
    #[serde(default)]
    pub design: Option<String>,
    /// `Render` only: the take's position in the chapter's recorded render
    /// plan (`data/render-NN.json`).
    ///
    /// A render is **one task per take**, not one per chapter: the offer then
    /// carries exactly the segment being spoken plus its `take_key`, so a
    /// local edit re-speaks one segment instead of shipping a chapter and
    /// hoping the worker re-derives the same names. The merge gate is the
    /// plan's coverage — every take's task `Done` — which is the same
    /// question the mixer asks, so the two cannot disagree.
    ///
    /// `None` means this task predates per-take rendering (a legacy
    /// `render:7` row). Reconciliation replaces such a row with its takes.
    #[serde(default)]
    pub take: Option<usize>,
    /// `Render` only: the **other** ledger rows one offer assigned along with
    /// this one.
    ///
    /// A take is still scheduled, gated and settled on its own row — that is
    /// what makes a local edit cost one segment instead of a chapter — but the
    /// *assignment* can cover several takes at once (`Settings::render_batch`),
    /// because a worker pays a round trip, a heartbeat and a report per offer.
    /// This field is where that grouping is recorded, so the completion gate
    /// knows the whole set without a word of it travelling on the wire: the
    /// offer carries the takes, the ledger carries the grouping.
    ///
    /// The row that owns the batch is the one the offer's `task_id` names; its
    /// own id is deliberately **not** repeated here. Empty is the ordinary
    /// single-take offer, and is what every pre-batch ledger holds.
    ///
    /// Cleared when the batch settles (done or struck), and rewritten by the
    /// next offer that names this row — so it is never read stale. The reads
    /// are gated on the reporting worker still owning the row, and only an
    /// offer grants that, so a grouping that survives a settle is unreachable
    /// rather than merely unused.
    #[serde(default)]
    pub batch: Vec<String>,
    /// How many times the lease reaper has returned this row to the pool
    /// **silently** — no strike, because silence is not failure.
    ///
    /// The count exists because that rule has a blind spot, and on 2026-09-22 it
    /// cost about eighty minutes. A worker that died deserves nothing; a worker
    /// that is **alive and stuck** looks identical from here — fresh beat, task
    /// never finished — so the row was handed out again and again with nothing
    /// anywhere saying so, and the operator watched a percentage that never
    /// moved. Counted only when the holder was *still beating* at the moment the
    /// lease ran out, so the number means "expired while its worker was alive",
    /// which is the hang signature rather than the lost-box one.
    ///
    /// Reset when the assignment actually resolves — a completion or a reported
    /// failure — so a row that once looped does not make every later, legitimate
    /// expiry look like a repeat. `0` is "never", which is also what every row
    /// written before this field existed reads as.
    #[serde(default)]
    pub expiries: u32,
}

impl Task {
    /// The ledger key. `render:7` for chapter-granular stages, and
    /// `render:7:3` for the fourth take of chapter seven.
    pub fn id(&self) -> String {
        match self.take {
            Some(pos) => format!("{}:{}:{}", self.stage, self.chapter, pos),
            None => format!("{}:{}", self.stage, self.chapter),
        }
    }

    /// The chapter a ledger key names — `"render:7:3"` is chapter 7. `None`
    /// for anything that is not `stage:chapter[:take]`.
    pub fn chapter_of(id: &str) -> Option<u32> {
        let mut it = id.split(':');
        let stage = it.next()?;
        Stage::parse(stage)?;
        it.next()?.parse().ok()
    }

    /// The take position a ledger key names, when it names one.
    pub fn take_of(id: &str) -> Option<usize> {
        let rest = id.splitn(3, ':').nth(2)?;
        rest.parse().ok()
    }

    pub fn new(chapter: u32, stage: Stage) -> Self {
        Task {
            chapter,
            stage,
            state: TaskState::Pending,
            attempts: 0,
            assigned_to: None,
            racers: Vec::new(),
            lease_until: None,
            detail: String::new(),
            updated: now_secs(),
            affinity: None,
            design: None,
            take: None,
            batch: Vec::new(),
            expiries: 0,
        }
    }

    /// One take of a chapter's render plan.
    pub fn new_take(chapter: u32, pos: usize) -> Self {
        let mut t = Task::new(chapter, Stage::Render);
        t.take = Some(pos);
        t
    }

    /// Every worker currently holding this row: the primary plus digest racers.
    pub fn holders(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        if let Some(w) = self.assigned_to.as_deref() {
            out.push(w);
        }
        out.extend(self.racers.iter().map(String::as_str));
        out
    }

    /// Whether this worker holds the row, as primary or racer.
    pub fn is_holder(&self, worker: &str) -> bool {
        self.assigned_to.as_deref() == Some(worker) || self.racers.iter().any(|r| r == worker)
    }

    /// Drop one holder. Promotes the next racer when the primary leaves and
    /// racers remain, so the row keeps an owner while any box is on it.
    /// Returns `true` when holders remain.
    pub fn remove_holder(&mut self, worker: &str) -> bool {
        if self.assigned_to.as_deref() == Some(worker) {
            self.assigned_to = None;
        } else {
            self.racers.retain(|r| r != worker);
        }
        if self.assigned_to.is_none() {
            if let Some(next) = self.racers.first().cloned() {
                self.racers.remove(0);
                self.assigned_to = Some(next);
            }
        }
        self.assigned_to.is_some()
    }

    /// Release every holder — what settling, requeueing or shelving does.
    pub fn clear_holders(&mut self) {
        self.assigned_to = None;
        self.racers.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
// kebab-case, and it must stay the **same spelling as [`MachineState::as_str`]**:
// the string form is the wire form. `as_str` is what the TUI posts to
// `/api/machines/state`, the body is deserialized back into this enum, and a
// mismatch is a 422 that looks like the update was refused for no reason. One
// word for every variant but one is what made the divergence invisible until
// there *was* a two-word variant.
#[serde(rename_all = "kebab-case")]
pub enum MachineState {
    /// In the registry, never contacted: a hand-written ledger entry, or a box
    /// the operator added and has not probed. "No opinion formed", which is why
    /// [`Self::accepts_work`] lets it through.
    #[default]
    Unknown,
    /// **Created, and the account has not given it an address yet.**
    ///
    /// The first thing a launched EC2 instance is: `RunInstances` returns a
    /// pending instance whose `public_ip` field is empty, and the address shows
    /// up seconds later. Such a box is registered by its *instance id* — stable
    /// for its whole life — so there is an entry to repair rather than none.
    ///
    /// The address column stays blank while this is the state, because an
    /// attempt to dial here is guaranteed to fail and an address that looks
    /// reachable while nothing can reach it is the confusion this state exists
    /// to end. [`Self::dialable`] is the one place that verdict lives.
    ///
    /// Like `Initializing`, it carries a deadline: a box the account never
    /// handed an address to is not coming, and waiting for ever would be a lie.
    ///
    /// Spelled `awaiting-ip` rather than `awaiting-address` because it is
    /// rendered in a fixed-width column beside thirteen other state words, and
    /// a word the pane truncates to `awaiting-a…` hides the one noun that
    /// matters. It also matches the column it replaces with `—`.
    AwaitingIp,
    /// **Created, not yet answering.** A launched EC2 instance spends its first
    /// half-minute here: the account has the box, the box is booting, and
    /// nothing can be pushed to it yet.
    ///
    /// Deliberately *not* `Offline`. Offline is a verdict about a box that was
    /// answering and went quiet; reading a boot as death is how a freshly
    /// launched pool looks broken. It is also the one state with a deadline —
    /// see the inductor's boot expiry.
    Initializing,
    /// An ssh probe is in flight.
    Probing,
    /// The box has everything it needs; no worker is beating yet.
    ///
    /// Reached two ways, and they mean the same thing: the probe found the box
    /// already provisioned and there was nothing to distribute, or a push
    /// finished and the worker was launched. Either way the next event is the
    /// worker's first beat, which is what moves it to `Online` — so this is
    /// "ready and idle", not "busy".
    Configured,
    /// Artifacts being pushed.
    Provisioning,
    /// A worker is answering beats. **The only state that works.**
    Online,
    /// Was answering, now silent.
    Offline,
    /// A transition failed. The note says which.
    Error,
}

impl MachineState {
    pub fn as_str(self) -> &'static str {
        match self {
            MachineState::Unknown => "unknown",
            MachineState::AwaitingIp => "awaiting-ip",
            MachineState::Initializing => "initializing",
            MachineState::Probing => "probing",
            MachineState::Configured => "configured",
            MachineState::Provisioning => "provisioning",
            MachineState::Online => "online",
            MachineState::Offline => "offline",
            MachineState::Error => "error",
        }
    }

    /// May this machine be handed a task?
    ///
    /// One state works: `Online`, which means a worker is answering. Every
    /// other state is a deliberate "not yet" — booting, being pushed to,
    /// provisioned but never started, silent, or broken — and offering work
    /// into any of them is how a task lands on a box that cannot run it.
    ///
    /// The caller decides what to do about `Unknown`: it means no opinion was
    /// ever formed, so it is neither a yes nor a no.
    pub fn accepts_work(self) -> bool {
        matches!(self, MachineState::Online)
    }

    /// On its way up: wait for it, and never read it as dead.
    ///
    /// The dispatcher asks every registered box `/status` every couple of
    /// seconds. A box that is still booting — or being pushed to, or
    /// provisioned with no worker started yet — cannot answer, and stamping
    /// `Offline` on it would replace the one state that says *this is expected,
    /// give it time* with "it is gone".
    pub fn coming_up(self) -> bool {
        matches!(
            self,
            MachineState::AwaitingIp
                | MachineState::Initializing
                | MachineState::Probing
                | MachineState::Provisioning
                | MachineState::Configured
        )
    }

    /// Is there an address to dial for this box?
    ///
    /// The scheduler asks every registered box `/status` every couple of
    /// seconds, and a box the account has not given an address yet has nothing
    /// to ask. It is keyed by its instance id, which is a *handle* — a name to
    /// repair the record by — not something ssh can answer on.
    ///
    /// `Unknown` deliberately passes: a hand-added machine has never been
    /// probed, and probing it is the only way to find out.
    pub fn dialable(self) -> bool {
        !matches!(self, MachineState::AwaitingIp)
    }
}

/// The port a worker answers the inverted protocol on. One constant for both
/// sides: the launcher records it on the `Machine` and passes it to the worker
/// as `--serve-tasks`, so a mismatch is impossible rather than merely unlikely.
pub const DEFAULT_TASK_PORT: u16 = 8917;

fn default_task_port() -> Option<u16> {
    Some(DEFAULT_TASK_PORT)
}

/// The port on a worker's **own loopback** that answers the completion hook —
/// `POST /api/complete` forwarded by the inductor's reverse tunnel, not a
/// server the worker runs. One constant for both ends: the tunnel is built as
/// `-R {DEFAULT_HOOK_PORT}:127.0.0.1:{api_port}` and the worker dials
/// `127.0.0.1:{DEFAULT_HOOK_PORT}`, so a mismatch is impossible rather than
/// merely unlikely. See `bm-inductor/src/tunnel.rs` for why the hook exists.
pub const DEFAULT_HOOK_PORT: u16 = 18901;

/// One stage's place in a machine's own work policy.
///
/// The scheduler walks the list in order and takes the first stage that has an
/// assignable task; a disabled entry is skipped. Storing the full list (not just
/// the enabled ones) keeps the operator's chosen order stable while they toggle
/// stages on and off in the dashboard's policy panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPref {
    pub stage: Stage,
    pub enabled: bool,
}

impl TaskPref {
    /// The default policy: every stage enabled, in [`Stage::DEFAULT_PRIORITY`].
    pub fn default_list() -> Vec<TaskPref> {
        Stage::DEFAULT_PRIORITY
            .iter()
            .map(|s| TaskPref {
                stage: *s,
                enabled: true,
            })
            .collect()
    }
}

/// A machine the inductor knows how to reach. Machines are added by address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub addr: String,
    /// Registry handle (`hawk`) from machines.json. Empty for boxes the
    /// inductor learned from a beat before ever seeing the registry —
    /// renderers fall back to the address. `id` stays the address: it is
    /// the map key everywhere, the name is display only.
    #[serde(default)]
    pub name: String,
    pub ssh_user: String,
    pub ssh_port: u16,
    #[serde(default)]
    pub ssh_key: Option<String>,
    /// `worker`, `tts`, or `both`.
    pub role: String,
    pub state: MachineState,
    /// When `state` last changed, unix seconds. `0` means "never stamped" —
    /// a record written before this field, which is adopted rather than
    /// expired on the strength of a missing timestamp.
    ///
    /// This is what lets a deadline tell "launched 20 s ago, still booting"
    /// from "has been `Initializing` since this morning, and is not coming".
    #[serde(default)]
    pub state_since: u64,
    pub last_seen: u64,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tts_url: Option<String>,
    /// Port this box answers the inverted protocol on.
    ///
    /// Defaulted rather than optional, so a box is driven because it is a
    /// worker — not because some other record remembers how it was started.
    /// An explicit `null` is the way to say "do not drive this one", which is
    /// what an operator wants for a box still running the pull protocol.
    #[serde(default = "default_task_port")]
    pub task_port: Option<u16>,
    /// This machine's own work policy: which stages it may run, most-preferred
    /// first. `None` means the default (all four, merge → render → digest →
    /// crawl). Persisted in `machines.json`, so it survives restarts.
    #[serde(default)]
    pub task_policy: Option<Vec<TaskPref>>,
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
            name: String::new(),
            ssh_user: ssh_user.to_string(),
            ssh_port,
            ssh_key,
            role: role.to_string(),
            state: MachineState::Unknown,
            state_since: 0,
            last_seen: 0,
            capabilities: Vec::new(),
            tts_url: None,
            task_port: default_task_port(),
            task_policy: None,
            note: String::new(),
        }
    }

    /// The stages this machine may run, most-preferred first. Falls back to the
    /// full default list when no policy is stored, so a machine that predates
    /// the policy behaves exactly as it always did.
    pub fn effective_task_policy(&self) -> Vec<TaskPref> {
        self.task_policy
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(TaskPref::default_list)
    }

    /// Move this machine to `state`, stamping when it happened.
    ///
    /// The one transition point, because a state change is a *decision* and the
    /// timestamp is half of it. The note is the caller's business: several of
    /// them fold theirs in through `preserve_ec2_id`, and doing it here would
    /// mean this crate knowing the note format.
    ///
    /// Re-stating the current state does **not** move the stamp — that is what
    /// keeps "online since 14:02" and "initializing for 4 minutes" meaningful
    /// while a poll re-states the same verdict every two seconds.
    pub fn set_state(&mut self, state: MachineState) {
        if self.state != state {
            self.state_since = now_secs();
        }
        self.state = state;
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
    /// Box load reported by the agent, for the workers pane (`5.2%`,
    /// `38% 6.1G`). `None` from older agents — the pane shows a dash.
    /// `Option` (not a bare 0.0) so a genuinely idle box is told apart
    /// from one that never measured.
    #[serde(default)]
    pub cpu_pct: Option<f32>,
    #[serde(default)]
    pub mem_pct: Option<f32>,
    /// Used RAM in GiB.
    #[serde(default)]
    pub mem_gb: Option<f32>,
    /// How many TTS sidecar processes are alive on the box.
    ///
    /// The quantity that actually kills these boxes, and until now nothing in
    /// the cluster could see it: one sidecar is ~2.85 GB resident the moment the
    /// weights load, and an 8 GiB box cannot hold two. `Some(n)` with `n > 1` is
    /// the OOM warming up — the inductor raises it as an event and stops
    /// offering the box work until it settles. `None` from an older agent.
    #[serde(default)]
    pub sidecars: Option<u32>,
    /// Their total resident memory in GiB (all of them, summed).
    #[serde(default)]
    pub sidecar_gb: Option<f32>,
    /// Stable display name chosen by the worker at startup and kept in its
    /// root (`worker.alias`). Empty from older agents — the TUI falls back to
    /// hashing the worker id, which churns on every restart.
    #[serde(default)]
    pub alias: String,
    /// What this worker can run. Carried here as well as on `Register`
    /// because the inverted direction has no registration: the inductor
    /// learns the worker from its first `/status` answer, and the render gate
    /// needs to know whether the box can produce units. Defaulted, so an
    /// agent that predates the field reports none rather than failing to
    /// parse — the gate then treats it as "no render", which is the safe read.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Whether this worker keeps a TTS sidecar, as it currently believes.
    /// `None` from an older agent — read as "keeps one", which is both the
    /// default and the safe read (the old behaviour, never a stuck refusal).
    ///
    /// The dispatcher's sidecar-policy convergence reads this: it knows what
    /// the box's policy says (render on/off) and what it last **told** the
    /// worker, but neither survives the box rebooting back to its default —
    /// only the worker's own answer does. A beat whose value disagrees with
    /// the policy is re-converged; one that agrees costs nothing.
    #[serde(default)]
    pub sidecar_keep: Option<bool>,
}

/// The heartbeat's answer: the only inductor→worker command channel.
///
/// `shutdown` defaults off so a new agent against an old inductor (whose
/// answer is just `{"ok": true}`) keeps working — and an old agent against a
/// new inductor ignores the answer entirely and is swept the old way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatAck {
    pub ok: bool,
    #[serde(default)]
    pub shutdown: bool,
}

/// One rendered take riding with its completion report, so the inductor
/// holds the audio the moment the take is Done — on every path, including
/// the hook's, where no collection round trip is possible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnitFile {
    pub name: String,
    pub b64: String,
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
    /// The cast that decided this chapter's segment **filenames**.
    ///
    /// Shipped for the same reason as the script, and it has to be: a
    /// provisioned worker has no `data/cast-*.json` (provisioning copies
    /// `prompts/`, `assets/` and `refs/`, never `data/`), so a merge that
    /// recomputed the plan locally would name every segment differently from
    /// the render that produced them and report all of them missing.
    /// Digest stage: the full script, so the inductor holds the artifact and
    /// can hand it to whichever machine renders.
    #[serde(default)]
    pub script: Option<serde_json::Value>,
    /// The cast that decided this chapter's segment **filenames**.
    ///
    /// Shipped for the same reason as the script, and it has to be: a
    /// provisioned worker has no `data/cast-*.json` — provisioning copies
    /// `prompts/`, `assets/` and `refs/`, never `data/` — so a merge that
    /// recomputed the plan locally would name every segment differently
    /// from the render that produced them and report all of them missing.
    /// Crawl stage: the cleaned chapter text, for the same reason.
    #[serde(default)]
    pub text: Option<String>,
    /// Crawl stage: what happened when it was not a plain success. `ok: false`
    /// cannot say whether a chapter is *absent* (a terminal non-failure) or
    /// *blocked in a way that will never retry* (a login wall, a 404), and the
    /// difference is three wasted attempts and a shelved row.
    #[serde(default)]
    pub crawl: Option<CrawlReport>,
    /// Merge stage: the final mp3, base64. Small enough for LAN; this is how
    /// a remote merge's product comes home without shared storage.
    #[serde(default)]
    pub mp3_b64: Option<String>,
    /// Render stage: the takes this report produced, base64. The inductor
    /// stores them before applying the completion, so a Done take's file is
    /// always home — no matter which channel delivered the report.
    #[serde(default)]
    pub unit_files: Vec<UnitFile>,
}

/// The `worker_id` an operator's own digest reports under.
///
/// A manual digest is a *report*, not a special case: it goes to `/api/complete`
/// with the same body a worker sends, so it flows through the same bible merge,
/// the same row transition, the same script write and the same
/// invalidate-on-changed-script rule. The one thing that has to differ is
/// ownership — the operator is not the worker holding the row — and this id is
/// how `complete` knows to accept it anyway.
///
/// **No `redigest` flag, deliberately.** A manual digest of a chapter the library
/// already has needs its segments and mp3 invalidated, and `complete` already
/// decides that by comparing the new script with the one on disk — so a
/// re-digest that changes nothing invalidates nothing, and one that changes a
/// line keeps every take whose inputs did not change. A flag would be the same
/// fact stored twice, and the copy that could go stale.
///
/// It is deliberately **not** a legal worker id: worker ids come from the host
/// (`localhost-caracal`, `marmot`), so a box cannot claim it by accident.
pub const MANUAL_WORKER: &str = "operator";

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
/// An absent `analyze_models` means "the inductor said nothing" and leaves the
/// worker's own value alone, so an older inductor's offer still behaves as
/// before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzerSettings {
    /// The fallback chain, tried in order.
    ///
    /// `None` means "the inductor said nothing" (an older inductor); `Some([])`
    /// means "explicitly no Gemini models".
    #[serde(default)]
    pub analyze_models: Option<Vec<String>>,
    #[serde(default)]
    pub opencode_model: String,
    #[serde(default)]
    pub openrouter_model: String,
    /// The model service's base URL, so a box behind a proxy or a gateway
    /// talks to the same endpoint the inductor does. Empty means "the inductor
    /// said nothing" and the box keeps its own, like every other field here.
    #[serde(default)]
    pub openrouter_url: String,
    #[serde(default)]
    pub local_model: String,
    #[serde(default)]
    pub ollama_url: String,
}

/// Everything one crawl needs that the worker cannot derive locally.
///
/// The **script travels with the offer** rather than being read on the worker.
/// A box that has not been re-provisioned then still runs the crawler the
/// operator edited, a worker needs no profile tree at all to crawl, and the
/// inductor stays the single source of truth for what a run does — the same
/// argument the analyzer block above is built on.
///
/// An empty `engine` is the built-in path: GET `url` and run the crate's own
/// extractor. That is what an old inductor's offer means too (the field is
/// simply absent), so either side may be upgraded first.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CrawlSpec {
    /// `lua` | `js` | empty for the built-in fetcher.
    #[serde(default)]
    pub engine: String,
    /// The script's path, for every message about it.
    #[serde(default)]
    pub script: String,
    /// The script's source.
    #[serde(default)]
    pub source: String,
    /// The workspace's `crawl.params`, opaque to the host and passed verbatim
    /// to the script. Deliberately not a schema: the moment the host validates
    /// its keys, the site-specific part of crawling is hardcoded again.
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// The built-in mapping, used when the manifest has no URL for a chapter.
    #[serde(default)]
    pub url_template: String,
    /// Extra headers on every fetch the crawler makes (a referer, a session
    /// cookie the operator pasted in).
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub user_agent: String,
    /// Minimum ms between two fetches of one host. `0` is off.
    #[serde(default)]
    pub pace_ms: u64,
    #[serde(default)]
    pub timeout_secs: u64,
    #[serde(default)]
    pub max_seconds: u64,
    #[serde(default)]
    pub max_fetches: u32,
}

/// Redacted on purpose: a spec carries whatever headers the operator set, and
/// one of them may well be a session cookie.
impl std::fmt::Debug for CrawlSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrawlSpec")
            .field("engine", &self.engine)
            .field("script", &self.script)
            .field("source_bytes", &self.source.len())
            .field("params", &self.params.keys().collect::<Vec<_>>())
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Which of the three things a crawl concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlVerdict {
    /// A chapter came back and is in `text`.
    Text,
    /// The site has no such chapter. Terminal, and **not** a failure: a range
    /// that runs past the end of a book must not shelve twenty rows for it.
    Absent,
    /// The site refused, with a class that decides whether another attempt is
    /// worth a box.
    Blocked,
}

/// How a crawl turned out, when `ok: false` alone cannot say — which is every
/// case where the answer is not "try again".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlReport {
    pub verdict: CrawlVerdict,
    /// Human-readable cause, shown on the ledger row (the `Enter` key).
    #[serde(default)]
    pub detail: String,
    /// The blocked class, e.g. `challenge` | `rate_limit` | `gone`. Empty for
    /// the other verdicts.
    #[serde(default)]
    pub class: String,
    /// Seconds the script asked to wait, when it knew.
    #[serde(default)]
    pub retry_after: Option<u64>,
    /// Network round trips the chapter cost, for the pacing conversation.
    #[serde(default)]
    pub fetches: u32,
}

impl CrawlReport {
    /// Whether another attempt is worth a worker. Absent chapters are not
    /// retried (there is nothing to fetch), and neither are terminal refusals.
    pub fn retryable(&self) -> bool {
        match self.verdict {
            CrawlVerdict::Text => false,
            CrawlVerdict::Absent => false,
            CrawlVerdict::Blocked => matches!(
                self.class.as_str(),
                "rate_limit" | "challenge" | "empty" | "unknown" | ""
            ),
        }
    }
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
    /// How to crawl this chapter (crawl stage). Absent — from an older
    /// inductor — means the built-in fetcher against `url`, which is exactly
    /// the behaviour that predates scripted crawls.
    #[serde(default)]
    pub crawl: Option<CrawlSpec>,
    /// 1-based attempt count for this task, so a crawler can try a mirror or
    /// back off on the second go instead of failing identically three times.
    /// Absent means 1.
    #[serde(default)]
    pub attempt: u32,
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
    /// The cast that decided this chapter's segment **filenames**.
    ///
    /// Shipped for the same reason as the script, and it has to be: a
    /// provisioned worker has no `data/cast-*.json` — provisioning copies
    /// `prompts/`, `assets/` and `refs/`, never `data/` — so a merge that
    /// recomputed the plan locally would name every segment differently
    /// from the render that produced them and report all of them missing.
    #[serde(default)]
    pub cast: Option<serde_json::Value>,
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
    /// Master gains for the merge stage: 1.0 = as authored, 0.0 = muted.
    /// Absent (old inductor) means 1.0, so either side may upgrade first.
    #[serde(default = "default_volume")]
    pub effect_volume: f64,
    #[serde(default = "default_volume")]
    pub music_volume: f64,
    #[serde(default = "default_volume")]
    pub inject_volume: f64,
    /// Render stage: the segments this task is responsible for. **One take**
    /// under the per-take schedule — the offer is sufficient on its own
    /// (voice, text, parameters), so the worker needs neither the script nor
    /// the cast to speak it, and a local edit costs one segment rather than a
    /// whole chapter.
    ///
    /// `None` (old inductor) means "plan from your own script as before";
    /// `Some([])` means the chapter has no units at all, so report `ok` with
    /// `units: 0` at once. The Option (not a bare Vec) is what keeps those two
    /// apart.
    ///
    /// Skipping is by **file presence under a content-addressed name**: a take
    /// the box already holds is by construction the right bytes, so nothing
    /// needs forcing. An adopted (pre-plan) take keeps its legacy name and is
    /// re-offered only once the inductor's own store lacks it, which is the
    /// one case a warm box can still hold the wrong bytes under a right name —
    /// see the render plan's `adopted` flag.
    #[serde(default)]
    pub render_units: Option<Vec<RenderUnitSpec>>,
    /// Filenames from `render_units` the inductor's own store lacks, so the
    /// worker must (re-)speak them even when its own disk already holds a
    /// file of that name. With content-addressed take files this is empty by
    /// construction and kept only for the adopted-take case and older
    /// agents. Absent (old inductor) means none forced.
    #[serde(default)]
    pub render_force: Vec<String>,
    /// Hash of the voice collection this chapter's plan was built from, keyed
    /// per speaker. The worker can compare it against the takes it holds to
    /// notice that the cast moved; the correctness check itself is the
    /// `take_key` on each unit, which is why nothing is invalidated on this
    /// alone. Empty from an old inductor.
    #[serde(default)]
    pub cast_hash: String,
    /// Merge stage: the chapter's take files **in mix order**, straight out of
    /// the recorded render plan.
    ///
    /// The mixer must read the names the renderer wrote. It used to re-derive
    /// them from the script and the cast, which was safe only while a filename
    /// was a pure function of those two; a content-addressed take name is a
    /// hash of the inputs instead, so the plan is the only thing that knows it.
    /// Empty from an old inductor — the worker then falls back to planning the
    /// names itself, exactly as before.
    #[serde(default)]
    pub merge_takes: Vec<String>,
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
    /// Hash of the inputs that decide these bytes — voice *key*, text,
    /// parameters and engine. The name is derived from it
    /// (`t-<take_key>.wav`), so a file in the store is proof of its own
    /// inputs and a re-plan can tell changed takes from unchanged ones
    /// without trusting a filename. Empty from an old inductor.
    #[serde(default)]
    pub take_key: String,
}

fn default_speed() -> f64 {
    1.0
}

fn default_volume() -> f64 {
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
    /// Adopt operator-supplied chapter text: the manual half of crawling, and
    /// the escape hatch for a single page a script cannot fetch.
    ///
    /// Carries the chapter number and the files (or literal text) to adopt.
    /// The text goes through the *same* boundary the crawler uses — site
    /// metadata out, entities decoded, a body too short to be a chapter
    /// refused — so importing a truncated paste fails here rather than three
    /// stages downstream.
    Import,
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
    /// Re-attribute speakers on one chapter's script (the digest routinely
    /// gives third-person narration to the character it describes, and
    /// quoted speech to the Narrator) and requeue exactly what the edit
    /// reached: the plan's diff re-speaks the changed takes, the merge
    /// re-mixes. A new speaker must already hold a voice, or the chapter
    /// would requeue into an unplannable row.
    Recast,
    /// Save a new mix (story speed + layer volumes) and requeue every merge:
    /// the finished mp3s were mixed with the old one. Render cache is kept —
    /// tempo and layers apply at merge time, so no segment needs re-speaking.
    Remix,
    /// The sound design was edited outside the scheduler — `:sound` writes the
    /// pool registries itself, from the TUI. This is the inductor being *told*
    /// to look, not the inductor having acted: it re-reads the registries and
    /// requeues every merge whose fingerprint the edit reached.
    ///
    /// A separate op from `Remix` because the two say different things. `Remix`
    /// carries the new knobs and saves them; this carries nothing and only
    /// asks "is what is on disk still what is published?". Collapsing them
    /// would mean a sound edit had to invent a mix.
    SoundChanged,
    /// Requeue every render task and its merge, deleting cached segments and
    /// finished mp3s: a full re-speak of the book. Mix-only changes use
    /// `Remix` instead — this one re-synthesizes every voice.
    Rerender,
    /// Requeue every merge without touching the mix or the render cache:
    /// effect clips, the scene map and the pools all apply at merge time.
    Remerge,
    /// Tell every beating worker to exit on its next heartbeat (2s). The
    /// graceful half of the cluster stop: in-flight tasks are reaped or
    /// requeued without strikes, and whatever stays behind (a dead box, the
    /// detached TTS sidecar) is still swept over ssh.
    ShutdownWorkers,
    /// Arm drain-then-exit: workers stop on their own once no unfinished
    /// task remains. Fires at once when the queue is already drained.
    ShutdownWhenIdle,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Translate => "translate",
            Op::CrawlSetup => "crawl-setup",
            Op::Import => "import",
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
            Op::Recast => "recast",
            Op::Remix => "remix",
            Op::SoundChanged => "sound-changed",
            Op::Rerender => "rerender",
            Op::Remerge => "remerge",
            Op::ShutdownWorkers => "shutdown-workers",
            Op::ShutdownWhenIdle => "shutdown-when-idle",
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        [
            Op::Translate,
            Op::CrawlSetup,
            Op::Import,
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
            Op::Recast,
            Op::Remix,
            Op::SoundChanged,
            Op::Rerender,
            Op::Remerge,
            Op::ShutdownWorkers,
            Op::ShutdownWhenIdle,
        ]
        .into_iter()
        .find(|o| o.as_str() == s)
    }
}

/// One speaker reassignment inside a chapter's script: the segment at
/// `index` (position in the script's `segments` array, sounds included)
/// is re-attributed to `speaker`. Part of the `recast` op.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerFix {
    pub index: usize,
    pub speaker: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpRequest {
    pub op: Op,
    /// Speaker reassignments for `recast`, in any order.
    #[serde(default)]
    pub fixes: Vec<SpeakerFix>,
    /// Segment indexes for `recast` to delete (duplicated lines the digest
    /// emitted twice). Sorted internally; every index must name a line, and
    /// at least one segment must remain.
    #[serde(default)]
    pub remove: Vec<usize>,
    #[serde(default)]
    pub start: Option<u32>,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub url_template: Option<String>,
    /// Files to adopt on `import` — one or more paths on the inductor's own
    /// filesystem. A path that does not exist is taken as literal chapter text,
    /// which is what makes a pasted chapter work as well as a drag-dropped
    /// file: most terminals deliver a drop as a pasted path, not an event.
    #[serde(default)]
    pub paths: Vec<String>,
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
    /// The new mix, for `remix`: story speed plus the three layer volumes.
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub effect_volume: Option<f64>,
    #[serde(default)]
    pub music_volume: Option<f64>,
    #[serde(default)]
    pub inject_volume: Option<f64>,
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

    /// Every variant, so a new one cannot be added without joining the families
    /// below. This list is the test suite's only exhaustive one on purpose: a
    /// hardcoded list *inside* a test is how `AwaitingIp` could have shipped
    /// diverging from its wire form while the test that exists to catch exactly
    /// that kept passing.
    const ALL: [MachineState; 9] = [
        MachineState::Unknown,
        MachineState::AwaitingIp,
        MachineState::Initializing,
        MachineState::Probing,
        MachineState::Configured,
        MachineState::Provisioning,
        MachineState::Online,
        MachineState::Offline,
        MachineState::Error,
    ];

    #[test]
    fn every_machine_state_roundtrips_through_its_wire_string() {
        // `as_str` is what the TUI posts to `/api/machines/state` and what the
        // machines pane renders; the enum is what the API deserializes back.
        // A state added to one list and not the other would show up as a
        // machine whose transition silently 422s, so pin both directions.
        for s in ALL {
            let wire = serde_json::to_string(&s).unwrap();
            assert_eq!(wire, format!("\"{}\"", s.as_str()), "serde vs as_str");
            assert_eq!(serde_json::from_str::<MachineState>(&wire).unwrap(), s);
            // One word: the pane's state column is fixed-width, and a word that
            // wraps or carries punctuation is one the column cannot show. How
            // wide it may be is the *pane's* business — see `tui/tests.rs`.
            assert!(
                !s.as_str().contains('_') && !s.as_str().contains(' '),
                "{s:?} is one word"
            );
        }
        // The match in `as_str` is exhaustive, so a new variant cannot compile
        // without a wire name — but it *can* be given a name that collides.
        let mut names: Vec<&str> = ALL.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "two states share a wire name");
    }

    #[test]
    fn only_online_accepts_work() {
        assert!(MachineState::Online.accepts_work());
        for s in [
            MachineState::Initializing,
            MachineState::Probing,
            MachineState::Configured,
            MachineState::Provisioning,
            MachineState::Offline,
            MachineState::Error,
        ] {
            assert!(!s.accepts_work(), "{s:?} must not be handed work");
        }
        // `Unknown` is "no opinion formed", not "ready" — the offer gate
        // treats it as a separate case on purpose.
        assert!(!MachineState::Unknown.accepts_work());
    }

    #[test]
    fn only_an_addressless_box_is_undialable() {
        // The gate on dialing is exact: every other state is *tried*, because a
        // failure to answer is itself the information (a box that went quiet is
        // how `Offline` is reached). Exactly one state has no address to try.
        for s in ALL {
            assert_eq!(s.dialable(), s != MachineState::AwaitingIp, "{s:?}");
        }
        // And the addressless wait is a wait, not a verdict: it may not be
        // stamped `Offline` for not answering when it cannot have been asked.
        assert!(MachineState::AwaitingIp.coming_up());
    }

    #[test]
    fn coming_up_covers_every_state_that_is_not_a_verdict() {
        // The dispatcher stamps `Offline` on any box that fails to answer
        // `/status`. These cannot answer *yet*, and reading a boot as death is
        // exactly what makes a freshly launched pool look broken.
        for s in [
            MachineState::AwaitingIp,
            MachineState::Initializing,
            MachineState::Probing,
            MachineState::Provisioning,
            MachineState::Configured,
        ] {
            assert!(s.coming_up(), "{s:?} is on its way up");
            assert!(!s.accepts_work(), "{s:?} is not ready for work");
        }
        // A verdict is not "coming up": silence about these is real news.
        for s in [
            MachineState::Online,
            MachineState::Offline,
            MachineState::Error,
            MachineState::Unknown,
        ] {
            assert!(!s.coming_up(), "{s:?} is a verdict, not a wait");
        }
        // `ALL` and the four lists above must partition it: every variant is
        // either coming up or a verdict, and never both.
        for s in ALL {
            assert_eq!(
                s.coming_up(),
                !matches!(
                    s,
                    MachineState::Online
                        | MachineState::Offline
                        | MachineState::Error
                        | MachineState::Unknown
                ),
                "{s:?} is in neither family or both"
            );
        }
    }

    #[test]
    fn set_state_stamps_only_real_transitions() {
        let mut m = Machine::new("10.0.0.5", "ubuntu", 22, None, "worker");
        assert_eq!(m.state, MachineState::Unknown);
        assert_eq!(m.state_since, 0, "a fresh record is never stamped");

        m.set_state(MachineState::Initializing);
        let born = m.state_since;
        assert!(born > 0, "a transition stamps the clock");

        // Re-stating the same state must not move the stamp, or "initializing
        // for 4 minutes" would reset on every poll and never reach a deadline.
        m.state_since = born.saturating_sub(60);
        let aged = m.state_since;
        m.set_state(MachineState::Initializing);
        assert_eq!(m.state_since, aged, "same state, same clock");

        // A real transition does move it.
        m.set_state(MachineState::Online);
        assert!(m.state_since >= aged);
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
            Op::Import,
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
            Op::Remix,
            Op::SoundChanged,
            Op::Rerender,
            Op::Remerge,
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
        assert_eq!(o.effect_volume, 1.0);
        assert_eq!(o.music_volume, 1.0);
        assert_eq!(o.inject_volume, 1.0);
    }

    #[test]
    fn remix_inject_volume_is_optional_and_roundtrips() {
        let mut req: OpRequest = serde_json::from_str(
            r#"{"op":"remix","speed":1.25,"effect_volume":0.5,"music_volume":0.0}"#,
        )
        .unwrap();
        assert_eq!(req.inject_volume, None);
        for volume in [None, Some(0.0), Some(0.25), Some(2.0)] {
            req.inject_volume = volume;
            let back: OpRequest =
                serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
            assert_eq!(back.inject_volume, volume);
        }
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
    fn a_batched_render_offer_survives_the_wire_intact() {
        // The whole change rests on one asymmetry: the **units** travel on the
        // wire, the **grouping** does not.
        //
        // `render_units` was already a `Vec`, which is what makes a batched
        // offer parse on a worker that predates batching — so the two sides can
        // be upgraded independently. The grouping is recorded on the ledger row
        // (`Task::batch`) and never serialised into an offer, so a worker can
        // neither see nor depend on a scheduling decision it has no business
        // knowing about.
        //
        // Both halves are easy to undo by accident — a `render_units` that
        // became a single struct would break the rollout, and a `batch` threaded
        // onto the offer would silently make the worker's behaviour depend on
        // the inductor's batch size — so both are pinned here.
        let units: Vec<RenderUnitSpec> = (0..10)
            .map(|i| RenderUnitSpec {
                tag: format!("000{i}"),
                name: format!("t-{i:016}.wav"),
                speaker: "A".into(),
                voice: "Adam".into(),
                text: format!("line {i}"),
                temperature: 0.8,
                silence_p: 0.15,
                take_key: format!("{i:016}"),
            })
            .collect();
        let offer = TaskOffer {
            task_id: "render:7:0".into(),
            chapter: 7,
            stage: Stage::Render,
            root: "/r".into(),
            url: None,
            crawl: None,
            attempt: 1,
            tts_url: Some("http://127.0.0.1:8818".into()),
            engine: "vieneu".into(),
            model_order: vec![],
            analyzer: "gemini".into(),
            analyzer_settings: AnalyzerSettings::default(),
            credentials: Credentials::default(),
            bible: None,
            script: None,
            cast: None,
            text: None,
            gap_ms: 300,
            speed: 1.0,
            ambience: false,
            music: false,
            effect_volume: 1.0,
            music_volume: 1.0,
            inject_volume: 1.0,
            render_units: Some(units.clone()),
            render_force: vec![],
            cast_hash: "abc123".into(),
            merge_takes: vec![],
            local_node: false,
        };

        let json = serde_json::to_string(&offer).unwrap();
        let back: TaskOffer = serde_json::from_str(&json).unwrap();
        let got = back.render_units.as_deref().expect("planned, not legacy");
        assert_eq!(got.len(), 10, "every take the offer carried");
        assert_eq!(
            got.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
            units.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
            "in order — a render speaks its chapter front to back"
        );
        assert_eq!(
            got[9].take_key, units[9].take_key,
            "with each take's own key"
        );
        assert_eq!(got[9].voice, "Adam");
        assert_eq!(back.cast_hash, "abc123", "and the chapter's cast hash");

        let as_value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            as_value.get("batch").is_none(),
            "the grouping is the ledger's, not the wire's: {json}"
        );

        // An offer that names no units is `Some([])`, not absent — the
        // distinction the worker reads as "report ok with zero units" against
        // "an old inductor, plan it yourself". A missing field must stay the
        // second of those.
        let old: TaskOffer =
            serde_json::from_str(&json.replace("\"render_units\"", "\"not_render_units\""))
                .unwrap();
        assert!(
            old.render_units.is_none(),
            "absent means an old inductor, and must not read as an empty chapter"
        );

        // The ledger side: the grouping is a *row's*, and round-trips there.
        let mut row = Task::new_take(7, 0);
        row.batch = vec!["render:7:1".into(), "render:7:2".into()];
        let row_back: Task = serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
        assert_eq!(row_back.batch, row.batch);
        assert_eq!(row_back.take, Some(0));
        // And a row written before the field existed reads as "no grouping",
        // which is what keeps a pre-batch ledger loading.
        let old_row: Task = serde_json::from_str(
            r#"{"chapter":7,"stage":"render","state":"done","attempts":0,
                "assigned_to":null,"lease_until":null,"detail":"","updated":1,"take":3}"#,
        )
        .unwrap();
        assert!(old_row.batch.is_empty(), "absent means no grouping");
        assert_eq!(old_row.take, Some(3));
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
            crawl: None,
            attempt: 1,
            tts_url: None,
            engine: "vieneu".into(),
            model_order: vec![],
            analyzer: "gemini".into(),
            analyzer_settings: AnalyzerSettings::default(),
            credentials: both_keys(),
            bible: None,
            script: None,
            cast: None,
            text: None,
            gap_ms: 300,
            speed: 1.0,
            ambience: false,
            music: false,
            effect_volume: 1.0,
            music_volume: 1.0,
            inject_volume: 1.0,
            render_units: None,
            render_force: vec![],
            cast_hash: String::new(),
            merge_takes: vec![],
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
        // And an old *worker* ignores the fields entirely — the serializer
        // emits them, the parser above proves absence is tolerated.
        let round: TaskOffer = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(round.credentials, o.credentials);
        assert_eq!(round.analyzer_settings, o.analyzer_settings);
    }

    #[test]
    fn an_empty_chain_is_not_the_same_as_saying_nothing() {
        let stated: AnalyzerSettings = serde_json::from_str(r#"{"analyze_models":[]}"#).unwrap();
        assert_eq!(stated.analyze_models, Some(vec![]));
        let silent: AnalyzerSettings = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(silent.analyze_models, None);
        // And both survive a round trip, which is what the worker sees.
        for block in [stated, silent] {
            let back: AnalyzerSettings =
                serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
            assert_eq!(back, block);
        }
    }

    #[test]
    fn an_old_inductors_heartbeat_answer_means_stay() {
        // The shutdown latch rides the heartbeat answer. An inductor that
        // predates it answers just `{"ok": true}` — the missing key must
        // default to "keep running", which is what makes either side
        // upgradable on its own.
        let old: HeartbeatAck = serde_json::from_str(r#"{"ok": true}"#).unwrap();
        assert!(old.ok && !old.shutdown);
        let told: HeartbeatAck = serde_json::from_str(r#"{"ok":true,"shutdown":true}"#).unwrap();
        assert!(told.shutdown);
        assert_eq!(Op::parse("shutdown-workers"), Some(Op::ShutdownWorkers));
        assert_eq!(Op::ShutdownWorkers.as_str(), "shutdown-workers");
    }
}
