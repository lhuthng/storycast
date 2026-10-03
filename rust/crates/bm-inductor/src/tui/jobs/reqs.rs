use super::*;

/// What `:workspace` was asked to do. Parsed at submit, so the prompt can
#[derive(Debug)]
pub(crate) enum WorkspaceReq {
    List,
    Use(String),
    New {
        name: String,
        profile: Option<String>,
        /// Built by the guided create flow: the crawler to seed the book with.
        crawler: Option<bm_core::preset::CrawlerSetup>,
    },
}

/// What `:profile` was asked to do.
#[derive(Debug)]
pub(crate) enum ProfileReq {
    List,
    /// `unpack`, replaces the live `assets/` + `prompts/` trees.
    Load(String),
    /// Bundle the live tree into `profiles/<name>.tar.zst`.
    Pack(String),
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // boxed at every send site (`Job::Tracked`); the variants are construction-time only
pub(crate) enum Job {
    Tracked {
        id: u64,
        job: Box<Job>,
    },
    /// Long SSH/rsync flow for exactly one box, executed off the UI task.
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
    StopBackend {
        layout: bm_core::Layout,
        machines: Vec<Machine>,
        api: String,
        settings_key: Option<String>,
    },
    /// Local file work: copy a clip into `refs/`, tag it from its filename,
    AddSample {
        layout: bm_core::Layout,
        path: String,
        name: Option<String>,
        tags: Option<Vec<String>>,
    },
    /// Deregister a machine. Idempotent, so it needs no confirmation beyond
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
    LoadLines {
        layout: bm_core::Layout,
    },
    /// Read the three sound-design registries, the scene map and every script,
    LoadSounds {
        layout: bm_core::Layout,
    },
    /// Serve one already-rendered segment from the local checkout: the
    Segment {
        layout: bm_core::Layout,
        character: String,
        voice: String,
        /// Exact sentence wanted (the shown line). Empty means triage.
        text: String,
    },
    /// Synthesize one line on this machine: the disconnected form of
    PreviewLocal {
        layout: bm_core::Layout,
        voice: String,
        text: String,
    },
    /// List, switch or create a workspace. Switching moves the pointer the
    Workspace {
        layout: bm_core::Layout,
        api: String,
        req: WorkspaceReq,
    },
    /// List, load or pack a profile bundle. `tools/profile.sh` does the tar +
    Profile {
        layout: bm_core::Layout,
        api: String,
        req: ProfileReq,
    },
    /// What the account holds: one read-only `describe-instances`, handed to
    AwsPool {
        root: std::path::PathBuf,
        api: String,
        http: reqwest::Client,
    },
    /// Save one machine's work policy: which stages it may run and in what
    SaveTaskPolicy {
        api: String,
        http: reqwest::Client,
        addr: String,
        task_policy: Vec<bm_proto::TaskPref>,
    },
    /// Park a machine, or wake it up: one bool of operator intent.
    SetAccepting {
        api: String,
        http: reqwest::Client,
        addr: String,
        accepting_work: bool,
    },
    /// Set one box's TTS sidecar thread count (`:threads`).
    SetTtsThreads {
        api: String,
        http: reqwest::Client,
        addr: String,
        threads: Option<u16>,
    },
    /// Report a digest the operator performed by hand.
    ManualDigest {
        api: String,
        http: reqwest::Client,
        chapter: u32,
        script: serde_json::Value,
        delta: serde_json::Value,
    },
    /// Turn digest work off, or back on, across every machine.
    DigestPolicy {
        api: String,
        http: reqwest::Client,
        layout: bm_core::Layout,
        /// `(addr, that machine's stored policy)`; `None` is "no policy", which
        machines: Vec<(String, Option<Vec<bm_proto::TaskPref>>)>,
        /// `true` puts the snapshot back; `false` takes one and disables digest.
        restore: bool,
    },
    /// Re-point a box whose EC2 public IP drifted (stop/start, spot relaunch)
    RelinkMachine {
        layout: bm_core::Layout,
        api: String,
        http: reqwest::Client,
        /// The registry record as the operator selected it, its note carries
        machine: Machine,
    },
    /// Store the app's IAM user from the console's CSV, off the UI thread.
    AwsLogin {
        root: std::path::PathBuf,
        csv: std::path::PathBuf,
    },
    /// Read the account and write the pool definition: AMI, subnet, security
    AwsDiscover {
        root: std::path::PathBuf,
        args: crate::aws_ops::DiscoverArgs,
    },
    /// Launch boxes, stream the lines, then **link what came back** into the
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
    /// List what one LLM provider serves (`GET {base}/models`), off the UI
    LlmModels {
        provider: String,
        kind: String,
        base_url: String,
        key: String,
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
            Job::SetAccepting {
                accepting_work: false,
                ..
            } => "park machine",
            Job::SetAccepting { .. } => "wake machine",
            Job::SetTtsThreads { .. } => "set tts threads",
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
                WorkspaceReq::New { .. } => "create workspace",
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
            Job::LlmModels { provider, .. } => return format!("fetch {provider} models"),
            Job::Tracked { .. } => unreachable!(),
        }
        .to_string()
    }

    /// What this job needs to itself for its whole life.
    pub(crate) fn resources(&self) -> Vec<Res> {
        match self.bare() {
            // One cluster, one lifecycle: `B` and `X` must never interleave,
            Job::StartBackend { .. } | Job::StopBackend { .. } => vec![Res::Cluster],
            // Per box, not per fleet: two boxes provision independently, and
            Job::Provision { machine, .. } => vec![Res::Box(machine.addr.clone())],
            // These four read-modify-write `.bm/aws/` and the account it
            Job::AwsUp { .. }
            | Job::AwsDown { .. }
            | Job::AwsLogin { .. }
            | Job::AwsDiscover { .. } => vec![Res::Aws],
            // Read-only indexes: a roster GET, a hundred file opens. They
            Job::LoadRoster { .. } | Job::LoadLines { .. } | Job::LoadSounds { .. } => vec![],
            // Read-only provider read: never queued behind anything.
            Job::LlmModels { .. } => vec![],
            _ => vec![Res::Command],
        }
    }

    /// The one thing worth naming on the jobs screen, or `None`.
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
        key: String,
        ok: bool,
        voice: Option<String>,
        /// The wav the op rendered, base64, if it rendered one. The TUI writes
        audio_b64: Option<String>,
        /// A book line served audio speaks (segment audition): whose line and
        line_speaker: Option<String>,
        line_text: Option<String>,
    },
    /// Pool changed under the roster: reload it (only if one is showing).
    ReloadRoster,
    /// A backend start sequence finished (backend up, catch-up done or
    StartDone,
    /// A workspace switch or profile load finished: the active workspace,
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
    /// The inductor's answer to a manual digest report, the line `complete`
    ManualDigest(Result<String, String>),
    /// The answer to a cluster-wide digest-policy change: what happened, or why
    DigestPolicy(Result<String, String>),
    /// The models one provider serves, or why the list never arrived. The
    LlmModels {
        provider: String,
        result: Result<Vec<String>, String>,
    },
    /// A `/api/state` snapshot from the background poller. Carrying the payload
    State(Result<serde_json::Value, String>),
    /// The backend a `B` job started is up enough to take work: enqueue this.
    BackendLive {
        start: u32,
        count: u32,
    },
    /// The boxes a `B` start still has to catch up, and the flag that can stop
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
    Sounds(Result<Box<crate::tui::sound::SoundData>, String>),
    /// A fresh account listing, or the reason it could not be read. The Cloud
    Cloud(Result<Vec<bm_core::provision::AwsInstance>, String>),
}

/// Bounded wait for a freshly spawned inductor to answer `/api/state`.
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
pub(crate) fn unreachable_verdict(api: &str, refused: bool, detail: &str) -> String {
    if refused {
        format!("inductor is down at {api} — :B to start it")
    } else {
        format!("inductor unreachable at {api}: {detail}")
    }
}

/// Fetch `/api/state` once.
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
pub(crate) fn send(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, level: Level, text: String) {
    let _ = tx.send(Ev::Log(LogLine {
        level,
        wall: bm_proto::now_secs(),
        text,
    }));
}

/// The state a failed provision leaves behind.
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
