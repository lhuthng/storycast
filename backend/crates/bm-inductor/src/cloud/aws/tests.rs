use super::*;
use bm_core::cloud::Reason;

fn facts() -> errors::Facts {
    errors::Facts {
        region: "eu-central-1".into(),
        instance_type: "c7i.xlarge".into(),
        subnet_id: "subnet-1".into(),
        keypair: "team".into(),
        quota_total: Some(32),
        vcpus_used: Some(20),
        vcpus_per_box: Some(4),
    }
}

#[test]
fn an_aws_error_line_splits_into_its_code_and_words() {
    let (code, message) = errors::split(
        "aws ec2 failed: An error occurred (VcpuLimitExceeded) when calling the \
         RunInstances operation: You have requested more vCPU capacity than your current limit",
    );
    assert_eq!(code, "VcpuLimitExceeded");
    assert_eq!(
        message,
        "You have requested more vCPU capacity than your current limit"
    );

    let (code, message) = errors::split("Unable to locate credentials");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("unknown", "Unable to locate credentials")
    );
}

#[test]
fn a_quota_refusal_clamps_and_builds_its_fix_from_live_numbers() {
    let s = errors::from_error(
        "An error occurred (VcpuLimitExceeded) when calling the RunInstances operation: too many",
        &facts(),
    );
    assert_eq!(s.reason, Reason::Clamp);
    assert_eq!(s.code, "VcpuLimitExceeded");
    let fix = s.fix.expect("a mapped code always carries a fix");
    for seen in ["eu-central-1", "L-1216C47A", "24"] {
        assert!(fix.contains(seen), "{seen} missing from {fix}");
    }
}

#[test]
fn capacity_and_credentials_are_told_apart() {
    let s = errors::from_error(
        "An error occurred (InsufficientInstanceCapacity) when calling the RunInstances \
         operation: no capacity",
        &facts(),
    );
    assert_eq!(s.reason, Reason::Capacity);
    assert!(s.fix.unwrap().contains("c7i.xlarge"));

    let s = errors::from_error("aws ec2 failed: Unable to locate credentials", &facts());
    assert_eq!(s.reason, Reason::Stop);
    assert_eq!(s.code, "credentials");
    assert!(s.fix.unwrap().contains("aws login"));
}

#[test]
fn an_unmapped_code_keeps_the_providers_own_words() {
    let s = errors::from_error(
        "An error occurred (WeirdNewLimit) when calling the RunInstances operation: ask support",
        &facts(),
    );
    assert_eq!(s.reason, Reason::Unknown);
    assert_eq!(s.code, "WeirdNewLimit");
    assert_eq!(s.message, "ask support");
    assert!(s.fix.is_none(), "advice we invented is worse than none");
}

#[test]
fn the_type_lookup_reads_vcpus_and_ram_from_one_call() {
    assert_eq!(plan::parse_type("4\t8192\n"), Some((4, 8192)));
    assert_eq!(plan::parse_type(" 2 2048 "), Some((2, 2048)));
    assert_eq!(plan::parse_type(""), None);
    assert_eq!(plan::parse_type("4"), None, "a count alone is not an answer");
    assert_eq!(plan::parse_type("0 0"), None);
}

#[test]
fn a_type_too_small_for_the_sidecar_is_refused_before_a_launch() {
    let cfg = AwsConfig::default();
    assert!(
        check_ram(&cfg, Some((4, 8192))).is_ok(),
        "the shipped default has room"
    );
    let s = check_ram(&cfg, Some((2, 2048))).expect_err("2 GiB cannot hold the sidecar");
    assert_eq!(s.reason, Reason::Stop);
    assert_eq!(s.code, "instance_too_small");
    assert!(s.message.contains("2048"), "{}", s.message);
    let fix = s.fix.expect("a fix");
    assert!(fix.contains("c7i.xlarge"), "{fix}");
    assert!(fix.contains("instance_type"), "{fix}");
    assert!(
        check_ram(&cfg, None).is_ok(),
        "a type this role cannot describe is not a refusal"
    );
}

#[test]
fn headroom_is_the_smallest_of_the_cap_and_the_quota() {
    let h = |live, cap, per, quota| plan::Headroom {
        live,
        cap,
        vcpus_per_box: per,
        quota,
    };
    assert_eq!(h(0, 8, Some(4), Some(32)).fit(10), 8, "our cap binds");
    assert_eq!(h(5, 8, Some(4), Some(32)).fit(10), 3, "the quota binds");
    assert_eq!(h(0, 8, Some(4), None).fit(10), 8, "no quota read: cap only");
    assert_eq!(h(0, 8, None, Some(32)).fit(10), 8, "no type read: cap only");
    assert_eq!(h(2, 1, Some(4), Some(32)).fit(10), 0, "already over cap");
    assert_eq!(h(0, 8, Some(4), Some(32)).fit(3), 3);
}

#[test]
fn the_shortfalls_add_up_to_what_was_not_created() {
    let h = plan::Headroom {
        live: 5,
        cap: 8,
        vcpus_per_box: Some(4),
        quota: Some(32),
    };
    let fit = h.fit(10);
    let short = h.shortfalls(10, fit, &facts());
    assert_eq!(fit, 3);
    assert_eq!(short.iter().map(|s| s.want).sum::<u32>(), 7);
    assert!(short
        .iter()
        .any(|s| s.code == "vcpu_quota" || s.code == "max_workers"));
    assert!(
        h.shortfalls(2, 2, &facts()).is_empty(),
        "nothing to explain"
    );
}

#[test]
fn live_counts_pending_running_and_stopping() {
    let inst = |id: &str, state: &str| AwsInstance {
        id: id.into(),
        instance_type: "c7i.xlarge".into(),
        state: state.into(),
        az: String::new(),
        spot: false,
        public_ip: String::new(),
        private_ip: String::new(),
        profile: String::new(),
        launch_time: String::new(),
    };
    let boxes = vec![
        inst("i-1", "running"),
        inst("i-2", "pending"),
        inst("i-3", "stopping"),
        inst("i-4", "stopped"),
        inst("i-5", "terminated"),
    ];
    assert_eq!(plan::live_count(&boxes), 3);
}

#[test]
fn a_box_without_an_address_is_tracked_not_linked() {
    let cfg = AwsConfig {
        region: "eu-central-1".into(),
        keypairs: [("eu-central-1".to_string(), "team".to_string())].into(),
        ..AwsConfig::default()
    };
    let i = AwsInstance {
        id: "i-0123456789abcdef0".into(),
        instance_type: "c7i.xlarge".into(),
        state: "pending".into(),
        az: "eu-central-1a".into(),
        spot: true,
        public_ip: String::new(),
        private_ip: "10.0.0.5".into(),
        profile: "abc".into(),
        launch_time: String::new(),
    };
    let b = box_of(&i, &cfg, Some(1_800_000_000));
    assert_eq!(
        b.addr, "i-0123456789abcdef0",
        "the id stands in until an IP"
    );
    assert_eq!(b.state, bm_proto::MachineState::AwaitingIp);
    assert_eq!(b.expires_at, Some(1_800_000_000));
    assert_eq!(
        b.machine().note,
        "EC2 i-0123456789abcdef0 (pending) · no public address yet · private 10.0.0.5"
    );
}

/// Every EC2 state word this build can meet, mapped once and kept verbatim.
#[test]
fn ec2_state_words_normalise_and_keep_the_providers_own() {
    let cases = [
        ("pending", Liveness::Running),
        ("running", Liveness::Running),
        ("stopping", Liveness::Stopped),
        ("stopped", Liveness::Stopped),
        ("shutting-down", Liveness::Dead),
        ("terminated", Liveness::Dead),
        // A word AWS adds later is not read as dead: the beat decides.
        ("rebalancing", Liveness::Running),
    ];
    for (raw, want) in cases {
        let i = AwsInstance {
            id: "i-1".into(),
            instance_type: "c7i.xlarge".into(),
            state: raw.into(),
            az: String::new(),
            spot: false,
            public_ip: String::new(),
            private_ip: String::new(),
            profile: String::new(),
            launch_time: String::new(),
        };
        let view = view_of(&i);
        assert_eq!(view.state, want, "{raw}");
        assert_eq!(view.raw, raw, "the raw word is kept for the operator");
        assert_eq!(view.provider_id, "i-1");
    }
}
