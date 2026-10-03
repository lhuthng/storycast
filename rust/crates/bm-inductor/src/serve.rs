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
    let binding = bm_core::profile::verify_layout(&inner.layout, Some(&inner.settings.engine))?;
    println!(
        "profile {} ({})",
        bm_core::profile::label(&binding),
        &binding.pack.hash[..12.min(binding.pack.hash.len())]
    );
    // The cluster token, generated on first use and then stable across
    let token = bm_core::token::load_or_create(&inner.layout.root)?;
    println!("cluster token {}", &token[..8.min(token.len())]);
    inner.load_ledger();
    // One-time, and before anything plans a path: the cast and the segment
    inner.migrate_engine_tree();
    inner.migrate_cache_keys();
    inner.check_profile()?;
    // The language the adapter writes against the language the bound engine can
    {
        let binding = inner
            .ledger_profile
            .as_ref()
            .unwrap_or(&inner.settings.profile)
            .clone();
        // `settings.engine`, matching `Inner::voice_gate`: the warning has to
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
    inner.set_authored_range(start, count);
    // Held, unless somebody asked for the old behaviour with `--go`.
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
    let driving = shared.clone();
    tokio::spawn(async move { dispatch::run(driving, drive_layout).await });
    // The reverse tunnels: one ssh client per remote box, forwarding the
    tokio::spawn(tunnel::supervise(shared.clone(), port));
    // Relink once at startup: an EC2 box that cycled while this inductor was
    {
        let relink_shared = shared.clone();
        let relink_root = drive_root.clone();
        tokio::spawn(async move { relink_once(&relink_shared, &relink_root).await });
    }
    // ...and then keep watching, while a launch is in flight.
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
