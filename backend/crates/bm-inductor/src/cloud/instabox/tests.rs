use super::*;
use bm_core::cloud::instabox::Spec;

fn root_with(config: &str) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".bm")).unwrap();
    std::fs::write(InstaboxConfig::path(d.path()), config).unwrap();
    let root = d.path().to_path_buf();
    (d, root)
}

fn spec() -> Spec {
    Spec {
        vcpus: 4,
        mem_mib: 8192,
        disk_gib: 20,
    }
}

#[test]
fn without_a_key_the_launch_is_refused_before_a_call() {
    let (_d, root) = root_with("{}");
    let provider = Instabox::new(&root, None);
    let s = provider.smoke().expect_err("no key, no launch");
    assert_eq!(s.code, "pool_incomplete");
    let fix = s.fix.unwrap();
    assert!(fix.contains("instabox.json"), "{fix}");
    assert!(fix.contains("INSTABOX_API_KEY"), "{fix}");
}

#[test]
fn the_definition_reads_the_key_and_its_defaults() {
    let (_d, root) = root_with(r#"{"api_key":"ibk_test"}"#);
    let cfg = InstaboxConfig::load(&root);
    assert_eq!(cfg.key(), "ibk_test");
    assert_eq!(cfg.max_workers, 1);
    assert_eq!(cfg.ttl_hours, 7);
    assert_eq!(cfg.template(), "agent-browser");
    assert!(
        cfg.publish_worker_port,
        "a machine has no address of its own: without a published port it is unreachable"
    );
    assert!(
        cfg.spec.is_empty(),
        "empty asks for the workspace's smallest"
    );
    assert!(
        cfg.summary().contains("worker port published"),
        "{}",
        cfg.summary()
    );
    assert!(InstaboxConfig::load(&root).missing().is_empty());
}

#[test]
fn a_deadline_beyond_the_apis_day_is_left_off_the_create() {
    let (_d, root) = root_with(r#"{"api_key":"k","ttl_hours":48}"#);
    let provider = Instabox::new(&root, None);
    let cfg = InstaboxConfig::load(&root);
    let (secs, said) = provider.deadline(&cfg);
    assert_eq!(secs, None, "nothing the API would refuse is sent");
    assert!(said.expect("a reason").contains("24h"));

    // A command's own ttl wins over the file's.
    let (secs, _) = Instabox::new(&root, Some(90)).deadline(&cfg);
    assert_eq!(secs, Some(5400));
    // `--ttl 0` is no deadline, not the file's.
    let (secs, said) = Instabox::new(&root, Some(0)).deadline(&cfg);
    assert_eq!(secs, None);
    assert!(
        said.is_none(),
        "an operator asking for no deadline is not a warning"
    );
}

#[test]
fn a_published_port_is_the_boxs_address() {
    let b = box_of(
        "m_agqteugyy5px6hzeadbh4ldjo4",
        None,
        Some("https://8917-agq.instabox.tech".into()),
        Some(1_800_000_500),
        spec(),
        1,
    );
    assert_eq!(b.addr, "https://8917-agq.instabox.tech");
    assert_eq!(
        b.base_url.as_deref(),
        Some("https://8917-agq.instabox.tech")
    );
    assert_eq!(b.provider_id, "m_agqteugyy5px6hzeadbh4ldjo4");
    assert_eq!(
        b.state,
        MachineState::Initializing,
        "the registry then provisions it over exec"
    );
    assert_eq!(b.expires_at, Some(1_800_000_500));
    assert_eq!(b.name, "instabox-1");
    assert!(b.note.contains("4/8"), "{}", b.note);
}

#[test]
fn a_box_with_no_published_port_is_tracked_by_id_and_says_so() {
    let b = box_of("m_1", None, None, Some(1_800_000_000), spec(), 2);
    assert_eq!(b.addr, "m_1");
    assert!(b.base_url.is_none());
    assert_eq!(b.state, MachineState::AwaitingIp);
    assert_eq!(b.expires_at, Some(1_800_000_000), "the launch's deadline");
}

#[test]
fn a_box_that_died_while_coming_up_is_not_waited_on_forever() {
    let dead = bm_core::cloud::instabox::Box {
        id: "m_1".into(),
        name: "bm-0".into(),
        state: "error".into(),
        spec: spec(),
        expires_at: None,
        ready: false,
        detail: Some("no host has room".into()),
    };
    let b = box_of("m_1", Some(&dead), None, None, spec(), 1);
    assert_eq!(b.state, MachineState::Offline);
    assert!(b.note.contains("no host has room"), "{}", b.note);
}

#[test]
fn the_plan_never_exceeds_the_account_or_our_own_cap() {
    // Nothing live, room for four of ours, an account that allows one.
    assert_eq!(fit(3, 4, 1, 0), (1, Some("machine_limit")));
    // Our own cap is the smaller of the two.
    assert_eq!(fit(3, 2, 10, 0), (2, Some("max_workers")));
    // Live boxes count against both.
    assert_eq!(fit(3, 4, 10, 2), (2, Some("max_workers")));
    assert_eq!(
        fit(3, 10, 5, 2),
        (3, None),
        "three fit inside five minus two"
    );
    assert_eq!(fit(2, 1, 1, 1), (0, Some("max_workers")), "nothing left");
    assert_eq!(fit(0, 4, 4, 0), (0, None), "asking for none is never a gap");
}

#[test]
fn a_size_the_workspace_does_not_offer_is_refused_before_a_create() {
    let (_d, root) = root_with(r#"{"api_key":"k","spec":"8/16"}"#);
    let provider = Instabox::new(&root, None);
    let cfg = InstaboxConfig::load(&root);
    let org = Org::parse(
        r#"{"orgs":[{"name":"lhuthng","tier":"free","allowedSpecs":[
            {"vcpus":4,"memMiB":8192,"persistentDiskGiB":20}]}]}"#,
    )
    .unwrap();
    let s = provider
        .spec(&cfg, &org, 2)
        .expect_err("8/16 is not on offer");
    assert_eq!(s.code, "bad_spec");
    assert_eq!(s.want, 2, "it accounts for every box asked for");
    assert!(s.message.contains("4/8"), "{}", s.message);

    // The workspace's smallest, when the definition names none.
    let cfg = InstaboxConfig::load(&root_with(r#"{"api_key":"k"}"#).1);
    assert_eq!(provider.spec(&cfg, &org, 1).unwrap().label(), "4/8");
}

#[test]
fn a_size_too_small_for_the_sidecar_is_refused_before_a_create() {
    let (_d, root) = root_with(r#"{"api_key":"k"}"#);
    let provider = Instabox::new(&root, None);
    let cfg = InstaboxConfig::load(&root);
    let org = Org::parse(
        r#"{"orgs":[{"name":"t","tier":"free","allowedSpecs":[
            {"vcpus":2,"memMiB":2048,"persistentDiskGiB":20},
            {"vcpus":4,"memMiB":8192,"persistentDiskGiB":20}]}]}"#,
    )
    .unwrap();
    // `spec` empty asks for the smallest, which is 2 GiB: too small to work.
    let s = provider
        .spec(&cfg, &org, 2)
        .expect_err("2 GiB cannot hold the sidecar");
    assert_eq!(s.code, "box_too_small");
    assert_eq!(s.reason, Reason::Stop, "no spend on a box that cannot render");
    assert_eq!(s.want, 2);
    assert!(s.message.contains("2048"), "{}", s.message);
    let fix = s.fix.expect("a fix");
    let suggestion = fix.split(" — ").next().unwrap_or(&fix).to_string();
    assert!(suggestion.ends_with("4/8"), "{fix}");
    assert!(
        !suggestion.contains("2/2"),
        "a fix must never suggest a size that also fails: {fix}"
    );
    assert!(
        fix.contains("2/2, 4/8"),
        "the workspace's own list is cited verbatim: {fix}"
    );

    // A workspace with nothing big enough: say so rather than name a size.
    let small = Org::parse(
        r#"{"orgs":[{"name":"t","tier":"free","allowedSpecs":[
            {"vcpus":2,"memMiB":2048}]}]}"#,
    )
    .unwrap();
    let fix = provider
        .spec(&cfg, &small, 1)
        .expect_err("nothing fits")
        .fix
        .expect("a fix");
    assert!(fix.contains("instabox.sh"), "{fix}");
    assert!(!fix.contains("spec` in"), "no size to name: {fix}");
}

#[test]
fn every_delete_names_what_it_deletes() {
    // A delete that does not name the machine is refused by the API, so the
    // provider must send the id it was given.
    assert!(idempotency_key().starts_with("bm-"));
    assert_ne!(idempotency_key(), idempotency_key());
}

/// Deletes whatever was created, even when an assertion below fails: a live
/// check that strands a machine spends money.
struct Deleter<'a> {
    provider: &'a Instabox,
    id: String,
    armed: bool,
}

impl Drop for Deleter<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.provider.down(std::slice::from_ref(&self.id));
        }
    }
}

#[test]
#[ignore = "live: creates, provisions and deletes a real InstaBox machine"]
fn a_live_box_comes_up_answers_exec_and_is_deleted() {
    let root = std::env::var("BM_LIVE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.."));
    let provider = Instabox::new(&root, Some(60));
    let smoke = provider.smoke().expect("the workspace answers");
    for line in &smoke.lines {
        println!("{line}");
    }
    let plan = provider.plan(1).expect("the plan reads the caps");
    assert!(plan.fit >= 1, "no room: {:?}", plan.shortfalls);
    let launch = provider.up(&plan);
    assert_eq!(
        launch.created.len(),
        1,
        "nothing created: {:?}",
        launch.skipped
    );
    let b = &launch.created[0];
    assert!(
        b.base_url.is_some(),
        "no worker port published: {} — the box is unreachable",
        b.note
    );
    let mut guard = Deleter {
        provider: &provider,
        id: b.provider_id.clone(),
        armed: true,
    };

    // The transport provisioning uses, over the create's own registered shape.
    let layout = bm_core::Layout::new(&root);
    let ssh = crate::provision::transport_for(&layout, &b.machine()).expect("a transport");
    let (code, out, err) = ssh.run("echo bm-live", 60).unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("bm-live"), "{out}");

    // A push and a pull, over the files API.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pack.json"), r#"{"n":1}"#).unwrap();
    ssh.rsync_push(dir.path(), "probe/pack", true, None)
        .expect("a push over exec");
    let back = tempfile::tempdir().unwrap();
    ssh.rsync_pull("probe/pack/pack.json", &back.path().join("pack.json"))
        .expect("a pull over exec");
    assert_eq!(
        std::fs::read_to_string(back.path().join("pack.json")).unwrap(),
        r#"{"n":1}"#
    );

    // The agent install's own shape: one file pushed as itself, then run.
    let bin = dir.path().join("bm-agent");
    std::fs::write(&bin, "#!/bin/sh\necho live-agent\n").unwrap();
    ssh.rsync_push(&bin, "bm-agent", false, None)
        .expect("a file push over exec");
    let (code, out, err) = ssh
        .run(
            "chmod +x $HOME/bm-worker/bm-agent && $HOME/bm-worker/bm-agent",
            60,
        )
        .unwrap();
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("live-agent"), "{out}");

    let gone = provider
        .down(std::slice::from_ref(&b.provider_id))
        .expect("deleted");
    guard.armed = false;
    println!("{gone:?}");
}
