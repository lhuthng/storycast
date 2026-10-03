use super::*;

fn scratch() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(d.path());
    layout.ensure().unwrap();
    std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
    d
}

/// A stub sidecar: 200 on `/health`, and on `/policy` whatever body the
/// test hands it. Returns the base URL.
async fn stub_sidecar(policy_body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 4096];
            let Ok(n) = s.read(&mut buf).await else {
                continue;
            };
            let req: String = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            let body: String = if path.starts_with("/policy") {
                policy_body.into()
            } else {
                r#"{"ok":true}"#.into()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    base
}

#[tokio::test]
async fn sidecar_check_demands_health_plus_policy() {
    // Healthy server with the policy endpoint: serving.
    let up = stub_sidecar(r#"{"allowed_voices":[]}"#).await;
    assert!(sidecar_serving(&up).await);
    // Healthy but stale (a server from before `/policy` existed): not
    // serving, the agent would refuse it too, so preview must not use it.
    let stale = stub_sidecar(r#"{"ok":true}"#).await;
    assert!(!sidecar_serving(&stale).await);
    // Nothing there at all: not serving (and fast, no 5-minute wait).
    assert!(!sidecar_serving("http://127.0.0.1:9").await);
}

/// A full policy list, as the policy panel sends it: every stage, with
/// render set to `enabled` and the rest untouched. Used by the
/// dispatcher's convergence test in `dispatch.rs`.
#[allow(dead_code)]
fn render_policy(enabled: bool) -> Vec<bm_proto::TaskPref> {
    bm_proto::Stage::DEFAULT_PRIORITY
        .iter()
        .map(|s| bm_proto::TaskPref {
            stage: *s,
            enabled: enabled || *s != bm_proto::Stage::Render,
        })
        .collect()
}

/// The policy edit persists, config, not runtime, and sends nothing
/// itself: delivery is the dispatcher's convergence job, which a one-shot
/// push misses in every state that matters (box down at edit time, box
/// rebooting into its default, inductor restarted, worker busy behind the
/// timeout, hand-edited machines.json).
#[tokio::test]
async fn a_policy_edit_persists_and_leaves_delivery_to_the_dispatcher() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::state::Inner::distributing(layout, bm_core::config::Settings::default()),
    ));
    {
        let mut inner = st.lock().await;
        let m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        inner.machines.insert("192.168.2.2".into(), m);
    }
    let resp = set_task_policy(
        State(st.clone()),
        Json(TaskPolicyUpdate {
            addr: "192.168.2.2".into(),
            task_policy: render_policy(false),
        }),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), StatusCode::OK);
    let inner = st.lock().await;
    let policy = inner.machines["192.168.2.2"].task_policy.clone().unwrap();
    assert!(
        !policy
            .iter()
            .find(|p| p.stage == bm_proto::Stage::Render)
            .unwrap()
            .enabled
    );
}

/// A plannable chapter: one run by A, so `0000_Adam.wav` is the whole
/// expected set.
fn one_run_layout() -> (tempfile::TempDir, bm_core::Layout) {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    std::fs::write(
        layout.script(1),
        r#"{"segments":[{"speaker":"A","text":"a full sentence for synthesis here"}]}"#,
    )
    .unwrap();
    std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
    (d, layout)
}

async fn seg_put(
    st: &Shared,
    chapter: u32,
    engine: &str,
    name: &str,
    body: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    use axum::response::IntoResponse;
    let resp = put_segment(
        State(st.clone()),
        Query(SegmentQuery {
            chapter,
            engine: engine.into(),
            name: name.into(),
        }),
        Bytes::from(body),
    )
    .await
    .into_response();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 10)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn segment_state(layout: &bm_core::Layout) -> Shared {
    std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::distributing(
        layout.clone(),
        bm_core::config::Settings::default(),
    )))
}

#[tokio::test]
async fn put_segment_stores_an_expected_file() {
    let (_d, layout) = one_run_layout();
    let st = segment_state(&layout);
    let wav = vec![7u8; 2000];
    let (status, v) = seg_put(&st, 1, "vieneu", "0000_Adam.wav", wav.clone()).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["bytes"], 2000);
    assert_eq!(
        std::fs::read(layout.seg_dir("vieneu", 1).join("0000_Adam.wav")).unwrap(),
        wav
    );
}

#[tokio::test]
async fn put_segment_rejects_unknown_names_and_bad_sizes() {
    let (_d, layout) = one_run_layout();
    let st = segment_state(&layout);
    // Outside the expected set: a worker may not write arbitrary paths.
    let (status, v) = seg_put(&st, 1, "vieneu", "../../../evil.wav", vec![7u8; 2000]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    let (status, v) = seg_put(&st, 1, "vieneu", "nope.wav", vec![7u8; 2000]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    // Below the completeness threshold and above the unit cap: the merger
    // would ignore both, so the store refuses them instead.
    let (status, _) = seg_put(&st, 1, "vieneu", "0000_Adam.wav", vec![7u8; 900]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let over = bm_core::assemble::MAX_SEGMENT_BYTES + 1;
    let (status, v) = seg_put(&st, 1, "vieneu", "0000_Adam.wav", vec![7u8; over]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    // Wrong engine for this run.
    let (status, _) = seg_put(&st, 1, "gemini", "0000_Adam.wav", vec![7u8; 2000]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !layout
            .seg_dir("vieneu", 1)
            .join("../../../evil.wav")
            .exists()
            && std::fs::read_dir(layout.seg_dir("vieneu", 1))
                .map(|rd| rd.count())
                .unwrap_or(0)
                == 0,
        "rejections store nothing"
    );
}

#[tokio::test]
async fn register_and_heartbeat_flip_a_machine_online() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::state::Inner::distributing(layout, bm_core::config::Settings::default()),
    ));
    register(
        State(st.clone()),
        Json(Register {
            worker_id: "w1".into(),
            addr: "192.168.2.2".into(),
            hostname: "box".into(),
            capabilities: vec![],
            sources_stages: Vec::new(),
            tts_url: None,
            version: "0.2.0".into(),
        }),
    )
    .await;
    {
        let inner = st.lock().await;
        assert_eq!(inner.machines["192.168.2.2"].state, MachineState::Online);
    }
    heartbeat(
        State(st.clone()),
        Json(Heartbeat {
            worker_id: "w1".into(),
            addr: "192.168.2.2".into(),
            task_id: None,
            stage: None,
            chapter: None,
            progress: 0.0,
            activity: "idle".into(),
            eta_secs: None,
            ts: bm_proto::now_secs(),
            hostname: "box".into(),
            alias: String::new(),
            cpu_pct: None,
            mem_pct: None,
            mem_gb: None,
            sidecars: None,
            sidecar_gb: None,
            capabilities: vec![],
            sources_stages: Vec::new(),
            sidecar_keep: None,
            tts_threads: None,
            cores: None,
        }),
    )
    .await;
    {
        let inner = st.lock().await;
        let m = &inner.machines["192.168.2.2"];
        assert_eq!(m.state, MachineState::Online);
        assert!(m.last_seen > 0);
    }
}

#[tokio::test]
async fn register_carries_the_registry_handle_to_the_panes() {
    // The hawk hunt, server side: provision logs the registry handle
    // while beats carry the OS hostname, the panes can only agree if
    // register keeps the handle on the machine.
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    bm_core::provision::save_box(
        &layout.machines(),
        &bm_core::provision::LinkedBox {
            name: "hawk".into(),
            addr: "192.168.2.2".into(),
            user: "thang".into(),
            port: 22,
            key: None,
            role: "worker".into(),
            task_policy: None,
            accepting_work: true,
            tts_threads: None,
        },
    )
    .unwrap();
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::state::Inner::distributing(layout, bm_core::config::Settings::default()),
    ));
    register(
        State(st.clone()),
        Json(Register {
            worker_id: "thang-marmot".into(),
            addr: "192.168.2.2".into(),
            hostname: "thang".into(),
            capabilities: vec!["render-segments".into()],
            sources_stages: Vec::new(),
            tts_url: None,
            version: "0.2.3".into(),
        }),
    )
    .await;
    {
        let inner = st.lock().await;
        assert_eq!(
            inner.machines["192.168.2.2"].name, "hawk",
            "panes must say what provision said"
        );
    }
}

#[tokio::test]
async fn a_beating_worker_clears_a_stale_would_not_start_note() {
    // Provision's verdict outlives its launch: the worker did start
    // (via :B, by hand) but the pane kept saying it would not. The
    // first beat with a pulse refutes exactly that wording, and a
    // live note is left alone.
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::state::Inner::distributing(layout, bm_core::config::Settings::default()),
    ));
    {
        let mut inner = st.lock().await;
        let mut m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        m.note = "provisioned but the worker would not start — :prov to retry".into();
        inner.machines.insert("192.168.2.2".into(), m);
        inner
            .workers
            .insert("thang-marmot".into(), "192.168.2.2".into());
    }
    let beat = || Heartbeat {
        worker_id: "thang-marmot".into(),
        addr: "192.168.2.2".into(),
        task_id: None,
        stage: None,
        chapter: None,
        progress: 0.0,
        activity: "idle".into(),
        eta_secs: None,
        ts: bm_proto::now_secs(),
        hostname: "thang".into(),
        alias: "marmot".into(),
        cpu_pct: None,
        mem_pct: None,
        mem_gb: None,
        sidecars: None,
        sidecar_gb: None,
        capabilities: vec![],
        sources_stages: Vec::new(),
        sidecar_keep: None,
        tts_threads: None,
        cores: None,
    };
    heartbeat(State(st.clone()), Json(beat())).await;
    {
        let inner = st.lock().await;
        let note = &inner.machines["192.168.2.2"].note;
        assert!(
            !note.contains("would not start"),
            "a live worker refutes it: {note}"
        );
    }
    {
        let mut inner = st.lock().await;
        inner.machines.get_mut("192.168.2.2").unwrap().note =
            "ready — Online on its first beat".into();
    }
    heartbeat(State(st.clone()), Json(beat())).await;
    {
        let inner = st.lock().await;
        assert_eq!(
            inner.machines["192.168.2.2"].note,
            "ready — Online on its first beat"
        );
    }
}

/// Parking writes intent, and intent has to outlive the process.
///
/// Two things are worth pinning: it lands in `machines.json` (not the
/// ledger, which is cleared on a re-provision) and it does **not** disturb
/// the state, a park is not a phase change, so a box that is `Online` when
/// it is parked must still be `Online` afterwards. Getting that wrong is how
/// a parked box would get stamped `Offline` for going quiet, which is the
/// one thing the operator did not ask for.
#[tokio::test]
async fn the_accepting_route_parks_a_box_and_the_park_outlives_a_restart() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let machines_path = layout.machines();
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::state::Inner::distributing(layout, bm_core::config::Settings::default()),
    ));
    {
        let mut inner = st.lock().await;
        let mut m = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        m.set_state(MachineState::Online);
        inner.machines.insert("192.168.2.2".into(), m);
    }
    let body = |accepting: bool| {
        Json(AcceptingUpdate {
            addr: "192.168.2.2".into(),
            accepting_work: accepting,
        })
    };
    set_accepting_work(State(st.clone()), body(false)).await;
    {
        let inner = st.lock().await;
        let m = &inner.machines["192.168.2.2"];
        assert!(m.relaxed());
        assert_eq!(
            m.state,
            MachineState::Online,
            "a park is intent, not a phase — the box is still alive"
        );
    }
    // Config, not runtime: read straight off the file the next process loads.
    let boxes = bm_core::provision::load_boxes(&machines_path);
    assert_eq!(boxes.len(), 1);
    assert!(
        !boxes[0].accepting_work,
        "the park must survive a restart, so it lives beside the policy"
    );
    // Idempotent, not a toggle: the same request twice leaves it parked.
    set_accepting_work(State(st.clone()), body(false)).await;
    assert!(st.lock().await.machines["192.168.2.2"].relaxed());
    // And waking is the same call with the other value.
    set_accepting_work(State(st.clone()), body(true)).await;
    assert!(!st.lock().await.machines["192.168.2.2"].relaxed());
    assert!(bm_core::provision::load_boxes(&machines_path)[0].accepting_work);
    // Unknown addresses are refused, never created, as every machine route is.
    let reply = set_accepting_work(
        State(st.clone()),
        Json(AcceptingUpdate {
            addr: "10.9.9.9".into(),
            accepting_work: false,
        }),
    )
    .await
    .into_response();
    assert_eq!(reply.status(), axum::http::StatusCode::OK);
    assert!(!st.lock().await.machines.contains_key("10.9.9.9"));
}

#[tokio::test]
async fn machine_state_route_updates_known_boxes_only() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::state::Inner::distributing(layout, bm_core::config::Settings::default()),
    ));
    {
        let mut inner = st.lock().await;
        inner.machines.insert(
            "192.168.2.2".into(),
            Machine::new("192.168.2.2", "thang", 22, None, "worker"),
        );
    }
    set_machine_state(
        State(st.clone()),
        Json(MachineStateUpdate {
            addr: "192.168.2.2".into(),
            state: MachineState::Provisioning,
            note: "pushing sources".into(),
            task_policy: None,
        }),
    )
    .await;
    {
        let inner = st.lock().await;
        let m = &inner.machines["192.168.2.2"];
        assert_eq!(m.state, MachineState::Provisioning);
        assert_eq!(m.note, "pushing sources");
    }
    // Unknown addresses are refused, never created.
    set_machine_state(
        State(st.clone()),
        Json(MachineStateUpdate {
            addr: "10.9.9.9".into(),
            state: MachineState::Error,
            note: String::new(),
            task_policy: None,
        }),
    )
    .await;
    {
        let inner = st.lock().await;
        assert!(!inner.machines.contains_key("10.9.9.9"));
    }
}

#[test]
fn local_roster_reads_cast_and_speakers_from_disk() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
    std::fs::write(
        layout.script(1),
        r#"{"roster":["A"],"segments":[{"speaker":"A","text":"x"}]}"#,
    )
    .unwrap();
    let r = local_roster(&layout);
    assert_eq!(r.source, "offline");
    assert_eq!(r.cast.get("A").map(|s| s.as_str()), Some("Đức Trí"));
    assert!(
        r.characters.contains(&"A".to_string()),
        "{:?}",
        r.characters
    );
    assert!(!r.voices.is_empty(), "catalogue fallback lists voices");
}

#[test]
fn offline_swap_applies_the_same_invalidation_as_live() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    std::fs::write(
        layout.script(1),
        r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
    )
    .unwrap();
    std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
    let seg = layout.seg_dir("vieneu", 1);
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
    std::fs::write(seg.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();

    let msg = offline_swap_apply(&layout, "A", "Minh Triết").expect("offline swap");
    assert!(msg.contains("Đức Trí -> Minh Triết"), "{msg}");
    assert!(msg.contains("offline"), "{msg}");
    assert!(
        !seg.join("0000_Đức Trí.wav").exists(),
        "stale run file must go"
    );
    assert!(
        seg.join("0001_Adam.wav").exists(),
        "other voices keep cache"
    );
    let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
    assert_eq!(cast["A"], "Minh Triết");
}

#[tokio::test]
async fn offline_remix_applies_the_same_invalidation_as_live() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    std::fs::create_dir_all(layout.output()).unwrap();
    std::fs::write(layout.final_mp3(1), b"old mix").unwrap();
    // A published merge implies a script existed: the design fingerprint is
    // computed from it, so without one the chapter has no mix to invalidate
    // and the requeue would be a no-op for a reason that has nothing to do
    // with the remix.
    std::fs::write(
        layout.script(1),
        r#"{"segments":[{"speaker":"A","text":"Chương 1"}]}"#,
    )
    .unwrap();
    bm_core::write_json(
        &layout.ledger(),
        &serde_json::json!({"tasks": [
            {"chapter": 1, "stage": "merge", "state": "done",
             "attempts": 0, "assigned_to": null, "lease_until": null,
             "detail": "", "updated": 0},
        ]}),
    )
    .unwrap();

    let msg = offline_remix_apply(&layout, Some(1.5), Some(0.5), Some(0.0), Some(0.25))
        .expect("offline remix");
    assert!(msg.contains("1.5"), "{msg}");
    assert!(msg.contains("offline"), "{msg}");
    assert!(!layout.final_mp3(1).exists(), "stale mp3 must go");
    let settings = bm_core::config::Settings::load(&layout.settings());
    assert_eq!(
        (
            settings.speed,
            settings.effect_volume,
            settings.music_volume,
            settings.inject_volume
        ),
        (1.5, 0.5, 0.0, 0.25)
    );
    offline_remix_apply(
        &bm_core::Layout::new(d.path()),
        Some(1.0),
        Some(1.0),
        Some(1.0),
        None,
    )
    .unwrap();
    assert_eq!(
        bm_core::config::Settings::load(&layout.settings()).inject_volume,
        0.25
    );
    let ledger: serde_json::Value =
        bm_core::read_json(&layout.bm_state().join("ledger.json")).unwrap();
    assert_eq!(ledger["tasks"][0]["state"], "pending");
}

/// Every entry under `root`, so "the op wrote nothing" can be asserted on
/// the tree rather than on one path somebody remembered to check.
fn tree(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p.strip_prefix(root).unwrap().display().to_string());
        }
    }
    out.sort();
    out
}

#[test]
fn a_rendered_sample_comes_back_as_bytes_and_leaves_no_file() {
    // The complaint this answers: auditioning wrote a clip per voice into
    // `data/previews/`, so a session of A/B-ing left a directory of wavs
    // nobody asked for. The audio now rides the wire and the *client* puts
    // it next to the speaker.
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let before = tree(d.path());

    // A few bytes that are not valid UTF-8, to catch a lossy round trip.
    let wav: Vec<u8> = (0u8..=255).collect();
    let res = audio_result("Đức Trí", "sample", &wav);
    assert!(res.ok, "{}", res.message);
    assert!(
        res.message.contains("sample"),
        "say which half: {}",
        res.message
    );
    assert!(
        !res.message.contains('/'),
        "there is no path to report any more: {}",
        res.message
    );

    let b64 = res.audio_b64.expect("the audio rides along");
    let back = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.as_bytes())
        .expect("valid base64");
    assert_eq!(back, wav, "the bytes survive the wire byte for byte");

    assert_eq!(
        tree(d.path()),
        before,
        "the op wrote nothing under the root"
    );
    assert!(!layout.data().join("previews").exists(), "no audition dump");
}

#[test]
fn an_empty_render_is_a_failure_not_an_empty_sample() {
    // A 200 with no body would otherwise be handed to the client as audio it
    // cannot play, and the client would blame the speaker.
    let res = audio_result("Adam", "line", b"");
    assert!(!res.ok, "{}", res.message);
    assert!(res.audio_b64.is_none(), "an empty body is not audio");
    assert!(res.message.contains("no audio"), "{}", res.message);
    assert!(
        res.message.contains("Adam"),
        "name the voice: {}",
        res.message
    );
}

/// The ledger as `id -> state`, sorted so a diff reads as a ledger diff.
async fn ledger(st: &Shared) -> Vec<(String, &'static str)> {
    let inner = st.lock().await;
    let mut out: Vec<(String, &'static str)> = inner
        .tasks
        .iter()
        .map(|(id, t)| (id.clone(), t.state.as_str()))
        .collect();
    out.sort();
    out
}

async fn shelve(st: &Shared, chapter: u32, stage: bm_proto::Stage) {
    let mut inner = st.lock().await;
    let mut t = bm_proto::Task::new(chapter, stage);
    t.state = bm_proto::TaskState::Shelved;
    t.attempts = 3;
    inner.tasks.insert(t.id(), t);
}

/// `Op::Retry` carries three scopes on one request shape, so the *dispatch*
/// is what decides how much a single call touches. The TUI parses the
/// argument and the state layer does the work; this is the seam between
/// them, and it is where a stage with no chapter must refuse rather than
/// widen to every chapter of that stage.
#[tokio::test]
async fn retry_dispatch_narrows_by_scope_and_refuses_a_bare_stage() {
    let (_d, layout) = one_run_layout();
    let st = segment_state(&layout);
    shelve(&st, 24, bm_proto::Stage::Render).await;
    shelve(&st, 24, bm_proto::Stage::Digest).await;
    shelve(&st, 25, bm_proto::Stage::Digest).await;

    let call = |stage, chapter, force| {
        op(
            State(st.clone()),
            Json(OpRequest {
                op: bm_proto::Op::Retry,
                stage,
                chapter,
                force,
                ..Default::default()
            }),
        )
    };

    // A stage on its own: refused, and the refusal is inert. Widening it
    // would silently requeue every chapter of that stage.
    let res = call(Some(bm_proto::Stage::Render), None, None).await;
    assert!(!res.0.ok, "{}", res.0.message);
    assert!(
        res.0.message.contains("needs a chapter"),
        "say what is missing: {}",
        res.0.message
    );
    assert_eq!(
        ledger(&st).await,
        [
            ("digest:24".to_string(), "shelved"),
            ("digest:25".to_string(), "shelved"),
            ("render:24".to_string(), "shelved"),
        ],
        "a refused scope must not move a single task"
    );

    // A chapter alone: every shelved stage of it, and nothing else.
    let res = call(None, Some(24), None).await;
    assert!(res.0.ok, "{}", res.0.message);
    assert_eq!(
        ledger(&st).await,
        [
            ("digest:24".to_string(), "pending"),
            ("digest:25".to_string(), "shelved"),
            ("render:24".to_string(), "pending"),
        ],
        "ch25 is untouched"
    );

    // Stage + chapter: exactly one task, what the Tasks screen sends.
    // Both of ch24's stages are shelved again so that "one task" and "every
    // shelved stage of the chapter" cannot produce the same ledger: a
    // chapter-wide dispatch would take `digest:24` too.
    shelve(&st, 24, bm_proto::Stage::Render).await;
    shelve(&st, 24, bm_proto::Stage::Digest).await;
    let res = call(Some(bm_proto::Stage::Render), Some(24), None).await;
    assert!(res.0.ok, "{}", res.0.message);
    assert_eq!(
        ledger(&st).await,
        [
            ("digest:24".to_string(), "shelved"),
            ("digest:25".to_string(), "shelved"),
            ("render:24".to_string(), "pending"),
        ],
        "the named task moves and its sibling stage does not"
    );

    // Neither: the blanket retry, which is what a bare `:retry` means.
    let res = call(None, None, None).await;
    assert!(res.0.ok, "{}", res.0.message);
    assert_eq!(
        ledger(&st).await,
        [
            ("digest:24".to_string(), "pending"),
            ("digest:25".to_string(), "pending"),
            ("render:24".to_string(), "pending"),
        ],
        "the blanket scope reaches the other chapter"
    );

    // `force` has to survive the wire, or the Tasks screen's `F` is a plain
    // requeue and the stale artifact it was meant to clear stays in place.
    // The message is the observable: only a forced retry says so.
    let res = call(Some(bm_proto::Stage::Render), Some(24), Some(true)).await;
    assert!(res.0.ok, "{}", res.0.message);
    assert!(
        res.0.message.contains("forced re-run"),
        "force must reach the state layer: {}",
        res.0.message
    );
}

fn dispatch_req(go: bool) -> Json<bm_proto::OpRequest> {
    Json(bm_proto::OpRequest {
        op: bm_proto::Op::Dispatch,
        go: Some(go),
        ..Default::default()
    })
}

/// `dispatch` says what it did, in both directions, and refuses to invent a
/// range: a ledger with no rows gets told how to set one up instead of
/// crawling chapters nobody asked for.
#[tokio::test]
async fn dispatch_holds_goes_and_refuses_to_invent_a_range() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
        layout,
        bm_core::config::Settings::default(),
    )));

    let Json(res) = op(State(st.clone()), dispatch_req(false)).await;
    assert!(res.ok, "{}", res.message);
    assert!(res.message.starts_with("hold:"), "{}", res.message);
    assert!(st.lock().await.dispatch_held);

    let Json(res) = op(State(st.clone()), dispatch_req(true)).await;
    assert!(res.ok, "{}", res.message);
    assert!(
        res.message.contains("nothing is queued yet"),
        "{}",
        res.message
    );
    let inner = st.lock().await;
    assert!(
        !inner.dispatch_held,
        "it did go — the enqueue is the half it could not do"
    );
    assert!(inner.tasks.is_empty(), "and nothing was invented");
}

/// Going enqueues the remainder: the range is set up once, three chapters
/// are finished, and `go` queues 4..6 out of rows that already exist.
#[tokio::test]
async fn dispatch_queues_the_remainder_of_the_range() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
        layout,
        bm_core::config::Settings::default(),
    )));
    {
        let mut inner = st.lock().await;
        inner.settings.start = 1;
        inner.settings.count = 6;
        inner.reconcile(1, 6);
        for n in 1..=3 {
            inner.tasks.get_mut(&format!("merge:{n}")).unwrap().state = bm_proto::TaskState::Done;
        }
    }

    let Json(res) = op(State(st.clone()), dispatch_req(true)).await;
    assert!(res.ok, "{}", res.message);
    assert!(
        res.message.starts_with("go: distributing ch4..6"),
        "{}",
        res.message
    );
    assert!(res.message.contains("queued for ch4..6"), "{}", res.message);
    let inner = st.lock().await;
    assert!(!inner.dispatch_held);
    assert!(
        inner.tasks.contains_key("digest:4"),
        "the remainder is queued"
    );
    assert!(inner.tasks.contains_key("merge:6"));
}

/// `:translate 4 3` authors the range, so the `:go` that follows
/// distributes *its* remainder — not the range the saved run config holds.
#[tokio::test]
async fn translate_authors_the_range_that_go_measures() {
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
        layout,
        bm_core::config::Settings::default(),
    )));
    {
        // The saved run config says the whole book, and nothing is queued.
        let mut inner = st.lock().await;
        inner.settings.start = 1;
        inner.settings.count = 100;
    }

    let Json(res) = op(
        State(st.clone()),
        Json(bm_proto::OpRequest {
            op: bm_proto::Op::Translate,
            start: Some(4),
            count: Some(3),
            ..Default::default()
        }),
    )
    .await;
    assert!(res.ok, "{}", res.message);
    assert!(res.message.contains("translate ch4.."), "{}", res.message);

    let Json(res) = op(State(st.clone()), dispatch_req(true)).await;
    assert!(res.ok, "{}", res.message);
    assert!(
        res.message.starts_with("go: distributing ch4..6"),
        "the range just authored, not the file's: {}",
        res.message
    );
}

/// The dashboard's readout: held-or-going and where the authored range
/// stands, in one place, because a held cluster and a finished one look
/// identical in the task table.
#[tokio::test]
async fn state_reports_dispatch_and_the_range() {
    use axum::response::IntoResponse;
    let d = scratch();
    let layout = bm_core::Layout::new(d.path());
    let st: Shared = std::sync::Arc::new(tokio::sync::Mutex::new(crate::state::Inner::new(
        layout,
        bm_core::config::Settings::default(),
    )));
    {
        let mut inner = st.lock().await;
        inner.settings.start = 1;
        inner.settings.count = 100;
        inner.reconcile(1, 100);
        for n in 1..=3 {
            inner.tasks.get_mut(&format!("merge:{n}")).unwrap().state = bm_proto::TaskState::Done;
        }
    }

    let resp = state(State(st.clone())).await.into_response();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["dispatch"]["held"], serde_json::json!(true));
    assert_eq!(v["dispatch"]["span"], "ch4..100 · 3 done, 97 to go");
    assert_eq!(v["dispatch"]["remaining"], serde_json::json!([4, 100, 3]));
}
