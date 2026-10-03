use super::*;

// --- rendering ----------------------------------------------------------

#[test]
fn the_log_pane_has_one_title_aliases_and_local_time() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.log_at(Level::Ok, "[192.168.2.2] enrolled x");
    app.log_at(Level::Error, "localhost-99: render failed: boom");
    app.log_at(Level::Info, "reconcile: nothing certain to fold");
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("Logs"), "pane renamed:\n{text}");
    assert!(!text.contains("Events"), "no stale title anywhere:\n{text}");
    // Address heads are box-level lines: shown as written, never hashed
    // into a phantom worker name.
    assert!(
        text.contains("[192.168.2.2]"),
        "machine line verbatim:\n{text}"
    );
    assert!(
        text.contains("[localhost-99]"),
        "unknown worker id verbatim:\n{text}"
    );
    assert!(
        text.contains("reconcile: nothing certain"),
        "plain lines pass through:\n{text}"
    );
}

#[tokio::test]
async fn paging_up_past_the_log_stops_at_the_buffers_own_top() {
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://x");
    for i in 0..20 {
        app.log_at(Level::Info, format!("line {i}"));
    }
    // A page is a page: the draw publishes the pane height, so the test pins
    // it to 4 and the walk is in screenfuls, not an arbitrary 5.
    app.events_rows = 4;

    // One page back is 4 lines; a page and a half lands on 4's second press…
    handle_key(&mut app, key(KeyCode::PageUp), &http, &job_tx).await;
    assert_eq!(app.events_scroll, 4);
    handle_key(&mut app, key(KeyCode::PageUp), &http, &job_tx).await;
    assert_eq!(app.events_scroll, 8);

    // …and the walk back costs exactly what the walk out did.
    handle_key(&mut app, key(KeyCode::PageDown), &http, &job_tx).await;
    assert_eq!(app.events_scroll, 4);
    handle_key(&mut app, key(KeyCode::PageDown), &http, &job_tx).await;
    assert_eq!(
        app.events_scroll, 0,
        "a second page-down is already at newest"
    );

    // Far more pages than lines: the buffer's own top is a real edge, not a
    // number that runs to a thousand.
    for _ in 0..50 {
        handle_key(&mut app, key(KeyCode::PageUp), &http, &job_tx).await;
    }
    assert_eq!(
        app.events_scroll, 20,
        "scroll stopped at the buffer length, not 400"
    );
    // At that edge the title names the edge instead of showing a stuck count.
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("oldest kept line"),
        "the top of the buffer is legible:\n{text}"
    );
}

#[test]
fn a_new_line_holds_the_reading_position_instead_of_shoving_it() {
    // The scroll offset is a distance from the live tail, so an arriving line
    // grows that distance and the line under the operator's eyes stays put.
    // Pinned to newest, the tail simply follows.
    let mut app = App::new("http://x");
    for i in 0..10 {
        app.log_at(Level::Info, format!("line {i}"));
    }
    app.events_scroll = 3;
    // The top visible line is `len - scroll`; that index must survive a push.
    let top_before = app.events[app.events.len() - app.events_scroll]
        .text
        .clone();
    app.log_at(Level::Info, "arrives while reading");
    assert_eq!(app.events_scroll, 4, "the distance grew by exactly one");
    let top_after = &app.events[app.events.len() - app.events_scroll];
    assert_eq!(
        top_after.text, top_before,
        "the view is still on the same line"
    );

    // Pinned to newest, arrivals scroll by, the tail follows.
    app.events_scroll = 0;
    app.log_at(Level::Info, "tail follows");
    assert_eq!(app.events_scroll, 0);
}

#[test]
fn log_lines_use_reported_aliases_when_beats_carry_them() {
    // The mismatch: the Workers pane said `marmot` while the log line
    // said `[hare] [thang-29486]`, the log hashed the raw id instead of
    // asking the beats.
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![beat("thang-29486", "192.168.2.2", 2, "marmot")];
    app.log_at(Level::Info, "[thang-29486] heartbeat slow");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[marmot]"),
        "the reported alias wins:\n{text}"
    );
    // ...but an id no beat knows stays itself. Hashing it once minted
    // `[hawk]` for the address `192.168.2.2`, a worker that never
    // existed, hunted across every pane.
    app.log_at(Level::Ok, "[ghost-1] render:24 done");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[ghost-1]"),
        "unknown ids stay verbatim:\n{text}"
    );
}

#[test]
fn task_lines_render_compact_with_their_own_colors() {
    use super::model::task_event;
    // Completions: tag, outcome, duration; remote workers read retrieved.
    assert!(matches!(
        task_event("[w1] merge:23 done in 14.6s — merge ch23 -> /x/y.mp3"),
        Some(super::model::TaskEvent::Done { task, secs, .. })
            if task == "merge:23" && secs == "14.6s"
    ));
    assert!(
        task_event("[w1] render:24 done").is_none(),
        "no duration, no shape"
    );
    assert!(
        task_event("reconcile done in 2s").is_none(),
        "no head, no shape"
    );
    // Failures keep the note and the reason.
    assert!(matches!(
        task_event("[w1] render:7 FAILED (will retry): boom"),
        Some(super::model::TaskEvent::Failed {
            note: Some("will retry"),
            reason: "boom",
            ..
        })
    ));
    assert!(matches!(
        task_event("render:5 SHELVED without retry: kaput (press u to requeue)"),
        Some(super::model::TaskEvent::Shelved {
            reason: "kaput",
            ..
        })
    ));

    let mut app = App::new("http://127.0.0.1:8901");
    let mut remote = beat("thang-29486", "192.168.2.2", 2, "marmot");
    remote.cores = Some(8);
    app.beats = vec![remote, beat("w-local", "127.0.0.1", 2, "")];
    app.log_at(Level::Ok, "[thang-29486] render:23 done in 255.3s");
    app.log_at(Level::Ok, "[w-local] crawl:4 done in 1.2s");
    app.log_at(
        Level::Warn,
        "[thang-29486] render:7 FAILED (will retry): boom",
    );
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("[T:render:23]"), "task tag:\n{text}");
    assert!(text.contains("Complete in"), "outcome:\n{text}");
    assert!(text.contains("255.3s"), "duration:\n{text}");
    assert_eq!(
        text.matches("(retrieved)").count(),
        1,
        "only the remote worker's product rode home:\n{text}"
    );
    assert!(
        !text.contains("255.3s —"),
        "the detail tail is dropped:\n{text}"
    );
    assert!(text.contains("[T:crawl:4]"), "local tag too:\n{text}");
    assert!(text.contains("[T:render:7]"), "failure tag:\n{text}");
    assert!(text.contains("Failed"), "failure outcome:\n{text}");
    assert!(
        text.contains("(will retry): boom"),
        "note and reason kept:\n{text}"
    );
}

#[test]
fn an_address_head_never_becomes_a_phantom_worker() {
    // The exact confusion: provision lines are tagged with the box
    // address while its worker beats as `thang-marmot`. The log must
    // show the address, not hash it into a third name.
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![beat("thang-marmot", "192.168.2.2", 2, "marmot")];
    app.log_at(
        Level::Ok,
        "[192.168.2.2] already configured (agent 0.2.3 + tts sidecar)",
    );
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("[192.168.2.2]"),
        "the address stays an address:\n{text}"
    );
    assert!(
        !text.contains("[hawk]"),
        "no phantom worker is minted:\n{text}"
    );
}

#[test]
fn the_size_guard_replaces_the_dashboard_below_the_floor() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 60, 16);
    assert!(text.contains("too small"), "{text}");
    assert!(
        !text.contains("Machines"),
        "no clipped panes behind the notice:\n{text}"
    );
    assert!(
        text.contains("60×16"),
        "the notice names the actual size:\n{text}"
    );
    assert!(text.contains("76×20"), "and the requirement:\n{text}");
}

#[test]
fn the_size_guard_does_not_panic_on_a_degenerate_area() {
    let mut app = App::new("http://127.0.0.1:8901");
    // Only the first has room for the full notice; the slivers must simply
    // not panic, and must never leak a clipped dashboard.
    for (w, h) in [(60u16, 16u16), (1, 1), (0, 0), (200, 3), (3, 200)] {
        let text = render_text(&mut app, w, h);
        assert!(
            !text.contains("Machines"),
            "{w}x{h} rendered panes:\n{text}"
        );
        assert!(!text.contains("Workers"), "{w}x{h} rendered panes:\n{text}");
    }
}

#[test]
fn the_compact_tier_folds_the_tasks_pane_into_the_footer() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 80, 24);
    assert!(text.contains("Machines"), "{text}");
    assert!(text.contains("Workers"), "{text}");
    assert!(text.contains("Logs"), "Logs keeps its pane:\n{text}");
    assert!(
        !text.contains("┌Tasks"),
        "the Tasks pane is collapsed:\n{text}"
    );
    assert!(
        text.contains("tasks:"),
        "its roll-up takes its place:\n{text}"
    );
}

#[test]
fn the_full_tier_shows_every_pane_and_the_new_key() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 140, 44);
    for pane in ["Machines", "Workers", "Tasks", "Logs"] {
        assert!(text.contains(pane), "{pane} is missing:\n{text}");
    }
    assert!(
        text.contains("S cast"),
        "the cast key is advertised:\n{text}"
    );
}

#[test]
fn an_empty_cluster_says_what_to_do_in_every_tier() {
    let mut app = App::new("http://127.0.0.1:8901");
    for (w, h) in [(80u16, 24u16), (140, 44)] {
        let text = render_text(&mut app, w, h);
        assert!(
            text.contains("no machines in the cluster"),
            "{w}x{h}:\n{text}"
        );
        assert!(text.contains("no workers connected"), "{w}x{h}:\n{text}");
        assert!(
            text.contains("nothing has happened yet"),
            "{w}x{h}:\n{text}"
        );
    }
}

#[test]
fn workers_pane_hides_stale_beats_and_shows_reported_aliases() {
    // The "two hares": a dead worker rendered as an idle row next to the
    // live one. Only fresh beats may draw; the name shown is the worker's
    // own (kept across restarts), with the id hash as fallback for older
    // agents that report none.
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![
        beat("thang-1", "192.168.2.2", 2, "quokka"),
        beat("thang-0", "192.168.2.2", 900, "quokka"),
        beat("localhost-9", "127.0.0.1", 3, ""),
    ];
    let text = render_text(&mut app, 140, 44);
    assert_eq!(
        text.matches("quokka").count(),
        2,
        "live once per pane (Workers, Stats), stale never:\n{text}"
    );
    let (fallback, _) = worker_alias("localhost-9");
    assert!(
        text.contains(fallback),
        "old agents keep the hash name:\n{text}"
    );
    assert!(
        !text.contains("no live workers"),
        "a live row draws:\n{text}"
    );
}

#[test]
fn workers_pane_says_so_when_every_beat_is_stale() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.beats = vec![beat("thang-0", "192.168.2.2", 900, "quokka")];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("no live workers"),
        "stale is not idle:\n{text}"
    );
    assert!(!text.contains("quokka"), "stale rows never draw:\n{text}");
}

#[test]
fn workers_pane_hides_ghosts_of_offline_boxes() {
    use super::model::beat_backed;
    use bm_proto::MachineState;
    // A beat the box's Offline verdict postdates is a ghost, not a worker.
    let mut m = named_machine("192.168.2.2", "hawk");
    m.set_state(MachineState::Offline);
    let ghost = beat("thang-marmot", "192.168.2.2", 30, "marmot");
    assert!(!beat_backed(&[m.clone()], &ghost));
    // A beat newer than the verdict still counts, one slow poll flickers
    // the dot without deleting the row.
    m.state_since = m.state_since.saturating_sub(100);
    let fresh = beat("thang-marmot", "192.168.2.2", 2, "marmot");
    assert!(beat_backed(&[m.clone()], &fresh));
    // Anything but Offline backs its beats; unknown boxes back everything.
    m.set_state(MachineState::Online);
    assert!(beat_backed(&[m.clone()], &ghost));
    assert!(beat_backed(&[], &ghost));

    // And the pane agrees: no ghost rows, and the count with them.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut off = named_machine("192.168.2.2", "hawk");
    off.set_state(MachineState::Offline);
    app.machines = vec![off];
    app.beats = vec![beat("thang-marmot", "192.168.2.2", 30, "marmot")];
    let text = render_text(&mut app, 140, 44);
    // The Workers block only (Stats keeps per-worker history rows, which
    // legitimately still name the box).
    let workers = text
        .split_once("Workers ·")
        .and_then(|(_, rest)| rest.split_once("╭"))
        .map(|(block, _)| block)
        .unwrap_or_default();
    assert!(
        !workers.contains("marmot"),
        "ghost rows never draw:\n{text}"
    );
    assert!(
        text.contains("0 live"),
        "the count drops with them:\n{text}"
    );
}

#[test]
fn machine_name_prefers_the_registry_handle() {
    use super::model::machine_name;
    let machines = vec![named_machine("192.168.2.2", "hawk")];
    // Known box: the handle the provision log used, not the OS hostname.
    let b = beat("thang-marmot", "192.168.2.2", 2, "marmot");
    assert_eq!(machine_name(&machines, &b), "hawk");
    // Unnamed box: the reported hostname, then the address.
    let machines = vec![Machine::new("192.168.2.2", "thang", 22, None, "worker")];
    assert_eq!(machine_name(&machines, &b), "host-thang-marmot");
    let mut nohost = b.clone();
    nohost.hostname.clear();
    assert_eq!(machine_name(&machines, &nohost), "192.168.2.2");
    // Unknown box entirely: same fallbacks, never empty.
    assert_eq!(machine_name(&[], &b), "host-thang-marmot");
    assert_eq!(machine_name(&[], &nohost), "192.168.2.2");
}

#[test]
fn both_panes_call_a_known_box_by_its_handle() {
    // The hawk hunt: provision says one name, panes must agree with it.
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![named_machine("192.168.2.2", "hawk")];
    app.beats = vec![beat("thang-marmot", "192.168.2.2", 2, "marmot")];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("marmot"),
        "the worker keeps its alias:\n{text}"
    );
    assert!(text.contains("hawk"), "both panes use the handle:\n{text}");
    assert!(
        !text.contains("host-thang-marmot"),
        "the OS hostname steps aside where a handle exists:\n{text}"
    );
}

#[test]
fn stats_pane_counts_completions_and_estimates_the_remainder() {
    let mut app = stats_app();
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("Stats"), "the panel exists:\n{text}");
    assert!(text.contains("marmot"), "rows are workers:\n{text}");
    // Half of a 100s render remains: the TUI-side ETA, not a report.
    assert!(
        text.contains("50s"),
        "remainder from history × progress:\n{text}"
    );
    // Idle with no active stage estimates nothing; unknown load dashes.
    assert!(text.contains("caracal"), "idle workers list too:\n{text}");
    assert!(text.contains("—"), "dashes where nothing is known:\n{text}");
    // The 100-column floor still fits the split row, not just wide terms.
    let narrow = render_text(&mut app, 100, 32);
    assert!(
        narrow.contains("Stats"),
        "panel survives the floor:\n{narrow}"
    );
    assert!(narrow.contains("50s"), "eta survives the floor:\n{narrow}");
}

#[test]
fn workers_pane_shows_load_where_measured() {
    let mut app = stats_app();
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("25.0%"), "cpu pct:\n{text}");
    assert!(text.contains("40% 4.5G"), "mem pct + gib:\n{text}");
}

#[test]
fn task_eta_scales_history_by_the_unworked_fraction() {
    use super::model::task_eta;
    assert_eq!(task_eta(Some(100.0), 0.5), Some(50));
    assert_eq!(task_eta(Some(100.0), 0.0), Some(100));
    assert_eq!(task_eta(Some(100.0), 1.0), Some(0));
    assert_eq!(task_eta(None, 0.5), None, "no history, no number");
    assert_eq!(task_eta(Some(0.0), 0.5), None, "no zero-duration average");
    assert_eq!(task_eta(Some(100.0), 9.9), Some(0), "clamped progress");
}

#[test]
fn machines_pane_shows_each_boxs_sidecar_threads_instead_of_the_worker_count() {
    // The column carries `eff/cores`: the `:threads` override (else the
    // sidecar default) over the beat's cores, `?` where either is unknown.
    let mut app = App::new("http://127.0.0.1:8901");
    let mut local = Machine::new("127.0.0.1", "local", 22, None, "worker");
    local.tts_threads = None;
    let mut remote = Machine::new("192.168.2.2", "thang", 22, None, "worker");
    remote.tts_threads = Some(14);
    let mut ghost = Machine::new("192.168.2.9", "ghost", 22, None, "worker");
    ghost.tts_threads = None;
    app.machines = vec![local, remote, ghost];
    let mut beat_local = beat("w-local", "127.0.0.1", 2, "quokka");
    beat_local.cores = Some(16);
    let mut beat_remote = beat("w-remote", "192.168.2.2", 2, "marmot");
    beat_remote.cores = Some(8);
    app.beats = vec![beat_local, beat_remote];
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("tts-threads"),
        "the count column is headed:\n{text}"
    );
    assert!(
        text.contains("8/16"),
        "no override reads the sidecar default over the cores:\n{text}"
    );
    assert!(
        text.contains("14/8"),
        "the box's override shows over its cores:\n{text}"
    );
    assert!(
        text.contains("?/?"),
        "no beat and no override reads unknown:\n{text}"
    );
    assert_eq!(
        text.matches("192.168.2.2").count(),
        1,
        "the addr appears once, never echoed:\n{text}"
    );
}

#[test]
fn workers_activity_drops_the_stage_and_chapter_the_columns_already_say() {
    use super::model::short_activity;
    let mut b = beat("w1", "192.168.2.2", 2, "marmot");
    b.stage = Some(Stage::Render);
    b.chapter = Some(91);
    b.activity = "render ch91 Accord (3/12)".into();
    assert_eq!(short_activity(&b), "Accord (3/12)");
    b.stage = Some(Stage::Digest);
    b.chapter = Some(7);
    b.activity = "digest ch7 via gemini".into();
    assert_eq!(short_activity(&b), "via gemini");
    b.stage = Some(Stage::Merge);
    b.chapter = Some(9);
    b.activity = "merge ch9".into();
    assert_eq!(short_activity(&b), "—", "nothing left past the prefix");
    b.stage = None;
    b.chapter = None;
    b.activity = "idle".into();
    assert_eq!(
        short_activity(&b),
        "idle",
        "no columns to repeat, keep it whole"
    );
}

#[test]
fn tasks_pane_counts_chapters_against_the_pipeline_that_feeds_them() {
    use super::model::pipeline_counts;
    // chapters {3, 4}: crawl done on 4; digest shelved on 3; render running on 3.
    let counts = pipeline_counts(&tasks_app().tasks);
    assert_eq!(
        counts
            .iter()
            .map(|(st, done, denom)| (st.as_str(), *done, *denom))
            .collect::<Vec<_>>(),
        vec![
            ("crawl", 1, 2),
            ("digest", 0, 1),
            ("render", 0, 0),
            ("merge", 0, 0),
        ]
    );
    let mut app = tasks_app();
    app.counts = serde_json::json!({});
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("1/2 done"),
        "crawl over every chapter:\n{text}"
    );
    assert!(
        text.contains("0/1 done"),
        "digest over crawled chapters:\n{text}"
    );
    assert!(text.contains("shelved"), "row faults still show:\n{text}");
    let (c, d, r) = (
        text.find("1/2 done").unwrap(),
        text.find("0/1 done").unwrap(),
        text.find("0/0 done").unwrap(),
    );
    assert!(c < d && d < r, "crawl → digest → render → merge:\n{text}");
}

#[test]
fn logs_filter_steps_through_stages_and_severities() {
    use super::model::LogFilter;
    assert_eq!(LogFilter::All.step(true), LogFilter::Crawl);
    assert_eq!(LogFilter::Error.step(true), LogFilter::All);
    assert_eq!(LogFilter::All.step(false), LogFilter::Error);
    let mut app = App::new("http://127.0.0.1:8901");
    app.log_at(Level::Info, "crawl ch1 fetched");
    app.log_at(Level::Warn, "digest ch2 slow");
    app.log_at(Level::Error, "render ch3 boom");
    let last = app.events.back().unwrap();
    assert!(LogFilter::All.matches(last));
    assert!(LogFilter::Render.matches(last));
    assert!(!LogFilter::Digest.matches(last));
    assert!(
        !LogFilter::Warn.matches(last),
        "severity is the level, not the text"
    );
    assert!(LogFilter::Error.matches(last));
    app.log_filter = LogFilter::Digest;
    let text = render_text(&mut app, 140, 44);
    assert!(text.contains("[digest]"), "title names the filter:\n{text}");
    assert!(text.contains("digest ch2 slow"), "match stays:\n{text}");
    assert!(!text.contains("render ch3 boom"), "others hide:\n{text}");
    assert!(!text.contains("crawl ch1 fetched"), "others hide:\n{text}");
}

#[tokio::test]
async fn arrows_step_the_logs_filter_and_pin_to_newest() {
    use super::model::LogFilter;
    let http = reqwest::Client::new();
    let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://x");
    handle_key(&mut app, key(KeyCode::Right), &http, &job_tx).await;
    assert_eq!(app.log_filter, LogFilter::Crawl);
    app.events_scroll = 5;
    handle_key(&mut app, key(KeyCode::Left), &http, &job_tx).await;
    assert_eq!(app.log_filter, LogFilter::All, "left steps back");
    assert_eq!(app.events_scroll, 0, "a new filter starts at the tail");
}

#[test]
fn the_cast_overview_renders_every_speaker_and_flags_shared_voices() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    for speaker in ["Narrator", "Kiên", "Vũ", "Lâm", "Hà", "Mới"] {
        assert!(text.contains(speaker), "{speaker} is missing:\n{text}");
    }
    assert!(
        text.contains("6 speakers"),
        "the summary counts them:\n{text}"
    );
    assert!(text.contains("4 voices in use"), "{text}");
    assert!(text.contains("1 shared"), "only Adam is shared:\n{text}");
    assert!(
        text.contains("1 to fix"),
        "Hà is the one the roster cannot resolve:\n{text}"
    );
    assert!(
        text.contains("1 unassigned — :v fills gaps"),
        "Mới:\n{text}"
    );
    // The table is three columns now: a speaker, its voice, and how many
    // *other* speakers share it. `gender` and `accent` were a third of the
    // width repeating `unknown`, and the prose status was four words that said
    // the same thing on every row that had anything to say — the verdict is
    // the count's colour and the summary's `1 to fix` above.
    assert!(text.contains("shared"), "the column is named:\n{text}");
    for gone in [
        "accent policy concern",
        "unknown voice — stale cast?",
        "shared with 1 other",
        "vi-VN   ",
    ] {
        assert!(
            !text.contains(gone),
            "the old column is gone: {gone}\n{text}"
        );
    }
    // Kiên and Vũ share Adam, so both of their rows count one other — the
    // number the old `shared with 1 other` was spelling out in six columns.
    let shared_rows: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("Adam"))
        .filter(|l| l.contains('1'))
        .collect();
    assert_eq!(shared_rows.len(), 2, "both sides are counted:\n{text}");
}

#[test]
fn the_cast_overview_without_a_roster_offers_the_retry_key() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 120, 32);
    assert!(text.contains("roster not loaded — press R"), "{text}");
}
