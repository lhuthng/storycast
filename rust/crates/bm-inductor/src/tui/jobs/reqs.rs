use super::*;

/// What `:workspace` was asked to do. Parsed at submit, so the prompt can
/// refuse nonsense before a job is queued.
#[derive(Debug)]
pub(crate) enum WorkspaceReq {
    List,
    Use(String),
    New {
        name: String,
        profile: Option<String>,
        /// Built by the guided create flow: the crawler to seed the book with.
        /// `None` falls back to the preset's own `crawler`.
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
    /// Touches nothing else: no veto, no restart, no other worker.
    /// `settings_key` is the app-wide default the ssh chain falls back to
    /// when the machine carries no key of its own.
    ///
    /// `cancel` is the `B` start's flag when the catch-up dispatched this, and
    /// `None` for a `:prov` the operator asked for by hand. It gates the *last*
    /// step, the worker launch, so an `X` that lands while a push is in
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
    /// boxes that still need work are handed out **as their own jobs**, a
    /// failing box lands in Error, never vetoes the rest. `cancel` lets `X`
    /// stop the catch-up between boxes.
    ///
    /// The catch-up is *not* run here: a job called "start backend" used to
    /// hold the cluster for as long as provisioning every machine took. It now
    /// finishes as soon as the inductor answers and emits [`Ev::CatchUp`]; the
    /// dashboard turns that into one `Provision` per box, each visible, each
    /// cancellable, all parallel.
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
    /// quiet afterwards, not just this box.
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
    /// it runs once per session, the result is cached, so the audition itself is
    /// instant.
    LoadLines {
        layout: bm_core::Layout,
    },
    /// Read the three sound-design registries, the scene map and every script,
    /// and work out what each pooled sound is still used for.
    ///
    /// A job for the same reason as `LoadLines`, it is a hundred file opens
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
    /// voice auditions with no worker on, at the cost of loading the
    /// model here, which is why the connected path stays first.
    PreviewLocal {
        layout: bm_core::Layout,
        voice: String,
        text: String,
    },
    /// List, switch or create a workspace. Switching moves the pointer the
    /// ledger, settings and data hang off, so it is refused while anything
    /// reads them, see `cluster_busy`.
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
    /// order. Small and quiet, the editor sends one after every change so the
    /// panel never holds an unsaved decision.
    SaveTaskPolicy {
        api: String,
        http: reqwest::Client,
        addr: String,
        task_policy: Vec<bm_proto::TaskPref>,
    },
    /// Park a machine, or wake it up: one bool of operator intent.
    ///
    /// Idempotent, not a toggle, and that is deliberate. The *dashboard* is what
    /// toggles, from the state it is showing; the request carries the value it
    /// wants. A toggle would make a retry after a failed `POST`, or a double
    /// keypress, land back where it started, which is the one outcome nobody
    /// can see.
    SetAccepting {
        api: String,
        http: reqwest::Client,
        addr: String,
        accepting_work: bool,
    },
    /// Set one box's TTS sidecar thread count (`:threads`).
    ///
    /// Config, like `task_policy`: written to `machines.json` and pushed to the
    /// worker by the dispatcher's convergent sidecar-policy channel, so a box
    /// that is down at edit time still converges when it comes back. `threads:
    /// None` clears the override, restoring the sidecar's own default.
    SetTtsThreads {
        api: String,
        http: reqwest::Client,
        addr: String,
        threads: Option<u16>,
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
    /// inductor may not share a filesystem with the dashboard, and because the
    /// script it holds is what every downstream machine is handed.
    ManualDigest {
        api: String,
        http: reqwest::Client,
        chapter: u32,
        script: serde_json::Value,
        delta: serde_json::Value,
    },
    /// Turn digest work off, or back on, across every machine.
    ///
    /// **Off snapshots each machine's whole policy, not just the digest flag**,
    /// because "on" has to mean *what that box had*, not *digest enabled*: a box
    /// whose digest was already off must stay off, and a box with no policy at
    /// all must get `None` back rather than a list it never had. That distinction
    /// is the entire reason this is a snapshot rather than a toggle.
    ///
    /// The snapshot is a **file**, so an inductor restart in between cannot
    /// silently turn digest work back on with no way to restore it, which is the
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
    /// at the address it carries *now*. The instance id, stable for the box's
    /// whole life, comes from the machine's note; the account read supplies
    /// the current address and the registry swap happens in the API, which
    /// also keeps the live inductor's map consistent.
    RelinkMachine {
        layout: bm_core::Layout,
        api: String,
        http: reqwest::Client,
        /// The registry record as the operator selected it, its note carries
        /// the EC2 instance id to match on, its name the handle to keep.
        machine: Machine,
    },
    /// Store the app's IAM user from the console's CSV, off the UI thread.
    ///
    /// The secret is never typed in the dashboard, so the CSV is the only
    /// route offered, it carries both halves and the verify-then-write order
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
    /// registry, the join is made in the same job that reads the launch reply,
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
    /// List what one LLM provider serves (`GET {base}/models`), off the UI
    /// task. Read-only: the answer lands in `Ev::LlmModels` and the `L`
    /// screen offers it as a pick, so a model name is never typed blind.
    /// `kind` is the backend slot, so the request shape never depends on
    /// the provider's name.
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
    ///
    /// The scheduler starts a job the moment nothing else holds any of these,
    /// so **two jobs with nothing in common run at once**. That is the whole
    /// difference from the boolean "lane" this replaced: `aws discover` used to
    /// queue behind a five-minute box provision because both were filed under
    /// "lifecycle", and a provision of box A queued behind one of box B.
    ///
    /// Only name a resource where there is a real conflict. A job that names
    /// nothing conflicting is `Command`, the old command lane, which stays
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
            // start on the next scan, never queued behind a five-minute
            // provision or a slow op. Callers already guard against
            // dispatching them twice (`lines_loading`, `roster_loading`).
            Job::LoadRoster { .. } | Job::LoadLines { .. } | Job::LoadSounds { .. } => vec![],
            // Read-only provider read: never queued behind anything.
            Job::LlmModels { .. } => vec![],
            _ => vec![Res::Command],
        }
    }

    /// The one thing worth naming on the jobs screen, or `None`.
    ///
    /// `Res::Command` is the default lane, true of most jobs and worth
    /// nothing on a row, so it is filtered out. A job that shows a resource
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
    /// The inductor's answer to a manual digest report, the line `complete`
    /// returned, or why it never arrived.
    ///
    /// Carried back rather than assumed: the report can be refused (`unknown
    /// task`, or the row moving under it), and the operator is looking at a
    /// screen that said "reporting it", so the screen has to be able to say what
    /// happened, including "no".
    ManualDigest(Result<String, String>),
    /// The answer to a cluster-wide digest-policy change: what happened, or why
    /// not. Shown either way, because "digest is off" is a claim the operator
    /// will act on.
    DigestPolicy(Result<String, String>),
    /// The models one provider serves, or why the list never arrived. The
    /// `L` screen offers the list as a pick when it is for the highlighted
    /// provider; a stale answer (cursor moved on) is dropped there, not here.
    LlmModels {
        provider: String,
        result: Result<Vec<String>, String>,
    },
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
    /// it. The job does not provision them itself, it hands them to the
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
    /// Boxed because this is the only large variant, `SoundData` is ~512 bytes
    /// against a 144-byte runner-up, and the channel is *unbounded*, so every
    /// message pays for the largest variant. `JobStarted(u64)` is eight bytes of
    /// payload allocating a 512-byte node. A reload is rare and one allocation
    /// is nothing; a progress tick is neither.
    Sounds(Result<Box<crate::tui::sound::SoundData>, String>),
    /// A fresh account listing, or the reason it could not be read. The Cloud
    /// view renders it and marks rows absent from the registry.
    Cloud(Result<Vec<bm_core::provision::AwsInstance>, String>),
}

/// Bounded wait for a freshly spawned inductor to answer `/api/state`.
/// True the moment it answers, false after `secs`, the caller reports and
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
    // ledger, a box's state belongs to the book being run.
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
/// **Pure on purpose.** The only input that matters is whether the failure was
/// a *refusal* (`reqwest::Error::is_connect`), and this is the one place that
/// decides; the alternative was testing the wording against an ephemeral port
/// race. `detail` is deliberately dropped on the refusal branch: a refused
/// connection is the normal cold start, so reqwest's prose for it is noise
/// exactly where the remedy (`:B`) belongs. On any other failure the detail
/// is kept, a timeout or a reset may be a *sick* inductor rather than an
/// absent one, and telling those apart is why there are two branches.
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
/// state, the whole point of the poller is that the drawing loop never waits
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
pub(crate) fn send(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, level: Level, text: String) {
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
/// is the same misreading the `initializing` state exists to prevent, and
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
