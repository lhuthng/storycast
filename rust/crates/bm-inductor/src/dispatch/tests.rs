use super::*;
use crate::state::Inner;
use bm_core::Layout;
use std::sync::Arc;
use tokio::sync::Mutex;

fn state() -> (tempfile::TempDir, Shared) {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    layout.ensure().unwrap();
    let st: Shared = Arc::new(Mutex::new(Inner::distributing(
        layout,
        bm_core::config::Settings::default(),
    )));
    (d, st)
}

async fn machine_with_policy(st: &Shared, addr: &str, port: u16, render_on: Option<bool>) {
    let mut inner = st.lock().await;
    let mut m = bm_proto::Machine::new(addr, "thang", 22, None, "worker");
    m.task_port = Some(port);
    if let Some(on) = render_on {
        m.task_policy = Some(
            Stage::DEFAULT_PRIORITY
                .iter()
                .map(|s| bm_proto::TaskPref {
                    stage: *s,
                    enabled: on || *s != Stage::Render,
                })
                .collect(),
        );
    }
    inner.machines.insert(addr.to_string(), m);
}

/// A box the account has not given an address yet is tracked, but not
/// dialed.
///
/// Its key is its instance id — a handle for the account read to repair, not
/// something `http` can answer on — so every poll of it would be a
/// guaranteed connection failure: noise in the log, and a workers pane full
/// of boxes that "did not answer" when they were never asked.
#[tokio::test]
async fn an_addressless_box_is_tracked_but_never_dialed() {
    use bm_proto::MachineState;
    let (_d, st) = state();
    machine_with_policy(&st, "10.0.0.5", 8917, None).await;
    {
        let mut inner = st.lock().await;
        let i = bm_core::provision::AwsInstance {
            id: "i-0123456789abcdef0".into(),
            instance_type: "t3.large".into(),
            state: "pending".into(),
            az: "eu-central-1a".into(),
            spot: false,
            public_ip: String::new(),
            private_ip: "172.31.21.86".into(),
            profile: "p".into(),
            launch_time: String::new(),
        };
        let m = bm_core::provision::machine_from_instance(
            &i,
            &bm_core::provision::AwsConfig::default(),
        );
        assert_eq!(m.state, MachineState::AwaitingIp);
        inner.machines.insert(m.addr.clone(), m);
    }
    assert_eq!(
        targets(&st).await,
        vec![("10.0.0.5".to_string(), 8917)],
        "only the dialable box is a target"
    );
    // Once the account hands it an address, it is a target like any other.
    {
        let mut inner = st.lock().await;
        let mut m = inner.machines.remove("i-0123456789abcdef0").unwrap();
        m.addr = "52.2.2.2".into();
        m.id = "52.2.2.2".into();
        m.set_state(MachineState::Initializing);
        inner.machines.insert(m.addr.clone(), m);
    }
    assert_eq!(
        targets(&st).await,
        vec![
            ("10.0.0.5".to_string(), 8917),
            ("52.2.2.2".to_string(), 8917)
        ]
    );
}

/// Parked beats the policy, and is expressible *without* one.
///
/// The order is the whole content of the function: a box the operator parked
/// must give up its sidecar whether or not anyone ever edited its stages, and
/// a box they did not park must keep the answer it had before — including
/// "no opinion", which is what stops the cluster being told `keep=true`
/// every two seconds for ever.
#[test]
fn parking_a_box_is_how_its_sidecar_gets_let_go() {
    use bm_proto::{MachineState, Stage, TaskPref};
    let mut m = bm_proto::Machine::new("10.0.0.5", "ubuntu", 22, None, "worker");
    m.set_state(MachineState::Online);

    // No policy, awake: nothing to say. The worker's own default stands.
    assert_eq!(desired_sidecar_keep(&m), None);

    // No policy, parked: the 2.85 GB comes back anyway. The case a
    // policy-only rule would miss, and the common one — most boxes never
    // have their stages edited.
    m.accepting_work = false;
    assert_eq!(
        desired_sidecar_keep(&m),
        Some(false),
        "parking must not need a stored policy to free the sidecar"
    );

    // Woken with render off: back to the box's own policy, not to `true` —
    // waking must not turn a feature back on that the operator chose to skip.
    let policy = |render: bool| {
        Some(
            Stage::DEFAULT_PRIORITY
                .iter()
                .map(|s| TaskPref {
                    stage: *s,
                    enabled: render || *s != Stage::Render,
                })
                .collect(),
        )
    };
    m.accepting_work = true;
    m.task_policy = policy(false);
    assert_eq!(desired_sidecar_keep(&m), Some(false));

    // And with render on, warm again.
    m.task_policy = policy(true);
    assert_eq!(desired_sidecar_keep(&m), Some(true));
}

fn beat(sidecar_keep: Option<bool>) -> Heartbeat {
    Heartbeat {
        worker_id: "w1".into(),
        addr: "127.0.0.1".into(),
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
        sidecar_keep,
        tts_threads: None,
        cores: None,
    }
}

/// A worker stub that answers 200 and records how many instruction
/// bodies arrived, plus an optional canned status per request.
async fn stub_worker(keep_answer_404: bool) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<tokio::sync::Mutex<Vec<String>>> = Default::default();
    let sink = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = s.read(&mut buf).await else {
                    return;
                };
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if keep_answer_404 && path.starts_with("/sidecar-policy") {
                    // An old agent: 404 and no record.
                    let _ = s.write_all(
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    )
                    .await;
                    return;
                } else if path.starts_with("/sidecar-policy") {
                    sink.lock().await.push(req);
                    r#"{"ok":true}"#
                } else {
                    // /status or anything else: a minimal valid body.
                    r#"{"ok":true}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (port, seen)
}

async fn wait_for(seen: &Arc<tokio::sync::Mutex<Vec<String>>>, n: usize) {
    for _ in 0..100 {
        if seen.lock().await.len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn render_off_tells_the_worker_to_drop_its_sidecar() {
    let (_d, st) = state();
    let (port, seen) = stub_worker(false).await;
    // Loopback: the stub is reachable exactly at the addr the ledger
    // names, which is the shape a real box has.
    machine_with_policy(&st, "127.0.0.1", port, Some(false)).await;
    let peer = Peer {
        addr: "127.0.0.1".into(),
        port,
        token: "t".into(),
    };
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut book = SidecarBook::default();

    // First beat: no stored policy read yet in the book, but desired
    // comes from the ledger — render off ⇒ keep=false must be pushed.
    converge_sidecar_policy(&st, &http, &peer, &mut book, &beat(None)).await;
    wait_for(&seen, 1).await;
    assert_eq!(seen.lock().await.len(), 1, "the instruction was pushed");
    assert!(
        seen.lock().await[0].contains("\"keep\":false"),
        "render off means drop the model"
    );
    assert_eq!(book.delivered, Some(false));

    // Second beat, same belief: converged — no repeat push.
    converge_sidecar_policy(&st, &http, &peer, &mut book, &beat(Some(false))).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        seen.lock().await.len(),
        1,
        "agreement costs nothing — no repeat push"
    );

    // The box reboots back into its default while machines.json still
    // says render off: the *reported* belief is what re-drives the push.
    converge_sidecar_policy(&st, &http, &peer, &mut book, &beat(Some(true))).await;
    wait_for(&seen, 2).await;
    assert_eq!(
        seen.lock().await.len(),
        2,
        "a rebooted box is re-told — delivered alone is not trusted"
    );
}

#[tokio::test]
async fn no_stored_policy_never_pushes_and_404_gives_up_after_five() {
    let (_d, st) = state();
    // No policy at all: the default needs no instruction.
    let (port, seen) = stub_worker(false).await;
    machine_with_policy(&st, "127.0.0.1", port, None).await;
    let peer = Peer {
        addr: "127.0.0.1".into(),
        port,
        token: "t".into(),
    };
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut book = SidecarBook::default();
    converge_sidecar_policy(&st, &http, &peer, &mut book, &beat(None)).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        seen.lock().await.is_empty(),
        "no policy stored — nothing to converge"
    );

    // An old agent answering 404: five tries, then quiet — the log-spam
    // trap the tunnel supervisor already documented.
    let (port404, _seen404) = stub_worker(true).await;
    machine_with_policy(&st, "127.0.0.1", port404, Some(false)).await;
    let peer404 = Peer {
        addr: "127.0.0.1".into(),
        port: port404,
        token: "t".into(),
    };
    let mut book404 = SidecarBook::default();
    for _ in 0..8 {
        converge_sidecar_policy(&st, &http, &peer404, &mut book404, &beat(None)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        book404.failures, SIDECAR_PUSH_TRIES,
        "the retry budget stops the pushes"
    );
    assert!(!book404.drifted(), "no further push is attempted");
}

#[tokio::test]
async fn a_refused_render_releases_its_rows_strike_free() {
    let (_d, st) = state();
    // A chapter-granular render row, assigned to the refusing box.
    {
        let mut inner = st.lock().await;
        let mut t = bm_proto::Task::new(7, Stage::Render);
        t.state = bm_proto::TaskState::Assigned;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(bm_proto::now_secs() + 600);
        t.attempts = 0;
        let id = t.id();
        inner.tasks.insert(id, t);
    }
    let mut inner = st.lock().await;
    let line = inner.release_render_rows("render:7", "worker's policy turns render off");
    assert!(line.contains("1 row"), "{line}");
    let t = inner.tasks.get("render:7").unwrap();
    assert_eq!(t.state, bm_proto::TaskState::Pending, "back to the pool");
    assert_eq!(t.attempts, 0, "a policy refusal is not a strike");
    assert_eq!(t.assigned_to, None);
    assert!(
        t.detail.contains("policy"),
        "the ledger says why: {}",
        t.detail
    );
}
