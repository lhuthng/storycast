//! Background work: one `Job` at a time, off the drawing loop.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use bm_proto::{Machine, MachineState, Op, OpRequest, Roster};
use crate::tui::{app::App, input::{op_key, urlencode}, style::{Level, LogLine}};

#[derive(Debug)]
pub(crate) enum Job {
    /// Long SSH/rsync flow for exactly one box, executed off the UI task.
    /// Touches nothing else: no veto, no restart, no other worker.
    /// `settings_key` is the app-wide default the ssh chain falls back to
    /// when the machine carries no key of its own.
    Provision {
        layout_root: std::path::PathBuf,
        api: String,
        machine: Machine,
        force: bool,
        settings_key: Option<String>,
    },
    AddMachine {
        api: String,
        http: reqwest::Client,
        m: Machine,
    },
    /// Start the local backend, then run a range on it once live.
    /// `enqueue` is false for bare `B` (backend only) and true for the run
    /// screen's Enter (backend + job). `machines` is the registry snapshot at
    /// submit. Degraded start: the backend goes up first (seconds), then each
    /// box provisions in the background and joins as it becomes ready — a
    /// failing box lands in Error, never vetoes the rest. `cancel` lets `X`
    /// stop the catch-up loop between boxes.
    StartBackend {
        layout_root: std::path::PathBuf,
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
        layout_root: std::path::PathBuf,
        machines: Vec<Machine>,
        api: String,
        settings_key: Option<String>,
    },
    /// Local file work: copy a clip into `refs/`, tag it from its filename,
    /// register it in the pool and in `voices.json`. Needs no inductor.
    AddSample {
        layout_root: std::path::PathBuf,
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
        layout_root: std::path::PathBuf,
    },
    LoadRoster {
        api: String,
        http: reqwest::Client,
        layout_root: std::path::PathBuf,
    },
    /// Read every `data/script-*.json` and index the lines by speaker.
    ///
    /// A job rather than a keypress handler because it is a hundred file opens
    /// (26 ms warm here, but unbounded on a cold or networked path) and because
    /// it runs once per session — the result is cached, so the audition itself is
    /// instant.
    LoadLines {
        layout_root: std::path::PathBuf,
    },
    /// Serve one already-rendered segment from the local checkout: the
    /// disconnected form of `Op::Segment`. Same lookup the inductor runs,
    /// against the TUI's own files, so listening needs no backend.
    Segment {
        layout_root: std::path::PathBuf,
        character: String,
        voice: String,
        /// Exact sentence wanted (the shown line). Empty means triage.
        text: String,
    },
}

#[derive(Debug)]
pub(crate) enum DoneKind {
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
    Other,
}

pub(crate) enum Ev {
    Log(LogLine),
    Roster(Result<Roster, String>),
    Done(DoneKind),
    /// A `/api/state` snapshot from the background poller. Carrying the payload
    /// (not the parsed structs) keeps the parse on the UI task, where the
    /// ordering/sort fixes already live.
    State(Result<serde_json::Value, String>),
    /// The backend a `B` job started is up enough to take work: enqueue this.
    BackendLive { start: u32, count: u32 },
    /// Push a machine's state directly into the TUI's in-memory list.
    MachineUpdate { addr: String, state: MachineState, note: String },
    /// The per-speaker line index, built off the UI thread.
    Lines(Result<std::collections::HashMap<String, Vec<String>>, String>),
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
    layout_root: &std::path::Path,
    addr: &str,
    state: MachineState,
    note: &str,
) {
    let body = serde_json::json!({"addr": addr, "state": state.as_str(), "note": note});
    let url = format!("{}/api/machines/state", api.trim_end_matches('/'));
    if let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
        let _ = client.post(&url).json(&body).send().await;
    }
    // Always write the ledger file as well: when the inductor is up the
    // TUI refreshes from the API, but the ledger is the only source
    // before the API starts or if the POST fails.
    let path = layout_root.join(".bm/ledger.json");
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
        layout_root: app.layout_root.clone(),
    }
}

/// Fetch `/api/state` once.
///
/// Free-standing so the background poller can use it without holding the UI
/// state — the whole point of the poller is that the drawing loop never waits
/// on this call.
pub(crate) async fn fetch_state(http: &reqwest::Client, api: &str) -> Result<serde_json::Value, String> {
    let url = format!("{}/api/state", api.trim_end_matches('/'));
    match http.get(&url).send().await {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("bad state payload: {e}")),
        Err(e) => Err(format!("inductor unreachable at {api}: {e}")),
    }
}

/// Report one line to the event pane.
fn send(tx: &tokio::sync::mpsc::UnboundedSender<Ev>, level: Level, text: String) {
    let _ = tx.send(Ev::Log(LogLine { level, wall: bm_proto::now_secs(), text }));
}

pub(crate) async fn job_provision(tx: tokio::sync::mpsc::UnboundedSender<Ev>, layout_root: std::path::PathBuf, api: String, machine: Machine, force: bool, settings_key: Option<String>) {
            let addr = machine.addr.clone();
            let mut again = machine.clone();
            // The app-wide default fills a keyless box; a box key always wins.
            let key = bm_core::provision::resolve_key(machine.ssh_key.as_deref(), settings_key.as_deref())
                .0
                .map(|p| p.to_string_lossy().to_string());
            // The relaunched worker must ssh the same way the provision did.
            again.ssh_key = key.clone();
            let send_update = |tx: &tokio::sync::mpsc::UnboundedSender<Ev>, state: MachineState, note: &str| {
                let _ = tx.send(Ev::MachineUpdate {
                    addr: addr.clone(),
                    state,
                    note: note.to_string(),
                });
            };
            send_update(&tx, MachineState::Provisioning, if force { "force re-provision (p)" } else { "provisioning (p)" });
            set_machine_state(
                &api,
                &layout_root,
                &addr,
                MachineState::Provisioning,
                if force { "force re-provision (p)" } else { "provisioning (p)" },
            )
            .await;
            let layout = bm_core::Layout::new(&layout_root);
            let out = tokio::task::spawn_blocking(move || {
                crate::provision_machine(
                    &layout,
                    &machine.addr,
                    &machine.ssh_user,
                    machine.ssh_port,
                    key,
                    force,
                )
            })
            .await;
            match out {
                Ok((ready, lines)) => {
                    // Extract the most actionable line from the provision log:
                    // prefer the inner root cause (e.g. "rsync: command not
                    // found") over the outer wrapper ("agent install failed").
                    let fail_reason = lines
                        .iter()
                        .rev()
                        .find(|l| l.contains("missing") || l.contains("not found"))
                        .or_else(|| lines.iter().rev().find(|l| l.contains("failed")))
                        .map(|l| {
                            // Strip the "[addr] " prefix if present.
                            let raw = if let Some(rest) = l.strip_prefix('[') {
                                rest.find(']').map_or(l.as_str(), |i| &rest[i + 2..])
                            } else {
                                l.as_str()
                            };
                            bm_core::util::head_chars(raw, 120)
                        })
                        .unwrap_or_default();
                    for l in lines {
                        send(&tx, Level::Info, l);
                    }
                    if ready {
                        send_update(&tx, MachineState::Provisioning, "worker launched — Online on its first beat");
                        send(&tx, Level::Ok, format!("[{addr}] provision complete — starting its worker"));
                        // Worker half only, never the inductor: a `p` retry
                        // finishes with the box joined, whatever else runs.
                        if crate::backend::is_local_addr(&addr) {
                            for l in crate::backend::start_local_worker(&layout_root, &api) {
                                send(&tx, Level::Info, format!("[{addr}] {l}"));
                            }
                        } else {
                            let port = crate::backend::api_port(&api);
                            match tokio::task::spawn_blocking(move || {
                                crate::backend::start_remote_workers(&[again], port)
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
                                    send_update(&tx, MachineState::Error, "provisioned but the worker would not start — press p again");
                                    set_machine_state(
                                        &api,
                                        &layout_root,
                                        &addr,
                                        MachineState::Error,
                                        "provisioned but the worker would not start — press p again",
                                    )
                                    .await;
                                    let _ = tx.send(Ev::Done(DoneKind::Other));
                                    return;
                                }
                                Err(e) => {
                                    send(&tx, Level::Error, format!("[{addr}] worker start task failed: {e}"));
                                    send_update(&tx, MachineState::Error, "provisioned but the worker start crashed — press p again");
                                    set_machine_state(
                                        &api,
                                        &layout_root,
                                        &addr,
                                        MachineState::Error,
                                        "provisioned but the worker start crashed — press p again",
                                    )
                                    .await;
                                    let _ = tx.send(Ev::Done(DoneKind::Other));
                                    return;
                                }
                            }
                        }
                        set_machine_state(
                            &api,
                            &layout_root,
                            &addr,
                            MachineState::Provisioning,
                            "worker launched — Online on its first beat",
                        )
                        .await;
                    } else {
                        // The note carries the actual failing step from the
                        // provision log (python missing, ssh abort, …) — a
                        // bare "INCOMPLETE" made the machine pane lie about
                        // what the box needs.
                        let reason = if fail_reason.is_empty() {
                            "provision INCOMPLETE".to_string()
                        } else {
                            fail_reason
                        };
                        send_update(&tx, MachineState::Error, &reason);
                        set_machine_state(
                            &api,
                            &layout_root,
                            &addr,
                            MachineState::Error,
                            &reason,
                        )
                        .await;
                        send(&tx, Level::Error, format!("[{addr}] {reason} — fix it and press p again"));
                    }
                }
                Err(e) => {
                    send(&tx, Level::Error, format!("[{addr}] provision task crashed: {e}"));
                    send_update(&tx, MachineState::Error, "provision task crashed — press p again");
                    set_machine_state(
                        &api,
                        &layout_root,
                        &addr,
                        MachineState::Error,
                        "provision task crashed — press p again",
                    )
                    .await;
                    send(&tx, Level::Error, format!("[{addr}] provision task failed: {e}"));
                }
            }
            let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_add_machine(tx: tokio::sync::mpsc::UnboundedSender<Ev>, api: String, http: reqwest::Client, m: Machine) {
            let addr = m.addr.clone();
            match http.post(format!("{api}/api/machines")).json(&m).send().await {
                Ok(r) if r.status().is_success() => send(&tx, Level::Ok,
                    format!("machine {addr} added — press p to provision"),
                ),
                Ok(r) => send(&tx, Level::Error,
                    format!("add {addr} rejected: HTTP {}", r.status()),
                ),
                Err(e) => send(&tx, Level::Error, format!("add {addr} failed: {e}")),
            }
            let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_add_sample(tx: tokio::sync::mpsc::UnboundedSender<Ev>, layout_root: std::path::PathBuf, path: String, name: Option<String>, tags: Option<Vec<String>>) {
            // Off the UI thread: enrollment loads the voice model and takes a
            // while. Same shape as the provision arm below.
            let for_log = path.clone();
            let out = tokio::task::spawn_blocking(move || {
                bm_core::pool::add_sample(&layout_root, std::path::Path::new(&path), tags, name)
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
                    send(&tx, Level::Error, format!("add-sample {for_log} task failed: {e}"));
                    let _ = tx.send(Ev::Done(DoneKind::Other));
                }
            }
}

#[allow(clippy::too_many_arguments)] // one param per run_job local; bundling them is a redesign, not this split
pub(crate) async fn job_start_backend(tx: tokio::sync::mpsc::UnboundedSender<Ev>, layout_root: std::path::PathBuf, api: String, mut api_up: bool, start: u32, count: u32, enqueue: bool, machines: Vec<Machine>, cancel: Arc<AtomicBool>, settings_key: Option<String>) {
            // Degraded start: the backend goes up first (seconds), then each
            // box provisions in the background and joins as it becomes ready.
            // A failing box lands in Error with its reason — it never vetoes
            // the rest. Sequential, not parallel: one ssh flow at a time keeps
            // `X` cancellation prompt between boxes.
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
            send(&tx, Level::Info, format!("starting backend now — {} machine(s) catch up in background…", targets.len()));
            let has_remotes = targets.iter().any(|m| !crate::backend::is_local_addr(&m.addr));
            let port = crate::backend::api_port(&api);
            if has_remotes {
                // A running inductor bound to loopback (old start, hand start)
                // is deaf to exactly these boxes: restart it LAN-wide first.
                // Workers ride through — they re-register on their own and
                // their in-flight reports still count afterwards.
                let dark = crate::backend::lan_blackout(&targets, port).await;
                if !dark.is_empty() {
                    send(&tx, Level::Warn,
                        format!(
                            "inductor invisible from {} — restarting it LAN-wide (workers ride through)…",
                            dark.join(", ")
                        ),
                    );
                    let (gone, lines) = crate::backend::stop_inductor(&layout_root).await;
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
                send(&tx, Level::Warn, "inductor already up: analyzer saved, takes effect on next restart (X, then B)".into());
            }
            // Inductor only: workers start per-box after that box provisions,
            // so an unready box never takes tasks it would fail. Spawning is
            // instant (the server boots in the background); the enqueue waits
            // for the first live refresh (see Ev::BackendLive) — and only
            // when asked: bare `B` brings the backend, nothing more.
            match crate::backend::start_backend(&layout_root, &api, api_up, crate::backend::public_bind(has_remotes), false) {
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
            if !wait_api_live(&api, 30).await {
                send(&tx, Level::Error, "backend spawned but never answered — check .bm/inductor.log, then B again".into());
                let _ = tx.send(Ev::Done(DoneKind::StartDone));
                return;
            }
            // Catch-up loop: provision one box, launch its worker, next.
            let mut failed: Vec<String> = Vec::new();
            for m in &targets {
                if cancel.load(Ordering::Relaxed) {
                    send(&tx, Level::Warn, "start cancelled (X) — remaining boxes stay unprovisioned; p retries one".into());
                    break;
                }
                let addr = m.addr.clone();
                set_machine_state(&api, &layout_root, &addr, MachineState::Provisioning, "catching up in background").await;
                let layout = bm_core::Layout::new(&layout_root);
                let (mc, mf, mp) = (m.addr.clone(), m.ssh_user.clone(), m.ssh_port);
                let mk = resolve(m);
                let out = tokio::task::spawn_blocking(move || {
                    crate::provision_machine(&layout, &mc, &mf, mp, mk, false)
                })
                .await;
                match out {
                    Ok((ready, lines)) => {
                        for l in lines {
                            send(&tx, Level::Info, l);
                        }
                        if !ready {
                            failed.push(addr.clone());
                            set_machine_state(&api, &layout_root, &addr, MachineState::Error, "catch-up failed — select it and press p to retry").await;
                            send(&tx, Level::Error, format!("[{addr}] catch-up failed — cluster runs without it; select it and press p to retry"));
                            continue;
                        }
                    }
                    Err(e) => {
                        failed.push(addr.clone());
                        set_machine_state(&api, &layout_root, &addr, MachineState::Error, "catch-up task crashed — press p to retry").await;
                        send(&tx, Level::Error, format!("[{addr}] catch-up task failed: {e}"));
                        continue;
                    }
                }
                if crate::backend::is_local_addr(&addr) {
                    for l in crate::backend::start_local_worker(&layout_root, &api) {
                        send(&tx, Level::Info, format!("[{addr}] {l}"));
                    }
                } else {
                    let mut one = m.clone();
                    one.ssh_key = resolve(m);
                    match tokio::task::spawn_blocking(move || {
                        crate::backend::start_remote_workers(&[one], port)
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
                            failed.push(addr.clone());
                            set_machine_state(&api, &layout_root, &addr, MachineState::Error, "provisioned but the worker would not start — press p to retry").await;
                            send(&tx, Level::Error, format!("[{addr}] worker start failed — press p to retry"));
                            continue;
                        }
                        Err(e) => {
                            failed.push(addr.clone());
                            set_machine_state(&api, &layout_root, &addr, MachineState::Error, "worker start task crashed — press p to retry").await;
                            send(&tx, Level::Error, format!("[{addr}] worker start task failed: {e}"));
                            continue;
                        }
                    }
                }
                set_machine_state(&api, &layout_root, &addr, MachineState::Provisioning, "ready — Online on its first beat").await;
            }
            if failed.is_empty() {
                send(&tx, Level::Ok, "all machines caught up — cluster complete".into());
            } else {
                send(&tx, Level::Warn, format!("{} machine(s) in Error — cluster runs degraded; select one and press p", failed.len()));
            }
            let _ = tx.send(Ev::Done(DoneKind::StartDone));
}

pub(crate) async fn job_stop_backend(tx: tokio::sync::mpsc::UnboundedSender<Ev>, layout_root: std::path::PathBuf, machines: Vec<Machine>, api: String, settings_key: Option<String>) {
            // Cluster-wide stop, off the UI task: ssh sweeps take seconds per
            // box and must never freeze the dashboard. Keyless boxes fall back
            // to the app-wide default, like every other ssh flow.
            let machines: Vec<Machine> = machines
                .into_iter()
                .map(|mut m| {
                    m.ssh_key = bm_core::provision::resolve_key(m.ssh_key.as_deref(), settings_key.as_deref())
                        .0
                        .map(|p| p.to_string_lossy().to_string());
                    m
                })
                .collect();
            for line in crate::backend::stop_everywhere(&layout_root, &machines, &api).await {
                send(&tx, Level::Info, line);
            }
            send(&tx, Level::Ok, "stop requested everywhere — see lines above per machine".into());
            let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_drop_machine(tx: tokio::sync::mpsc::UnboundedSender<Ev>, api: String, http: reqwest::Client, addr: String) {
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

pub(crate) async fn job_op(tx: tokio::sync::mpsc::UnboundedSender<Ev>, api: String, http: reqwest::Client, req: OpRequest, layout_root: std::path::PathBuf) {
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
                    // Swap-voice survives a dead inductor: same mutation against
                    // the files, guarded by inductor-down + no-local-workers.
                    // Every other op genuinely needs the scheduler.
                    if op == Op::SwapVoice {
                        match crate::api::offline_swap(
                            &api,
                            &layout_root,
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
                                send(&tx, Level::Error, format!("{name}: {msg} (inductor also unreachable: {e})"));
                                false
                            }
                        }
                    } else {
                        send(&tx, Level::Error, format!("{name} failed: {e}"));
                        false
                    }
                }
            };
            let _ = tx.send(Ev::Done(DoneKind::Op { op, key, ok, voice, audio_b64, line_speaker, line_text }));
}

pub(crate) async fn job_load_roster(tx: tokio::sync::mpsc::UnboundedSender<Ev>, api: String, http: reqwest::Client, layout_root: std::path::PathBuf) {
            let res = match http.get(format!("{api}/api/roster")).send().await {
                Ok(r) => match r.json::<Roster>().await {
                    Ok(roster) => Ok(roster),
                    Err(e) => Err(format!("bad roster payload: {e}")),
                },
                Err(_) => {
                    // Inductor down (X stops it): build from files so picking
                    // voices never needs the control plane.
                    Ok(crate::api::offline_roster(&layout_root).await)
                }
            };
            let _ = tx.send(Ev::Roster(res));
            let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Index every script's lines by speaker, off the UI thread.
///
/// `spawn_blocking` because this is a hundred file opens: cheap warm, but it is
/// I/O, and the UI task is the one thing the TUI is not allowed to stall.
pub(crate) async fn job_load_lines(tx: tokio::sync::mpsc::UnboundedSender<Ev>, layout_root: std::path::PathBuf) {
    let res = tokio::task::spawn_blocking(move || crate::tui::audition::index_lines(&layout_root))
        .await
        .unwrap_or_else(|e| Err(format!("line index task failed: {e}")));
    let _ = tx.send(Ev::Lines(res));
    // `dispatch` counts every job and only `Done` decrements, so a job that
    // reports its payload without one leaves the footer claiming a job is
    // running for the rest of the session — and nothing else ever clears it.
    // Every arm of `run_job` owes exactly one of these.
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Serve one already-rendered segment without an inductor: the same lookup
/// `Op::Segment` runs server-side, against this checkout's files. Reports
/// through `DoneKind::Op` with the same shape, so the Done handler — line
/// holding, playback, marker release — cannot tell the two paths apart.
pub(crate) async fn job_segment(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    layout_root: std::path::PathBuf,
    character: String,
    voice: String,
    text: String,
) {
    let key = op_key(&OpRequest { op: Op::Segment, ..Default::default() });
    let voice_job = voice.clone();
    let out = tokio::task::spawn_blocking(move || {
        let layout = bm_core::Layout::new(&layout_root);
        let engine = bm_core::config::Settings::load(&layout.settings()).engine;
        let cands = bm_core::assemble::rendered_segments(&layout, &engine, &voice_job);
        if cands.is_empty() {
            let mut msg = bm_core::assemble::segment_miss(&layout, &character, &voice_job, false);
            msg.push_str("; connect (:B) to synthesize instead");
            return Err(msg);
        }
        // An exact line plays that sentence or misses honestly, like the op.
        let want = text.trim();
        if !want.is_empty() {
            match bm_core::assemble::pick_exact(&cands, &character, want) {
                Some(pick) => return serve_local_segment(pick),
                None => {
                    return Err(format!(
                        "{} (needs :B to render it)",
                        bm_core::assemble::segment_miss(&layout, &character, &voice_job, true)
                    ))
                }
            }
        }
        let pick = bm_core::assemble::pick_rendered(&cands, &character)
            .expect("a non-empty pool always picks");
        serve_local_segment(pick)
    })
    .await;
    match out {
        Ok(Ok((speaker, text, b64, len))) => {
            send(&tx, Level::Ok, format!(
                "segment: “{speaker}” ({} KB, local — nothing synthesized)",
                len / 1024
            ));
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
    Ok((pick.speaker.clone(), pick.text.clone(), B64.encode(&bytes), bytes.len()))
}

fn fail_segment(
    tx: &tokio::sync::mpsc::UnboundedSender<Ev>,
    key: &str,
    voice: &str,
    msg: String,
) {    send(tx, Level::Error, format!("segment failed: {msg}"));
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

pub(crate) async fn run_job(job: Job, tx: tokio::sync::mpsc::UnboundedSender<Ev>) {
    match job {
        Job::Provision { layout_root, api, machine, force, settings_key } => job_provision(tx, layout_root, api, machine, force, settings_key).await,
        Job::AddMachine { api, http, m } => job_add_machine(tx, api, http, m).await,
        Job::AddSample { layout_root, path, name, tags } => job_add_sample(tx, layout_root, path, name, tags).await,
        Job::StartBackend { layout_root, api, api_up, start, count, enqueue, machines, cancel, settings_key } => job_start_backend(tx, layout_root, api, api_up, start, count, enqueue, machines, cancel, settings_key).await,
        Job::StopBackend { layout_root, machines, api, settings_key } => job_stop_backend(tx, layout_root, machines, api, settings_key).await,
        Job::DropMachine { api, http, addr } => job_drop_machine(tx, api, http, addr).await,
        Job::Op { api, http, req, layout_root } => job_op(tx, api, http, req, layout_root).await,
        Job::LoadRoster { api, http, layout_root } => job_load_roster(tx, api, http, layout_root).await,
        Job::LoadLines { layout_root } => job_load_lines(tx, layout_root).await,
        Job::Segment { layout_root, character, voice, text } => job_segment(tx, layout_root, character, voice, text).await,
    }
}
