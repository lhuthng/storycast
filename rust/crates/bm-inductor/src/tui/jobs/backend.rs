use super::provision::start_cancelled;
use super::reqs::send;
use super::reqs::wait_api_live;
use super::reqs::DoneKind;
use super::reqs::Ev;
use super::*;

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
    // A failing box lands in Error with its reason, it never vetoes
    // the rest. The catch-up is handed to the dashboard rather than run
    // here (see the hand-off at the end of this function), so the boxes
    // provision **concurrently**, each one holds its own `Res::Box`,
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
        // Workers ride through, they re-register on their own and
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
    // The LLM choice lives in `.bm/llm.json`, which every offer reads when
    // it is built — so a model switch takes effect on the next offer even
    // with a live inductor, and this stays a note rather than a warning.
    if api_up {
        send(
            &tx,
            Level::Info,
            "inductor already up: the next offer carries the saved LLM key + model".into(),
        );
    }
    // Inductor only: workers start per-box after that box provisions,
    // so an unready box never takes tasks it would fail. Spawning is
    // instant (the server boots in the background); the enqueue waits
    // for the first live refresh (see Ev::BackendLive), and only
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
    // as long as provisioning every box took, so `start backend`, a press that
    // should be seconds, showed minutes and every later job sat queued behind
    // it. Each box now gets its own `provision machine` row, all of them
    // running at once because they hold different boxes.
    //
    // A box already beating needs nothing: `online` is the state this path
    // exists to reach, and it is *working*. Re-provisioning it anyway is why
    // `B` on a healthy cluster took minutes, so it is skipped and said out
    // loud, `p` is the deliberate re-provision.
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
/// submit (`api_up`): with it down, the states are the last live poll's
/// frozen by `state_failed`, which keeps the rows, and trusting them skips
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
    // heartbeat answer (2s) tells every worker to exit on its own, no
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
/// the on-disk `machines.json` agree. Success is silent, the panel is the
/// feedback, but a rejection is named, because a policy that did not stick is
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

/// Park a machine or wake it up, through the API so the live inductor and the
/// on-disk `machines.json` agree.
///
/// Success is silent, the pane is the feedback, and the `relaxed` state word
/// only appears once a poll has come back with it. A rejection is named, because
/// a park that silently did not stick is a box that keeps taking work the
/// operator believes it has stopped, which is worse than one that refuses
/// loudly.
/// Set one box's TTS sidecar thread count, through the API so the live
/// inductor and the on-disk `machines.json` agree, and so the dispatcher's
/// convergence loop carries it to a box that is down right now.
///
/// The pane is the feedback once a poll has come back; the line says what was
/// asked for, because the box restarts its sidecar on its *next* render and the
/// new count is not in force until then. A rejection is named for the same
/// reason a failed park is: a thread count that silently did not stick is a box
/// the operator believes they tuned.
pub(crate) async fn job_set_tts_threads(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    addr: String,
    threads: Option<u16>,
) {
    let url = format!("{}/api/machines/tts-threads", api.trim_end_matches('/'));
    let body = serde_json::json!({"addr": addr, "threads": threads});
    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => send(
            &tx,
            Level::Ok,
            match threads {
                Some(n) => format!(
                    "{addr}: tts sidecar set to {n} thread(s) — it restarts on the next render"
                ),
                None => format!(
                    "{addr}: tts threads back to the sidecar default — it restarts on the next render"
                ),
            },
        ),
        Ok(r) => send(
            &tx,
            Level::Error,
            format!("threads {addr}: the inductor answered HTTP {}", r.status()),
        ),
        Err(e) => send(&tx, Level::Error, format!("threads {addr} failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_set_accepting(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    addr: String,
    accepting_work: bool,
) {
    let url = format!("{}/api/machines/accepting", api.trim_end_matches('/'));
    let body = serde_json::json!({"addr": addr, "accepting_work": accepting_work});
    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => send(
            &tx,
            Level::Error,
            format!(
                "{} {addr}: the inductor answered HTTP {}",
                if accepting_work { "wake" } else { "park" },
                r.status()
            ),
        ),
        Err(e) => send(
            &tx,
            Level::Error,
            format!(
                "{} {addr} failed: {e}",
                if accepting_work { "wake" } else { "park" }
            ),
        ),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}
