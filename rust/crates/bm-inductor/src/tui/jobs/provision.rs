use super::reqs::send;
use super::reqs::set_machine_state;
use super::reqs::verdict_after_failed_provision;
use super::reqs::DoneKind;
use super::reqs::Ev;
use super::*;

/// Run a blocking provision with its log lines streaming into the event pane
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
    let _ = pump.await;
    out
}

/// Elapsed-push stamp for the provision launch line (`4s`, `3m41s`): the
fn push_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// Why a provision run did not leave the box ready, in the operator's terms.
/// really is in the log: prefer the inner root cause ("rsync: command not
/// found") over its wrapper ("agent install failed").
pub(crate) fn provision_stop_reason(stop: Option<&str>, lines: &[String]) -> String {
    if let Some(why) = stop.filter(|s| !s.trim().is_empty()) {
        return bm_core::util::head_chars(why, 160);
    }
    /// A log line without its `[addr] ` prefix, the pane already shows the
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
    let t0 = Instant::now();
    send(&tx, Level::Info, format!("[{addr}] provisioning machine…"));
    // Read before the run: the job stamps `provisioning` below, so the state on
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
            // `None` reads `models_release` out of the workspace settings, which
            None,
        )
    })
    .await;
    match out {
        Ok(out) => {
            let ready = out.ready;
            // Lines already streamed live above, `lines` stays for the
            let fail_reason = provision_stop_reason(out.stop.as_deref(), &out.lines);
            if ready {
                // `X` landed while this box was being pushed: the box is
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
                // It never answered ssh, and we already knew it was booting
                let note = "still booting — nothing to do yet, :prov again in a moment";
                send_update(&tx, MachineState::Initializing, note);
                set_machine_state(&api, &layout, &addr, MachineState::Initializing, note).await;
                send(&tx, Level::Info, format!("[{addr}] {note}"));
            } else {
                // The note carries the actual failing step, a missing local
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
    let for_log = path.clone();
    let out = tokio::task::spawn_blocking(move || {
        bm_core::pool::add_sample(&layout, std::path::Path::new(&path), tags, name)
    })
    .await;
    match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Ok, l);
            }
            // The picker may be showing the pre-sample roster: fetch a
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

pub(crate) fn start_cancelled(
    tx: &tokio::sync::mpsc::UnboundedSender<Ev>,
    cancel: &AtomicBool,
) -> bool {
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
