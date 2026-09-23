//! The completion hook: how a finished stage gets home when the road it was
//! on is gone.
//!
//! The pushed task's answer *is* the report (`POST /task`'s response), so a
//! mid-task uplink blip kills the completion, not just the connection: the
//! stage finished on this box, the lease expires, the chapter re-renders
//! somewhere else — wasted hours with nothing to show. The reverse tunnel
//! (`bm-inductor/src/tunnel.rs`) gives this worker a loopback address that
//! **is** the inductor's control API; this module is the sender that uses it.
//!
//! ## The rule: the hook is a backup, never a rival
//!
//! The task handler stashes every outcome where [`supervise`] can find it,
//! then answers on the primary channel exactly as before. The hook sends that
//! stash **only after the inductor has been silent on every channel for
//! [`SILENCE_SECS`]** — judged on the inductor's own requests to us
//! (`Push::silent_for`), not on our failed sends, because a healthy
//! dispatcher asks every 2 s and its silence is the one honest signal that
//! the primary road is dead. A live inductor never sees a hook post; a dead
//! one sees the same `Complete` the live answer would have carried, through
//! the tunnel, and its own gates (stale-report check, render's file-on-disk
//! proof, per-task strikes) judge it like any other report. Reachability is
//! all the tunnel grants; authority stays with the scheduler.
//!
//! A pull-protocol worker has no stash (its reports already retry) and no
//! tunnel (nothing forwarded the port), so it runs exactly as it did.

use crate::push::Push;
use crate::Shared;
use bm_proto::Complete;
use std::sync::Arc;
use std::time::Duration;

/// How long the inductor must have asked *nothing* before the hook dares to
/// speak. Longer than a dispatcher poll cycle (2 s) by an order of magnitude,
/// so a healthy inductor — even one with no work to give — never meets a hook
/// post; shorter than a lease, so the report beats the expiry that would
/// requeue the task.
const SILENCE_SECS: u64 = 30;

/// The tunnel's worker-side end, and the token the reports carry.
///
/// The bearer token is belt-and-braces today (the control API sits behind a
/// loopback bind at both ends of the tunnel) and load-bearing the day the API
/// grows auth: the hook is already compliant, and a hook that authenticates
/// cannot become the unauthenticated instruction path the cluster token
/// exists to prevent.
#[derive(Debug, Clone)]
pub(crate) struct Hook {
    base: String,
    token: String,
}

impl Hook {
    /// `base` is the forwarded control API — `http://127.0.0.1:{hook port}` —
    /// and `token` the cluster token this worker already holds.
    pub(crate) fn from_base(base: &str, token: &str) -> Hook {
        Hook {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// Deliver one completion through the tunnel. `false` means "not yet":
    /// the stash stays, and the next pass tries again — a tunnel that is down
    /// is exactly the state [`super::tunnel_missing_hint`] expects, and the
    /// supervisor respawns clients on its own clock.
    ///
    /// `no_proxy` is not optional, for the reason this repo has paid for three
    /// times: the tunnel is loopback, and an ambient `HTTP_PROXY` answering in
    /// its place turns "report failed" into "stranger accepted the report".
    pub(crate) async fn fire(&self, report: &Complete, silent_for: Duration) -> bool {
        if silent_for.as_secs() < SILENCE_SECS {
            // The inductor is still asking; the primary channel owns this
            // outcome. Refraining here is the whole design.
            return false;
        }
        let http = match reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                println!("hook: could not build a client: {e}");
                return false;
            }
        };
        match http
            .post(format!("{}/api/complete", self.base))
            .bearer_auth(&self.token)
            .json(report)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => true,
            Ok(r) => {
                println!("hook: the inductor answered {} — not accepted", r.status());
                false
            }
            Err(e) => {
                println!("hook: no answer through the tunnel: {e}");
                false
            }
        }
    }
}

/// Watch for outcomes the primary channel never delivered, and deliver them.
///
/// Runs forever, at hook cadence (5 s). The gate is deliberately threefold —
/// not busy (a running task reports through its own answer), the inductor
/// silent past [`SILENCE_SECS`] (every channel it normally uses is dead), a
/// stashed completion present (something finished unreported). A successful
/// delivery clears the stash; a failed one leaves it for the next pass, which
/// is the retry loop. A report the inductor already applied before dying
/// mid-ack is harmless to re-send: it comes back as `stale` and is ignored —
/// the ledger's own idempotence, doing the deduplication.
pub(crate) async fn supervise(hook: Hook, push: Arc<Push>, shared: Shared) {
    // Consecutive failures at the same stash: feeds [`super::tunnel_missing_hint`]
    // so a down tunnel is diagnosed once a minute, not once every 5 s.
    let mut misses: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if push.is_busy() {
            continue;
        }
        let silent = push.silent_for();
        if silent.as_secs() < SILENCE_SECS {
            continue;
        }
        let Some(report) = shared.lock().ok().and_then(|p| p.pending.clone()) else {
            misses = 0;
            continue;
        };
        if misses == 0 {
            println!(
                "no word from the inductor for {}s — reporting {} through the tunnel",
                silent.as_secs(),
                report.task_id
            );
        }
        if hook.fire(&report, silent).await {
            if let Ok(mut p) = shared.lock() {
                p.pending = None;
            }
            misses = 0;
            println!("hook accepted — the completion is home");
        } else {
            misses += 1;
            super::tunnel_missing_hint(&report.task_id, misses);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Complete {
        Complete {
            worker_id: "box-1".into(),
            task_id: "render:7:0".into(),
            ok: true,
            detail: "render ch7 (1 calls)".into(),
            duration_secs: 12.0,
            bible_delta: None,
            units: 1,
            script: None,
            text: None,
            mp3_b64: None,
            unit_files: Vec::new(),
        }
    }

    /// The inductor's control API, answering success on the one route the
    /// hook posts. A real socket, because the point is the round trip.
    async fn serve_complete() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/api/complete",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"ok": true})) }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_hook_refrains_while_the_primary_channel_is_alive() {
        // The design's whole point, pinned: under SILENCE_SECS the hook does
        // not even dial — the inductor is still asking, so the answer it is
        // waiting for belongs on the channel it asked on.
        let hook = Hook::from_base("http://127.0.0.1:1", "t");
        assert!(
            !hook
                .fire(&report(), Duration::from_secs(SILENCE_SECS - 1))
                .await,
            "a live inductor must never see a hook post"
        );
    }

    #[tokio::test]
    async fn a_silent_inductor_gets_the_completion_through_the_hook() {
        let base = serve_complete().await;
        let hook = Hook::from_base(&base, "s3cret");
        assert!(
            hook.fire(&report(), Duration::from_secs(SILENCE_SECS + 1))
                .await,
            "past the silence gate, the stash is delivered"
        );
    }

    #[tokio::test]
    async fn no_tunnel_means_no_delivery_and_no_loss() {
        // Port 1 is nothing, ever: the failure every real blip looks like.
        // `false` is the contract — the stash survives, the next pass retries.
        let hook = Hook::from_base("http://127.0.0.1:1", "s3cret");
        assert!(
            !hook
                .fire(&report(), Duration::from_secs(SILENCE_SECS + 1))
                .await
        );
    }
}
