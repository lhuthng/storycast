use super::*;
use bm_proto::MachineState;

#[test]
fn ttl_reads_hours_minutes_and_zero() {
    assert_eq!(parse_ttl_minutes("7"), Some(420));
    assert_eq!(parse_ttl_minutes("7h"), Some(420));
    assert_eq!(parse_ttl_minutes("90m"), Some(90));
    assert_eq!(parse_ttl_minutes("0"), Some(0), "zero is a real answer");
    assert_eq!(parse_ttl_minutes("1.5h"), None);
    assert_eq!(parse_ttl_minutes("h"), None);
    assert_eq!(parse_ttl_minutes(""), None);
    assert_eq!(ttl_label(Some(420)), "7h");
    assert_eq!(ttl_label(Some(90)), "90m");
    assert_eq!(ttl_label(Some(0)), "no deadline");
}

#[test]
fn a_shortfall_prints_the_providers_words_then_its_fix() {
    let s =
        Shortfall::new(Reason::Clamp, "vcpu_quota", "quota allows 6").with_fix("raise the quota");
    assert_eq!(
        s.line(),
        "vcpu_quota: quota allows 6 — fix: raise the quota"
    );
    let s = Shortfall::new(Reason::Unknown, "WeirdNewLimit", "no idea").with_docs("docs/x");
    assert_eq!(s.line(), "WeirdNewLimit: no idea — docs/x");
    let s = Shortfall::new(Reason::Stop, "no_region", "region is empty");
    assert_eq!(s.line(), "no_region: region is empty");
}

#[test]
fn a_box_below_the_sidecars_footprint_is_refused_with_its_numbers() {
    let s = too_small("box_too_small", 2048, "set `spec` to 4/8");
    assert_eq!(
        s.reason,
        Reason::Stop,
        "no spend on a box too small to work"
    );
    assert_eq!(s.code, "box_too_small");
    for seen in [
        "2048".to_string(),
        SIDECAR_RESIDENT_MIB.to_string(),
        MIN_BOX_MIB.to_string(),
    ] {
        assert!(
            s.message.contains(&seen),
            "{seen} missing from {}",
            s.message
        );
    }
    assert_eq!(s.fix.as_deref(), Some("set `spec` to 4/8"));
    // The floor comes from the measurement, plus room for the agent and the OS.
    const { assert!(MIN_BOX_MIB > SIDECAR_RESIDENT_MIB) };
    // A 4 GiB box leaves ~3.9 GiB usable, below the sidecar and its agent.
    const { assert!(MIN_BOX_MIB > 4096) };
}

#[test]
fn a_launch_counts_what_was_skipped_by_reason() {
    let empty = Launch {
        created: Vec::new(),
        skipped: Vec::new(),
    };
    assert_eq!(empty.summary(), "launched 0");

    let partial = Launch {
        created: Vec::new(),
        skipped: vec![
            Shortfall::new(Reason::Clamp, "max_workers", "cap").covering(7),
            Shortfall::new(Reason::Capacity, "no_capacity", "none").covering(2),
        ],
    };
    assert_eq!(
        partial.summary(),
        "launched 0 — 9 not created (fewer, capacity)"
    );
}

#[test]
fn a_cloud_box_becomes_a_registry_row_with_its_deadline() {
    let b = CloudBox {
        provider: Provider::Aws,
        provider_id: "i-abc".into(),
        name: "hawk".into(),
        addr: "203.0.113.9".into(),
        note: "EC2 i-abc (pending)".into(),
        state: MachineState::Initializing,
        ssh_user: "ubuntu".into(),
        ssh_port: 22,
        key: Some(".bm/aws/eu-central-1.pem".into()),
        expires_at: Some(1_800_000_000),
        base_url: None,
    };
    let m = b.machine();
    assert_eq!((m.addr.as_str(), m.name.as_str()), ("203.0.113.9", "hawk"));
    assert_eq!(m.state, MachineState::Initializing);
    assert_eq!(m.expires_at, Some(1_800_000_000));
    assert_eq!(m.ssh_user, "ubuntu");
    assert_eq!(m.note, "EC2 i-abc (pending)");
    assert_eq!(m.provider.as_deref(), Some("aws"));
    assert_eq!(m.provider_id.as_deref(), Some("i-abc"));
    assert!(m.base_url.is_none(), "AWS dials the address it gave us");
}

/// The verdict table from `docs/cloud.md`, one row per line.
#[test]
fn a_running_box_is_kept_only_while_it_answers() {
    let now = 1_800_000_000;
    let running = ProviderView::new("i-1", Liveness::Running, "running");
    assert_eq!(
        fate(&running, Some(now - 5), None, now),
        Fate::Keep,
        "a fresh beat is life"
    );
    assert_eq!(
        fate(&running, Some(now - 5), Some(now + 60), now),
        Fate::Keep,
        "before the deadline"
    );
    assert_eq!(
        fate(&running, Some(now - BEAT_WINDOW_SECS - 1), None, now),
        Fate::Suspect {
            since: now - BEAT_WINDOW_SECS - 1 + BEAT_WINDOW_SECS
        },
        "silence is suspect, never death"
    );
    assert_eq!(
        fate(&running, None, None, now),
        Fate::Suspect { since: now },
        "never heard from it at all"
    );
}

#[test]
fn the_providers_word_is_the_verdict_for_stopped_and_gone() {
    let now = 1_800_000_000;
    let beat = Some(now);
    let cases = [
        (
            Liveness::Stopped,
            "stopped",
            Fate::Dead {
                why: "stopped".into(),
            },
        ),
        (
            Liveness::Dead,
            "terminated",
            Fate::Dead {
                why: "terminated".into(),
            },
        ),
        (Liveness::Gone, "", Fate::Dead { why: "gone".into() }),
        (Liveness::Unknown, "", Fate::Suspect { since: now }),
    ];
    for (state, raw, want) in cases {
        let view = ProviderView::new("i-1", state, raw);
        assert_eq!(fate(&view, beat, None, now), want, "{}", raw);
    }
}

#[test]
fn a_providers_own_explanation_wins_over_its_state_word() {
    let now = 1_800_000_000;
    let view = ProviderView::new("m-1", Liveness::Stopped, "paused").saying("idle-paused");
    assert_eq!(
        fate(&view, None, None, now),
        Fate::Dead {
            why: "idle-paused".into()
        }
    );
}

#[test]
fn a_stopping_box_is_kept_until_the_time_the_provider_gave() {
    let now = 1_800_000_000;
    let soon = ProviderView::new("i-1", Liveness::Stopping, "running")
        .saying("spot reclaim")
        .reclaim_at(Some(now + 120));
    assert_eq!(fate(&soon, Some(now), None, now), Fate::Keep);
    let reached = soon.clone().reclaim_at(Some(now - 1));
    assert_eq!(
        fate(&reached, Some(now), None, now),
        Fate::Dead {
            why: "spot reclaim".into()
        }
    );
    let untimed = ProviderView::new("i-2", Liveness::Stopping, "shutting-down");
    assert_eq!(
        fate(&untimed, Some(now), None, now),
        Fate::Dead {
            why: "shutting-down".into()
        },
        "no window given: nothing to wait for"
    );
}

#[test]
fn our_deadline_ends_a_box_the_provider_still_calls_running() {
    let now = 1_800_000_000;
    let running = ProviderView::new("i-1", Liveness::Running, "running");
    assert_eq!(
        fate(&running, Some(now), Some(now), now),
        Fate::Dead {
            why: "past its deadline".into()
        },
        "the deadline is conclusive whatever the provider says"
    );
}

#[test]
fn a_failed_poll_is_never_a_death() {
    let now = 1_800_000_000;
    let unknown = ProviderView::new("i-1", Liveness::Unknown, "");
    assert_eq!(
        fate(&unknown, Some(now), None, now),
        Fate::Suspect { since: now }
    );
    // And it stays a suspicion on every later tick.
    let later = fate(&unknown, Some(now), None, now + 3600);
    assert!(matches!(later, Fate::Suspect { .. }), "{later:?}");
}
