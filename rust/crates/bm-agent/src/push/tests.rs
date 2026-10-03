use super::*;
use crate::{LoadProbe, Progress, Sidecar};

fn push(token: &str) -> Arc<Push> {
    push_at(token, &std::env::temp_dir().join("bm-push-unused"))
}

fn push_at(token: &str, root: &std::path::Path) -> Arc<Push> {
    Arc::new(Push {
        who: WorkerIdentity {
            worker_id: "box-1".into(),
            addr: "192.168.2.2".into(),
            hostname: "box".into(),
            alias: "hawk".into(),
            root: root.to_path_buf(),
        },
        token: token.into(),
        shared: Arc::new(Mutex::new(Progress {
            task_id: Some("render:7".into()),
            stage: Some("render".into()),
            chapter: Some(7),
            frac: 0.5,
            activity: "render ch7".into(),
            pending: None,
        })),
        probe: Mutex::new(LoadProbe::new()),
        layout: Layout::new(root),
        settings: Settings::default(),
        sidecar: tokio::sync::Mutex::new(Sidecar::new("http://127.0.0.1:8818")),
        busy: AtomicBool::new(false),
        // "The inductor just spoke" — the watchdog's clock starts now, so
        // a test that never polls does not trip it immediately.
        last_contact: AtomicU64::new(bm_proto::now_secs()),
        last_task_end: AtomicU64::new(bm_proto::now_secs()),
        // The invariant is that the sidecar is kept, and the invariant
        // holds in tests: a test that wants the *refusing* state flips it
        // itself, so nothing else in this file drifts.
        keep_sidecar: AtomicBool::new(true),
        tts_threads: AtomicU64::new(THREADS_UNSET),
        fetch_http: reqwest::Client::builder().no_proxy().build().unwrap(),
        fetch_base: "http://127.0.0.1:1".into(),
    })
}

fn headers(token: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(t) = token {
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {t}").parse().unwrap(),
        );
    }
    h
}

#[test]
fn the_header_is_the_whole_gate() {
    // "Authenticated or off" is the only safe pair of states, and the check
    // is one shape for every endpoint — a per-endpoint scheme is how a
    // forgotten check becomes a public one.
    assert!(check(&headers(Some("s3cret")), "s3cret").is_ok());
    assert!(
        check(&headers(None), "s3cret").is_err(),
        "no header, no entry"
    );
    assert!(
        check(&headers(Some("")), "s3cret").is_err(),
        "empty is not a token"
    );
    assert!(
        check(&headers(Some("s3cres")), "s3cret").is_err(),
        "one byte off"
    );
    assert!(
        check(&headers(Some("s3cretx")), "s3cret").is_err(),
        "a longer guess is not a match"
    );
    // Padding is tolerated, because some clients add it and trimming can
    // never turn a wrong token into a right one.
    assert!(check(&headers(Some("s3cret ")), "s3cret").is_ok());
    assert!(check(&headers(Some(" s3cret")), "s3cret").is_ok());
    // A different scheme is not a bearer token.
    let mut h = HeaderMap::new();
    h.insert(
        axum::http::header::AUTHORIZATION,
        "Basic czNjcmV0".parse().unwrap(),
    );
    assert!(check(&h, "s3cret").is_err(), "Basic is not Bearer");
}

#[test]
fn a_unit_name_is_one_filename_or_it_is_refused() {
    // This is the only place a name from the network reaches `Path::join`,
    // and `join` follows `..` happily — so the guard is the difference
    // between serving a segment and serving `/etc/passwd`.
    for good in [
        "0007_Voice.wav",
        "0007-0012_Voice.wav",
        "title_Narrator.wav",
        // The storage tier's encoded takes: the default names these, and a
        // guard that refused them broke every remote render.
        "t-7a5fee039840532e.mp3",
        "0007_Voice.mp3",
    ] {
        assert!(safe_unit_name(good), "{good} is a real unit name");
    }
    for bad in [
        "../../etc/passwd",
        "..",
        ".",
        "/etc/passwd",
        "a/b.wav",
        "a\\b.wav",
        ".hidden.wav",
        "",
        "0007_Voice.ogg",
        "0007_Voice.wav\0",
    ] {
        assert!(!safe_unit_name(bad), "{bad:?} must not reach Path::join");
    }
    // Long enough to be an attack rather than a name.
    assert!(!safe_unit_name(&format!("{}.wav", "a".repeat(300))));
}

/// A real socket, because the gate's value is that it applies to the route.
/// `no_proxy` for the same reason the production clients use it: an ambient
/// `HTTP_PROXY` would answer the loopback request and this test would be
/// asserting the shell's environment.
async fn serve(push: Arc<Push>) -> (String, reqwest::Client) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(push)).await;
    });
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://{addr}"), http)
}

#[tokio::test]
async fn status_reports_the_worker_and_refuses_without_the_token() {
    let (base, http) = serve(push("s3cret")).await;
    let url = format!("{base}/status");

    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        http.get(&url)
            .bearer_auth("nope")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    let ok = http.get(&url).bearer_auth("s3cret").send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let beat: Heartbeat = ok.json().await.unwrap();
    // The same fields the pull protocol's heartbeat carries, so the
    // inductor's bookkeeping and panes do not care which direction it came
    // from.
    assert_eq!(beat.worker_id, "box-1");
    assert_eq!(beat.addr, "192.168.2.2");
    assert_eq!(beat.alias, "hawk");
    assert_eq!(beat.chapter, Some(7));
    assert_eq!(beat.progress, 0.5);
    assert_eq!(beat.activity, "render ch7");
    assert!(beat.ts > 0);
}

#[tokio::test]
async fn a_unit_is_served_from_disk_and_absent_is_not_an_error() {
    // The collection half of the inversion: the inductor asks for the names
    // it knows it is missing, so "not here" has to be a plain answer rather
    // than a failure — a different box may have rendered it.
    let dir = std::env::temp_dir().join(format!("bm-push-units-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let push = push_at("s3cret", &dir);
    let seg = push.layout.seg_dir("vieneu", 7);
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0007_Voice.wav"), b"RIFF-fake").unwrap();

    let (base, http) = serve(push).await;
    let url = format!("{base}/unit?chapter=7&engine=vieneu&name=0007_Voice.wav");

    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);

    let ok = http.get(&url).bearer_auth("s3cret").send().await.unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["content-type"], "audio/wav");
    assert_eq!(ok.bytes().await.unwrap().as_ref(), b"RIFF-fake");

    // A name this worker does not have: 404, not 500.
    let missing = http
        .get(format!(
            "{base}/unit?chapter=7&engine=vieneu&name=0008_Voice.wav"
        ))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // And a traversal attempt is refused before it touches the filesystem.
    let escape = http
        .get(format!(
            "{base}/unit?chapter=7&engine=vieneu&name=../../../../etc/passwd"
        ))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(escape.status(), 400, "a separator must never reach join");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_sidecar_instruction_is_carried_and_gated_by_the_token() {
    let push = push("s3cret");
    assert!(push.keep_sidecar(), "the default is to keep the sidecar");
    let (base, http) = serve(push.clone()).await;
    let url = format!("{base}/sidecar-policy");

    // No token, no instruction — this carries an order, not a question.
    let refused = http
        .post(&url)
        .json(&serde_json::json!({"keep": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    assert!(
        push.keep_sidecar(),
        "a refused request must not change the flag"
    );

    // The real instruction: drop it. Acknowledged immediately — the
    // answer deliberately does not wait on the sidecar's mutex, which a
    // running render holds for its whole duration.
    let ok = http
        .post(&url)
        .bearer_auth("s3cret")
        .json(&serde_json::json!({"keep": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let v: serde_json::Value = ok.json().await.unwrap();
    assert_eq!(v["ok"], true);
    assert!(!push.keep_sidecar(), "the instruction is in force");

    // And back on: same endpoint, opposite value.
    let ok = http
        .post(&url)
        .bearer_auth("s3cret")
        .json(&serde_json::json!({"keep": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert!(push.keep_sidecar(), "the policy can be restored");
}

/// The beat carries the worker's own sidecar belief, so the dispatcher's
/// convergence can see a box that rebooted into its default.
#[tokio::test]
async fn the_status_answer_reports_the_sidecar_belief() {
    let push = push("s3cret");
    let (base, http) = serve(push.clone()).await;

    let beat: Heartbeat = http
        .get(format!("{base}/status"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(beat.sidecar_keep, Some(true), "the default is reported");

    http.post(format!("{base}/sidecar-policy"))
        .bearer_auth("s3cret")
        .json(&serde_json::json!({"keep": false}))
        .send()
        .await
        .unwrap();
    let beat: Heartbeat = http
        .get(format!("{base}/status"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        beat.sidecar_keep,
        Some(false),
        "the instruction shows in the next beat"
    );
}

/// `POST /task` while the policy says keep no sidecar: the render is
/// skipped — not served by re-warming the model behind the operator's
/// back. The instruction arrives through the **real endpoint**, so the
/// mirror onto the sidecar's own gate is exercised too; running the whole
/// task handler afterwards is as far as a test can reach without a
/// sidecar binary.
#[tokio::test]
async fn a_render_offer_is_skipped_while_the_policy_says_keep_no_sidecar() {
    let push = push("s3cret");
    let (base, http) = serve(push.clone()).await;
    let told = http
        .post(format!("{base}/sidecar-policy"))
        .bearer_auth("s3cret")
        .json(&serde_json::json!({"keep": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(told.status(), 200);
    let offer = serde_json::json!({
        "task_id": "render:7", "chapter": 7, "stage": "render",
        "root": "/tmp", "engine": "vieneu",
    });
    let resp = http
        .post(format!("{base}/task"))
        .bearer_auth("s3cret")
        .json(&offer)
        .send()
        .await
        .unwrap();
    // **403, not 200 with `ok: false`**: the dispatcher reads 403 as
    // "refused on policy — release the rows strike-free", while a failed
    // report would cost the chapter one of its three strikes. Three
    // policy flips would otherwise shelve a chapter for a decision the
    // operator made.
    assert_eq!(resp.status(), 403);
    // The render restored the permission on its way out, and the flag
    // itself is untouched — the inductor's instruction still says "drop".
    assert!(
        !push.keep_sidecar(),
        "the endpoint flag stays as the inductor set it"
    );
}

/// A render offered while the policy still says "keep": the normal path.
/// No sidecar binary exists in a test box, so the render fails at `ensure`
/// — but with the *startup* error, not the policy refusal. The gate must
/// not change what an allowed render does.
#[tokio::test]
async fn a_render_offer_under_the_default_policy_runs_the_normal_path() {
    let push = push("s3cret");
    let (base, http) = serve(push).await;
    let offer = serde_json::json!({
        "task_id": "render:7", "chapter": 7, "stage": "render",
        "root": "/tmp", "engine": "vieneu",
    });
    let resp = http
        .post(format!("{base}/task"))
        .bearer_auth("s3cret")
        .json(&offer)
        .send()
        .await
        .unwrap();
    let done: Complete = resp.json().await.unwrap();
    assert!(!done.ok);
    assert!(
        !done.detail.contains("policy"),
        "an allowed render must not be gated: {}",
        done.detail
    );
}

#[tokio::test]
async fn a_second_task_is_refused_while_one_runs() {
    // One task at a time, which is what the pull protocol's single slot
    // gave. The inductor owns the schedule; a worker holding a backlog is
    // one whose lease the inductor cannot reason about.
    let push = push("s3cret");
    push.busy.store(true, Ordering::SeqCst);
    let (base, http) = serve(push).await;
    let offer = serde_json::json!({
        "task_id": "render:7", "chapter": 7, "stage": "render",
        "root": "/tmp", "engine": "vieneu",
    });
    let busy = http
        .post(format!("{base}/task"))
        .bearer_auth("s3cret")
        .json(&offer)
        .send()
        .await
        .unwrap();
    assert_eq!(busy.status(), 409);
    assert!(
        busy.text().await.unwrap().contains("render:7"),
        "the refusal names what is running"
    );

    // Without the token it is refused before the busy check, so an
    // unauthenticated caller cannot even learn what is running.
    let unauth = http
        .post(format!("{base}/task"))
        .json(&offer)
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
}
