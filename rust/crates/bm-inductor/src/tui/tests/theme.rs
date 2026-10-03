use super::*;

// --- visual polish: theme, header, spinner, selection --------------------

#[tokio::test]
async fn the_c_key_cycles_the_three_themes_and_mono_drops_the_hues() {
    use crate::tui::style::{theme_label, themed};
    use ratatui::style::Color;

    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    let job_tx = job_channel();
    assert_eq!(theme_label(), "default");
    assert!(app.colour(), "default keeps the hues");

    // default → dim
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_eq!(theme_label(), "dim");
    assert!(app.colour(), "dim is still colour");
    assert_ne!(
        themed(Color::Green),
        Color::Green,
        "dim remaps the stock hues"
    );

    // dim → mono
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_eq!(theme_label(), "mono");
    assert!(!app.colour(), "mono drops the hues");
    assert_eq!(
        themed(Color::Green),
        Color::White,
        "mono reads every state hue as white"
    );

    // mono → default, closing the cycle
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_eq!(theme_label(), "default");
    assert!(themed(Color::Green) == Color::Green);
}

#[tokio::test]
async fn the_theme_cycle_lands_on_the_same_theme_every_time() {
    use crate::tui::style::theme_label;
    // The thread-local must not depend on which test ran before it.
    for expected in ["dim", "mono", "default", "dim"] {
        let mut app = App::new("http://127.0.0.1:8901");
        let http = http_client();
        let job_tx = job_channel();
        handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
        assert_eq!(theme_label(), expected);
    }
}

/// The mouse can be handed back to the terminal, so an error can be copied.
///
/// While mouse reporting is on the terminal routes every drag to the program
/// instead of treating it as a selection, which is why an error message could
/// not be highlighted and copied, the one thing anybody wants to do with an
/// error. `M` turns reporting off and back on again.
#[tokio::test]
async fn the_mouse_can_be_handed_back_to_the_terminal_to_copy_an_error() {
    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    let job_tx = job_channel();
    assert!(
        app.mouse_capture,
        "reporting starts on, so panes are clickable"
    );

    handle_key(&mut app, key(KeyCode::Char('M')), &http, &job_tx).await;
    assert!(
        !app.mouse_capture,
        "M must hand the mouse back for selection"
    );
    assert!(
        app.mouse_toggle,
        "and ask the loop, which owns the terminal, to do it"
    );
    assert!(
        app.status.text.contains("select"),
        "the status line must say what M did, or it is a key nobody finds again: {}",
        app.status.text
    );

    // And back again, because click-to-select is worth having too.
    app.mouse_toggle = false;
    handle_key(&mut app, key(KeyCode::Char('M')), &http, &job_tx).await;
    assert!(app.mouse_capture);
    assert!(app.mouse_toggle);
}

/// `M` is a new key, so it must not have been somebody else's.
///
/// `m` is the documented alias for `:m` (reconcile) and is deliberately left
/// alone: two keys one letter apart doing unrelated things is exactly how a
/// dashboard grows a wrong muscle memory.
#[tokio::test]
async fn the_mouse_key_does_not_collide_with_the_reconcile_alias() {
    let (job_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    handle_key(&mut app, key(KeyCode::Char('m')), &http, &job_tx).await;
    assert!(app.mouse_capture, "lowercase m must not toggle the mouse");
    assert!(!app.mouse_toggle);
    // The existing guarantee still holds: a bare m from Normal mode dispatches
    // nothing and opens nothing.
    assert!(matches!(app.screen, Screen::Normal));
    assert!(rx.try_recv().is_err(), "a bare m must dispatch nothing");
}

/// The crawl view answers the question the dashboard could not: what will this
/// crawl, and is anything wrong with it — in three lines, with the whole
/// configuration one keypress away.
#[tokio::test]
async fn the_crawl_key_answers_what_is_in_force() {
    let dir = std::env::temp_dir().join("bm-crawlview-render");
    let _ = std::fs::remove_dir_all(&dir);
    let layout = bm_core::Layout::new(&dir);
    std::fs::create_dir_all(layout.settings().parent().unwrap()).unwrap();
    std::fs::write(
        layout.settings(),
        serde_json::to_string_pretty(&serde_json::json!({
            "crawl": { "mode": "script", "script": "crawl/truyencom.lua", "pace_ms": 0 }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut app = App::new("http://127.0.0.1:8901");
    app.layout = layout.clone();
    let http = http_client();
    let job_tx = job_channel();
    handle_key(&mut app, key(KeyCode::Char('c')), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Crawl { .. }),
        "c opens the crawl view, not nothing"
    );

    // The default is the verdict, and the two faults this settings file causes
    // without saying so anywhere else.
    let text = render_text(&mut app, 120, 44);
    assert!(text.contains("Reading"), "{text}");
    assert!(text.contains("Faults"), "{text}");
    assert!(
        text.contains("NOT FOUND"),
        "a crawler that is not there must say so on screen:\n{text}"
    );
    assert!(
        text.contains("pacing off"),
        "pace 0 is a decision, so it is flagged:\n{text}"
    );
    // The configuration nobody edited is **not** on this screen. That is the
    // change: it was, and it put a screenful of defaults above the two facts
    // the operator pressed a key to see.
    for noise in ["max_fetches", "timeout_secs", "user_agent"] {
        assert!(
            !text.contains(noise),
            "{noise} is not an answer, and the default screen must not be one:\n{text}"
        );
    }
    assert!(
        text.contains("Enter detail"),
        "and the way to it is named: {text}"
    );

    // Enter opens the whole configuration, which is what the key is for.
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(app.screen, Screen::Crawl { expanded: true, .. }),
        "Enter expands, not closes: {:?}",
        app.screen
    );
    let detail = render_text(&mut app, 120, 44);
    assert!(detail.contains("truyencom.lua"), "{detail}");
    assert!(detail.contains("max_fetches"), "{detail}");
    assert!(detail.contains("Enter verdict"), "and back: {detail}");

    // Enter again returns to the verdict, from the top.
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    assert!(
        matches!(
            app.screen,
            Screen::Crawl {
                expanded: false,
                scroll: 0,
                ..
            }
        ),
        "Enter collapses: {:?}",
        app.screen
    );

    // Esc still closes it, which is what three keys already did.
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
    assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Uppercase `C` is the palette cycle, so the two must not trade places.
#[tokio::test]
async fn the_crawl_key_does_not_steal_the_palette_cycle() {
    let (job_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = App::new("http://127.0.0.1:8901");
    let http = http_client();
    let before = app.theme;
    handle_key(&mut app, key(KeyCode::Char('C')), &http, &job_tx).await;
    assert_ne!(app.theme, before, "C still cycles the palette");
    assert!(matches!(app.screen, Screen::Normal), "C opens nothing");

    app.theme = before;
    handle_key(&mut app, key(KeyCode::Char('c')), &http, &job_tx).await;
    assert_eq!(
        app.theme, before,
        "c must not change the theme — it only opens the view"
    );
    assert!(matches!(app.screen, Screen::Crawl { .. }));
    assert!(
        rx.try_recv().is_err(),
        "the view reads; it dispatches nothing"
    );
}

/// A view nobody can find is a view that does not exist.
#[test]
fn the_crawl_view_is_in_the_footer_and_the_help_screen() {
    assert!(
        KEYS_FULL.iter().any(|k| k.contains("c crawl")),
        "{KEYS_FULL:?}"
    );
    let mut help_app = App::new("http://127.0.0.1:8901");
    help_app.screen = Screen::Help { scroll: 0 };
    let help = render_text(&mut help_app, 140, 60);
    assert!(
        help.contains("crawl view"),
        "the help screen must list it:\n{help}"
    );
}

/// The key that turns the mouse off has to be findable without being told.
#[test]
fn the_mouse_key_is_in_the_footer_and_the_help_screen() {
    // One line a tier is enough, what must not happen is the key being
    // nowhere in the footer, which is how it is never found.
    for (tier, lines) in [("full", &KEYS_FULL), ("compact", &KEYS_COMPACT)] {
        assert!(
            lines.iter().any(|k| k.contains('M')),
            "the {tier} footer must name the mouse key: {lines:?}"
        );
    }
    // The help screen is where a key nobody uses daily is looked up.
    let mut help_app = App::new("http://127.0.0.1:8901");
    help_app.screen = Screen::Help { scroll: 0 };
    let help = render_text(&mut help_app, 140, 60);
    assert!(
        help.contains("select") && help.contains("copy"),
        "the help screen must explain what M is for:\n{help}"
    );
}

#[tokio::test]
async fn the_full_tier_has_a_header_strip_and_the_compact_tier_does_not() {
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("ws: default"),
        "the header names the book:\n{text}"
    );
    assert!(
        text.contains("profile:"),
        "the header names the profile:\n{text}"
    );
    assert!(
        text.contains("C cycles"),
        "the theme chip advertises the key:\n{text}"
    );

    // The compact tier keeps ws/profile where its footer can show them.
    let mut app = App::new("http://127.0.0.1:8901");
    let text = render_text(&mut app, 80, 24);
    assert!(
        text.contains("ws: default"),
        "compact keeps the workspace in the footer:\n{text}"
    );
}

#[tokio::test]
async fn pending_jobs_show_a_spinner_and_live_shows_a_pulse() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.pending = 2;
    app.tick = 3;
    app.conn = Conn::Up;
    app.refreshed = Some(std::time::Instant::now());
    let text = render_text(&mut app, 140, 44);
    let frames: Vec<char> = "⠋⠙⠹⠸⠼⠴⠦⠇".chars().collect();
    assert!(
        text.contains(&format!("{} 2 job(s) running", frames[3])),
        "the spinner steps with the tick:\n{text}"
    );
}

#[test]
fn the_log_severity_column_is_fixed_width() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.log_at(Level::Error, "boom one");
    app.log_at(Level::Ok, "fine two");
    let text = render_text(&mut app, 140, 44);
    // Both tags start their message at the same column; the old mixed-width
    // glyphs (`OK`, `ERROR`) left the text ragged.
    for tag in ["err ", " ok "] {
        assert!(text.contains(tag), "fixed-width `{tag}` tag:\n{text}");
    }
    assert!(!text.contains("ERROR "), "no wide ERROR tag:\n{text}");
}

#[test]
fn the_state_column_leads_with_a_glyph_and_the_word_stays() {
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines.push(Machine {
        id: "192.168.2.2".into(),
        addr: "192.168.2.2".into(),
        name: "box-1".into(),
        ssh_user: "ubuntu".into(),
        ssh_port: 22,
        ssh_key: None,
        role: "worker".into(),
        state: MachineState::Online,
        state_since: bm_proto::now_secs(),
        last_seen: bm_proto::now_secs(),
        capabilities: Vec::new(),
        tts_url: None,
        task_port: None,
        task_policy: None,
        note: String::new(),
        accepting_work: true,
        tts_threads: None,
    });
    let text = render_text(&mut app, 140, 44);
    assert!(
        text.contains("● online"),
        "a healthy box reads at a glance:\n{text}"
    );
    assert!(
        text.contains("box-1") && text.contains("1 up"),
        "the right title counts the boxes:\n{text}"
    );
}
