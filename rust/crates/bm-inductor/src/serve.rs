use super::stage::relink_once;
use super::stage::AWS_WATCH_SECS;
use super::*;

pub(crate) async fn cmd_serve(
    layout: Layout,
    settings: Settings,
    port: u16,
    bind: &str,
    start: u32,
    count: u32,
    go: bool,
) -> anyhow::Result<()> {
    // `Inner` takes the layout; the dispatcher needs its own handle on it.
    let drive_layout = layout.clone();
    let drive_root = drive_layout.root.clone();
    let mut inner = state::Inner::new(layout, settings);
    // No profile, no run. A drifted live tree is adopted by verify
    // (a `:sound` retune), not refused, only a missing binding or an
    // empty live tree stops us before touching the ledger. The binding is the
    // one **in force** — the active workspace's, not the checkout's — so
    // running one book neither reports nor re-stamps another's.
    let binding = bm_core::profile::verify_layout(&inner.layout, Some(&inner.settings.engine))?;
    println!(
        "profile {} ({})",
        bm_core::profile::label(&binding),
        &binding.pack.hash[..12.min(binding.pack.hash.len())]
    );
    // The cluster token, generated on first use and then stable across
    // restarts: a worker that outlived a restart must not be locked out, and
    // provisioning copies this file to every box it onboards. Only a
    // fingerprint is printed, the log is not a place for a secret.
    let token = bm_core::token::load_or_create(&inner.layout.root)?;
    println!("cluster token {}", &token[..8.min(token.len())]);
    inner.load_ledger();
    // One-time, and before anything plans a path: the cast and the segment
    // directories now carry the adapter as well as the engine, so a workspace
    // from before the split is renamed into the new shape rather than
    // re-rendering every chapter it already spoke.
    // Also one-time, and before any path is planned: the weights, the sidecar
    // and its runtime used to sit at the root, and they are the bound engine's
    // files now. A checkout from before this moves rather than re-provisions.
    inner.migrate_engine_tree();
    inner.migrate_cache_keys();
    inner.check_profile()?;
    // The language the adapter writes against the language the bound engine can
    // voice. Raised **once, here**, because a mismatch withholds every digest
    // and render (`Inner::voice_gate`) and a withheld row is indistinguishable
    // from an idle cluster on the dashboard — this is the one moment the
    // operator is certainly reading, and it is the only warning the run gets.
    //
    // A warning and not a refusal to start: nothing is broken on disk, the
    // other two stages (`crawl`, `prepare`) are adapter-independent and still
    // work, and one keypress fixes it.
    {
        let binding = inner
            .ledger_profile
            .as_ref()
            .unwrap_or(&inner.settings.profile)
            .clone();
        // `settings.engine`, matching `Inner::voice_gate`: the warning has to
        // be about the engine the run will name, or it would name one engine
        // while the scheduler judged another.
        let verdict =
            bm_core::adapter::inspect(&inner.layout, &binding.pack.name, &inner.settings.engine);
        if !verdict.agrees() {
            inner.push_event(
                "warn",
                format!(
                    "{} — no digest or render will be offered until this is fixed",
                    verdict.reason()
                ),
            );
        }
    }
    inner.reconcile(start, count);
    // The range this process works on is the one it was just told to reconcile.
    //
    // `make serve` (and the plain binary) always name a range, so the CLI is
    // authoritative here — the saved run config is a *prefill* for the
    // dashboard's `t` prompt, and a process that quietly preferred it would
    // reconcile 1..10 and then distribute whatever the file happened to hold.
    // In memory only: the file is the operator's.
    inner.set_authored_range(start, count);
    // Held, unless somebody asked for the old behaviour with `--go`.
    //
    // `reconcile` above is bookkeeping — it makes sure every stage of the range
    // has a row — and it is deliberately still run while held: the operator's
    // first question is always "what is left", which is a question about rows.
    // What the hold stops is the *offers*, which is the part that spends hours
    // and money, so a box that reboots and finds the inductor up does not
    // resume a run nobody asked it to resume.
    if go {
        let line = inner.set_dispatch(true);
        println!("[inductor] {line}");
    } else {
        println!(
            "[inductor] held · {} · `:go` starts distributing, `--go` to come up that way",
            inner.remaining_line()
        );
    }
    let shared = std::sync::Arc::new(tokio::sync::Mutex::new(inner));
    // `--go` is the *same* control as `:go`, so it does the same two things: the
    // flag above, and the remainder queued. Anything less would make the
    // automation path a different feature from the one an operator uses, and the
    // difference would show up as crawl rows no manual-mode worker can run.
    if go {
        if let Some(more) = crate::api::enqueue_remainder(&shared, true).await {
            println!("[inductor] {more}");
        }
    }
    // Lease reaper: expired leases return to the pool, no strike.
    let reaper = shared.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let (expired, never_came_up) = {
                let mut inner = reaper.lock().await;
                // Boot deadline first, then leases: a box that never answered
                // has no leases to expire, and one lock covers both.
                let expired = inner.reap();
                let never_came_up = inner.expire_initializing();
                (expired, never_came_up)
            };
            for id in expired {
                println!("[inductor] lease expired, requeued {id} (no strike)");
            }
            for line in never_came_up {
                println!("[inductor] {line} — marked error");
            }
        }
    });
    // The driving half. Nothing dials this process, so if this loop is not
    // running, no work moves at all, the workers are servers waiting to be
    // asked, and this is the only thing that asks.
    let driving = shared.clone();
    tokio::spawn(async move { dispatch::run(driving, drive_layout).await });
    // The reverse tunnels: one ssh client per remote box, forwarding the
    // box's own loopback hook port to this API. A worker uses it only when
    // this process has gone silent on every normal channel (see
    // `bm-agent/src/hook.rs`), holding it open costs one idle ssh per box,
    // and gives a finished stage a road home when the uplink blips mid-task.
    tokio::spawn(tunnel::supervise(shared.clone(), port));
    // Relink once at startup: an EC2 box that cycled while this inductor was
    // down is sitting in the registry at an address that no longer answers.
    // Best-effort, no account, no creds or an offline CLI only means the
    // repairs wait for the next `:pool` refresh.
    {
        let relink_shared = shared.clone();
        let relink_root = drive_root.clone();
        tokio::spawn(async move { relink_once(&relink_shared, &relink_root).await });
    }
    // ...and then keep watching, while a launch is in flight.
    //
    // `RunInstances` answers before the instance has an address, so a box is
    // registered under its instance id and the address that makes it *usable*
    // arrives later, asynchronously, with nothing to notify. Waiting for the
    // operator to run `:relink` is what made `:up 3` end with three boxes the
    // dashboard could not dial and no indication that anything was missing.
    //
    // The watch is self-terminating: it asks `has_pending_launch` first, which is
    // false once no box is still booting or addressless, so the account is read
    // only while there is a reason to and a settled cluster polls nothing.
    // Overlapping ticks are refused rather than queued, an `aws` call is a
    // process spawn, and stacking them would only make a slow API worse.
    {
        let watch_shared = shared.clone();
        let watch_root = drive_root.clone();
        tokio::spawn(async move {
            let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(AWS_WATCH_SECS)).await;
                if in_flight.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let pending = watch_shared.lock().await.has_pending_launch();
                if !pending {
                    continue;
                }
                in_flight.store(true, std::sync::atomic::Ordering::Relaxed);
                let shared = watch_shared.clone();
                let root = watch_root.clone();
                let flag = in_flight.clone();
                tokio::spawn(async move {
                    relink_once(&shared, &root).await;
                    flag.store(false, std::sync::atomic::Ordering::Relaxed);
                });
            }
        });
    }
    let app = api::router(shared);
    let addr = format!("{bind}:{port}");
    println!("inductor on http://{addr}");
    axum::serve(
        tokio::net::TcpListener::bind(&addr).await?,
        app.into_make_service(),
    )
    .await?;
    Ok(())
}
