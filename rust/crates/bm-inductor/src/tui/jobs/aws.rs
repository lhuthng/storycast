use super::reqs::send;
use super::reqs::DoneKind;
use super::reqs::Ev;
use super::*;

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
    // it there is nothing to match a relaunched box by, say so instead of
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
    // if the address were equal, it is not, so the old entry is dropped
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

/// One account listing, off the UI thread. Errors are returned as strings so
/// the Cloud view can render the reason instead of an empty account, which is
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
    // the registry with the addresses the account carries now. Best-effort
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
            // stale, refresh it in the same job that wrote the file.
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
    // is registered too, keyed by its instance id, see `machine_from_instance`.
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
