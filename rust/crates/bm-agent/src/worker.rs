use super::heartbeat::heartbeat_loop;
use super::*;

pub(crate) async fn worker_loop(
    layout: Layout,
    settings: Settings,
    inductor: Option<String>,
    worker_id: String,
    addr: String,
    tts_url: String,
    serve_tasks: Option<u16>,
) -> Result<()> {
    announce_budget();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        // The inductor is loopback or LAN. An ambient `HTTP_PROXY` would
        .no_proxy()
        .build()?;
    let hostname = hostname_simple();
    let alias = worker_alias_for(&layout.root);
    let who = WorkerIdentity {
        worker_id: worker_id.clone(),
        addr: addr.clone(),
        hostname: hostname.clone(),
        alias: alias.clone(),
        root: layout.root.clone(),
    };
    let shared: Shared = Arc::new(Mutex::new(Progress {
        activity: "starting".to_string(),
        ..Default::default()
    }));
    // The instruction channel: this worker answers instead of only asking.
    let mut channel: Option<std::sync::Arc<push::Push>> = None;
    if let Some(port) = serve_tasks {
        // A worker with no token refuses to serve rather than serving openly:
        let Some(token) = bm_core::token::read(&layout.root) else {
            anyhow::bail!(
                "--serve-tasks needs a cluster token at {} — the inductor generates one and provisioning ships it; without it this worker would accept instructions from anything that can reach the port",
                layout.root.join(".bm").join(bm_core::token::FILE).display()
            );
        };
        let push = std::sync::Arc::new(push::Push {
            who: who.clone(),
            token,
            shared: shared.clone(),
            probe: Mutex::new(LoadProbe::new()),
            layout: layout.clone(),
            settings: settings.clone(),
            sidecar: tokio::sync::Mutex::new(Sidecar::new(&tts_url)),
            busy: std::sync::atomic::AtomicBool::new(false),
            last_contact: std::sync::atomic::AtomicU64::new(bm_proto::now_secs()),
            last_task_end: std::sync::atomic::AtomicU64::new(bm_proto::now_secs()),
            // The sidecar is kept by default, the render lifecycle is written
            keep_sidecar: std::sync::atomic::AtomicBool::new(true),
            // No opinion until the inductor pushes one: the sidecar's own
            tts_threads: std::sync::atomic::AtomicU64::new(push::THREADS_UNSET),
            // Merge data-plane: the hook base *is* the inductor API through
            fetch_http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            fetch_base: format!("http://127.0.0.1:{}", bm_proto::DEFAULT_HOOK_PORT),
        });
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        println!("instruction channel on 0.0.0.0:{port} (token required)");
        let serving = push.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, push::router(serving)).await {
                eprintln!("instruction channel died: {e}");
            }
        });
        channel = Some(push);
    }

    // ── Serve-only ──────────────────────────────────────────────────────────
    let Some(inductor) = inductor else {
        let Some(push) = channel else {
            anyhow::bail!(
                "neither --inductor nor --serve-tasks: this worker would have nothing to do and no way to be given work"
            );
        };
        println!(
            "serve-only as {} — answering on port {}, dialling nothing",
            push.who.worker_id,
            serve_tasks.unwrap_or_default()
        );
        // The warm sidecar is reaped when the box goes idle, so keeping it
        tokio::spawn(push::sidecar_reaper(push.clone()));
        // The completion hook: a loopback address that **is** the inductor's
        let hook = hook::Hook::from_base(
            &format!("http://127.0.0.1:{}", bm_proto::DEFAULT_HOOK_PORT),
            &push.token,
        );
        tokio::spawn(hook::supervise(hook, push.clone(), shared.clone()));
        // The worker's own off switch. The inductor's timer covers the normal
        idle_watchdog(push, Duration::from_secs(idle_secs(&settings) + 90)).await;
        return Ok(());
    };

    tokio::spawn(heartbeat_loop(
        http.clone(),
        inductor.clone(),
        who.clone(),
        shared.clone(),
    ));
    let reg = Register {
        worker_id: worker_id.clone(),
        addr: addr.clone(),
        hostname,
        capabilities: capabilities(),
        sources_stages: bundle_slots(&layout.root),
        tts_url: Some(tts_url.clone()),
        version: VERSION.into(),
    };
    // The inductor may not be up yet (or the network may flap): retry
    loop {
        match http
            .post(format!("{inductor}/api/register"))
            .json(&reg)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => break,
            Ok(r) => {
                set_progress(&shared, 0.0, format!("register refused: {}", r.status()));
            }
            Err(e) => {
                set_progress(&shared, 0.0, format!("inductor unreachable, retrying: {e}"));
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    println!("registered as {worker_id} (alias {alias}), pulling tasks");
    let mut sidecar = Sidecar::new(&tts_url);
    loop {
        let offer: Option<TaskOffer> = match http
            .get(format!("{inductor}/api/task"))
            .query(&[("worker_id", &worker_id)])
            .send()
            .await
        {
            Ok(r) if r.status() == reqwest::StatusCode::NO_CONTENT => None,
            Ok(r) => Some(r.json().await.context("parsing task offer")?),
            Err(e) => {
                set_progress(&shared, 0.0, format!("inductor unreachable: {e}"));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let Some(offer) = offer else {
            set_progress(&shared, 0.0, "idle".to_string());
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        set_task(&shared, &offer);
        let t0 = Instant::now();
        let res = match run_offer(
            &layout,
            &settings,
            &offer,
            &shared,
            &mut sidecar,
            Some((&http, inductor.as_str())),
            true,
            None,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => TaskResult {
                ok: false,
                detail: format!("{} ch{} failed: {e:#}", offer.stage, offer.chapter),
                delta: None,
                units: 0,
                script: None,
                text: None,
                crawl: None,
                mp3_b64: None,
                unit_files: Vec::new(),
            },
        };
        // The stage is over as soon as `run_offer` returns. Do not leave its
        clear_task(&shared);
        println!("[{}] {}", if res.ok { "ok" } else { "FAIL" }, res.detail);
        // Reports must land: a lost merge report strands a finished mp3 on
        let report = Complete {
            worker_id: worker_id.clone(),
            task_id: offer.task_id.clone(),
            ok: res.ok,
            detail: res.detail,
            duration_secs: t0.elapsed().as_secs_f64(),
            bible_delta: res.delta,
            units: res.units,
            script: res.script,
            text: res.text,
            crawl: res.crawl,
            mp3_b64: res.mp3_b64,
            unit_files: res.unit_files,
        };
        let mut reported = false;
        for attempt in 1..=3 {
            match http
                .post(format!("{inductor}/api/complete"))
                .json(&report)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    reported = true;
                    break;
                }
                Ok(r) => println!(
                    "[WARN] complete report refused ({}), retry {attempt}/3",
                    r.status()
                ),
                Err(e) => println!("[WARN] complete report lost, retry {attempt}/3: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        // **No sweep.** A non-local worker's segment directory used to be
        let _ = (offer.render_units.as_deref(), reported);
    }
}

pub(crate) fn hostname_simple() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

/// What a hook post that goes nowhere means, said once. When the worker
pub(crate) fn tunnel_missing_hint(task_id: &str, attempt: u64) {
    if attempt.is_multiple_of(12) {
        println!(
            "hook: {task_id} still unreported — no tunnel answers on 127.0.0.1:{}; \
             the inductor's lease reaper will requeue it if this tunnel never comes back",
            bm_proto::DEFAULT_HOOK_PORT
        );
    }
}
