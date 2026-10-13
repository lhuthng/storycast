use super::reqs::DoneKind;
use super::reqs::Ev;

pub(crate) async fn job_manual_digest(
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
    api: String,
    http: reqwest::Client,
    chapter: u32,
    script: serde_json::Value,
    delta: serde_json::Value,
) {
    // One report body, shared with the headless backup runner (`manual::report`):
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
