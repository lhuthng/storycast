use super::*;

// --- responsive layout --------------------------------------------------

#[test]
fn size_class_picks_a_tier_per_axis() {
    assert_eq!(size_class(120, 40), Size::Full);
    assert_eq!(size_class(FULL_W, FULL_H), Size::Full);
    assert_eq!(
        size_class(80, 24),
        Size::Compact,
        "the common default terminal"
    );
    assert_eq!(
        size_class(MIN_W, MIN_H),
        Size::Compact,
        "the floor is still usable"
    );
    assert_eq!(size_class(60, 24), Size::TooSmall, "too narrow");
    assert_eq!(size_class(120, 10), Size::TooSmall, "too short");
    assert_eq!(
        size_class(0, 0),
        Size::TooSmall,
        "a degenerate area must not divide by zero"
    );
}

#[test]
fn compact_columns_fit_a_minimum_width_terminal() {
    // The same totals the compile-time guards prove; asserted here too so a
    // failure names the pane instead of just refusing to compile.
    let machines = cols(&COMPACT_MACHINE_COLS);
    let workers = cols(&COMPACT_WORKER_COLS);
    assert!(
        machines + 2 <= MIN_W,
        "machines needs {machines}+2 columns, terminal floor is {MIN_W}"
    );
    assert!(
        workers + 2 <= MIN_W,
        "workers needs {workers}+2 columns, terminal floor is {MIN_W}"
    );
}

#[test]
fn compact_layout_fits_the_hard_minimum() {
    // The **floors**, because that is the case where every pane has nothing to
    // show and is therefore at its minimum. Longer content takes its rows out
    // of Logs, which is the pane meant to give them up.
    let panes = COMPACT_MACHINES_MIN_H
        + COMPACT_WORKERS_MIN_H
        + COMPACT_TASKS_MIN_H
        + COMPACT_EVENTS_MIN_H
        + COMPACT_FOOTER_H;
    assert!(
        panes <= MIN_H,
        "compact floors need {panes} rows, floor is {MIN_H}"
    );
    // The full tier must not be tighter than the compact one.
    let full = FULL_HEADER_H
        + FULL_MACHINES_MIN_H
        + FULL_WORKERS_MIN_H
        + FULL_TASKS_MIN_H
        + FULL_EVENTS_MIN_H
        + FULL_FOOTER_H;
    assert!(
        full <= FULL_H,
        "full floors need {full} rows, threshold is {FULL_H}"
    );
}

/// A pane's ceiling must not be so high that one busy pane crowds out the rest.
///
/// The ceilings exist so a thirty-machine cluster scrolls instead of pushing
/// Logs and the footer off the screen. This is the arithmetic behind that: at
/// the ceiling, **Logs still gets its readable floor** and the footer is never
/// squeezed out.
#[test]
fn a_busy_pane_at_its_ceiling_still_leaves_the_log_and_the_footer_room() {
    let worst = FULL_MACHINES_MAX_H + FULL_WORKERS_MAX_H + FULL_TASKS_MAX_H;
    let rest = FULL_HEADER_H + worst + FULL_EVENTS_MIN_H + FULL_FOOTER_H;
    assert!(
        rest <= FULL_H,
        "every pane at its ceiling needs {rest} rows, threshold is {FULL_H} — \
         a busy cluster would push the log off the screen"
    );
    // The compact tier is the tighter one and has the smaller ceilings, so it
    // is the one that actually has to hold.
    let compact_worst = COMPACT_MACHINES_MAX_H + COMPACT_WORKERS_MAX_H + COMPACT_TASKS_MAX_H;
    let compact_rest = compact_worst + COMPACT_EVENTS_MIN_H + COMPACT_FOOTER_H;
    assert!(
        compact_rest <= MIN_H,
        "compact ceilings need {compact_rest} rows, floor is {MIN_H}"
    );
}

#[test]
fn key_hints_fit_their_tier_without_clipping() {
    // The single 161-character line this replaced was clipped on every
    // terminal, and the lost tail held the least guessable keys.
    for k in KEYS_FULL {
        assert!(
            width_of(k) <= FULL_W as usize,
            "{k} is {} columns",
            width_of(k)
        );
    }
    for k in KEYS_COMPACT {
        assert!(
            width_of(k) <= MIN_W as usize,
            "{k} is {} columns",
            width_of(k)
        );
    }
}

#[test]
fn the_footer_advertises_jobs_on_tab_in_both_tiers() {
    // The footer is the only map of the dashboard; the key it names must be
    // the key that works, in both tiers, or the overlay is undiscoverable.
    assert!(KEYS_FULL.iter().any(|k| k.contains("Tab jobs")));
    assert!(KEYS_COMPACT.iter().any(|k| k.contains("Tab jobs")));
}

#[test]
fn every_dashboard_header_reads_in_full_at_the_100_column_floor() {
    // Regression guard for the two header crops an operator actually read:
    // the full-tier Workers pane drew `box cp` (7 glyphs in a 6-wide column)
    // and the Machines table overflowed its 98 interior columns, pushing
    // `seen` and half of `state` off the pane. Both are rendered here at the
    // exact terminal where they broke.
    let mut app = stats_app();
    let text = render_text(&mut app, 100, 32);
    for header in ["box cpu", "box ram", "tts-threads", "activity", "seen"] {
        assert!(text.contains(header), "{header} clipped:\n{text}");
    }
}

/// A pane is sized to its content, and the content is what the terminal can
/// actually show.
///
/// This replaced a fixed row count per pane, under which a one-box cluster was
/// shown an eight-row Machines pane that was mostly border and a ten-worker
/// cluster had workers clipped with nothing saying so.
#[test]
fn every_live_worker_is_visible_on_a_terminal_that_can_hold_them() {
    let mut app = stats_app();
    let before = app.live_workers().len();
    for i in 0..8 {
        app.beats.push(beat(
            &format!("worker-{i}"),
            &format!("192.0.2.{i}"),
            2,
            &format!("worker-{i}"),
        ));
    }
    // The pane is sized from the same filtered set the renderer draws, so the
    // count that drives the layout is the count of rows on screen.
    let live = app.live_workers().len();
    assert_eq!(live, before + 8, "the fixture's own beats count too");
    let text = render_text(&mut app, 140, 44);
    for i in 0..8 {
        assert!(
            text.contains(&format!("worker-{i}")),
            "worker {i} was clipped:\n{text}"
        );
    }
}

/// Tasks and Stats are drawn in the compact tier too.
///
/// They used to be carved out of the Workers pane *only on the full tier*, so
/// on a 76x24 terminal, the default on most setups, both were simply not
/// drawn, and the footer carried a roll-up instead. A pane that cannot be seen
/// is a pane that cannot answer the question you opened the dashboard to ask.
#[test]
fn the_compact_tier_still_shows_tasks_and_stats() {
    let mut app = stats_app();
    let text = render_text(&mut app, 80, 24);
    assert!(
        text.contains("╭Tasks"),
        "no Tasks pane on a default terminal:\n{text}"
    );
    assert!(
        text.contains("╭Stats"),
        "no Stats pane on a default terminal:\n{text}"
    );
    assert!(
        text.contains("╭Logs"),
        "and the log is still there:\n{text}"
    );
    // Every pane has to fit the compact floor, or something is pushed off.
    for pane in ["╭Machines", "╭Workers", "╭Tasks", "╭Stats", "╭Logs"] {
        assert!(text.contains(pane), "{pane} missing:\n{text}");
    }
}

/// The log is the one pane that grows, because a message is the point of it.
///
/// Before this change the spare rows went to the worker list, which meant a
/// terminal with room to spare still showed a five-line log on a failing
/// cluster. Everything else now takes exactly its content, so whatever is left
/// lands here.
#[test]
fn the_log_takes_the_rows_the_other_panes_do_not_need() {
    // `stats_app` has machines and workers, so the three content panes are all
    // above their floors and the difference between these two renders is the
    // log.
    let mut app = stats_app();
    let short = render_text(&mut app, 140, 32);
    let tall = render_text(&mut app, 140, 52);

    // Measured from the rendered box itself: the number of rows between the
    // Logs top border and the footer. Counting lines that look like log lines
    // would pass whether the pane grew or not.
    let log_height = |t: &str| -> usize {
        let lines: Vec<&str> = t.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains("╭Logs"))
            .unwrap_or_else(|| panic!("no Logs pane in:\n{t}"));
        let bottom = lines
            .iter()
            .rposition(|l| l.contains("Tab jobs") || l.contains(":add"))
            .unwrap_or_else(|| panic!("no footer in:\n{t}"));
        bottom
            .checked_sub(top)
            .expect("the log sits above the footer")
    };
    assert!(
        log_height(&short) >= FULL_EVENTS_MIN_H as usize,
        "the log lost its floor at the tier threshold:\n{short}"
    );
    assert!(
        log_height(&tall) > log_height(&short),
        "a taller terminal must grow the log, not the borders: {} -> {}",
        log_height(&short),
        log_height(&tall)
    );
}

#[test]
fn the_workers_headers_fit_their_columns() {
    // Regression guard for the `box cp` crop: the full-tier load columns
    // must be at least as wide as their headers (the `box cpu` cell carries
    // a trailing space against the edge, so its column needs 8).
    for (header, w) in [("box cpu ", 8usize), ("box ram", 10), ("progress", 19)] {
        assert!(
            width_of(header) <= w,
            "{header:?} is {} columns in a {w}-wide one",
            width_of(header)
        );
    }
}

#[test]
fn the_footer_advertises_the_cast_key_in_both_tiers() {
    // Regression guard: at 80 columns `S cast` fell off the clipped tail of
    // the old one-line hint, so the feature was undiscoverable exactly
    // where the terminal was most cramped.
    assert!(
        KEYS_FULL.iter().any(|k| k.contains("S cast")),
        "{KEYS_FULL:?}"
    );
    assert!(
        KEYS_COMPACT.iter().any(|k| k.contains("S cast")),
        "{KEYS_COMPACT:?}"
    );
}

#[test]
fn task_rollup_survives_a_missing_or_empty_counts_object() {
    let text = |v: &serde_json::Value| -> String {
        task_rollup(v, false)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    };
    assert!(text(&serde_json::Value::Null).contains("waiting"));
    assert!(text(&serde_json::json!({})).contains("none queued"));
}

#[test]
fn task_rollup_totals_every_stage_and_flags_shelved() {
    let counts = serde_json::json!({
        "crawl":  {"done": 3, "failed": 1},
        "render": {"done": 1, "shelved": 2},
    });
    let text: String = task_rollup(&counts, false)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("4/7 done"), "{text}");
    assert!(text.contains("1 open"), "{text}");
    assert!(text.contains("1 failed"), "{text}");
    assert!(text.contains("2 shelved"), "{text}");
}

#[test]
fn task_rollup_hides_zero_failure_and_shelved_counters() {
    let counts = serde_json::json!({"crawl": {"done": 2, "failed": 0, "shelved": 0}});
    let text: String = task_rollup(&counts, false)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("2/2 done"), "{text}");
    assert!(!text.contains("failed"), "a zero counter is noise: {text}");
    assert!(!text.contains("shelved"), "a zero counter is noise: {text}");
}
