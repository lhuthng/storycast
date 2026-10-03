use super::*;

#[test]
fn machines_pane_shows_no_address_for_a_box_the_account_has_not_addressed_yet() {
    // The confusion this ends: a launched box keyed by an address nothing can
    // dial. `RunInstances` answers before the address exists, and the old
    // fallback printed the *private* address there, a real-looking IP for a box
    // across the internet, which the scheduler then failed to reach every two
    // seconds.
    let mut app = App::new("http://127.0.0.1:8901");
    let pending = bm_core::provision::AwsInstance {
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
        &pending,
        &bm_core::provision::AwsConfig::default(),
    );
    assert_eq!(m.state, MachineState::AwaitingIp);
    assert_eq!(
        super::model::addr_label(&m),
        "—",
        "the ip column says nothing rather than something undialable"
    );
    assert_eq!(
        super::model::machine_label(&m),
        "i-0123456789abcdef0",
        "but the row is named by the handle the account read repairs it by"
    );
    assert_eq!(super::model::machine_kind(&m), "aws");
    // The private address is not lost, it is in the note, for the detail panel
    // and for an operator whose inductor sits in the same VPC.
    assert!(m.note.contains("172.31.21.86"));

    let addressed = bm_core::provision::machine_from_instance(
        &bm_core::provision::AwsInstance {
            public_ip: "52.2.2.2".into(),
            ..pending.clone()
        },
        &bm_core::provision::AwsConfig::default(),
    );
    assert_eq!(addressed.state, MachineState::Initializing);
    assert_eq!(super::model::addr_label(&addressed), "52.2.2.2");

    app.machines = vec![m];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("◐ awaiting-ip"),
        "the state says what is being waited for — and fits the column, which \
         `awaiting-address` did not:\n{text}"
    );
    assert!(
        text.contains("i-0123456789"),
        "and the handle names the row; the column clips it, leaving the \
         recognizable head of the id:\n{text}"
    );
    assert!(
        !text.contains("172.31.21.86"),
        "no undialable address is offered as though it were one:\n{text}"
    );
}

#[test]
fn a_box_whose_address_just_arrived_is_queued_for_onboarding_once() {
    // `:up 3` used to end with boxes nobody would ever provision: the address
    // arrives asynchronously, nothing noticed, and the operator was told to
    // `:prov` each one by hand. The marker `relink` writes is read here.
    let mut app = App::new("http://x");
    let mut newborn = named_machine("52.2.2.2", "box-1");
    newborn.set_state(MachineState::Initializing);
    newborn.note = format!(
        "EC2 i-0123456789abcdef0 (running) · {}",
        bm_core::provision::AWAITING_ONBOARD
    );
    let payload = serde_json::json!({ "machines": [newborn.clone()] });

    app.apply_state(payload.clone());
    assert_eq!(app.pending_onboard.len(), 1, "a new box is offered");
    assert_eq!(app.pending_onboard[0].addr, "52.2.2.2");

    // The dashboard records the hand-out before dispatching, so the ~800 ms poll
    // cannot queue a second provision for a box the first job has not reached
    // yet. Without this the same box would be pushed two or three times.
    app.onboarded.insert("52.2.2.2".into());
    app.apply_state(payload.clone());
    assert!(app.pending_onboard.is_empty(), "not queued twice");

    // The provision job clears the marker by rewriting the note, which is why
    // the trigger is a note and not a field: nothing has to remember to clear it.
    let mut taken = newborn.clone();
    taken.set_state(MachineState::Provisioning);
    taken.note = "provisioning (p) · EC2 i-0123456789abcdef0".into();
    app.apply_state(serde_json::json!({ "machines": [taken.clone()] }));
    assert!(app.pending_onboard.is_empty());
    assert!(
        !app.onboarded.contains("52.2.2.2"),
        "and the guard is released, so a future re-mark would be seen"
    );

    // A box that was already working carries no marker and is never offered
    // the expensive mistake a marker written on *rotation* would cause.
    let mut working = named_machine("52.2.2.3", "box-2");
    working.set_state(MachineState::Configured);
    working.note = "EC2 i-0ffffffffffffffff (running)".into();
    app.apply_state(serde_json::json!({ "machines": [working] }));
    assert!(app.pending_onboard.is_empty());
}

#[test]
fn machines_pane_shows_every_state_word_whole() {
    // A state column that truncates the verdict it exists to show is worse than
    // one with slack: `initializing` rendered as `initializin`, and the first
    // two-word state would have hidden the noun that carried the meaning. This
    // is the test that fails when a state is added and the column is not widened
    // with it.
    for state in [
        MachineState::Unknown,
        MachineState::AwaitingIp,
        MachineState::Initializing,
        MachineState::Probing,
        MachineState::Configured,
        MachineState::Provisioning,
        MachineState::Online,
        MachineState::Offline,
        MachineState::Error,
    ] {
        let mut app = App::new("http://127.0.0.1:8901");
        let mut m = named_machine("52.2.2.2", "box-1");
        m.set_state(state);
        app.machines = vec![m];
        let text = render_text(&mut app, 140, 44);
        // The glyph is part of the cell, so this asserts the word is complete
        // *and* that the pane still renders it with its dot. The trailing space
        // is what makes it a completeness check rather than a prefix check.
        let needle = format!(" {} ", state.as_str());
        assert!(
            text.contains(&needle),
            "`{}` is clipped by the state column:\n{text}",
            state.as_str()
        );
    }
}

#[test]
fn a_parked_machine_reads_relaxed_but_a_fault_still_outranks_it() {
    // The state column is asked "why is nothing happening on this box", and for a
    // parked box the honest answer is `relaxed`, its real state is `online`,
    // which says the opposite of what the operator did.
    let mut parked = named_machine("52.2.2.2", "box-1");
    parked.set_state(MachineState::Online);
    parked.accepting_work = false;
    assert_eq!(super::model::work_label(&parked), "relaxed");
    // Returned to work, the column goes back to reporting the state.
    parked.accepting_work = true;
    assert_eq!(super::model::work_label(&parked), "online");

    // A verdict is news about a box and is not made less true by the park.
    // Showing `relaxed` over it would hide the one fact worth acting on, on the
    // box the operator is least likely to look at again.
    for fault in [MachineState::Offline, MachineState::Error] {
        let mut m = parked.clone();
        m.set_state(fault);
        m.accepting_work = false;
        assert_eq!(
            super::model::work_label(&m),
            fault.as_str(),
            "a parked box that broke must still say so"
        );
    }

    // And it reaches the pane, with its own colour and glyph rather than a
    // clipped state word: `relaxed` is 7 of the column's 13 usable cells.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut relaxed = named_machine("52.2.2.2", "box-1");
    relaxed.set_state(MachineState::Online);
    relaxed.accepting_work = false;
    app.machines = vec![relaxed];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("○ relaxed"),
        "parked reads at a glance, hollow dot and all:\n{text}"
    );
}

#[tokio::test]
async fn g_toggles_the_machines_pane_between_the_table_and_the_graph() {
    let http = reqwest::Client::new();
    let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![named_machine("52.2.2.2", "box-1")];

    // The table is the default, and it says the key that leaves it, the pane
    // advertises the toggle rather than the help screen being the only way in.
    let table = render_text(&mut app, 140, 44);
    assert!(
        table.contains("Machines · g graph"),
        "the pane must name its own toggle:\n{table}"
    );
    assert!(
        table.contains("policy") && table.contains("seen"),
        "the table's columns are what the default shows:\n{table}"
    );

    handle_key(&mut app, key(KeyCode::Char('g')), &http, &job_tx).await;
    assert!(app.machines_graph, "g turns the picture on");
    let graph = render_text(&mut app, 140, 44);
    assert!(
        graph.contains("inductor"),
        "the hub is the inductor, named:\n{graph}"
    );
    assert!(graph.contains("box-1"), "the box is a node:\n{graph}");
    // The picture is drawn, not tabulated: a bus out of the console to every box,
    // and the box itself as art. Neither glyph appears in the table.
    for glyph in ["└", "┬", "|[_]|"] {
        assert!(
            graph.contains(glyph),
            "`{glyph}` is missing — the rack is not drawn:\n{graph}"
        );
    }
    // Lean by design: the state word and the policy column are one `g` away. The
    // footer names them too, so the check is scoped to the Machines pane itself.
    let pane = graph
        .split_once('╭')
        .and_then(|(_, r)| r.split_once('╮'))
        .map(|(body, _)| body.to_string())
        .unwrap_or_default();
    assert!(
        !pane.contains("workers") && !pane.contains("policy"),
        "the graph is not the table with a different shape:\n{pane}"
    );

    // And back, with the state kept, a view preference, not a reset.
    handle_key(&mut app, key(KeyCode::Char('g')), &http, &job_tx).await;
    assert!(!app.machines_graph);
    assert!(render_text(&mut app, 140, 44).contains("policy"));
}

#[test]
fn the_graph_marks_are_distinct_and_group_the_coming_up_states() {
    use super::model::graph_mark;
    use bm_proto::MachineState;
    // The graph spends one character on the verdict, so the characters have to
    // carry it. Colour is the fast path, not the only one: a mono palette and a
    // colour-blind read still tell the five families apart, which is the same
    // promise the table's `state` column keeps by spelling the word out.
    let mark = |state: MachineState, relaxed: bool| {
        let mut m = named_machine("52.2.2.2", "box-1");
        m.set_state(state);
        m.accepting_work = !relaxed;
        graph_mark(&m)
    };
    let families = [
        mark(MachineState::Online, false),
        mark(MachineState::Initializing, false),
        mark(MachineState::Online, true),
        mark(MachineState::Error, false),
        mark(MachineState::Unknown, false),
    ];
    let distinct: std::collections::BTreeSet<_> = families.iter().collect();
    assert_eq!(distinct.len(), 5, "a family shares a glyph: {families:?}");
    // The three states that mean `on its way up` share one mark on purpose: the
    // graph says *coming up*, and which of the three it is is the table's job.
    for s in [
        MachineState::AwaitingIp,
        MachineState::Initializing,
        MachineState::Probing,
    ] {
        assert_eq!(mark(s, false), families[1], "{s:?} is not its own family");
    }
    // A fault outranks a park here as in the table, and a never-contacted box is
    // not a parked one.
    assert_eq!(mark(MachineState::Error, true), families[3]);
    assert_eq!(mark(MachineState::Offline, true), families[3]);
    assert_eq!(mark(MachineState::Unknown, true), families[2]);
}

#[test]
fn the_graph_draws_a_parked_box_hollow_and_says_what_it_is_doing() {
    let mut app = App::new("http://127.0.0.1:8901");
    let mut parked = named_machine("52.2.2.2", "box-1");
    parked.set_state(MachineState::Online);
    parked.accepting_work = false;
    app.machines = vec![parked];
    app.machines_graph = true;
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("○ box-1"),
        "a parked node is the hollow mark beside its name:\n{text}"
    );
    assert!(
        text.contains('—'),
        "no live worker reads as a dash, not as idle:\n{text}"
    );
}

#[test]
fn the_graph_says_how_many_boxes_it_did_not_fit() {
    // A truncated picture with no notice is the failure mode that matters: the
    // operator counts the nodes on screen and nothing tells them the cluster is
    // bigger. The window ends in a count, and the arrows that move it.
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = (0..30)
        .map(|i| named_machine(&format!("52.2.2.{i}"), &format!("box-{i}")))
        .collect();
    app.machines_graph = true;
    let text = render_text(&mut app, 100, 44);
    assert!(
        text.contains("more —"),
        "what did not fit must be counted, not dropped:\n{text}"
    );
    assert!(text.contains('←'), "and the keys that reach it:\n{text}");
    // And the picture still draws its window rather than panicking on the boxes
    // it cannot reach. How many that is depends on the pane's width, so the
    // exact boundary is held by the plan's own tests; what matters here is that
    // the rack is drawn and the first box is in it.
    assert!(text.contains("box-0"), "{text}");
    assert!(!text.contains("box-29"), "the window stops short:\n{text}");
}

#[test]
fn a_rack_node_says_who_is_working_on_what_and_takes_the_stage_colour() {
    // The rack absorbs the Workers pane: the animal the box's worker reports,
    // and the task with its chapter. One `machine_alias` and one `node_stage`
    // feed both, so the rack and the Workers pane cannot call the same box two
    // things or colour it two ways.
    use super::model::{machine_alias, node_stage};
    use super::style::machine_tint;
    let machines = vec![named_machine("52.2.2.2", "box-1")];
    let now = bm_proto::now_secs();
    let mut b = beat("thang-w", "52.2.2.2", 2, "marmot");
    b.stage = Some(Stage::Digest);
    b.task_id = Some("t1".into());
    b.chapter = Some(12);
    b.progress = 0.5;
    let beats = vec![b];

    assert_eq!(
        node_stage(&machines, &beats, "52.2.2.2", now),
        Some("digest")
    );
    assert_eq!(machine_alias(&machines, &beats, "52.2.2.2", now), "marmot");
    assert_eq!(
        machine_tint("online", Some("digest")),
        stage_color("digest"),
        "the art wears the stage's colour, not a second palette"
    );
    // Idle and never-contacted are different answers and different greys: a box
    // up with nothing to do is the cluster working; one nobody has reached is
    // the thing being hunted.
    assert_ne!(machine_tint("online", None), machine_tint("unknown", None));
    // A fault outranks the stage, exactly as it does the table's state column.
    assert_eq!(machine_tint("error", Some("render")), Color::Red);
    assert_eq!(machine_tint("offline", None), Color::Red);

    // And it all reaches the pane.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut m = named_machine("52.2.2.2", "box-1");
    m.set_state(MachineState::Online);
    app.machines = vec![m];
    app.beats = beats;
    app.machines_graph = true;
    let text = render_text(&mut app, 120, 44);
    assert!(
        text.contains("● marmot"),
        "the animal, not the handle:\n{text}"
    );
    assert!(
        text.contains("digest 12 50%"),
        "the task and its chapter, with the progress:\n{text}"
    );
}

#[tokio::test]
async fn up_and_down_walk_a_whole_row_of_the_rack() {
    let http = reqwest::Client::new();
    let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    // More boxes than the pane has rows for, so the window has somewhere to go.
    app.machines = (0..40)
        .map(|i| named_machine(&format!("52.2.2.{i}"), &format!("box-{i}")))
        .collect();
    app.machines_graph = true;
    // The drawer publishes the band's width; a key handler cannot know it.
    let _ = render_text(&mut app, 120, 44);
    let cols = app.graph_cols;
    assert!(
        cols > 1,
        "a rack this wide has a row of boxes in it: {cols}"
    );

    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    assert_eq!(app.selected, cols, "one row of the rack, not one box");
    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    assert_eq!(app.selected, cols + 1, "and one box across");
    handle_key(&mut app, key(KeyCode::Up), &http, &job_tx).await;
    assert_eq!(app.selected, 1);
    let back = render_text(&mut app, 120, 44);
    assert!(back.contains("box-1"), "{back}");

    // The window is the drawer's: it follows the cursor down a band when the
    // cursor would otherwise be off screen, and stays put when it would not.
    // Three rows down out of a two-row page is the first press that must move it.
    let bands = app.graph_band;
    assert_eq!(bands, 0, "the first page");
    for _ in 0..8 {
        handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    }
    let scrolled = render_text(&mut app, 120, 44);
    assert!(app.graph_band > 0, "the rack scrolled: {}", app.graph_band);
    assert!(
        scrolled.contains(&format!("box-{}", app.selected)),
        "the box the cursor is on is on screen:\n{scrolled}"
    );
}

#[test]
fn the_rack_replaces_the_workers_pane_rather_than_sitting_above_it() {
    // Every box in the rack is drawn with the worker standing on it, the same
    // animal name, the same task, the same chapter. A second list of the same
    // facts underneath is the pane arguing with itself, and it costs the rows
    // the rack wanted most. So in rack mode the Workers pane is not drawn and
    // its rows go to the rack and the log.
    let mut app = stats_app();
    let table = render_text(&mut app, 140, 44);
    assert!(table.contains("Workers"), "the table mode keeps the list");

    app.machines_graph = true;
    let rack = render_text(&mut app, 140, 44);
    assert!(
        !rack.contains("╭Workers"),
        "the Workers pane must be gone, not just its header:\n{rack}"
    );
    // And the focus cycle must not park the bright border on a pane that is not
    // on screen, `f` would then have nowhere to go but back.
    assert_eq!(
        crate::tui::app::Panel::Workers.next_visible(true),
        crate::tui::app::Panel::Machines,
        "an unfocusable pane is skipped"
    );
    assert_eq!(
        crate::tui::app::Panel::Machines.next_visible(true),
        crate::tui::app::Panel::Events,
        "and the cycle carries on to the next one that is there"
    );
    assert_eq!(
        crate::tui::app::Panel::Machines.next_visible(false),
        crate::tui::app::Panel::Workers,
        "in table mode every pane is still in the cycle"
    );
}

#[tokio::test]
async fn the_arrows_walk_the_rack_and_the_console_stays_put() {
    let http = reqwest::Client::new();
    let (job_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = (0..12)
        .map(|i| named_machine(&format!("52.2.2.{i}"), &format!("box-{i}")))
        .collect();
    app.machines_graph = true;
    let first = render_text(&mut app, 100, 44);
    assert!(first.contains("box-0"), "{first}");

    // Right twice: the window moves with the cursor, and the console does not
    // it is anchored at the left, so a wide terminal cannot slide it into empty
    // space and leave the boxes to slide past it.
    for _ in 0..2 {
        handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    }
    assert_eq!(app.selected, 2, "the arrows move the cursor along the rack");
    let scrolled = render_text(&mut app, 100, 44);
    assert!(
        scrolled.contains("inductor"),
        "the console is still there:\n{scrolled}"
    );

    // And on: the last box is reachable, and the window cannot walk past it.
    for _ in 0..40 {
        handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    }
    assert_eq!(app.selected, 11, "the cursor stops at the last box");
    let far = render_text(&mut app, 100, 44);
    assert!(far.contains("box-11"), "the last box is on screen:\n{far}");
    // And back off the left end.
    for _ in 0..40 {
        handle_key(&mut app, key(KeyCode::Left), &http, &job_tx).await;
    }
    assert_eq!(app.selected, 0);
    assert!(render_text(&mut app, 100, 44).contains("box-0"));

    // The arrows are the graph's: with the table up they do nothing, rather
    // than walking a list sideways that has no sideways.
    handle_key(&mut app, key(KeyCode::Char('g')), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    assert_eq!(app.selected, 0, "the table's cursor moves down, not across");
}

#[test]
fn machines_pane_names_the_kind_and_the_address() {
    // A row reads `box-1 · rmt · 192.168.2.2`, whose box, where it came from,
    // and how to reach it. The old `role` column said only "worker".
    let mut app = App::new("http://127.0.0.1:8901");
    let mut remote = named_machine("192.168.2.2", "box-1");
    remote.ssh_user = "thang".into();
    let mut aws = named_machine("52.2.2.2", "box-2");
    aws.note = "EC2 i-0123456789abcdef0 (running)".into();
    app.machines = vec![
        Machine::new("127.0.0.1", "local", 22, None, "both"),
        remote,
        aws,
    ];
    let text = render_text(&mut app, 140, 44);
    for head in ["machine", "kind", "ip"] {
        assert!(text.contains(head), "missing `{head}` column:\n{text}");
    }
    for cell in [
        "local",
        "rmt",
        "aws",
        "127.0.0.1",
        "192.168.2.2",
        "52.2.2.2",
    ] {
        assert!(text.contains(cell), "missing `{cell}`:\n{text}");
    }
    // Default policy reads at a glance, most-preferred first.
    assert!(text.contains("M>R>D>C"), "policy summary:\n{text}");
}
