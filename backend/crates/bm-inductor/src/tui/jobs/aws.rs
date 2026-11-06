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

/// Store the InstaBox key. The proof is the launch's own first call, made off
/// the UI thread because it is two round trips.
pub(crate) async fn job_ibx_login(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    key: String,
) {
    let out = tokio::task::spawn_blocking(move || {
        crate::instabox_ops::login(&root, crate::instabox_ops::LoginArgs::default(), Some(key))
    })
    .await;
    match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Info, l);
            }
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("instabox login: {e:#}")),
        Err(e) => send(
            &tx,
            Level::Error,
            format!("instabox login task failed: {e}"),
        ),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// Read the account into the pool definition. Several read-only AWS calls, so
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
            let _ = tx.send(Ev::Cloud(cloud_snapshot(&root_for_pool).await));
        }
        Ok(Err(e)) => send(&tx, Level::Error, format!("aws discover: {e:#}")),
        Err(e) => send(&tx, Level::Error, format!("aws discover task failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

pub(crate) async fn job_cloud_up(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    api: String,
    http: reqwest::Client,
    req: LaunchReq,
) {
    let LaunchReq {
        provider,
        count,
        ttl_minutes,
        strict,
        from_machine,
    } = req;
    let work_root = root.clone();
    let work_tx = tx.clone();
    // Packs, credentials and the capacity plan all run before a call that
    // spends; a shortfall is the answer when any refuses.
    let out = tokio::task::spawn_blocking(move || {
        let layout = bm_core::Layout::resolve_or_root(&work_root).0;
        if from_machine {
            send(
                &work_tx,
                Level::Warn,
                "packs: --from-machine — the box takes its files from this machine's uplink".into(),
            );
        } else {
            let settings = bm_core::config::Settings::load(&layout.settings());
            let gaps = crate::cloud::pack_gaps(&layout, &settings);
            for gap in &gaps {
                send(&work_tx, Level::Warn, format!("packs: {}", gap.line()));
            }
            if !gaps.is_empty() {
                return Err(format!(
                    "packs: {} gap(s) — a box would fetch its files from here; publish them, \
                     or pass --from-machine",
                    gaps.len()
                ));
            }
            send(
                &work_tx,
                Level::Info,
                "packs: models, profile and voice pins are published".into(),
            );
        }
        let cloud = crate::cloud::for_provider(Some(provider), &work_root, ttl_minutes)
            .map_err(|e| format!("{e:#}"))?;
        let smoke = cloud.smoke().map_err(|s| s.line())?;
        for line in &smoke.lines {
            send(&work_tx, Level::Info, line.clone());
        }
        let plan = cloud.plan(count).map_err(|s| s.line())?;
        for note in &plan.notes {
            send(&work_tx, Level::Info, note.clone());
        }
        if plan.clamped() && strict {
            for s in &plan.shortfalls {
                send(&work_tx, Level::Warn, format!("not created: {}", s.line()));
            }
            return Err(format!(
                "strict: {} of {} would be created — launched none",
                plan.fit, plan.want
            ));
        }
        if plan.fit == 0 {
            for s in &plan.shortfalls {
                send(&work_tx, Level::Warn, format!("not created: {}", s.line()));
            }
            return Err("nothing to launch — the numbers above say why".into());
        }
        let launch = cloud.up(&plan);
        for b in &launch.created {
            // A box still waiting for its address is not news: it links and
            // onboards itself once one arrives.
            if !b.addr.is_empty() {
                send(&work_tx, Level::Info, format!("launched {}", b.label()));
            }
        }
        for s in &launch.skipped {
            send(&work_tx, Level::Warn, format!("not created: {}", s.line()));
        }
        send(
            &work_tx,
            if launch.skipped.is_empty() {
                Level::Ok
            } else {
                Level::Warn
            },
            launch.summary(),
        );
        Ok::<_, String>(launch.created)
    })
    .await;
    let launched = match out {
        Ok(Ok(created)) => created,
        Ok(Err(line)) => {
            send(&tx, Level::Error, line);
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
        Err(e) => {
            send(&tx, Level::Error, format!("{provider} up task failed: {e}"));
            let _ = tx.send(Ev::Done(DoneKind::Other));
            return;
        }
    };
    // The join, made at birth: every returned box becomes a registry entry,
    let mut linked = 0usize;
    let mut waiting = 0usize;
    for b in &launched {
        let m = b.machine();
        let newborn = m.state == MachineState::AwaitingIp;
        let at = if m.addr.contains("://") {
            String::new()
        } else {
            format!(" → {}", m.addr)
        };
        let url = format!("{}/api/machines", api.trim_end_matches('/'));
        match http.post(&url).json(&m).send().await {
            Ok(r) if r.status().is_success() => {
                linked += 1;
                if newborn {
                    waiting += 1;
                } else {
                    let ttl = b
                        .expires_at
                        .map(|at| {
                            let left = at.saturating_sub(bm_proto::now_secs()) / 60;
                            format!(" · {} left", bm_core::cloud::ttl_label(Some(left as u32)))
                        })
                        .unwrap_or_default();
                    send(&tx, Level::Ok, format!("linked {}{}{ttl}", m.handle(), at));
                }
            }
            Ok(r) => send(
                &tx,
                Level::Warn,
                format!(
                    "launched {} but the registry refused it: HTTP {} — start the inductor (:B) and launch again",
                    m.handle(),
                    r.status()
                ),
            ),
            Err(e) => send(
                &tx,
                Level::Warn,
                format!(
                    "launched {} but linking failed ({e}) — launch again once the inductor is up",
                    m.handle()
                ),
            ),
        }
    }
    if linked > 0 {
        // The advice is not "provision them" any more. A box that was tracked
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

pub(crate) async fn job_cloud_down(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    root: std::path::PathBuf,
    layout: bm_core::Layout,
    api: String,
    http: reqwest::Client,
    req: DownReq,
) {
    let DownReq {
        provider,
        ids,
        machines,
    } = req;
    let down_root = root.clone();
    let wanted = ids.clone();
    let out = tokio::task::spawn_blocking(move || {
        let cloud = crate::cloud::for_provider(Some(provider), &down_root, None)
            .map_err(|e| format!("{e:#}"))?;
        cloud.down(&wanted).map_err(|s| s.line())
    })
    .await;
    let terminated = match out {
        Ok(Ok(lines)) => {
            for l in lines {
                send(&tx, Level::Ok, l);
            }
            true
        }
        Ok(Err(line)) => {
            send(&tx, Level::Error, format!("{provider} down: {line}"));
            false
        }
        Err(e) => {
            send(
                &tx,
                Level::Error,
                format!("{provider} down task failed: {e}"),
            );
            false
        }
    };
    // Only what is really gone loses its record: a failed terminate leaves live
    // boxes, and a live box with no record is invisible while it burns.
    if terminated {
        // Both sources, because either can be ahead: the panes' poll may be
        // stale, and with the inductor down the registry file is all there is.
        let mut known = machines;
        known.extend(crate::tui::model::registry_machines(&layout));
        let doomed = instance_machines(&known, &ids);
        if doomed.is_empty() {
            send(
                &tx,
                Level::Info,
                "no registry entry carried those instance ids — nothing to drop".into(),
            );
        } else {
            let live_api = crate::backend::inductor_up(&api).await;
            let mut dropped = 0usize;
            for addr in doomed {
                let (ok, line) = drop_box(&api, &http, &layout, &addr, live_api).await;
                send(&tx, if ok { Level::Ok } else { Level::Error }, line);
                if ok {
                    dropped += 1;
                    let _ = tx.send(Ev::MachineDropped { addr });
                }
            }
            if dropped > 0 {
                send(
                    &tx,
                    Level::Info,
                    format!("{dropped} box record(s) went with their instances"),
                );
            }
        }
    }
    let _ = tx.send(Ev::Cloud(cloud_snapshot(&root).await));
    let _ = tx.send(Ev::Done(DoneKind::Other));
}

/// The registry addresses whose EC2 instance is one of `ids`. The note is the
/// only place a machine remembers which instance it is.
pub(crate) fn instance_machines(machines: &[Machine], ids: &[String]) -> Vec<String> {
    let mut addrs: Vec<String> = machines
        .iter()
        .filter(|m| {
            // The registry's own half first: a provider that keeps its device
            // list (InstaBox) names the id there. EC2 boxes predate that field,
            // so their note carries it.
            m.provider_id
                .as_deref()
                .is_some_and(|id| ids.iter().any(|want| want == id))
                || bm_core::provision::ec2_id_from_note(&m.note).is_some_and(|id| ids.contains(&id))
        })
        .map(|m| m.addr.clone())
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs
}

/// Take one terminated box's record off the cluster: through the API when it
/// answers, against the registry and ledger files when it does not.
async fn drop_box(
    api: &str,
    http: &reqwest::Client,
    layout: &bm_core::Layout,
    addr: &str,
    live: bool,
) -> (bool, String) {
    if live {
        let url = format!(
            "{}/api/machines?addr={}",
            api.trim_end_matches('/'),
            urlencode(addr)
        );
        return match http.delete(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let line = r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("line").and_then(|l| l.as_str()).map(str::to_string))
                    .unwrap_or_else(|| format!("dropped {addr} from the registry"));
                (true, line)
            }
            Ok(r) => (
                false,
                format!("drop {addr}: the inductor answered HTTP {}", r.status()),
            ),
            Err(e) => (false, format!("drop {addr} failed: {e}")),
        };
    }
    let (l, a) = (layout.clone(), addr.to_string());
    match tokio::task::spawn_blocking(move || crate::api::offline::offline_drop_apply(&l, &a)).await
    {
        Ok(line) => (true, line),
        Err(e) => (false, format!("drop {addr} task failed: {e}")),
    }
}

/// `:cloud reap`: the inductor retries the deletes it still owes, then sweeps
/// immediately. Nothing here spends; the boxes are already gone or going.
pub(crate) async fn job_cloud_reap(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
) {
    let url = format!("{}/api/cloud/reap", api.trim_end_matches('/'));
    match http
        .post(&url)
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            let doc = r.json::<serde_json::Value>().await.unwrap_or_default();
            let line = doc
                .get("line")
                .and_then(|l| l.as_str())
                .unwrap_or("reap")
                .to_string();
            send(&tx, Level::Info, line);
            for l in doc
                .get("lines")
                .and_then(|l| l.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                .unwrap_or_default()
            {
                send(&tx, Level::Info, l.to_string());
            }
        }
        Ok(r) => send(&tx, Level::Error, format!("reap: HTTP {}", r.status())),
        Err(e) => send(&tx, Level::Error, format!("reap failed: {e}")),
    }
    let _ = tx.send(Ev::Done(DoneKind::Other));
}
