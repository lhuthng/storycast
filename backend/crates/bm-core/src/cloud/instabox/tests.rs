use super::*;

// The shapes below are what the API answers today, trimmed to the fields read.
const ORGS: &str = r#"{"orgs":[{"id":"org_4014","name":"lhuthng","tier":"free",
  "kind":"personal","personal":true,"role":"admin",
  "allowedSpecs":[{"vcpus":4,"memMiB":8192,"persistentDiskGiB":20}]}]}"#;

const USAGE: &str = r#"{"machines":0,"snapshots":0,"storedBytes":0,
  "limits":{"maxMachines":5,"maxSnapshots":5,"maxStoredGiB":200,
    "machineSpecs":[{"vcpus":4,"memMiB":8192,"persistentDiskGiB":20}]}}"#;

const MACHINE: &str = r#"{"id":"m_agqteugyy5px6hzeadbh4ldjo4","name":"bm-agentb-0",
  "state":"running","runtime":{"ready":true},
  "spec":{"vcpus":4,"memMiB":8192,"persistentDiskGiB":20},
  "guestUser":"user","deleteAt":"2026-11-01T17:22:12Z",
  "createdAt":"2026-11-01T10:22:12Z"}"#;

fn org() -> Org {
    Org::parse(ORGS).expect("the workspace fixture parses")
}

#[test]
fn a_machine_reads_its_state_size_and_deadline() {
    let boxes = parse_machines(MACHINE).expect("a single machine parses");
    let m = &boxes[0];
    assert_eq!(m.id, "m_agqteugyy5px6hzeadbh4ldjo4");
    assert_eq!(m.state, "running");
    assert!(m.ready && m.usable());
    assert_eq!(m.spec.label(), "4/8");
    assert_eq!(
        m.expires_at,
        Some(1_793_553_732),
        "deleteAt is RFC 3339, read as unix seconds"
    );
    assert!(m.note().contains("InstaBox m_agqte"), "{}", m.note());
}

#[test]
fn a_listing_pages_and_orders_by_id() {
    let json = format!(r#"{{"machines":[{MACHINE}],"nextCursor":null}}"#);
    assert_eq!(parse_machines(&json).unwrap().len(), 1);
    let two = r#"{"machines":[
      {"id":"m_b","state":"stopped","spec":{"vcpus":2,"memMiB":4096}},
      {"id":"m_a","state":"creating","spec":{"vcpus":2,"memMiB":4096}}]}"#;
    let boxes = parse_machines(two).unwrap();
    assert_eq!(boxes[0].id, "m_a", "ordered by id");
    assert!(!boxes[0].ready, "creating is not ready");
}

#[test]
fn every_state_word_the_api_uses_maps_once() {
    let view = |state: &str| {
        parse_machines(&format!(
            r#"{{"id":"m","state":"{state}","spec":{{"vcpus":4,"memMiB":8192}}}}"#
        ))
        .unwrap()
        .remove(0)
        .view()
    };
    for state in ["creating", "starting", "running", "resuming"] {
        assert_eq!(view(state).state, Liveness::Running, "{state}");
    }
    for state in ["stopping", "stopped", "suspending", "suspended"] {
        assert_eq!(view(state).state, Liveness::Stopped, "{state}");
    }
    assert_eq!(view("error").state, Liveness::Dead);
    assert_eq!(view("deleting").state, Liveness::Gone);
    // A word this build has not met is not read as dead.
    assert_eq!(view("hibernating").state, Liveness::Unknown);
    assert_eq!(view("running").raw, "running");
}

#[test]
fn an_error_machine_carries_the_providers_reason() {
    let json = r#"{"id":"m_1","state":"error",
      "error":{"code":"no_capacity","message":"no host has room"},
      "spec":{"vcpus":4,"memMiB":8192}}"#;
    let m = parse_machines(json).unwrap().remove(0);
    assert_eq!(m.detail.as_deref(), Some("no host has room"));
    assert!(!m.usable());
}

#[test]
fn the_workspace_reads_its_own_sizes_not_its_tier_name() {
    let org = org();
    assert_eq!(org.tier, "free");
    assert_eq!(org.name, "lhuthng");
    assert_eq!(org.labels(), "4/8");
    assert_eq!(org.smallest().unwrap().label(), "4/8");
    assert_eq!(org.resolve("").unwrap().unwrap().vcpus, 4);
    assert_eq!(org.resolve("4/8").unwrap().unwrap().mem_mib, 8192);
    let err = org.resolve("8/16").expect_err("not in allowedSpecs");
    assert!(err.contains("4/8"), "{err}");
    let err = org.resolve("medium").expect_err("a name is not a size");
    assert!(err.contains("vcpus"), "{err}");
    assert!(org.notes(org.smallest()).iter().any(|n| n.contains("4/8")));
}

#[test]
fn the_machine_cap_comes_from_usage_not_from_the_org() {
    let limits = Limits::parse(USAGE).expect("usage parses");
    assert_eq!(limits.max_machines, 5);
    assert_eq!(limits.max_snapshots, 5);
    assert!(limits.notes()[0].contains("at most 5 machine(s)"));
    // The org answer has no cap of its own: nothing is invented for it.
    assert!(Org::parse(ORGS).is_some());
}

#[test]
fn a_deadline_over_a_day_is_left_off_and_said_so() {
    assert_eq!(delete_after_seconds(Some(60)), (Some(3600), None));
    assert_eq!(delete_after_seconds(None), (None, None));
    assert_eq!(
        delete_after_seconds(Some(0)),
        (None, None),
        "0 is no deadline"
    );
    let (value, said) = delete_after_seconds(Some(48 * 60));
    assert_eq!(value, None);
    assert!(said.expect("a reason").contains("24h"));
    // The provider's own floor.
    assert_eq!(delete_after_seconds(Some(1)).0, Some(60));
}

#[test]
fn a_create_names_a_template_and_a_size_and_no_port() {
    let spec = org().smallest();
    let body = create_body(spec, "agent-browser", "bm-0", Some(3600));
    assert_eq!(body["templateId"], "agent-browser");
    assert_eq!(body["vcpus"], 4);
    assert_eq!(body["memMiB"], 8192);
    assert_eq!(body["deleteAfterSeconds"], 3600);
    // `ports` and `spec` are not create fields: the API refuses unknown ones.
    assert!(body.get("ports").is_none());
    assert!(body.get("spec").is_none());
    // No size asked for: the workspace's default carrier is left to the API.
    let bare = create_body(None, "agent-browser", "bm-0", None);
    assert!(bare.get("vcpus").is_none());
    assert!(bare.get("deleteAfterSeconds").is_none());
}

#[test]
fn a_create_answer_names_the_machine_not_the_operation() {
    let json = r#"{"id":"op_7xk2","machineId":"m_agq","type":"create","state":"queued"}"#;
    assert_eq!(created_machine_id(json).as_deref(), Some("m_agq"));
    assert!(created_machine_id("not json").is_none());
}

#[test]
fn the_worker_url_is_the_published_worker_port() {
    let json = r#"{"enabled":true,"ports":[
      {"port":3000,"name":"Web","url":"https://3000-agq.instabox.tech"},
      {"port":8917,"name":"worker","url":"8917-agq.instabox.tech"}]}"#;
    let ports = parse_ports(json);
    assert_eq!(ports.len(), 2);
    assert_eq!(
        worker_url(&ports).as_deref(),
        Some("https://8917-agq.instabox.tech"),
        "a published port is the only way in"
    );
    // Another port is not the worker.
    let web = r#"{"ports":[{"port":3000,"url":"https://3000-agq.instabox.tech"}]}"#;
    assert!(worker_url(&parse_ports(web)).is_none());
    assert!(parse_ports("not json").is_empty());
}

#[test]
fn a_bad_key_is_named_with_the_command_that_replaces_it() {
    let s = shortfall(
        401,
        r#"{"error":{"code":"invalid_key","message":"key revoked"}}"#,
        None,
        None,
        1,
    );
    assert_eq!(s.reason, Reason::Stop);
    assert_eq!(s.code, "invalid_key");
    assert_eq!(s.message, "key revoked");
    assert!(s.fix.unwrap().contains("ibx login"));
}

#[test]
fn a_quota_refusal_names_the_accounts_own_cap() {
    let limits = Limits::parse(USAGE);
    let s = shortfall(
        429,
        r#"{"error":{"code":"quota_exceeded","message":"5 machines is the limit",
            "retryable":true,"retryAfterSeconds":60}}"#,
        Some(&org()),
        limits.as_ref(),
        2,
    );
    assert_eq!(s.reason, Reason::Clamp);
    assert_eq!(s.want, 2, "it accounts for every box asked for");
    assert!(s.retryable);
    let fix = s.fix.unwrap();
    assert!(fix.contains("5 machine(s)"), "{fix}");
}

#[test]
fn a_spec_refusal_lists_what_this_workspace_allows() {
    let s = shortfall(
        400,
        r#"{"error":{"code":"spec_not_allowed","message":"spec is not available"}}"#,
        Some(&org()),
        None,
        1,
    );
    assert_eq!(s.reason, Reason::Clamp);
    assert!(s.fix.unwrap().contains("4/8"));
}

#[test]
fn capacity_and_cooldowns_are_retryable_with_the_providers_delay() {
    let s = shortfall(
        503,
        r#"{"error":{"code":"no_capacity","message":"no host has room",
            "retryable":true,"retryAfterSeconds":60}}"#,
        None,
        None,
        1,
    );
    assert_eq!(s.reason, Reason::Capacity);
    assert_eq!(s.retry_after_s, Some(60));
    let cooldown = shortfall(
        409,
        r#"{"error":{"code":"machine_cooldown","message":"waking in 5m"}}"#,
        None,
        None,
        1,
    );
    assert!(cooldown.retryable);
}

#[test]
fn a_field_this_build_sends_wrong_is_never_the_operators_to_fix() {
    let s = shortfall(
        400,
        r#"{"error":{"code":"unknown_field","message":"`spec`: did you mean `templateId`?"}}"#,
        None,
        None,
        1,
    );
    assert_eq!(s.reason, Reason::Stop);
    assert_eq!(s.code, "unknown_field");
    assert!(
        s.fix.is_none(),
        "our own body is our own bug: no advice to the operator"
    );
}

#[test]
fn an_unknown_code_keeps_the_providers_words_and_invents_nothing() {
    let s = shortfall(
        418,
        r#"{"code":"teapot_limit","message":"ask support","docs_url":"https://ibx/docs/teapot"}"#,
        None,
        None,
        1,
    );
    assert_eq!(s.reason, Reason::Unknown);
    assert_eq!(s.code, "teapot_limit");
    assert_eq!(
        s.line(),
        "teapot_limit: ask support — https://ibx/docs/teapot"
    );
    // And a body that is not JSON at all still reads as its own words.
    let s = shortfall(500, "upstream exploded", None, None, 1);
    assert_eq!(s.code, "provider_error");
    assert_eq!(s.message, "upstream exploded");
    assert!(s.retryable, "a 5xx is worth one more try");
}
