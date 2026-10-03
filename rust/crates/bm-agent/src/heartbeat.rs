use super::*;

pub(crate) async fn heartbeat_loop(
    http: reqwest::Client,
    inductor: String,
    who: WorkerIdentity,
    shared: Shared,
) {
    let url = format!("{inductor}/api/heartbeat");
    let mut probe = LoadProbe::new();
    loop {
        let p = shared.lock().map(|p| p.clone()).unwrap_or_default();
        // Pull mode has no instruction channel, so the sidecar's own default
        // (or its `BM_TTS_THREADS` environment) stands: `None`.
        let body = heartbeat_now(&p, &who, &mut probe, true, None);
        // The inductor's only command channel: a shutdown latch read on
        // every answer. Exiting here strands nothing, the inductor
        // reaps the lease (no strike) or requeues the ledger on its way
        // down, and an old inductor's `{"ok": true}` parses as "stay".
        if let Ok(resp) = http.post(&url).json(&body).send().await {
            if let Ok(bytes) = resp.bytes().await {
                if wants_shutdown(&bytes) {
                    // No sidecar is stopped here, and it is a known gap rather
                    // than an oversight: this is the *pull* protocol, whose
                    // sidecar lives in the task loop's own locals and is not
                    // reachable from this task (the inverted protocol, the one
                    // the inductor actually drives now, reaps in the
                    // `/shutdown` handler and in `idle_watchdog`). A child left
                    // here is cleaned up by the cluster sweep (`X`) until the
                    // pull path is retired.
                    println!("inductor asked for shutdown — exiting");
                    std::process::exit(0);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Seconds of silence before a worker gives up on its inductor.
///
/// Derived from the same setting the inductor's own idle timer uses, so the
/// two cannot be configured into a state where the worker quits before the
/// inductor gets a chance to say goodbye.
pub(crate) fn idle_secs(s: &Settings) -> u64 {
    s.idle_mins.max(1) as u64 * 60
}

/// Exit when the inductor stops asking.
///
/// In serve-only mode this worker has no way to *notice* the inductor is gone:
/// no dial to fail, no report to be refused, just silence. Without this it
/// would hold its port and its TTS sidecar indefinitely.
///
/// A busy worker is exempt, and that exemption is load-bearing: the inductor
/// is blocked inside its own `POST /task` for the whole stage, so no polls
/// arrive by design, and counting that as idleness would kill a render
/// halfway through.
pub(crate) async fn idle_watchdog(push: std::sync::Arc<push::Push>, timeout: Duration) {
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if push.is_busy() {
            push.touch();
            continue;
        }
        if push.silent_for() >= timeout {
            println!(
                "no contact from the inductor for {}s — exiting",
                timeout.as_secs()
            );
            // Do not orphan the sidecar, ours or an adopted one: an exiting
            // worker that leaves a model behind is 2.85 GB held by a box nobody
            // drives. Safe to wait for the lock, this branch is only reached
            // when no task is running.
            push.sidecar.lock().await.reap_all().await;
            std::process::exit(0);
        }
    }
}
