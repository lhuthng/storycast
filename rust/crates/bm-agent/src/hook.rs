//! The completion hook: how a finished stage gets home when the road it was

use crate::push::Push;
use crate::Shared;
use bm_proto::Complete;
use std::sync::Arc;
use std::time::Duration;

/// How long the inductor must have asked *nothing* before the hook dares to
const SILENCE_SECS: u64 = 30;

/// The tunnel's worker-side end, and the token the reports carry.
#[derive(Debug, Clone)]
pub(crate) struct Hook {
    base: String,
    token: String,
}

impl Hook {
    /// `base` is the forwarded control API — `http://127.0.0.1:{hook port}` —
    pub(crate) fn from_base(base: &str, token: &str) -> Hook {
        Hook {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// Deliver one completion through the tunnel. `false` means "not yet":
    pub(crate) async fn fire(&self, report: &Complete, silent_for: Duration) -> bool {
        if silent_for.as_secs() < SILENCE_SECS {
            // The inductor is still asking; the primary channel owns this
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
pub(crate) async fn supervise(hook: Hook, push: Arc<Push>, shared: Shared) {
    // Consecutive failures at the same stash: feeds [`super::tunnel_missing_hint`]
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
            crawl: None,
            units: 1,
            script: None,
            text: None,
            mp3_b64: None,
            unit_files: Vec::new(),
        }
    }

    /// The inductor's control API, answering success on the one route the
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
        let hook = Hook::from_base("http://127.0.0.1:1", "s3cret");
        assert!(
            !hook
                .fire(&report(), Duration::from_secs(SILENCE_SECS + 1))
                .await
        );
    }
}
