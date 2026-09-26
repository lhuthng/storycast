//! Background work: one `Job` at a time, off the drawing loop.
use crate::tui::{
    app::App,
    input::{op_key, urlencode},
    style::{Level, LogLine},
};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bm_proto::{Machine, MachineState, Op, OpRequest, Roster};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub(crate) struct BackgroundJob {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) queued: Instant,
    pub(crate) started: Option<Instant>,
    pub(crate) activity: String,
}

/// Something only one job at a time may hold.
///
/// Ordered so a job that names several can take them in a stable order — two
/// jobs with overlapping sets then queue rather than deadlock. See
/// [`Job::resources`] for which job names what.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Res {
    /// The default lane: every job with no real conflict with anything.
    Command,
    /// The backend and the worker fleet as a whole.
    Cluster,
    /// One machine's ssh/rsync channel.
    Box(String),
    /// The AWS account, and the `.bm/aws/` document it is written into.
    Aws,
}

impl Res {
    /// How the jobs screen names this resource. Short — it sits on one row.
    pub(crate) fn label(&self) -> String {
        match self {
            Res::Command => "the command lane".into(),
            Res::Cluster => "the cluster".into(),
            Res::Box(addr) => format!("box {addr}"),
            Res::Aws => "the aws account".into(),
        }
    }
}

/// What `:workspace` was asked to do. Parsed at submit, so the prompt can
/// refuse nonsense before a job is queued.
#[derive(Debug)]
pub(crate) enum WorkspaceReq {
    List,
    Use(String),
    New(String),
}

/// What `:profile` was asked to do.
#[derive(Debug)]
pub(crate) enum ProfileReq {
    List,
    /// `unpack` — replaces the live `assets/` + `prompts/` trees.
    Load(String),
    /// Bundle the live tree into `profiles/<name>.tar.zst`.
    Pack(String),
}

#[derive(Debug)]
pub(crate) enum Job {
    Tracked {
        id: u64,
        job: Box<Job>,
    },
    /// Long SSH/rsync flow for exactly one box, executed off the UI task.
    /// Touches nothing else: no veto, no restart, no other worker.
    /// `settings_key` is the app-wide default the ssh chain falls back to
    /// when the machine carries no key of its own.
    ///
    /// `cancel` is the `B` start's flag when the catch-up dispatched this, and
    /// `None` for a `:prov` the operator asked for by hand. It gates the *last*
    /// step — the worker launch — so an `X` that lands while a push is in
    /// flight cannot be followed by a fresh worker on a box the operator just
    /// stopped. It does not abort the push: that is one blocking rsync, and
    /// killing it mid-file is how a box ends up half-configured.
    Provision {
        layout: bm_core::Layout,
        api: String,
        machine: Machine,
        force: bool,
        settings_key: Option<String>,
        cancel: Option<Arc<AtomicBool>>,
    },
    AddMachine {
        api: String,
        http: reqwest::Client,
        m: Machine,
    },
    /// Start the local backend, then run a range on it once live.
    /// `enqueue` is false for bare `B` (backend only) and true for the run
    /// screen's Enter (backend + job). `machines` is the registry snapshot at
    /// submit. Degraded start: the backend goes up first (seconds), then the
    /// boxes that still need work are handed out **as their own jobs** — a
    /// failing box lands in Error, never vetoes the rest. `cancel` lets `X`
    /// stop the catch-up between boxes.
    ///
    /// The catch-up is *not* run here. This job used to loop over every box
    /// itself, so a job called "start backend" — which should take seconds —
    /// held the cluster for as long as provisioning every machine took, and
    /// everything else queued behind it. It now finishes as soon as the
    /// inductor answers and emits [`Ev::CatchUp`]; the dashboard turns that
    /// into one `Provision` per box, each visible, each cancellable, and all
    /// of them in parallel.
    StartBackend {
        layout: bm_core::Layout,
        api: String,
        api_up: bool,
        start: u32,
        count: u32,
        enqueue: bool,
        machines: Vec<Machine>,
        cancel: Arc<AtomicBool>,
        settings_key: Option<String>,
    },
    /// Stop everything: the local backend by PID file, strays by sweep, and
    /// every registered remote worker over ssh. `X` means the cluster is
    /// quiet afterwards — not just this box.
    StopBackend {
        layout: bm_core::Layout,
        machines: Vec<Machine>,
        api: String,
        settings_key: Option<String>,
    },
    /// Local file work: copy a clip into `refs/`, tag it from its filename,
    /// register it in the pool and in `voices.json`. Needs no inductor.
    AddSample {
        layout: bm_core::Layout,
        path: String,
        name: Option<String>,
        tags: Option<Vec<String>>,
    },
    /// Deregister a machine. Idempotent, so it needs no confirmation beyond
    /// the one the operator already gave.
    DropMachine {
        api: String,
        http: reqwest::Client,
        addr: String,
    },
    Op {
        api: String,
        http: reqwest::Client,
        req: OpRequest,
        layout: bm_core::Layout,
    },
    LoadRoster {
        api: String,
        http: reqwest::Client,
        layout: bm_core::Layout,
    },
    /// Read every `data/script-*.json` and index the lines by speaker.
    ///
    /// A job rather than a keypress handler because it is a hundred file opens
    /// (26 ms warm here, but unbounded on a cold or networked path) and because
    /// it runs once per session — the result is cached, so the audition itself is
    /// instant.
    LoadLines {
        layout: bm_core::Layout,
    },
    /// Read the three sound-design registries, the scene map and every script,
    /// and work out what each pooled sound is still used for.
    ///
    /// A job for the same reason as `LoadLines` — it is a hundred file opens —
    /// and one more: the removal guard is read off this data, so it is also
    /// re-run after every save rather than cached for the session.
    LoadSounds {
        layout: bm_core::Layout,
    },
    /// Serve one already-rendered segment from the local checkout: the
    /// disconnected form of `Op::Segment`. Same lookup the inductor runs,
    /// against the TUI's own files, so listening needs no backend.
    Segment {
        layout: bm_core::Layout,
        character: String,
        voice: String,
        /// Exact sentence wanted (the shown line). Empty means triage.
        text: String,
    },
    /// Synthesize one line on this machine: the disconnected form of
    /// `Op::PreviewVoice`. Same engine call the sidecar makes, so a fresh
    /// voice auditions with no worker on — at the cost of loading the
    /// model here, which is why the connected path stays first.
    PreviewLocal {
        layout: bm_core::Layout,
        voice: String,
        text: String,
    },
    /// List, switch or create a workspace. Switching moves the pointer the
    /// ledger, settings and data hang off, so it is refused while anything
    /// reads them — see `cluster_busy`.
    Workspace {
        layout: bm_core::Layout,
        api: String,
        req: WorkspaceReq,
    },
    /// List, load or pack a profile bundle. `tools/profile.sh` does the tar +
    /// zstd; loading replaces the live tree every worker reads, so it takes
    /// the same lock as the workspace switch.
    Profile {
        layout: bm_core::Layout,
        api: String,
        req: ProfileReq,
    },
    /// What the account holds: one read-only `describe-instances`, handed to
    /// the Cloud view. Launches nothing, touches no worker. After the read it
    /// also asks the inductor to auto-relink any EC2 box whose address moved.
    AwsPool {
        root: std::path::PathBuf,
        api: String,
        http: reqwest::Client,
    },
    /// Save one machine's work policy: which stages it may run and in what
    /// order. Small and quiet — the editor sends one after every change so the
    /// panel never holds an unsaved decision.
    SaveTaskPolicy {
        api: String,
        http: reqwest::Client,
        addr: String,
        task_policy: Vec<bm_proto::TaskPref>,
    },
    /// Report a digest the operator performed by hand.
    ///
    /// **It posts the same `Complete` a worker posts, to the same endpoint.** The
    /// manual route exists to be the same digest with a person standing in for
    /// the model, so it must land through the same door: the inductor merges the
    /// bible delta, writes the script, marks the row Done, and invalidates the
    /// chapter's audio when the script changed. A private endpoint for the
    /// operator would be a second implementation of all of that.
    ///
    /// The body carries the whole `DigestOutcome` rather than a path, because the
    /// inductor may not share a filesystem with the dashboard — and because the
    /// script it holds is what every downstream machine is handed.
    ManualDigest {
        api: String,
        http: reqwest::Client,
        chapter: u32,
        script: serde_json::Value,
        delta: serde_json::Value,
    },
    /// Turn digest work off — or back on — across every machine.
    ///
    /// **Off snapshots each machine's whole policy, not just the digest flag**,
    /// because "on" has to mean *what that box had*, not *digest enabled*: a box
    /// whose digest was already off must stay off, and a box with no policy at
    /// all must get `None` back rather than a list it never had. That distinction
    /// is the entire reason this is a snapshot rather than a toggle.
    ///
    /// The snapshot is a **file**, so an inductor restart in between cannot
    /// silently turn digest work back on with no way to restore it — which is the
    /// failure a purely in-memory latch would have.
    DigestPolicy {
        api: String,
        http: reqwest::Client,
        layout: bm_core::Layout,
        /// `(addr, that machine's stored policy)`; `None` is "no policy", which
        /// `effective_task_policy` reads as the default list.
        machines: Vec<(String, Option<Vec<bm_proto::TaskPref>>)>,
        /// `true` puts the snapshot back; `false` takes one and disables digest.
        restore: bool,
    },
    /// Re-point a box whose EC2 public IP drifted (stop/start, spot relaunch)
    /// at the address it carries *now*. The instance id — stable for the box's
    /// whole life — comes from the machine's note; the account read supplies
    /// the current address and the registry swap happens in the API, which
    /// also keeps the live inductor's map consistent.
    RelinkMachine {
        layout: bm_core::Layout,
        api: String,
        http: reqwest::Client,
        /// The registry record as the operator selected it — its note carries
        /// the EC2 instance id to match on, its name the handle to keep.
        machine: Machine,
    },
    /// Store the app's IAM user from the console's CSV, off the UI thread.
    ///
    /// The secret is never typed in the dashboard, so the CSV is the only
    /// route offered — it carries both halves and the verify-then-write order
    /// is `aws_ops::login`, the same one the CLI runs.
    AwsLogin {
        root: std::path::PathBuf,
        csv: std::path::PathBuf,
    },
    /// Read the account and write the pool definition: AMI, subnet, security
    /// group, keypair and instance profile. `aws show` without the terminal.
    AwsDiscover {
        root: std::path::PathBuf,
        args: crate::aws_ops::DiscoverArgs,
    },
    /// Launch boxes, stream the lines, then **link what came back** into the
    /// registry — the join is made in the same job that reads the launch reply,
    /// so no instance id ever has to be correlated to a machine later.
    AwsUp {
        root: std::path::PathBuf,
        api: String,
        http: reqwest::Client,
        count: u32,
    },
    /// Terminate explicit instance ids, stream the lines, refresh the list.
    AwsDown {
        root: std::path::PathBuf,
        ids: Vec<String>,
    },
}

impl Job {
    pub(crate) fn bare(&self) -> &Job {
        let mut job = self;
        while let Job::Tracked { job: inner, .. } = job {
            job = inner;
        }
        job
    }

    pub(crate) fn into_bare(mut self) -> Job {
        while let Job::Tracked { job, .. } = self {
            self = *job;
        }
        self
    }

    pub(crate) fn label(&self) -> String {
        match self.bare() {
            Job::Provision { .. } => "provision machine",
            Job::AddMachine { .. } => "add machine",
            Job::StartBackend { .. } => "start backend",
            Job::StopBackend { .. } => "stop backend",
            Job::AddSample { .. } => "add sample",
            Job::DropMachine { .. } => "drop machine",
            Job::RelinkMachine { .. } => "relink machine",
            Job::SaveTaskPolicy { .. } => "save task policy",
            Job::ManualDigest { .. } => "report manual digest",
            Job::DigestPolicy { restore, .. } => {
                if *restore {
                    "digest policy: restore"
                } else {
                    "digest policy: off"
                }
            }
            Job::Op { req, .. } => req.op.as_str(),
            Job::LoadRoster { .. } => "load roster",
            Job::LoadLines { .. } => "index audition lines",
            Job::LoadSounds { .. } => "load sound design",
            Job::Segment { .. } => "local segment",
            Job::PreviewLocal { .. } => "preview voice (local)",
            Job::Workspace { req, .. } => match req {
                WorkspaceReq::List => "list workspaces",
                WorkspaceReq::Use(_) => "switch workspace",
                WorkspaceReq::New(_) => "create workspace",
            },
            Job::Profile { req, .. } => match req {
                ProfileReq::List => "list profiles",
                ProfileReq::Load(_) => "load profile",
                ProfileReq::Pack(_) => "pack profile",
            },
            Job::AwsPool { .. } => "aws pool",
            Job::AwsUp { .. } => "aws up",
            Job::AwsDown { .. } => "aws down",
            Job::AwsLogin { .. } => "aws login",
            Job::AwsDiscover { .. } => "aws discover",
            Job::Tracked { .. } => unreachable!(),
        }
        .to_string()
    }

    /// What this job needs to itself for its whole life.
    ///
    /// The scheduler starts a job the moment nothing else holds any of these,
    /// so **two jobs with nothing in common run at once**. That is the whole
    /// difference from the boolean "lane" this replaced: `aws discover` used to
    /// queue behind a five-minute box provision because both were filed under
    /// "lifecycle", and a provision of box A queued behind one of box B.
    ///
    /// Only name a resource where there is a real conflict. A job that names
    /// nothing conflicting is `Command` — the old command lane, which stays
    /// serial among its own members on purpose (two model-loading previews at
    /// once is not a thing anyone asked for).
    pub(crate) fn resources(&self) -> Vec<Res> {
        match self.bare() {
            // One cluster, one lifecycle: `B` and `X` must never interleave,
            // and either is meaningless while the other runs.
            Job::StartBackend { .. } | Job::StopBackend { .. } => vec![Res::Cluster],
            // Per box, not per fleet: two boxes provision independently, and
            // pushing to both at once is the point of naming the address.
            Job::Provision { machine, .. } => vec![Res::Box(machine.addr.clone())],
            // These four read-modify-write `.bm/aws/` and the account it
            // describes. Interleaving them loses a write or double-launches.
            Job::AwsUp { .. }
            | Job::AwsDown { .. }
            | Job::AwsLogin { .. }
            | Job::AwsDiscover { .. } => vec![Res::Aws],
            // Read-only indexes: a roster GET, a hundred file opens. They
            // hold nothing any other job needs, so they name nothing and
            // start on the next scan — never queued behind a five-minute
            // provision or a slow op. Callers already guard against
            // dispatching them twice (`lines_loading`, `roster_loading`).
            Job::LoadRoster { .. } | Job::LoadLines { .. } | Job::LoadSounds { .. } => vec![],
            _ => vec![Res::Command],
        }
    }

    /// The one thing worth naming on the jobs screen, or `None`.
    ///
    /// `Res::Command` is the default lane — true of most jobs and worth
    /// nothing on a row — so it is filtered out. A job that shows a resource
    /// is a job that can be *blocked*, and the row says by what: "queued ·
    /// needs box 10.0.0.5" is the answer to "why is this not running".
    pub(crate) fn resource_label(&self) -> Option<String> {
        self.resources()
            .iter()
            .find(|r| !matches!(r, Res::Command))
            .map(Res::label)
    }

    pub(crate) fn fallback_done(&self) -> DoneKind {
        let req = match self.bare() {
            Job::Op { req, .. } => req.clone(),
            Job::Segment { voice, .. } => OpRequest {
                op: Op::Segment,
                voice: Some(voice.clone()),
                ..Default::default()
            },
            Job::PreviewLocal { voice, .. } => OpRequest {
                op: Op::PreviewVoice,
                voice: Some(voice.clone()),
                ..Default::default()
            },
            Job::StartBackend { .. } => return DoneKind::StartDone,
            Job::LoadRoster { .. } => return DoneKind::RosterDone,
            Job::LoadLines { .. } => return DoneKind::LinesDone,
            Job::LoadSounds { .. } => return DoneKind::SoundsDone,
            // Both move what the dashboard is reading; the UI re-resolves
            // the layout and re-reads the pointer when one finishes.
            Job::Workspace { .. } | Job::Profile { .. } => return DoneKind::Relayout,
            _ => return DoneKind::Other,
        };
        DoneKind::Op {
            op: req.op,
            key: op_key(&req),
            ok: false,
            voice: req.voice,
            audio_b64: None,
            line_speaker: None,
            line_text: None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum DoneKind {
    RosterDone,
    LinesDone,
    SoundsDone,
    Op {
        op: Op,
        /// The in-flight key this job was dispatched under, so completion frees
        /// exactly that slot (two retries of different chapters can coexist).
        key: String,
        ok: bool,
        voice: Option<String>,
        /// The wav the op rendered, base64, if it rendered one. The TUI writes
        /// it next to the speaker and plays it; the inductor never assumes a
        /// speaker, and never keeps the audio either.
        audio_b64: Option<String>,
        /// A book line served audio speaks (segment audition): whose line and
        /// which sentence, so the client can show it and hold it for A/B.
        line_speaker: Option<String>,
        line_text: Option<String>,
    },
    /// Pool changed under the roster: reload it (only if one is showing).
    ReloadRoster,
    /// A backend start sequence finished (backend up, catch-up done or
    /// cancelled). Clears the double-`B` guard; anything else is Other.
    StartDone,
    /// A workspace switch or profile load finished: the active workspace,
    /// the live profile tree, or both have moved, so the dashboard must
    /// re-resolve its layout and re-read the pointer before the next frame.
    Relayout,
    Other,
}

pub(crate) enum Ev {
    JobStarted(u64),
    JobProgress {
        id: u64,
        text: String,
    },
    JobFinished(u64),
    Log(LogLine),
    Roster(Result<Roster, String>),
    Done(DoneKind),
    /// The inductor's answer to a manual digest report — the line `complete`
    /// returned, or why it never arrived.
    ///
    /// Carried back rather than assumed: the report can be refused (`unknown
    /// task`, or the row moving under it), and the operator is looking at a
    /// screen that said "reporting it" — so the screen has to be able to say what
    /// happened, including "no".
    ManualDigest(Result<String, String>),
    /// The answer to a cluster-wide digest-policy change: what happened, or why
    /// not. Shown either way, because "digest is off" is a claim the operator
    /// will act on.
    DigestPolicy(Result<String, String>),
    /// A `/api/state` snapshot from the background poller. Carrying the payload
    /// (not the parsed structs) keeps the parse on the UI task, where the
    /// ordering/sort fixes already live.
    State(Result<serde_json::Value, String>),
    /// The backend a `B` job started is up enough to take work: enqueue this.
    BackendLive {
        start: u32,
        count: u32,
    },
    /// The boxes a `B` start still has to catch up, and the flag that can stop
    /// it. The job does not provision them itself — it hands them to the
    /// dashboard, which dispatches one `Provision` per box so each gets its own
    /// row on the jobs screen and its own resource to hold. `cancel` is the
    /// same flag the `B` press created, so `X` still stops the catch-up.
    CatchUp {
        machines: Vec<Machine>,
        cancel: Arc<AtomicBool>,
    },
    /// Push a machine's state directly into the TUI's in-memory list.
    MachineUpdate {
        addr: String,
        state: MachineState,
        note: String,
    },
    /// The per-speaker line index, built off the UI thread.
    Lines(Result<std::collections::HashMap<String, Vec<String>>, String>),
    /// The sound-design pools, the scene map and each entry's usage.
    ///
    /// Boxed because this is the only large variant — `SoundData` is ~512 bytes
    /// against a 144-byte runner-up — and the channel is *unbounded*, so every
    /// message pays for the largest variant. `JobStarted(u64)` is eight bytes of
    /// payload allocating a 512-byte node. A reload is rare and one allocation
    /// is nothing; a progress tick is neither.
    Sounds(Result<Box<crate::tui::sound::SoundData>, String>),
    /// A fresh account listing, or the reason it could not be read. The Cloud
    /// view renders it and marks rows absent from the registry.
    Cloud(Result<Vec<bm_core::provision::AwsInstance>, String>),
}

/// Bounded wait for a freshly spawned inductor to answer `/api/state`.
/// True the moment it answers, false after `secs` — the caller reports and
/// quits instead of blocking a job (and the dashboard) forever.
pub(crate) async fn wait_api_live(api: &str, secs: u64) -> bool {
    for _ in 0..secs.max(1) {
        if crate::backend::inductor_up(api).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// Record one box's phase (`provisioning`, `error`, …) with a note, for the
/// Machines pane. API first; the ledger file only as a fallback while the
/// inductor is confirmed down (never fight a live scheduler for its file).
pub(crate) async fn set_machine_state(
    api: &str,
    layout: &bm_core::Layout,
    addr: &str,
    state: MachineState,
    note: &str,
) {
    let body = serde_json::json!({"addr": addr, "state": state.as_str(), "note": note});
    let url = format!("{}/api/machines/state", api.trim_end_matches('/'));
    if let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        let _ = client.post(&url).json(&body).send().await;
    }
    // Always write the ledger file as well: when the inductor is up the
    // TUI refreshes from the API, but the ledger is the only source
    // before the API starts or if the POST fails. It is the *workspace's*
    // ledger — a box's state belongs to the book being run.
    let path = layout.ledger();
    let mut doc: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::json!({"machines": []}));
    let mut changed = false;
    if let Some(arr) = doc.get_mut("machines").and_then(|m| m.as_array_mut()) {
        if let Some(e) = arr
            .iter_mut()
            .find(|x| x.get("addr").and_then(|a| a.as_str()) == Some(addr))
        {
            e["state"] = serde_json::Value::String(state.as_str().into());
            if !note.is_empty() {
                e["note"] = serde_json::Value::String(note.into());
            }
            changed = true;
        }
    }
    if changed {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(&doc).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub(crate) fn op_job(app: &App, http: &reqwest::Client, req: OpRequest) -> Job {
    Job::Op {
        api: app.api.clone(),
        http: http.clone(),
        req,
        layout: app.layout.clone(),
    }
}

/// The verdict for a fetch that never got an answer, from the two facts that
/// decide it.
///
/// **Pure on purpose.** The only input that matters is whether the failure was a
/// *refusal* (`reqwest::Error::is_connect`), and this is the one place that
/// decides. It used to be three lines inside `fetch_state`'s `match`, which meant
/// the only way to test the wording was to bind an ephemeral port, drop the
/// listener and hope nothing else was handed the same port before the request —
/// a race that failed once in six full-suite runs. Everything the verdict needs
/// can be handed to it.
///
/// `detail` is deliberately dropped on the refusal branch: a refused connection
/// is the normal cold start, so reqwest's prose for it is noise exactly where the
/// remedy (`:B`) belongs. On any other failure the detail is kept — a timeout or
/// a reset may be a *sick* inductor rather than an absent one, and telling those
/// apart is why there are two branches at all.
pub(crate) fn unreachable_verdict(api: &str, refused: bool, detail: &str) -> String {
    if refused {
        format!("inductor is down at {api} — :B to start it")
    } else {
        format!("inductor unreachable at {api}: {detail}")
    }
}

/// Fetch `/api/state` once.
///
/// Free-standing so the background poller can use it without holding the UI
/// state — the whole point of the poller is that the drawing loop never waits
/// on this call.
pub(crate) async fn fetch_state(
    http: &reqwest::Client,
    api: &str,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/api/state", api.trim_end_matches('/'));
    match http.get(&url).send().await {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("bad state payload: {e}")),
        // Which verdict, and why, is `unreachable_verdict`'s business.
        Err(e) => Err(unreachable_verdict(api, e.is_connect(), &e.to_string())),
    }
}

/// Report one line to the event pane.
fn send(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, level: Level, text: String) {
    let _ = tx.send(Ev::Log(LogLine {
        level,
        wall: bm_proto::now_secs(),
        text,
    }));
}

/// The state a failed provision leaves behind.
///
/// Usually `Error`: the operator asked, it did not work, and the note says why.
///
/// The exception is a box we already knew was **booting** that never answered
/// ssh. The probe learned nothing it did not already know, so calling it broken
/// is the same misreading the `initializing` state exists to prevent — and
/// `:prov` a few seconds after `:up` is the most likely way to hit it. It stays
/// `initializing`, and the boot deadline is what gives up.
///
/// Note that restoring `initializing` re-stamps its clock, so a box nobody
/// touches again expires five minutes after the *last* attempt. That is the
/// right rule: while the operator is retrying, somebody is watching it.
pub(crate) fn verdict_after_failed_provision(
    was_initializing: bool,
    reachable: bool,
) -> MachineState {
    if was_initializing && !reachable {
        MachineState::Initializing
    } else {
        MachineState::Error
    }
}

/// Run a blocking provision with its log lines streaming into the event pane
/// as they happen: each step lands with a wall timestamp, so a slow box
/// reads as progress rather than a stall. The pump drains before returning,
/// so everything the caller sends afterwards stays in order.
async fn provision_live(
    tx: &tokio::sync::mpsc::UnboundedSender<Ev>,
    run: impl FnOnce(tokio::sync::mpsc::UnboundedSender<String>) -> crate::ProvisionOutcome
        + Send
        + 'static,
) -> Result<crate::ProvisionOutcome, tokio::task::JoinError> {
    let (live_tx, mut live_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let fwd = tx.clone();
    let pump = tokio::spawn(async move {
        while let Some(line) = live_rx.recv().await {
            send(&fwd, Level::Info, line);
        }
    });
    let out = tokio::task::spawn_blocking(|| run(live_tx)).await;
    // The run owned the only sender, so its end closes the channel: awaiting
    // the pump flushes every line before the caller continues.
    let _ = pump.await;
    out
}

/// Elapsed-push stamp for the provision launch line (`4s`, `3m41s`): the
/// reason a box's `worker started` can land minutes after faster boxes are
/// already beating. Same shape as the jobs screen's label, kept beside its
/// only caller rather than shared.
fn push_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// Why a provision run did not leave the box ready, in the operator's terms.
///
/// The run's own `stop` wins: the pre-flight steps that fail before a step can
/// log (`no TTS sidecar binary for …`, `no local profile loaded`) have no
/// shared vocabulary, and a scanner that only knows "missing"/"not found"/
/// "failed" silently reduced them to "provision INCOMPLETE" — which names no
/// cause and answers a question the operator never asked. The log scan stays
/// as the fallback for failures inside the step flow, where the useful line
/// really is in the log: prefer the inner root cause ("rsync: command not
/// found") over its wrapper ("agent install failed").
fn provision_stop_reason(stop: Option<&str>, lines: &[String]) -> String {
    if let Some(why) = stop.filter(|s| !s.trim().is_empty()) {
        return bm_core::util::head_chars(why, 160);
    }
    /// A log line without its `[addr] ` prefix — the pane already shows the
    /// machine, and the address is the widest part of the note.
    fn body(l: &str) -> &str {
        match l.strip_prefix('[') {
            Some(rest) => match rest.find(']') {
                Some(i) => &rest[i + 2..],
                None => l,
            },
            None => l,
        }
    }
    lines
        .iter()
        .rev()
        .find(|l| l.contains("missing") || l.contains("not found"))
        .or_else(|| lines.iter().rev().find(|l| l.contains("failed")))
        .map(|l| bm_core::util::head_chars(body(l), 120))
        .unwrap_or_else(|| "provision INCOMPLETE".to_string())
}

pub(crate) async fn job_provision(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    machine: Machine,
    force: bool,
    settings_key: Option<String>,
    cancel: Option<Arc<AtomicBool>>,
) {
    let addr = machine.addr.clone();
    // Anchor for the launch line below: the push ahead is blocking and slow
    // on some boxes, so the worker starts minutes after faster boxes are
    // already beating — the elapsed on that line is what says so.
    let t0 = Instant::now();
    send(&tx, Level::Info, format!("[{addr}] provisioning machine…"));
    // Read before the run: the job stamps `provisioning` below, so the state on
    // the way in is the only record that this box was still booting. A failed
    // probe against a box we knew was booting is a wait, not a fault.
    let was_initializing = machine.state == MachineState::Initializing;
    let mut again = machine.clone();
    // The app-wide default fills a keyless box; a box key always wins.
    let key = bm_core::provision::resolve_key(machine.ssh_key.as_deref(), settings_key.as_deref())
        .0
        .map(|p| p.to_string_lossy().to_string());
    // The relaunched worker must ssh the same way the provision did.
    again.ssh_key = key.clone();
    let send_update =
        |tx: &tokio::sync::mpsc::UnboundedSender<Ev>, state: MachineState, note: &str| {
            let _ = tx.send(Ev::MachineUpdate {
                addr: addr.clone(),
                state,
                note: note.to_string(),
            });
        };
    send_update(
        &tx,
        MachineState::Provisioning,
        if force {
            "force re-provision (p)"
        } else {
            "provisioning (p)"
        },
    );
    set_machine_state(
        &api,
        &layout,
        &addr,
        MachineState::Provisioning,
        if force {
            "force re-provision (p)"
        } else {
            "provisioning (p)"
        },
    )
    .await;
    let for_provision = layout.clone();
    let out = provision_live(&tx, move |live| {
        crate::provision_machine(
            &for_provision,
            &machine.addr,
            &machine.ssh_user,
            machine.ssh_port,
            key,
            force,
            Some(live),
        )
    })
    .await;
    match out {
        Ok(out) => {
            let ready = out.ready;
            // Lines already streamed live above — `lines` stays for the
            // reason scan only, never re-sent.
            let fail_reason = provision_stop_reason(out.stop.as_deref(), &out.lines);
            if ready {
                // `X` landed while this box was being pushed: the box is
                // provisioned, but giving it a worker now would leave the
                // cluster running after the stop the operator asked for. The
                // push itself is not aborted — it is one blocking rsync, and
                // killing it mid-file is how a box ends up half-configured —
                // so the stop is honoured at the last point that matters.
                if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                    let note = "start cancelled (X) — provisioned, worker not launched";
                    send_update(&tx, MachineState::Configured, note);
                    set_machine_state(&api, &layout, &addr, MachineState::Configured, note).await;
                    send(&tx, Level::Info, format!("[{addr}] {note}"));
                    let _ = tx.send(Ev::Done(DoneKind::Other));
                    return;
                }
                send_update(
                    &tx,
                    MachineState::Configured,
                    "provisioned — waiting for the worker's first beat",
                );
                send(
                    &tx,
                    Level::Ok,
                    format!(
                        "[{addr}] provision complete in {} — starting its worker",
                        push_label(t0.elapsed().as_secs())
                    ),
                );
                // Worker half only, never the inductor: a `p` retry
                // finishes with the box joined, whatever else runs.
                // One entry point, whatever the box is: the launcher decides
                // fork-versus-ssh and nothing here asks which it got.
                let root = layout.root.clone();
                let boxm = again;
                match tokio::task::spawn_blocking(move || {
                    crate::backend::start_workers(&[boxm], &root)
                })
                .await
                {
                    Ok((true, lines)) => {
                        for l in lines {
                            send(&tx, Level::Info, l);
                        }
                    }
                    Ok((false, lines)) => {
                        for l in lines {
                            send(&tx, Level::Error, l);
                        }
                        send_update(
                            &tx,
                            MachineState::Error,
                            "provisioned but the worker would not start — :prov again",
                        );
                        set_machine_state(
                            &api,
                            &layout,
                            &addr,
                            MachineState::Error,
                            "provisioned but the worker would not start — :prov again",
                        )
                        .await;
                        let _ = tx.send(Ev::Done(DoneKind::Other));
                        return;
                    }
                    Err(e) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("[{addr}] worker start task failed: {e}"),
                        );
                        send_update(
                            &tx,
                            MachineState::Error,
                            "provisioned but the worker start crashed — :prov again",
                        );
                        set_machine_state(
                            &api,
                            &layout,
                            &addr,
                            MachineState::Error,
                            "provisioned but the worker start crashed — :prov again",
                        )
                        .await;
                        let _ = tx.send(Ev::Done(DoneKind::Other));
                        return;
                    }
                }
                set_machine_state(
                    &api,
                    &layout,
                    &addr,
                    MachineState::Configured,
                    "provisioned — waiting for the worker's first beat",
                )
                .await;
            } else if verdict_after_failed_provision(was_initializing, out.reachable)
                == MachineState::Initializing
            {
                // It never answered ssh, and we already knew it was booting —
                // so this is a wait, not a fault. Reporting `Error` here would
                // call a box twenty seconds into its first boot broken, which
                // is precisely the misreading `initializing` exists to stop.
                let note = "still booting — nothing to do yet, :prov again in a moment";
                send_update(&tx, MachineState::Initializing, note);
                set_machine_state(&api, &layout, &addr, MachineState::Initializing, note).await;
                send(&tx, Level::Info, format!("[{addr}] {note}"));
            } else {
                // The note carries the actual failing step — a missing local
                // build, python missing, an ssh abort — because a bare
                // "INCOMPLETE" made the machine pane lie about what the box
                // needs, and ":prov again" is the wrong advice for a failure
                // that only a build on this machine can fix.
                let reason = fail_reason;
                send_update(&tx, MachineState::Error, &reason);
                set_machine_state(&api, &layout, &addr, MachineState::Error, &reason).await;
                send(
                    &tx,
                    Level::Error,
                    format!("[{addr}] {reason} — fix it and run :prov again"),
                );
            }
        }
        Err(e) => {
            send(
                &tx,
                Level::Error,
                format!("[{addr}] provision task crashed: {e}"),
            );
            send_update(
                &tx,
                MachineState::Error,
                "provision task crashed — :prov again",
            );
            set_machine_state(
                &api,
                &layout,
                &addr,
                MachineState::Error,
                "provision task crashed — :prov again",
            )
            .await;
            send(
                &tx,
                Level::Error,
                format!("[{addr}] provision task failed: {e}"),
            );
        }
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_add_machine(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    m: Machine,
) {
    let addr = m.addr.clone();
    match http
        .post(format!("{api}/api/machines"))
        .json(&m)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => send(
            &tx,
            Level::Ok,
            format!("machine {addr} added — :prov provisions it"),
        ),
        Ok(r) => send(
            &tx,
            Level::Error,
            format!("add {addr} rejected: HTTP {}", r.status()),
        ),
        Err(e) => send(&tx, Level::Error, format!("add {addr} failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_add_sample(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    path: String,
    name: Option<String>,
    tags: Option<Vec<String>>,
) {
    // Off the UI thread: enrollment loads the voice model and takes a
    // while. Same shape as the provision arm below.
    let for_log = path.clone();
    let out = tokio::task::spawn_blocking(move || {
        bm_core::pool::add_sample(&layout.root, std::path::Path::new(&path), tags, name)
    })
    .await;
    match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Ok, l);
            }
            // The picker may be showing the pre-sample roster: fetch a
            // fresh one so the new voice is there without pressing R.
            let _ = tx.send(Ev::Done(DoneKind::ReloadRoster));
        }
        Ok(Err(e)) => {
            send(&tx, Level::Error, format!("add-sample {for_log}: {e:#}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
        Err(e) => {
            send(
                &tx,
                Level::Error,
                format!("add-sample {for_log} task failed: {e}"),
            );
            let _ = tx.send(Ev::Done(DoneKind::Other));
        }
    }
}

fn start_cancelled(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, cancel: &AtomicBool) -> bool {
    if !cancel.load(Ordering::Relaxed) {
        return false;
    }
    send(
        tx,
        Level::Warn,
        "start cancelled (X) — no more workers will launch".into(),
    );
    let _ = tx.send(Ev::Done(DoneKind::StartDone));
    true
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn job_start_backend(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    mut api_up: bool,
    start: u32,
    count: u32,
    enqueue: bool,
    machines: Vec<Machine>,
    cancel: Arc<AtomicBool>,
    settings_key: Option<String>,
) {
    // Degraded start: the backend goes up first (seconds), then each
    // box provisions in the background and joins as it becomes ready.
    // A failing box lands in Error with its reason — it never vetoes
    // the rest. The catch-up is handed to the dashboard rather than run
    // here (see the hand-off at the end of this function), so the boxes
    // provision **concurrently** — each one holds its own `Res::Box`,
    // and `X` reaches them all through the shared `cancel` flag.
    if start_cancelled(&tx, &cancel) {
        return;
    }
    let resolve = |m: &Machine| {
        bm_core::provision::resolve_key(m.ssh_key.as_deref(), settings_key.as_deref())
            .0
            .map(|p| p.to_string_lossy().to_string())
    };
    let mut targets = machines;
    targets.sort_by(|a, b| a.addr.cmp(&b.addr));
    targets.dedup_by(|a, b| a.addr == b.addr);
    if targets.is_empty() {
        targets = vec![Machine::new("127.0.0.1", "local", 22, None, "worker")];
    }
    send(
        &tx,
        Level::Info,
        format!(
            "starting backend now — {} machine(s) catch up in background, each box's worker starts when its own push lands…",
            targets.len()
        ),
    );
    let has_remotes = targets
        .iter()
        .any(|m| !crate::backend::is_local_addr(&m.addr));
    let port = crate::backend::api_port(&api);
    if has_remotes {
        // A running inductor bound to loopback (old start, hand start)
        // is deaf to exactly these boxes: restart it LAN-wide first.
        // Workers ride through — they re-register on their own and
        // their in-flight reports still count afterwards.
        let dark =
            crate::backend::lan_blackout(&targets, port, advertised_host(&layout).as_deref()).await;
        if !dark.is_empty() {
            send(
                &tx,
                Level::Warn,
                format!(
                    "inductor invisible from {} — restarting it LAN-wide (workers ride through)…",
                    dark.join(", ")
                ),
            );
            let (gone, lines) = crate::backend::stop_inductor(&layout.root).await;
            for l in lines {
                send(&tx, Level::Info, l);
            }
            if !gone {
                send(&tx, Level::Error,
                            "cannot rebind an inductor this TUI didn't start — stop it by hand (or restart it with --bind 0.0.0.0), then B again".into(),
                        );
                let _ = tx.send(Ev::Done(DoneKind::StartDone));
                return;
            }
            api_up = false;
        }
    }
    // The analyzer was already saved to the settings file at submit,
    // so a fresh backend picks it up — but a live one never re-reads
    // it, hence the warning.
    if api_up {
        send(
            &tx,
            Level::Warn,
            "inductor already up: analyzer saved, takes effect on next restart (X, then B)".into(),
        );
    }
    // Inductor only: workers start per-box after that box provisions,
    // so an unready box never takes tasks it would fail. Spawning is
    // instant (the server boots in the background); the enqueue waits
    // for the first live refresh (see Ev::BackendLive) — and only
    // when asked: bare `B` brings the backend, nothing more.
    if start_cancelled(&tx, &cancel) {
        return;
    }
    match crate::backend::start_backend(
        &layout.root,
        &api,
        api_up,
        crate::backend::public_bind(has_remotes),
        false,
    ) {
        Ok(lines) => {
            for l in lines {
                send(&tx, Level::Ok, l);
            }
            // Only on success: no backend, no job.
            if enqueue {
                let _ = tx.send(Ev::BackendLive { start, count });
            }
        }
        Err(e) => {
            send(&tx, Level::Error, format!("backend start failed: {e:#}"));
            let _ = tx.send(Ev::Done(DoneKind::StartDone));
            return;
        }
    }
    send(&tx, Level::Info, "[local backend] waiting for API…".into());
    let live = wait_api_live(&api, 30).await;
    if start_cancelled(&tx, &cancel) {
        return;
    }
    if !live {
        send(
            &tx,
            Level::Error,
            "backend spawned but never answered — check .bm/inductor.log, then B again".into(),
        );
        let _ = tx.send(Ev::Done(DoneKind::StartDone));
        return;
    }
    // The boxes that still need work are handed to the dashboard as jobs of
    // their own, and this job ends here.
    //
    // This is where the inline catch-up loop used to be, and the difference is
    // the whole point of the change: the loop made one job hold the cluster for
    // as long as provisioning every box took, so `start backend` — a press that
    // should be seconds — showed minutes and every later job sat queued behind
    // it. Each box now gets its own `provision machine` row, all of them
    // running at once because they hold different boxes.
    //
    // A box already beating needs nothing: `online` is the state this path
    // exists to reach, and it is *working*. Re-provisioning it anyway is why
    // `B` on a healthy cluster took minutes, so it is skipped and said out
    // loud — `p` is the deliberate re-provision.
    let (todo, online) = split_catchup(targets, api_up, &resolve);
    for addr in &online {
        send(
            &tx,
            Level::Info,
            format!("[{addr}] already online — nothing to catch up (press p to re-provision it)"),
        );
    }
    if !todo.is_empty() {
        send(
            &tx,
            Level::Info,
            format!(
                "{} machine(s) to catch up — one job each, running together",
                todo.len()
            ),
        );
        let _ = tx.send(Ev::CatchUp {
            machines: todo,
            cancel: cancel.clone(),
        });
    }
    let _ = tx.send(Ev::Done(DoneKind::StartDone));
}

/// Split a `B` start's boxes into catch-ups and skips.
///
/// A box is skipped as "already online" only when the inductor was up at
/// submit (`api_up`): with it down, the states are the last live poll's —
/// frozen by `state_failed`, which keeps the rows — and trusting them skips
/// every catch-up, so the first `:B` starts the inductor and no worker and
/// only the second `:B` brings the boxes. A box that truly is beating costs
/// one cheap "already running" check inside its provision; a box wrongly
/// skipped costs the whole cluster.
pub(crate) fn split_catchup(
    targets: Vec<Machine>,
    api_up: bool,
    resolve: &dyn Fn(&Machine) -> Option<String>,
) -> (Vec<Machine>, Vec<String>) {
    let mut todo: Vec<Machine> = Vec::new();
    let mut online: Vec<String> = Vec::new();
    for mut m in targets {
        if api_up && m.state == MachineState::Online {
            online.push(m.addr.clone());
            continue;
        }
        // The key is resolved here, once: the catch-up job is handed a box
        // that already knows how to reach it, so the dispatcher needs no
        // settings of its own.
        let key = resolve(&m);
        m.ssh_key = key;
        todo.push(m);
    }
    (todo, online)
}

pub(crate) async fn job_stop_backend(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    machines: Vec<Machine>,
    api: String,
    settings_key: Option<String>,
) {
    // Cluster-wide stop, off the UI task: ssh sweeps take seconds per
    // box and must never freeze the dashboard. Keyless boxes fall back
    // to the app-wide default, like every other ssh flow.
    //
    // Graceful first: the shutdown op latches the inductor, whose next
    // heartbeat answer (2s) tells every worker to exit on its own — no
    // ssh needed for the living. The sweep below stays as the fallback
    // for what cannot hear it: dead boxes, old agents, and the detached
    // TTS sidecar, which is nobody's child.
    if let Ok(http) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        match http
            .post(format!("{}/api/op", api.trim_end_matches('/')))
            .json(&serde_json::json!({"op": "shutdown-workers"}))
            .send()
            .await
        {
            Ok(_) => {
                send(
                    &tx,
                    Level::Info,
                    "shutdown asked — workers exit on next beat, sweeping strays…".into(),
                );
                tokio::time::sleep(std::time::Duration::from_secs(6)).await;
            }
            Err(e) => send(
                &tx,
                Level::Warn,
                format!("shutdown op unreachable ({e}) — falling back to ssh sweep"),
            ),
        }
    }
    let machines: Vec<Machine> = machines
        .into_iter()
        .map(|mut m| {
            m.ssh_key =
                bm_core::provision::resolve_key(m.ssh_key.as_deref(), settings_key.as_deref())
                    .0
                    .map(|p| p.to_string_lossy().to_string());
            m
        })
        .collect();
    for line in crate::backend::stop_everywhere(&layout, &machines, &api).await {
        send(&tx, Level::Info, line);
    }
    send(
        &tx,
        Level::Ok,
        "stop requested everywhere — see lines above per machine".into(),
    );
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_drop_machine(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    addr: String,
) {
    let url = format!("{api}/api/machines?addr={}", urlencode(&addr));
    let (level, text) = match http.delete(&url).send().await {
        Ok(r) if r.status().is_success() => {
            (Level::Ok, format!("dropped {addr} from the registry"))
        }
        Ok(r) => (Level::Error, format!("drop {addr}: HTTP {}", r.status())),
        Err(e) => (Level::Error, format!("drop {addr} failed: {e}")),
    };
    send(&tx, level, text);
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Persist one machine's work policy through the API, so the live inductor and
/// the on-disk `machines.json` agree. Success is silent — the panel is the
/// feedback — but a rejection is named, because a policy that did not stick is
/// a scheduling surprise later.
pub(crate) async fn job_save_task_policy(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    addr: String,
    task_policy: Vec<bm_proto::TaskPref>,
) {
    let url = format!("{}/api/machines/policy", api.trim_end_matches('/'));
    let body = serde_json::json!({"addr": addr, "task_policy": task_policy});
    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => send(
            &tx,
            Level::Error,
            format!(
                "policy save {addr}: the inductor answered HTTP {}",
                r.status()
            ),
        ),
        Err(e) => send(&tx, Level::Error, format!("policy save {addr} failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_manual_digest(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    chapter: u32,
    script: serde_json::Value,
    delta: serde_json::Value,
) {
    // One report body, shared with the headless backup runner (`manual::report`):
    // the inductor cannot tell a by-hand chapter from a backup one except by the
    // `operator` id they both claim it under.
    let ev = crate::manual::report(
        &api,
        &http,
        chapter,
        &script,
        &delta,
        format!("digest ch{chapter} by hand"),
    )
    .await;
    let _ = tx.send(Ev::ManualDigest(ev));
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_digest_policy(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    layout: bm_core::Layout,
    machines: Vec<(String, Option<Vec<bm_proto::TaskPref>>)>,
    restore: bool,
) {
    let path = layout.bm_state().join("digest-suspend.json");
    let ev = if restore {
        digest_restore(&path, &api, &http, &machines).await
    } else {
        digest_suspend(&path, &api, &http, &machines).await
    };
    let _ = tx.send(Ev::DigestPolicy(ev));
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Save every machine's policy, then write back a copy with digest disabled.
///
/// The write-back goes through the same `/api/machines/policy` the policy editor
/// uses, so there is one place a machine's policy is set — and the live inductor
/// updates its own copy of the registry rather than only the file.
async fn digest_suspend(
    path: &std::path::Path,
    api: &str,
    http: &reqwest::Client,
    machines: &[(String, Option<Vec<bm_proto::TaskPref>>)],
) -> Result<String, String> {
    let snapshot: serde_json::Map<String, serde_json::Value> = machines
        .iter()
        .map(|(addr, policy)| {
            (
                addr.clone(),
                serde_json::to_value(policy).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    bm_core::atomic_write(
        path,
        &serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    )
    .map_err(|e| format!("could not save the snapshot to {}: {e}", path.display()))?;

    let mut off = Vec::new();
    for (addr, policy) in machines {
        // Disable digest in the policy that is *in force*, so a box with no
        // stored policy gets the default list with digest turned off rather than
        // a list invented here.
        let mut next = policy
            .clone()
            .unwrap_or_else(bm_proto::TaskPref::default_list);
        for p in next.iter_mut() {
            if p.stage == bm_proto::Stage::Digest {
                p.enabled = false;
            }
        }
        match put_policy(api, http, addr, &next).await {
            Ok(()) => off.push(addr.clone()),
            Err(e) => {
                return Err(format!(
                    "digest is off and the snapshot is saved, but {addr} refused it ({e}) — \
                     `:on` will still put everything back"
                ))
            }
        }
    }
    Ok(format!(
        "digest off on {} machine(s) — snapshot saved to {}; `:on` restores each box's own policy",
        off.len(),
        path.display()
    ))
}

/// Put each machine's snapshotted policy back, verbatim.
pub(crate) async fn digest_restore(
    path: &std::path::Path,
    api: &str,
    http: &reqwest::Client,
    machines: &[(String, Option<Vec<bm_proto::TaskPref>>)],
) -> Result<String, String> {
    let saved: serde_json::Map<String, serde_json::Value> =
        bm_core::read_json(path).map_err(|e| {
            format!(
            "no snapshot at {} ({e}) — digest was not turned off from here, so there is nothing \
             to restore; set each box's policy in the policy editor (`P`)",
            path.display()
        )
        })?;
    let mut back = 0;
    for (addr, current) in machines {
        // A machine that is not in the snapshot was added while digest was off.
        // Leave it alone and say so: restoring it to `None` would silently
        // re-enable digest on a box the operator never switched off.
        let Some(value) = saved.get(addr) else {
            continue;
        };
        let policy: Option<Vec<bm_proto::TaskPref>> =
            serde_json::from_value(value.clone()).unwrap_or(None);
        if policy == *current {
            back += 1;
            continue;
        }
        put_policy(api, http, addr, policy.as_deref().unwrap_or(&[])).await?;
        back += 1;
    }
    // Only now: a restore that failed half way must leave the snapshot in place,
    // or the boxes it did not reach have no way back.
    let _ = std::fs::remove_file(path);
    Ok(format!(
        "digest policy restored on {back} machine(s) — each box is back to what it had"
    ))
}

async fn put_policy(
    api: &str,
    http: &reqwest::Client,
    addr: &str,
    task_policy: &[bm_proto::TaskPref],
) -> Result<(), String> {
    let url = format!("{}/api/machines/policy", api.trim_end_matches('/'));
    let body = serde_json::json!({"addr": addr, "task_policy": task_policy});
    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("HTTP {}", r.status())),
        Err(e) => Err(format!("{e}")),
    }
}

pub(crate) async fn job_relink_machine(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    http: reqwest::Client,
    machine: Machine,
) {
    let old_addr = machine.addr.clone();
    // The machine the operator selected carries its identity in the note:
    // the EC2 instance id `machine_from_instance` stamped at launch. Without
    // it there is nothing to match a relaunched box by — say so instead of
    // guessing across the account.
    let Some(id) = bm_core::provision::ec2_id_from_note(&machine.note) else {
        send(
            &tx,
            Level::Error,
            format!(
                "[{}] relink: no EC2 instance id on this machine's record — only EC2-launched boxes can be relinked; :add the new address by hand",
                old_addr
            ),
        );
        let _ = tx.send(Ev::Done(DoneKind::Other));
        return;
    };
    // Read the account off the UI thread: one describe-instances.
    let root = layout.root.clone();
    let pool = tokio::task::spawn_blocking(move || crate::aws_ops::pool(&root)).await;
    let (cfg, instances) = match pool {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            send(&tx, Level::Error, format!("relink: aws pool: {e:#}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
        Err(e) => {
            send(&tx, Level::Error, format!("relink task failed: {e}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
    };
    let Some(i) = instances.iter().find(|i| i.id == id) else {
        send(
            &tx,
            Level::Error,
            format!(
                "[{old_addr}] relink: {id} is not in the account anymore (terminated, or the marker tag is gone) — :drop it and :add the replacement",
            ),
        );
        let _ = tx.send(Ev::Done(DoneKind::Other));
        return;
    };
    if i.state != "running" {
        send(
            &tx,
            Level::Warn,
            format!(
                "[{old_addr}] relink: {} is {} — linking anyway, it must be running to provision",
                i.id, i.state
            ),
        );
    }
    if i.public_ip.is_empty() {
        send(
            &tx,
            Level::Error,
            format!(
                "[{old_addr}] relink: {} has no public address yet — :pool in a moment, then relink again",
                i.id
            ),
        );
        let _ = tx.send(Ev::Done(DoneKind::Other));
        return;
    }
    let new_addr = i.public_ip.clone();
    if new_addr == old_addr {
        send(
            &tx,
            Level::Info,
            format!("[{old_addr}] relink: already points at the current address — nothing to do"),
        );
        let _ = tx.send(Ev::Done(DoneKind::Other));
        return;
    }
    // The machine, re-born at its new address: same login, same key (the
    // pool's own .pem), same handle. The API's POST replaces the entry only
    // if the address were equal — it is not — so the old entry is dropped
    // first and this is an add.
    let mut m = bm_core::provision::machine_from_instance(i, &cfg);
    m.name = if machine.name.is_empty() {
        old_addr.clone()
    } else {
        machine.name.clone()
    };
    match http
        .delete(format!("{api}/api/machines?addr={}", urlencode(&old_addr)))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            send(
                &tx,
                Level::Error,
                format!("relink: could not drop {old_addr}: HTTP {}", r.status()),
            );
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
        Err(e) => {
            send(
                &tx,
                Level::Error,
                format!("relink: drop {old_addr} failed: {e}"),
            );
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
    }
    match http
        .post(format!("{api}/api/machines"))
        .json(&m)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            send(
                &tx,
                Level::Ok,
                format!(
                    "[{old_addr}] relinked → {new_addr} ({}) — select it and :prov to onboard it",
                    i.id
                ),
            );
        }
        Ok(r) => send(
            &tx,
            Level::Error,
            format!("relink: add {new_addr} rejected: HTTP {}", r.status()),
        ),
        Err(e) => send(
            &tx,
            Level::Error,
            format!("relink: add {new_addr} failed: {e}"),
        ),
    }
    // The Cloud view's linked marks are now stale.
    let _ = tx.send(Ev::Cloud(cloud_snapshot(&layout.root).await));
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_op(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    req: OpRequest,
    layout: bm_core::Layout,
) {
    // Ops can wait on the analyzer for minutes; the shared 15s
    // client would time them out. Polling keeps the short one.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or(http);
    let name = req.op.as_str().to_string();
    let voice = req.voice.clone();
    let op = req.op;
    let key = op_key(&req);
    let character = req.character.clone();
    let mut audio_b64: Option<String> = None;
    let mut line_speaker: Option<String> = None;
    let mut line_text: Option<String> = None;
    let ok = match http.post(format!("{api}/api/op")).json(&req).send().await {
        Ok(r) => match r.json::<bm_proto::OpResult>().await {
            Ok(res) => {
                let level = if res.ok { Level::Ok } else { Level::Error };
                send(&tx, level, format!("{name}: {}", res.message));
                audio_b64 = res.audio_b64;
                line_speaker = res.line_speaker;
                line_text = res.line_text;
                res.ok
            }
            Err(e) => {
                send(&tx, Level::Error, format!("{name}: bad result: {e}"));
                false
            }
        },
        Err(e) => {
            // Swap-voice and remix survive a dead inductor: same mutation
            // against the files, guarded by inductor-down + no-local-workers.
            // Every other op genuinely needs the scheduler.
            if op == Op::SwapVoice {
                match crate::api::offline_swap(
                    &api,
                    &layout,
                    &character.clone().unwrap_or_default(),
                    &voice.clone().unwrap_or_default(),
                )
                .await
                {
                    Ok(msg) => {
                        send(&tx, Level::Ok, format!("{name}: {msg}"));
                        true
                    }
                    Err(msg) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("{name}: {msg} (inductor also unreachable: {e})"),
                        );
                        false
                    }
                }
            } else if op == Op::Remix {
                match crate::api::offline_remix(
                    &api,
                    &layout,
                    req.speed,
                    req.effect_volume,
                    req.music_volume,
                    req.inject_volume,
                )
                .await
                {
                    Ok(msg) => {
                        send(&tx, Level::Ok, format!("{name}: {msg}"));
                        true
                    }
                    Err(msg) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("{name}: {msg} (inductor also unreachable: {e})"),
                        );
                        false
                    }
                }
            } else if op == Op::SoundChanged {
                match crate::api::offline_sound_changed(&api, &layout).await {
                    Ok(msg) => {
                        send(&tx, Level::Ok, format!("{name}: {msg}"));
                        true
                    }
                    Err(msg) => {
                        send(
                            &tx,
                            Level::Error,
                            format!("{name}: {msg} (inductor also unreachable: {e})"),
                        );
                        false
                    }
                }
            } else {
                send(&tx, Level::Error, format!("{name} failed: {e}"));
                false
            }
        }
    };
    let _ = tx.send(Ev::Done(DoneKind::Op {
        op,
        key,
        ok,
        voice,
        audio_b64,
        line_speaker,
        line_text,
    }));
}

pub(crate) async fn job_load_roster(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    layout: bm_core::Layout,
) {
    // Instant first: every piece the picker needs is on this disk, so show
    // it now instead of after an inductor hop plus a sidecar round trip.
    // (That chain cost 35s worst case while the sidecar booted: 15s TUI
    // timeout, then 20s of server-side sidecar timeouts, then the offline
    // build anyway.)
    let disk = layout.clone();
    match tokio::task::spawn_blocking(move || crate::api::local_roster(&disk)).await {
        Ok(roster) => {
            let _ = tx.send(Ev::Roster(Ok(roster)));
        }
        Err(e) => {
            let _ = tx.send(Ev::Roster(Err(format!("local roster failed: {e}"))));
        }
    }
    // ...then upgrade to live when the inductor answers with a sidecar
    // behind it. Anything else keeps the local roster already shown.
    if let Ok(r) = http.get(format!("{api}/api/roster")).send().await {
        if let Ok(roster) = r.json::<Roster>().await {
            if roster.source.starts_with("live") {
                let _ = tx.send(Ev::Roster(Ok(roster)));
            }
        }
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Index every script's lines by speaker, off the UI thread.
///
/// `spawn_blocking` because this is a hundred file opens: cheap warm, but it is
/// I/O, and the UI task is the one thing the TUI is not allowed to stall.
pub(crate) async fn job_load_lines(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
) {
    let res = tokio::task::spawn_blocking(move || crate::tui::audition::index_lines(&layout))
        .await
        .unwrap_or_else(|e| Err(format!("line index task failed: {e}")));
    let _ = tx.send(Ev::Lines(res));
    // `dispatch` counts every job and only `Done` decrements, so a job that
    // reports its payload without one leaves the footer claiming a job is
    // running for the rest of the session — and nothing else ever clears it.
    // Every arm of `run_job` owes exactly one of these.
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Read the sound-design pools and what each entry is used for, off the UI
/// thread. Same `spawn_blocking` reasoning as `job_load_lines`: a hundred file
/// opens, and the UI task is the one thing the TUI may not stall.
pub(crate) async fn job_load_sounds(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
) {
    let res = tokio::task::spawn_blocking(move || crate::tui::sound::load(&layout))
        .await
        .unwrap_or_else(|e| Err(format!("sound design task failed: {e}")));
    let _ = tx.send(Ev::Sounds(res.map(Box::new)));
    // Every arm of `run_job` owes exactly one of these; see `job_load_lines`.
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Serve one already-rendered segment without an inductor: the same lookup
/// `Op::Segment` runs server-side, against this checkout's files. Reports
/// through `DoneKind::Op` with the same shape, so the Done handler — line
/// holding, playback, marker release — cannot tell the two paths apart.
pub(crate) async fn job_segment(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    character: String,
    voice: String,
    text: String,
) {
    let key = op_key(&OpRequest {
        op: Op::Segment,
        ..Default::default()
    });
    let voice_job = voice.clone();
    let out = tokio::task::spawn_blocking(move || {
        let engine = bm_core::config::Settings::load(&layout.settings()).engine;
        let cands = bm_core::assemble::rendered_segments(&layout, &engine, &voice_job);
        if cands.is_empty() {
            let mut msg = bm_core::assemble::segment_miss(&layout, &character, &voice_job, false);
            msg.push_str("; connect (:B) to synthesize instead");
            return Err(msg);
        }
        // An exact line plays that sentence or misses honestly, like the op —
        // with the same fallback to one of hers that did render, so a
        // fresh swap (rendered chapter by chapter) still auditions.
        let want = text.trim();
        if !want.is_empty() {
            match bm_core::assemble::pick_exact(&cands, &character, want) {
                Some(pick) => return serve_local_segment(pick),
                None => match bm_core::assemble::pick_rendered(&cands, &character) {
                    Some(pick) => return serve_local_segment(pick),
                    None => {
                        return Err(format!(
                            "{} (needs :B to render it)",
                            bm_core::assemble::segment_miss(&layout, &character, &voice_job, true)
                        ))
                    }
                },
            }
        }
        let pick = bm_core::assemble::pick_rendered(&cands, &character)
            .expect("a non-empty pool always picks");
        serve_local_segment(pick)
    })
    .await;
    match out {
        Ok(Ok((speaker, text, b64, len))) => {
            send(
                &tx,
                Level::Ok,
                format!(
                    "segment: “{speaker}” ({} KB, local — nothing synthesized)",
                    len / 1024
                ),
            );
            let (line_speaker, line_text) = if text.trim().is_empty() {
                (None, None)
            } else {
                (Some(speaker), Some(text))
            };
            let _ = tx.send(Ev::Done(DoneKind::Op {
                op: Op::Segment,
                key,
                ok: true,
                voice: Some(voice),
                audio_b64: Some(b64),
                line_speaker,
                line_text,
            }));
        }
        Ok(Err(msg)) => fail_segment(&tx, &key, &voice, msg),
        Err(e) => fail_segment(&tx, &key, &voice, format!("segment task crashed: {e}")),
    }
}

/// A picked local segment into the job's answer shape: speaker, text, base64
/// audio and its size for the status line.
fn serve_local_segment(
    pick: &bm_core::assemble::RenderedSegment,
) -> Result<(String, String, String, usize), String> {
    let bytes = pick.read_bytes()?;
    Ok((
        pick.speaker.clone(),
        pick.text.clone(),
        B64.encode(&bytes),
        bytes.len(),
    ))
}

/// Synthesize one line with this checkout's venv: the disconnected form of
/// `Op::PreviewVoice`. Reports through the same `DoneKind::Op`, so playback,
/// markers and the previewed checklist cannot tell it from a render.
pub(crate) async fn job_preview_local(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    voice: String,
    text: String,
) {
    let key = op_key(&OpRequest {
        op: Op::PreviewVoice,
        ..Default::default()
    });
    let voice_done = voice.clone();
    let done = |ok: bool, audio_b64: Option<String>| {
        Ev::Done(DoneKind::Op {
            op: Op::PreviewVoice,
            key: key.clone(),
            ok,
            voice: Some(voice_done.clone()),
            audio_b64,
            line_speaker: None,
            line_text: None,
        })
    };
    let out = tokio::task::spawn_blocking(move || {
        let wav = std::env::temp_dir().join(format!(
            "bm-preview-{}-{}.wav",
            std::process::id(),
            bm_proto::now_secs()
        ));
        (|| {
            bm_core::pool::synth_preview(&layout.root, &voice, &text, &wav)
                .map_err(|e| format!("{e:#}"))?;
            let bytes = std::fs::read(&wav).map_err(|e| format!("reading preview wav: {e}"))?;
            let _ = std::fs::remove_file(&wav);
            Ok::<Vec<u8>, String>(bytes)
        })()
    })
    .await;
    match out {
        Ok(Ok(bytes)) if !bytes.is_empty() => {
            send(
                &tx,
                Level::Ok,
                format!("preview {voice_done} (local): {} KB", bytes.len() / 1024),
            );
            let _ = tx.send(done(true, Some(B64.encode(&bytes))));
        }
        Ok(Ok(_)) => {
            send(
                &tx,
                Level::Error,
                format!("preview {voice_done} (local): no audio rendered"),
            );
            let _ = tx.send(done(false, None));
        }
        Ok(Err(e)) => {
            send(
                &tx,
                Level::Error,
                format!("preview {voice_done} (local): {e}"),
            );
            let _ = tx.send(done(false, None));
        }
        Err(e) => {
            send(
                &tx,
                Level::Error,
                format!("preview {voice_done} (local) task failed: {e}"),
            );
            let _ = tx.send(done(false, None));
        }
    }
}

fn fail_segment(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, key: &str, voice: &str, msg: String) {
    send(tx, Level::Error, format!("segment failed: {msg}"));
    let _ = tx.send(Ev::Done(DoneKind::Op {
        op: Op::Segment,
        key: key.to_string(),
        ok: false,
        voice: Some(voice.to_string()),
        audio_b64: None,
        line_speaker: None,
        line_text: None,
    }));
}

pub(crate) async fn run_jobs(
    job_rx: tokio::sync::mpsc::UnboundedReceiver<Job>,
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
) {
    run_jobs_with(job_rx, tx, run_job).await;
}

pub(crate) async fn run_jobs_with<F, Fut>(
    mut job_rx: tokio::sync::mpsc::UnboundedReceiver<Job>,
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    runner: F,
) where
    F: Fn(Job, tokio::sync::mpsc::UnboundedSender<Ev>) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // A job is queued only behind another job that holds something it needs.
    //
    // `pending` is scanned in arrival order, so the queue is still FIFO among
    // jobs that *do* contend — the change is only that a job with nothing in
    // common with what is running never waits at all. `busy` is the set of
    // resources held right now; `done_rx` is how a running job gives them back.
    let mut pending: VecDeque<(Job, Vec<Res>)> = VecDeque::new();
    let mut busy: BTreeSet<Res> = BTreeSet::new();
    // Held by the scheduler as well, so a `recv` here never returns `None`
    // while jobs are still running — the `None` case is unreachable, and the
    // loop below never has to distinguish "no completions" from "all done".
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<Res>>();
    let mut accepting = true;

    loop {
        // Start everything that can start, in arrival order. Re-scan from the
        // head after each launch: a later job may fit where an earlier one did
        // not, and skipping it would be exactly the unnecessary queueing this
        // exists to remove.
        let mut i = 0;
        while i < pending.len() {
            if pending[i].1.iter().any(|r| busy.contains(r)) {
                i += 1;
                continue;
            }
            let (job, res) = pending.remove(i).expect("index checked above");
            busy.extend(res.iter().cloned());
            let run = runner.clone();
            let tx = tx.clone();
            let done = done_tx.clone();
            tokio::spawn(async move {
                run_one(job, tx, run).await;
                let _ = done.send(res);
            });
        }
        if !accepting && pending.is_empty() && busy.is_empty() {
            return;
        }
        if accepting {
            tokio::select! {
                job = job_rx.recv() => match job {
                    Some(job) => {
                        let res = job.resources();
                        pending.push_back((job, res));
                    }
                    None => accepting = false,
                },
                Some(res) = done_rx.recv() => release(&mut busy, &res),
            }
        } else {
            match done_rx.recv().await {
                Some(res) => release(&mut busy, &res),
                None => return,
            }
        }
    }
}

fn release(busy: &mut BTreeSet<Res>, res: &[Res]) {
    for r in res {
        busy.remove(r);
    }
}

/// Run one job to completion on its own task: announce it, forward its events
/// (tagging log lines as this job's activity), and guarantee exactly one
/// `Done` and one `JobFinished` whatever the job does.
async fn run_one<F, Fut>(job: Job, tx: tokio::sync::mpsc::UnboundedSender<Ev>, runner: F)
where
    F: Fn(Job, tokio::sync::mpsc::UnboundedSender<Ev>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let id = match &job {
        Job::Tracked { id, .. } => Some(*id),
        _ => None,
    };
    let fallback = job.fallback_done();
    if let Some(id) = id {
        let _ = tx.send(Ev::JobStarted(id));
    }
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut task = tokio::spawn(async move { runner(job.into_bare(), job_tx).await });
    let mut done = false;
    let mut forward = |ev: Ev| {
        if matches!(ev, Ev::Done(_)) {
            if done {
                return;
            }
            done = true;
        }
        if let (Some(id), Ev::Log(line)) = (id, &ev) {
            let _ = tx.send(Ev::JobProgress {
                id,
                text: line.text.clone(),
            });
        }
        let _ = tx.send(ev);
    };
    let result = loop {
        tokio::select! {
            result = &mut task => break result,
            Some(ev) = job_rx.recv() => forward(ev),
        }
    };
    job_rx.close();
    while let Some(ev) = job_rx.recv().await {
        forward(ev);
    }
    if result.is_err() {
        send(
            &tx,
            Level::Error,
            "background job crashed — retry the operation".into(),
        );
    }
    if !done {
        let _ = tx.send(Ev::Done(fallback));
    }
    if let Some(id) = id {
        let _ = tx.send(Ev::JobFinished(id));
    }
}

/// Refuse to move what the cluster is reading, or `None` when it is quiet.
///
/// Two locks, the same two the offline voice swap takes: a running inductor
/// owns the ledger and settings the switch would move out from under it, and a
/// live local worker is mid-render against the `assets/` + `prompts/` a profile
/// load would replace. Remote strays are the operator's responsibility — the
/// supported flow is `X` (which sweeps them) and then the switch.
async fn cluster_busy(api: &str) -> Option<String> {
    if crate::backend::inductor_up(api).await {
        return Some(
            "inductor is answering — :X first (it owns the ledger this would move)".into(),
        );
    }
    if crate::backend::local_workers_alive() {
        return Some("local workers still running — :X first, then try again".into());
    }
    None
}

/// The operator's advertised address for this cluster, if they set one.
///
/// From the *workspace's* settings, and read at the point of use because the
/// launcher runs in a blocking task with no access to them. Unset is the normal
/// case: the launcher then asks the routing table which address reaches each
/// box, which is right on a LAN. Set it when the workers are somewhere the
/// routing table cannot describe — anything behind NAT, including a cloud box.
fn advertised_host(layout: &bm_core::Layout) -> Option<String> {
    bm_core::config::Settings::load(&layout.settings())
        .advertised_host()
        .map(str::to_string)
}

/// List, switch or create a workspace.
///
/// The work itself is [`crate::workspace_cmd`], the same function the CLI
/// runs — one implementation, two presentations. Listing needs no lock (it
/// reads a pointer and a directory); switching and creating take both.
pub(crate) async fn job_workspace(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    req: WorkspaceReq,
) {
    // Read before the request is consumed: only a switch moves anything, and
    // only a switch owes the UI a re-resolve.
    let listing = matches!(req, WorkspaceReq::List);
    let run = |cmd: crate::WorkspaceCmd| {
        let root = layout.root.clone();
        async move {
            tokio::task::spawn_blocking(move || crate::workspace_cmd(&root, cmd))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("workspace task failed: {e}")))
        }
    };
    let out = match req {
        WorkspaceReq::List => run(crate::WorkspaceCmd::List).await,
        WorkspaceReq::Use(name) => match cluster_busy(&api).await {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => run(crate::WorkspaceCmd::Use { name }).await,
        },
        WorkspaceReq::New(name) => match cluster_busy(&api).await {
            Some(why) => Err(anyhow::anyhow!("{why}")),
            None => run(crate::WorkspaceCmd::New { name }).await,
        },
    };
    let switched = out.is_ok() && !listing;
    match out {
        Ok(lines) => {
            for l in lines {
                send(&tx, Level::Info, l);
            }
        }
        Err(e) => send(&tx, Level::Error, format!("workspace: {e:#}")),
    }
    // Every arm owes exactly one Done; see `job_load_lines`. A successful
    // switch reports `Relayout` so the dashboard re-resolves before its next
    // frame — a listing changed nothing.
    let _ = tx.send(Ev::Done(if switched {
        DoneKind::Relayout
    } else {
        DoneKind::Other
    }));
}

/// List, load or pack a profile bundle, through `tools/profile.sh`.
///
/// Shell rather than Rust for the same reason ssh and ffmpeg are: tar + zstd +
/// GitHub releases are the tools, and the script is the one place the bundle
/// format lives. Its own output is the report — this streams it line by line
/// instead of re-deriving it.
pub(crate) async fn job_profile(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout: bm_core::Layout,
    api: String,
    req: ProfileReq,
) {
    let root = layout.root.clone();
    let script = root.join("tools/profile.sh");
    let (verb, arg) = match &req {
        ProfileReq::List => ("list", None),
        ProfileReq::Load(name) => ("unpack", Some(name.clone())),
        ProfileReq::Pack(name) => ("pack", Some(name.clone())),
    };
    // A load replaces the live tree; a pack only reads it and writes into
    // profiles/, so it takes no lock.
    let loading = matches!(req, ProfileReq::Load(_));
    if loading {
        if let Some(why) = cluster_busy(&api).await {
            send(&tx, Level::Error, format!("profile: {why}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
    }
    if !script.is_file() {
        send(
            &tx,
            Level::Error,
            format!("no {} — the bundle format lives there", script.display()),
        );
        let _ = tx.send(Ev::Done(DoneKind::Other));
        return;
    }
    send(
        &tx,
        Level::Info,
        match &arg {
            Some(a) => format!("profile {verb} {a}"),
            None => format!("profile {verb}"),
        },
    );
    let out = tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("bash");
        cmd.arg(&script).arg(verb);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        cmd.output()
    })
    .await;
    match out {
        Ok(Ok(o)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let mut any = false;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                send(&tx, Level::Info, line.to_string());
                any = true;
            }
            if !any {
                send(&tx, Level::Warn, "profile.sh said nothing".into());
            }
            if !o.status.success() {
                send(
                    &tx,
                    Level::Error,
                    format!("profile.sh {verb} exited {}", o.status.code().unwrap_or(-1)),
                );
            }
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("profile.sh failed to run: {e}")),
        Err(e) => send(&tx, Level::Error, format!("profile task failed: {e}")),
    }
    // A load changes which profile the live tree claims to be; the UI re-reads
    // the pointer when it lands. Listing and packing change nothing.
    let _ = tx.send(Ev::Done(if loading {
        DoneKind::Relayout
    } else {
        DoneKind::Other
    }));
}

/// One account listing, off the UI thread. Errors are returned as strings so
/// the Cloud view can render the reason instead of an empty account — which is
/// the one wrong answer that costs money.
async fn cloud_snapshot(
    root: &std::path::Path,
) -> Result<Vec<bm_core::provision::AwsInstance>, String> {
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || crate::aws_ops::pool(&root)).await {
        Ok(Ok((_cfg, instances))) => Ok(instances),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("aws pool task failed: {e}")),
    }
}

pub(crate) async fn job_aws_pool(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    api: String,
    http: reqwest::Client,
) {
    let res = cloud_snapshot(&root).await;
    match &res {
        Ok(instances) if instances.is_empty() => send(
            &tx,
            Level::Info,
            "cloud: no boxes running (nothing carries the marker tag)".into(),
        ),
        Ok(instances) => send(
            &tx,
            Level::Info,
            format!("cloud: {} box(es)", instances.len()),
        ),
        Err(e) => send(&tx, Level::Error, format!("aws pool: {e}")),
    }
    let readable = res.is_ok();
    let _ = tx.send(Ev::Cloud(res));
    // Auto-relink: after a fresh account read, ask the inductor to reconcile
    // the registry with the addresses the account carries now. Best-effort —
    // a down inductor (or an offline TUI) simply skips it, and the repair lines
    // land in the events pane so the drift is never silent.
    if readable {
        let url = format!("{}/api/relink", api.trim_end_matches('/'));
        if let Ok(r) = http.post(&url).send().await {
            if r.status().is_success() {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    if let Some(lines) = v.get("lines").and_then(|l| l.as_array()) {
                        for l in lines.iter().filter_map(|x| x.as_str()) {
                            send(&tx, Level::Info, l.to_string());
                        }
                    }
                }
            }
        }
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Store the IAM user's key from the console's CSV. `aws_ops::login` is the
/// same verify-then-write path the CLI runs, so the two cannot disagree about
/// what a root key or an assumed role means.
///
/// `csv` is the only input: the secret is read from a hidden stdin by the CLI
/// and must never be typed where it would be echoed, so the dashboard offers
/// the download the console already made.
pub(crate) async fn job_aws_login(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    csv: std::path::PathBuf,
) {
    let out =
        tokio::task::spawn_blocking(move || crate::aws_ops::login(&root, Some(csv), None, None))
            .await;
    match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Info, l);
            }
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("aws login: {e:#}")),
        Err(e) => send(&tx, Level::Error, format!("aws login task failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Read the account into the pool definition. Several read-only AWS calls, so
/// it runs off the UI thread and takes the lifecycle lane with `up`/`down`.
pub(crate) async fn job_aws_discover(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    args: crate::aws_ops::DiscoverArgs,
) {
    let root_for_pool = root.clone();
    let out = tokio::task::spawn_blocking(move || crate::aws_ops::discover(&root, args)).await;
    match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Info, l);
            }
            // The pool definition just changed, so the Cloud view's header is
            // stale — refresh it in the same job that wrote the file.
            let _ = tx.send(Ev::Cloud(cloud_snapshot(&root_for_pool).await));
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("aws discover: {e:#}")),
        Err(e) => send(&tx, Level::Error, format!("aws discover task failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_aws_up(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    api: String,
    http: reqwest::Client,
    count: u32,
) {
    let up_root = root.clone();
    let out = tokio::task::spawn_blocking(move || crate::aws_ops::launch(&up_root, count)).await;
    let (cfg, launched) = match out {
        Ok(Ok((cfg, lines, launched))) => {
            for l in lines {
                send(&tx, Level::Info, l);
            }
            (cfg, launched)
        }
        Ok(Err(e)) => {
            send(&tx, Level::Error, format!("aws up: {e:#}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
        Err(e) => {
            send(&tx, Level::Error, format!("aws up task failed: {e}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
    };
    // The join, made at birth: every returned instance becomes a registry entry,
    // carrying the pool's own key and login. A box the reply gave no address for
    // is registered too, keyed by its instance id — see `machine_from_instance`.
    // It used to be skipped with "has no address yet, :add it later", which is
    // the *normal* case (an address is assigned asynchronously) and left the
    // dashboard with nothing to show, nothing to repair, and no record that the
    // box existed at all.
    let mut linked = 0usize;
    let mut waiting = 0usize;
    for i in &launched {
        let m = bm_core::provision::machine_from_instance(i, &cfg);
        let newborn = m.state == MachineState::AwaitingIp;
        let url = format!("{}/api/machines", api.trim_end_matches('/'));
        match http.post(&url).json(&m).send().await {
            Ok(r) if r.status().is_success() => {
                linked += 1;
                if newborn {
                    waiting += 1;
                    send(
                        &tx,
                        Level::Info,
                        format!(
                            "tracking {} — no address yet; it will be relinked and onboarded by itself",
                            i.id
                        ),
                    );
                } else {
                    send(&tx, Level::Ok, format!("linked {} → {}", i.id, m.addr));
                }
            }
            Ok(r) => send(
                &tx,
                Level::Warn,
                format!(
                    "launched {} but the registry refused it: HTTP {} — start the inductor (:B), then `:add {}`",
                    i.id,
                    r.status(),
                    m.addr
                ),
            ),
            Err(e) => send(
                &tx,
                Level::Warn,
                format!(
                    "launched {} but linking failed ({e}) — `:add {}` once the inductor is up",
                    i.id, m.addr
                ),
            ),
        }
    }
    if linked > 0 {
        // The advice is not "provision them" any more. A box that was tracked
        // without an address gets its address from the account watch, is relinked
        // to it, and is provisioned on arrival; saying `:prov each one` would
        // describe a chore the inductor now does. Only a box the reply *did*
        // address needs the operator, and only because nothing is going to
        // onboard it behind their back.
        let msg = if waiting > 0 && waiting == linked {
            format!("{linked} box(es) tracked — addresses arrive in a moment, then they onboard themselves")
        } else {
            format!("{linked} box(es) linked — the ones without an address onboard themselves; :prov the rest")
        };
        send(&tx, Level::Ok, msg);
    }
    // The launch changed the account, so the Cloud view is now stale.
    let _ = tx.send(Ev::Cloud(cloud_snapshot(&root).await));
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_aws_down(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    ids: Vec<String>,
) {
    let down_root = root.clone();
    let out =
        tokio::task::spawn_blocking(move || crate::aws_ops::terminate(&down_root, &ids)).await;
    match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Ok, l);
            }
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("aws down: {e:#}")),
        Err(e) => send(&tx, Level::Error, format!("aws down task failed: {e}")),
    }
    let _ = tx.send(Ev::Cloud(cloud_snapshot(&root).await));
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn run_job(job: Job, tx: tokio::sync::mpsc::UnboundedSender<Ev>) {
    match job.into_bare() {
        Job::Tracked { .. } => unreachable!(),
        Job::Provision {
            layout,
            api,
            machine,
            force,
            settings_key,
            cancel,
        } => job_provision(tx, layout, api, machine, force, settings_key, cancel).await,
        Job::AddMachine { api, http, m } => job_add_machine(tx, api, http, m).await,
        Job::AwsPool { root, api, http } => job_aws_pool(tx, root, api, http).await,
        Job::AwsLogin { root, csv } => job_aws_login(tx, root, csv).await,
        Job::AwsDiscover { root, args } => job_aws_discover(tx, root, args).await,
        Job::AwsUp {
            root,
            api,
            http,
            count,
        } => job_aws_up(tx, root, api, http, count).await,
        Job::AwsDown { root, ids } => job_aws_down(tx, root, ids).await,
        Job::AddSample {
            layout,
            path,
            name,
            tags,
        } => job_add_sample(tx, layout, path, name, tags).await,
        Job::StartBackend {
            layout,
            api,
            api_up,
            start,
            count,
            enqueue,
            machines,
            cancel,
            settings_key,
        } => {
            job_start_backend(
                tx,
                layout,
                api,
                api_up,
                start,
                count,
                enqueue,
                machines,
                cancel,
                settings_key,
            )
            .await
        }
        Job::StopBackend {
            layout,
            machines,
            api,
            settings_key,
        } => job_stop_backend(tx, layout, machines, api, settings_key).await,
        Job::DropMachine { api, http, addr } => job_drop_machine(tx, api, http, addr).await,
        Job::SaveTaskPolicy {
            api,
            http,
            addr,
            task_policy,
        } => job_save_task_policy(tx, api, http, addr, task_policy).await,
        Job::ManualDigest {
            api,
            http,
            chapter,
            script,
            delta,
        } => job_manual_digest(tx, api, http, chapter, script, delta).await,
        Job::DigestPolicy {
            api,
            http,
            layout,
            machines,
            restore,
        } => job_digest_policy(tx, api, http, layout, machines, restore).await,
        Job::RelinkMachine {
            layout,
            api,
            http,
            machine,
        } => job_relink_machine(tx, layout, api, http, machine).await,
        Job::Op {
            api,
            http,
            req,
            layout,
        } => job_op(tx, api, http, req, layout).await,
        Job::LoadRoster { api, http, layout } => job_load_roster(tx, api, http, layout).await,
        Job::LoadLines { layout } => job_load_lines(tx, layout).await,
        Job::LoadSounds { layout } => job_load_sounds(tx, layout).await,
        Job::Segment {
            layout,
            character,
            voice,
            text,
        } => job_segment(tx, layout, character, voice, text).await,
        Job::PreviewLocal {
            layout,
            voice,
            text,
        } => job_preview_local(tx, layout, voice, text).await,
        Job::Workspace { layout, api, req } => job_workspace(tx, layout, api, req).await,
        Job::Profile { layout, api, req } => job_profile(tx, layout, api, req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provision_stop_names_the_cause_the_run_carried() {
        // The pre-flight failure that started all this: the log line is right
        // there, and it contains none of the words the old scan looked for, so
        // the pane said "provision INCOMPLETE" and told the operator to retry
        // the same click. The run now carries the reason, and the pane shows
        // it whatever the log happens to say.
        let why = "no TTS sidecar binary for linux/x86_64 at /repo/rust/target/x86_64-unknown-linux-gnu/release/bm-tts (linux/x86_64: `make tts`)";
        let lines = vec![
            "[10.0.0.1] profile: xianxia (6b8d5fc00761)".to_string(),
            format!("[10.0.0.1] {why}"),
        ];
        assert_eq!(provision_stop_reason(Some(why), &lines), why);
        // The scan alone still cannot see it — the honest reason the field
        // exists, pinned so nobody deletes the field and calls it a cleanup.
        assert_eq!(provision_stop_reason(None, &lines), "provision INCOMPLETE");
    }

    #[test]
    fn a_carried_reason_is_used_verbatim_and_keeps_its_own_words() {
        let why = "no local profile loaded — load one first (`:profile` in the dashboard)";
        assert_eq!(
            provision_stop_reason(Some(why), &["[10.0.0.1] unrelated".to_string()]),
            why
        );
    }

    #[test]
    fn the_log_scan_still_finds_the_inner_cause_and_drops_the_address() {
        let lines = vec![
            "[10.0.0.1] agent install failed: rsync push failed".to_string(),
            "[10.0.0.1] rsync: command not found".to_string(),
        ];
        assert_eq!(
            provision_stop_reason(None, &lines),
            "rsync: command not found",
            "the root cause, not the wrapper, and without the address the pane already shows"
        );
    }

    #[test]
    fn a_failure_with_nothing_to_say_says_only_that_it_is_incomplete() {
        // The last resort, and the string this whole change exists to avoid.
        let lines = vec!["[10.0.0.1] starting worker".to_string()];
        assert_eq!(provision_stop_reason(None, &lines), "provision INCOMPLETE");
        // An empty carried reason is not a reason.
        assert_eq!(
            provision_stop_reason(Some("   "), &lines),
            "provision INCOMPLETE"
        );
    }
}
