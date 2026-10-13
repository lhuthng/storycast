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
        let body = heartbeat_now(&p, &who, &mut probe, true, None);
        // The inductor's only command channel: a shutdown latch read on
        if let Ok(resp) = http.post(&url).json(&body).send().await {
            if let Ok(bytes) = resp.bytes().await {
                if wants_shutdown(&bytes) {
                    // No sidecar is stopped here, and it is a known gap rather
                    println!("inductor asked for shutdown — exiting");
                    std::process::exit(0);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Seconds of silence before a worker gives up on its inductor.
pub(crate) fn idle_secs(s: &Settings) -> u64 {
    s.idle_mins.max(1) as u64 * 60
}

/// Exit when the inductor stops asking.
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
            push.sidecar.lock().await.reap_all().await;
            std::process::exit(0);
        }
    }
}
