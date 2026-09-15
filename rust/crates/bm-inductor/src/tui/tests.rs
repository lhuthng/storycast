//! Key and render tests, moved as one file.
use super::app::App;
use super::draw::draw;
use super::input::{handle_key, op_key, urlencode};
use super::input::command::{Command, command_key};
use super::input::runconfig::{parse_run_config, run_preview, save_run_config};
use super::input::submit::submit_text;
use super::jobs::{DoneKind, Ev, Job, run_job, set_machine_state};
use super::layout::{Size, size_class, cols, width_of, MIN_W, MIN_H, FULL_W, FULL_H, FULL_MACHINES_H, FULL_WORKERS_H, FULL_TASKS_H, FULL_EVENTS_MIN_H, FULL_FOOTER_H, COMPACT_MACHINES_H, COMPACT_WORKERS_H, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H, KEYS_FULL, KEYS_COMPACT, COMPACT_MACHINE_COLS, COMPACT_WORKER_COLS};
use super::model::*;
use super::screen::*;
use super::style::*;
use bm_proto::{Machine, MachineState, Op, OpRequest, Roster, Stage, Task, TaskState, VoiceInfo};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use std::collections::BTreeMap;

    #[test]
    fn accents_are_folded_so_filters_ignore_diacritics() {
        assert_eq!(fold("Thái Sơn"), "thai son");
        assert_eq!(fold("Đức Trí"), "duc tri");
        assert_eq!(fold("Thục Đoan"), "thuc doan");
        assert_eq!(fold("Lạc Lan Tuyết"), "lac lan tuyet");
        assert!(matches("thai son", "Thái Sơn"));
        assert!(matches("duc", "Đức Trí"));
        assert!(matches("", "anything"), "an empty filter matches everything");
        assert!(!matches("adam", "Thái Sơn"));
    }

    #[test]
    fn neutral_does_not_fold_to_a_female_marker() {
        // Guards the Python/Rust twin of the same bug.
        assert_eq!(fold("neutral"), "neutral");
    }

    #[test]
    fn text_prompt_edits_by_character_not_byte() {
        let mut p = TextPrompt::new(TextKind::AddMachine, "t", "h", "Đức");
        // Three characters, six bytes: a byte-indexed cursor would land inside
        // 'ứ' and panic on the next edit.
        assert_eq!(p.len(), 3);
        assert_eq!(p.cursor, 3);
        p.left();
        assert_eq!(p.cursor, 2, "cursor 2 sits before the third character");
        p.insert('x');
        assert_eq!(p.buf, "Đứxc");
        assert_eq!(p.cursor, 3);
        p.backspace();
        assert_eq!(p.buf, "Đức");
        assert_eq!(p.cursor, 2);
        p.home();
        p.delete();
        assert_eq!(p.buf, "ức");
        p.kill_to_start();
        assert_eq!(p.buf, "ức", "cursor is already at 0, so nothing is cut");
        p.end();
        assert_eq!(p.cursor, 2);
        p.kill_word();
        assert_eq!(p.buf, "");
    }

    #[test]
    fn kill_word_stops_at_a_space() {
        let mut p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
        p.kill_word();
        assert_eq!(p.buf, "21 ");
        p.kill_word();
        assert_eq!(p.buf, "");
    }

    #[test]
    fn translate_prompt_rejects_garbage_instead_of_defaulting() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::Translate, "t", "h", "abc 80");
        let err = submit_text(&mut app, &p).unwrap_err();
        assert!(err.contains("not a chapter number"), "{err}");

        let p = TextPrompt::new(TextKind::Translate, "t", "h", "21");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("expected"));

        let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 0");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("at least 1"));

        let p = TextPrompt::new(TextKind::Translate, "t", "h", "21 80");
        assert!(submit_text(&mut app, &p).is_ok());
    }

    #[test]
    fn run_config_parses_range_analyzer_and_models() {
        let (s, c, a, m) = parse_run_config("1 1", "opencode").unwrap();
        assert_eq!((s, c), (1, 1));
        assert_eq!(a, "opencode");
        assert!(m.is_none(), "omitted models stay out of the file");
        let (_, _, a, m) = parse_run_config("2 5 gemini 3.8-flash, 3.7-flash", "opencode").unwrap();
        assert_eq!(a, "gemini");
        assert_eq!(m.unwrap(), vec!["3.8-flash", "3.7-flash"]);
        assert!(parse_run_config("abc 80", "opencode").unwrap_err().contains("not a chapter number"));
        assert!(parse_run_config("1 1 watson", "opencode").unwrap_err().contains("unknown"));
        assert!(parse_run_config("1 1 gemini 3.8-flash 3.7-flash", "opencode")
            .unwrap_err()
            .contains("comma-separated"));
        assert!(parse_run_config("1 1 gemini ,", "opencode").unwrap_err().contains("empty"));
    }

    #[test]
    fn run_config_save_persists_everything_it_parsed() {
        let dir = std::env::temp_dir().join("bm-runconfig-save");
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new("http://x");
        app.layout_root = dir.clone();

        let msg = save_run_config(&app, "1 1 gemini 3.8-flash,3.7-flash").unwrap();
        assert!(msg.contains("ch1"), "{msg}");
        let saved: bm_core::config::Settings =
            bm_core::read_json(&bm_core::Layout::new(&dir).settings()).unwrap();
        assert_eq!((saved.start, saved.count), (1, 1));
        assert_eq!(saved.analyzer, "gemini");
        assert_eq!(saved.analyze_models, vec!["3.8-flash", "3.7-flash"]);

        // Omitted models keep the saved chain — a blank field must not wipe it.
        save_run_config(&app, "1 1 gemini").unwrap();
        let saved: bm_core::config::Settings =
            bm_core::read_json(&bm_core::Layout::new(&dir).settings()).unwrap();
        assert_eq!(saved.analyze_models, vec!["3.8-flash", "3.7-flash"]);

        assert!(save_run_config(&app, "1 1 watson").unwrap_err().contains("unknown"));
    }

    #[test]
    fn run_preview_prefers_live_api_then_file_then_defaults() {
        // Live backend: its boot-time settings, labeled as such.
        let mut app = App::new("http://x");
        app.settings = Some(serde_json::json!({
            "start": 5, "count": 2, "analyzer": "gemini",
            "analyze_models": ["3.8-flash"], "engine": "vieneu",
        }));
        let cfg = run_preview(&app);
        assert!(cfg.live);
        assert_eq!((cfg.start, cfg.count), (5, 2));
        assert_eq!(cfg.analyzer, "gemini");
        assert_eq!(cfg.models, vec!["3.8-flash"]);

        // Down backend: the saved file is what the next boot will use.
        let dir = std::env::temp_dir().join("bm-runconfig-preview");
        let _ = std::fs::remove_dir_all(&dir);
        let settings = bm_core::config::Settings { start: 1, count: 1, ..bm_core::config::Settings::default() };
        settings.save(&bm_core::Layout::new(&dir).settings()).unwrap();
        let mut app = App::new("http://x");
        app.layout_root = dir;
        let cfg = run_preview(&app);
        assert!(!cfg.live);
        assert!(cfg.saved, "a settings file exists");
        assert_eq!((cfg.start, cfg.count), (1, 1));

        // Neither: honest defaults, labeled as nobody's choice.
        let app = App::new("http://x");
        let cfg = run_preview(&app);
        assert!(!cfg.live);
        assert!(!cfg.saved);
        assert_eq!((cfg.start, cfg.count), (1, 1));
    }

    #[tokio::test]
    async fn run_screen_enters_and_launches_with_previewed_values() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // `e` opens the config editor prefilled from the preview.
        let mut app = App::new("http://x");
        app.settings = Some(serde_json::json!({
            "start": 1, "count": 1, "analyzer": "opencode", "engine": "vieneu",
        }));
        app.screen = Screen::Run;
        handle_key(&mut app, key(KeyCode::Char('e')), &http, &job_tx).await;
        match &app.screen {
            Screen::Text(p) => assert_eq!(p.buf, "1 1 opencode"),
            other => panic!("expected the config editor, got {other:?}"),
        }

        // `Enter` launches backend-if-needed plus the job: one dispatch.
        app.screen = Screen::Run;
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal));
        match job_rx.try_recv() {
            Ok(Job::StartBackend { start, count, enqueue, .. }) => {
                assert_eq!((start, count), (1, 1));
                assert!(enqueue, "the run screen always brings a job");
            }
            other => panic!("expected a start-backend job, got {other:?}"),
        }

        // Bare `:B` brings the backend and nothing else.
        let mut app = App::new("http://x");
        handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "B"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal));
        match job_rx.try_recv() {
            Ok(Job::StartBackend { enqueue, .. }) => assert!(!enqueue, "bare :B carries no job"),
            other => panic!("expected a start-backend job, got {other:?}"),
        }

        // A second `:B` while the first sequence runs dispatches nothing;
        // `StartDone` re-arms it.
        assert!(app.backend_start_outstanding, "B marks the start in flight");
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "B"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(job_rx.try_recv().is_err(), "double B must not queue another start");
        app.apply(Ev::Done(DoneKind::StartDone));
        assert!(!app.backend_start_outstanding, "StartDone re-arms B");

        // `Esc` just closes.
        app.screen = Screen::Run;
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal));
    }

    #[tokio::test]
    async fn machine_state_falls_back_to_the_ledger_file_while_down() {
        // Nothing answers on port 9 (discard): the API post fails fast and
        // the ledger patch carries the phase instead.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".bm")).unwrap();
        std::fs::write(
            d.path().join(".bm/ledger.json"),
            r#"{"tasks": [], "machines": [
                {"id": "a", "addr": "a", "ssh_user": "u", "ssh_port": 22, "role": "worker", "state": "unknown", "last_seen": 0, "note": ""}
            ]}"#,
        )
        .unwrap();
        set_machine_state("http://127.0.0.1:9", d.path(), "a", MachineState::Provisioning, "catching up").await;
        let doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join(".bm/ledger.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc["machines"][0]["state"], "provisioning");
        assert_eq!(doc["machines"][0]["note"], "catching up");
    }

    #[test]
    fn machine_targets_fall_back_to_the_ledger_file() {
        // The trap: fresh TUI + dead inductor leaves app.machines empty, and
        // B used to default to local-only, silently dropping remotes. The
        // registry file (addr + ssh credentials) stands in instead.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".bm")).unwrap();
        std::fs::write(
            d.path().join(".bm").join("ledger.json"),
            r#"{"tasks": [], "machines": [
                {"id": "192.168.2.2", "addr": "192.168.2.2", "ssh_user": "thang", "ssh_port": 22, "ssh_key": "/k", "role": "worker", "state": "unknown", "last_seen": 0, "note": ""},
                {"id": "127.0.0.1", "addr": "127.0.0.1", "ssh_user": "local", "ssh_port": 22, "role": "worker", "state": "unknown", "last_seen": 0, "note": ""}
            ]}"#,
        )
        .unwrap();
        let found = registry_machines(d.path());
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].addr, "192.168.2.2");
        assert_eq!(found[1].ssh_key.as_deref(), Some("/k"), "credentials ride along");

        let mut app = App::new("http://x");
        app.layout_root = d.path().to_path_buf();
        assert_eq!(app.effective_machines().len(), 2, "empty memory reads the file");
        app.machines = vec![Machine::new("127.0.0.1", "local", 22, None, "worker")];
        assert_eq!(app.effective_machines().len(), 1, "live data wins when present");

        let nowhere = std::path::Path::new("/nonexistent-root-xyz");
        assert!(registry_machines(nowhere).is_empty(), "missing file means local-only, not a crash");
    }

    #[test]
    fn the_run_screen_renders_without_a_roster_or_backend() {
        let mut app = App::new("http://127.0.0.1:8901");
        app.conn = Conn::Down("inductor unreachable at http://127.0.0.1:8901".into());
        app.screen = Screen::Run;
        let text = render_text(&mut app, 140, 44);
        assert!(text.contains("System"), "{text}");
        assert!(text.contains("Enter launches"), "{text}");
        assert!(text.contains("DOWN"), "no backend is attached:\n{text}");
    }

    #[test]
    fn backend_live_parks_the_range_until_the_next_refresh() {
        let mut app = App::new("http://x");
        assert!(app.pending_enqueue.is_none());
        app.apply(Ev::BackendLive { start: 1, count: 1 });
        assert_eq!(app.pending_enqueue, Some((1, 1)));
    }

    #[tokio::test]
    async fn retry_command_dispatches_the_retry_op_once() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut app = App::new("http://x");
        handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "u"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        match job_rx.try_recv() {
            Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Retry),
            other => panic!("expected a retry op, got {other:?}"),
        }
        // A second :u while one is in flight is refused, not queued twice.
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "u"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(job_rx.try_recv().is_err(), "duplicate retry must be refused");
        // A bare `u` from Normal mode is a stray key: it must not dispatch.
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app2 = App::new("http://x");
        handle_key(&mut app2, key(KeyCode::Char('u')), &http, &tx2).await;
        assert!(matches!(app2.screen, Screen::Normal));
        assert!(app2.status.text.contains("command line"), "{}", app2.status.text);
        assert!(rx2.try_recv().is_err(), "a stray u must never dispatch a retry");
    }

    #[test]
    fn range_label_names_the_configured_chapters() {
        assert_eq!(
            range_label(&Some(serde_json::json!({"start": 1, "count": 1}))).as_deref(),
            Some("chapters 1–1")
        );
        assert_eq!(
            range_label(&Some(serde_json::json!({"start": 21, "count": 80}))).as_deref(),
            Some("chapters 21–100")
        );
        assert!(range_label(&None).is_none());
        assert!(range_label(&Some(serde_json::json!({"start": 1, "count": 0}))).is_none());
    }

    #[test]
    fn crawl_template_requires_the_chapter_placeholder() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("{n}"));
        let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "https://x/chuong-{n}");
        assert!(submit_text(&mut app, &p).is_ok());
        let p = TextPrompt::new(TextKind::CrawlTemplate, "t", "h", "   ");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
    }

    #[test]
    fn add_sample_rejects_an_empty_path() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddSample, "t", "h", "  ");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
        let p = TextPrompt::new(TextKind::AddSample, "t", "h", "~/dl/young-female-4.mp3");
        assert!(matches!(submit_text(&mut app, &p), Ok(Job::AddSample { .. })));
    }

    #[test]
    fn add_sample_refuses_names_and_points_at_the_named_window() {
        // One window, one job: `as` belongs to N, never smuggled through A.
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddSample, "t", "h", "refs/narrator.mp3 as Narrator");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("press N"));
    }

    #[test]
    fn add_named_requires_path_as_name_and_stays_private() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddNamed, "t", "h", "refs/trien-chieu.mp3 as Triển Chiêu");
        match submit_text(&mut app, &p) {
            Ok(Job::AddSample { path, name, tags, .. }) => {
                assert_eq!(path, "refs/trien-chieu.mp3");
                assert_eq!(name.as_deref(), Some("Triển Chiêu"));
                assert_eq!(tags, Some(Vec::new()), "named voices carry no pool tags");
            }
            other => panic!("expected an add-sample job, got {other:?}"),
        }
        // Half a rename keeps the window open.
        for bad in ["refs/narrator.mp3 as ", " as Narrator", "refs/narrator.mp3"] {
            let p = TextPrompt::new(TextKind::AddNamed, "t", "h", bad);
            assert!(submit_text(&mut app, &p).is_err(), "{bad:?} must not submit");
        }
    }

    #[tokio::test]
    async fn add_sample_success_reloads_the_roster() {
        let dir = std::env::temp_dir().join("bm-addsample-reload");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("young-male-9.mp3");
        std::fs::write(&src, b"fake").unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
        run_job(
            Job::AddSample { layout_root: dir, path: src.display().to_string(), name: None, tags: None },
            tx,
        )
        .await;
        let mut dones = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Ev::Done(k) = ev {
                dones.push(k);
            }
        }
        assert!(
            dones.iter().any(|k| matches!(k, DoneKind::ReloadRoster)),
            "success must refresh the showing roster: {dones:?}"
        );
        assert!(
            dones.iter().all(|k| !matches!(k, DoneKind::Other)),
            "no stale Done: {dones:?}"
        );

        // Failure refreshes nothing — the roster it would fetch is unchanged.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
        run_job(
            Job::AddSample {
                layout_root: std::env::temp_dir().join("bm-addsample-reload"),
                path: "/nonexistent/clip.mp3".into(),
                name: None,
                tags: None,
            },
            tx,
        )
        .await;
        let mut dones = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Ev::Done(k) = ev {
                dones.push(k);
            }
        }
        assert!(dones.iter().all(|k| matches!(k, DoneKind::Other)), "{dones:?}");
    }

    #[tokio::test]
    async fn text_prompt_closes_on_submit_or_esc_but_stays_open_on_error() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // Esc closes without dispatching.
        let mut app = App::new("http://x");
        app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "x.mp3"));
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "Esc must close the prompt");
        assert!(job_rx.try_recv().is_err(), "a cancelled prompt dispatches nothing");

        // A good submit closes and dispatches exactly one job.
        app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "x.mp3"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "submit must close the prompt");
        assert!(job_rx.try_recv().is_ok());

        // A bad submit keeps the prompt (and its text) open.
        app.screen = Screen::Text(TextPrompt::new(TextKind::AddSample, "t", "h", "   "));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Text(_)), "an error must keep the prompt open");
    }

    #[test]
    fn add_machine_rejects_whitespace_addresses() {
        let mut app = App::new("http://x");
        let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "192.168.2.7 extra");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("whitespace"));
        let p = TextPrompt::new(TextKind::AddMachine, "t", "h", "  ");
        assert!(submit_text(&mut app, &p).unwrap_err().contains("empty"));
    }

    #[test]
    fn scroll_clamping_keeps_the_cursor_visible() {
        let mut scroll = 0;
        clamp_scroll(0, &mut scroll, 100, 10);
        assert_eq!(scroll, 0);
        clamp_scroll(15, &mut scroll, 100, 10);
        assert_eq!(scroll, 6, "cursor 15 in a 10-row window starts at 6");
        clamp_scroll(2, &mut scroll, 100, 10);
        assert_eq!(scroll, 2);
        // A short list must not scroll past its end.
        let mut s2 = 5;
        clamp_scroll(0, &mut s2, 3, 10);
        assert_eq!(s2, 0);
    }

    #[test]
    fn seen_label_says_never_rather_than_a_fifty_year_uptime() {
        let mut m = Machine::new("10.0.0.5", "u", 22, None, "worker");
        assert_eq!(seen_label(&m), "never");
        m.last_seen = bm_proto::now_secs().saturating_sub(5);
        assert_eq!(seen_label(&m), "5s");
        m.last_seen = bm_proto::now_secs().saturating_sub(120);
        assert_eq!(seen_label(&m), "2m");
        m.last_seen = bm_proto::now_secs().saturating_sub(7200);
        assert_eq!(seen_label(&m), "2h");
    }

    #[test]
    fn stages_and_states_have_distinct_palettes() {
        // The old build coloured the Workers stage column with the task-state
        // palette, which no stage name matched.
        assert_eq!(stage_color("render"), Color::Cyan);
        assert_eq!(state_color("online"), Color::Green);
        assert_ne!(stage_color("render"), state_color("render"));
    }

    #[test]
    fn users_of_lists_every_character_on_a_voice() {
        let mut cast = BTreeMap::new();
        cast.insert("Narrator".to_string(), "Đức Trí".to_string());
        cast.insert("A".to_string(), "Đức Trí".to_string());
        cast.insert("B".to_string(), "Adam".to_string());
        assert_eq!(users_of(&cast, "Đức Trí").len(), 2);
        assert_eq!(users_of(&cast, "Adam"), vec!["B".to_string()]);
        assert!(users_of(&cast, "Nobody").is_empty());
    }

    #[test]
    fn urlencode_leaves_hostnames_alone_and_escapes_the_rest() {
        assert_eq!(urlencode("192.168.2.7"), "192.168.2.7");
        assert_eq!(urlencode("host name"), "host%20name");
    }

    #[test]
    fn wall_clock_stamps_read_as_local_hh_mm_ss() {
        // Shape, not value: the machine's timezone is whatever it is.
        for epoch in [1u64, 1_789_485_796u64] {
            let s = wall_hms(epoch);
            assert_eq!(s.len(), 8, "{s}");
            assert_eq!(&s[2..3], ":");
            assert_eq!(&s[5..6], ":");
            assert!(s.chars().filter(|c| *c != ':').all(|c| c.is_ascii_digit()), "{s}");
        }
    }

    #[test]
    fn log_heads_alias_machines_and_workers_but_not_sentences() {
        assert_eq!(log_head("[192.168.2.2] enrolled x"), Some("192.168.2.2"));
        assert_eq!(log_head("localhost-4578: render done"), Some("localhost-4578"));
        assert_eq!(log_head("DESKTOP-V1JNVB0-18150: digest done"), Some("DESKTOP-V1JNVB0-18150"));
        assert_eq!(log_head("reconcile: nothing to fold"), None);
        assert_eq!(log_head("render:52 done"), None);
        assert_eq!(log_head("backend starting"), None);
        assert_eq!(log_head("[broken"), None);
    }

    #[test]
    fn command_line_maps_keys_and_words() {
        assert_eq!(command_key("m"), Some(Command::Reconcile));
        assert_eq!(command_key("B"), Some(Command::Backend));
        assert_eq!(command_key("?"), Some(Command::Key(KeyCode::Char('?'))));
        assert_eq!(command_key("u"), Some(Command::Retry), "single chars are commands");
        assert_eq!(command_key("r"), Some(Command::Key(KeyCode::Char('r'))));
        assert_eq!(command_key("reconcile"), Some(Command::Reconcile));
        assert_eq!(command_key("backend"), Some(Command::Backend));
        assert_eq!(command_key("stop"), Some(Command::Stop));
        assert_eq!(command_key("quit"), Some(Command::Key(KeyCode::Char('q'))));
        assert_eq!(command_key("colour"), Some(Command::Key(KeyCode::Char('C'))));
        assert_eq!(command_key("color"), Some(Command::Key(KeyCode::Char('C'))));
        assert_eq!(command_key(":"), None, "a bare colon reopens nothing");
        assert_eq!(command_key("frobnicate"), None);
        assert_eq!(command_key(""), None);
    }

    #[test]
    fn app_starts_on_normal_with_a_hint_not_a_blank_status() {
        let app = App::new("http://127.0.0.1:8901/");
        assert_eq!(app.api, "http://127.0.0.1:8901", "trailing slash is trimmed");
        assert!(matches!(app.screen, Screen::Normal));
        assert!(!app.status.text.is_empty());
        assert_eq!(app.pending, 0);
        assert!(app.colour);
    }

    // --- responsive layout --------------------------------------------------

    #[test]
    fn size_class_picks_a_tier_per_axis() {
        assert_eq!(size_class(120, 40), Size::Full);
        assert_eq!(size_class(FULL_W, FULL_H), Size::Full);
        assert_eq!(size_class(80, 24), Size::Compact, "the common default terminal");
        assert_eq!(size_class(MIN_W, MIN_H), Size::Compact, "the floor is still usable");
        assert_eq!(size_class(60, 24), Size::TooSmall, "too narrow");
        assert_eq!(size_class(120, 10), Size::TooSmall, "too short");
        assert_eq!(size_class(0, 0), Size::TooSmall, "a degenerate area must not divide by zero");
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
        let panes = COMPACT_MACHINES_H
            + COMPACT_WORKERS_H
            + COMPACT_EVENTS_MIN_H
            + COMPACT_FOOTER_H;
        assert!(panes <= MIN_H, "compact panes need {panes} rows, floor is {MIN_H}");
        // The full tier must not be tighter than the compact one.
        let full = FULL_MACHINES_H + FULL_WORKERS_H + FULL_TASKS_H + FULL_EVENTS_MIN_H + FULL_FOOTER_H;
        assert!(full <= FULL_H, "full panes need {full} rows, threshold is {FULL_H}");
    }

    #[test]
    fn key_hints_fit_their_tier_without_clipping() {
        // The single 161-character line this replaced was clipped on every
        // terminal, and the lost tail held the least guessable keys.
        for k in KEYS_FULL {
            assert!(width_of(k) <= FULL_W as usize, "{k} is {} columns", width_of(k));
        }
        for k in KEYS_COMPACT {
            assert!(width_of(k) <= MIN_W as usize, "{k} is {} columns", width_of(k));
        }
    }

    #[test]
    fn the_footer_advertises_the_cast_key_in_both_tiers() {
        // Regression guard: at 80 columns `S cast` fell off the clipped tail of
        // the old one-line hint, so the feature was undiscoverable exactly
        // where the terminal was most cramped.
        assert!(KEYS_FULL.iter().any(|k| k.contains("S cast")), "{KEYS_FULL:?}");
        assert!(KEYS_COMPACT.iter().any(|k| k.contains("S cast")), "{KEYS_COMPACT:?}");
    }

    #[test]
    fn task_rollup_survives_a_missing_or_empty_counts_object() {
        let text = |v: &serde_json::Value| -> String {
            task_rollup(v, false).spans.iter().map(|s| s.content.as_ref()).collect()
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
        let text: String =
            task_rollup(&counts, false).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("4/7 done"), "{text}");
        assert!(text.contains("1 open"), "{text}");
        assert!(text.contains("1 failed"), "{text}");
        assert!(text.contains("2 shelved"), "{text}");
    }

    #[test]
    fn task_rollup_hides_zero_failure_and_shelved_counters() {
        let counts = serde_json::json!({"crawl": {"done": 2, "failed": 0, "shelved": 0}});
        let text: String =
            task_rollup(&counts, false).spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("2/2 done"), "{text}");
        assert!(!text.contains("failed"), "a zero counter is noise: {text}");
        assert!(!text.contains("shelved"), "a zero counter is noise: {text}");
    }

    // --- cast overview ------------------------------------------------------

    fn roster_fixture() -> Roster {
        let voice = |name: &str, gender: &str, accent: &str, allowed: bool, enrolled: bool| {
            VoiceInfo {
                // Keys come from the real catalogue, so the fixture cannot drift
                // from what the picker actually receives — and a name the
                // catalogue does not declare keeps an empty key, as a clone does.
                key: bm_core::voices::key_for_name("vieneu", name).unwrap_or_default(),
                name: name.to_string(),
                gender: gender.to_string(),
                accent: accent.to_string(),
                language: "vi-VN".to_string(),
                style: "tin tức".to_string(),
                enrolled,
                allowed,
            }
        };
        Roster {
            engine: "vieneu".into(),
            source: "live".into(),
            voices: vec![
                voice("Đức Trí", "male", "South", true, false),
                voice("Adam", "male", "unknown", false, true),
                voice("Bắc Kỳ", "male", "Northern", false, false),
            ],
            cast: BTreeMap::from([
                ("Narrator".to_string(), "Đức Trí".to_string()),
                ("Kiên".to_string(), "Adam".to_string()),
                ("Vũ".to_string(), "Adam".to_string()),
                ("Lâm".to_string(), "Bắc Kỳ".to_string()),
                ("Hà".to_string(), "Đã Biến Mất".to_string()),
            ]),
            characters: vec![
                "Narrator".into(),
                "Kiên".into(),
                "Vũ".into(),
                "Lâm".into(),
                "Hà".into(),
                "Mới".into(),
            ],
            policy_note: "Central/South only".into(),
        }
    }

    #[test]
    fn cast_rows_put_narrator_first_and_keep_unassigned_speakers() {
        let rows = cast_rows(&roster_fixture());
        assert_eq!(rows[0].character, "Narrator", "Narrator is the fallback voice");
        assert_eq!(rows.len(), 6, "every speaker appears exactly once");
        let moi = rows.iter().find(|r| r.character == "Mới").unwrap();
        assert!(moi.unassigned());
        assert_eq!(moi.verdict(), Verdict::Unassigned);
    }

    #[test]
    fn cast_rows_flag_shared_voices_from_both_sides() {
        let rows = cast_rows(&roster_fixture());
        let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().clone();
        assert_eq!(by("Kiên").shared_with, vec!["Vũ".to_string()]);
        assert_eq!(by("Vũ").shared_with, vec!["Kiên".to_string()]);
        assert!(by("Kiên").shared());
        assert!(!by("Narrator").shared(), "a sole user of a voice is not flagged");
    }

    #[test]
    fn cast_rows_separate_blocked_from_unknown_and_accept_enrolled_clones() {
        let rows = cast_rows(&roster_fixture());
        let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().verdict();
        assert_eq!(by("Lâm"), Verdict::Blocked, "listed, and the policy rejects it");
        assert_eq!(by("Hà"), Verdict::Unknown, "the roster has never heard of it");
        assert_eq!(by("Kiên"), Verdict::Ok, "enrolled clones bypass the policy");
        assert_eq!(by("Narrator"), Verdict::Ok);
    }

    #[test]
    fn unassigned_speakers_do_not_count_as_sharing_the_empty_voice() {
        let mut r = roster_fixture();
        r.cast.retain(|k, _| k == "Kiên");
        let rows = cast_rows(&r);
        let unassigned: Vec<&CastRow> = rows.iter().filter(|x| x.unassigned()).collect();
        assert!(unassigned.len() > 1, "the fixture must have several unassigned speakers");
        assert!(unassigned.iter().all(|x| x.shared_with.is_empty()));
    }

    #[test]
    fn cast_rows_filter_by_speaker_voice_or_style_ignoring_diacritics() {
        let rows = cast_rows(&roster_fixture());
        assert_eq!(filtered_cast_rows(&rows, "duc tri").len(), 1, "matches the voice");
        assert_eq!(filtered_cast_rows(&rows, "adam").len(), 2, "both speakers on Adam");
        assert_eq!(filtered_cast_rows(&rows, "kien").len(), 1, "matches the speaker");
        assert_eq!(
            filtered_cast_rows(&rows, "tin tuc").len(),
            4,
            "the style is searchable too, and without diacritics"
        );
        assert_eq!(filtered_cast_rows(&rows, "   ").len(), rows.len(), "a blank filter keeps all");
        assert!(filtered_cast_rows(&rows, "nobody").is_empty());
    }

    #[test]
    fn cast_filter_preserves_row_order_and_never_invents_rows() {
        let rows = cast_rows(&roster_fixture());
        let filtered = filtered_cast_rows(&rows, "adam");
        let order: Vec<&String> = filtered.iter().map(|r| &r.character).collect();
        assert_eq!(order, vec!["Kiên", "Vũ"]);
    }

    #[test]
    fn cast_rows_carry_the_voice_metadata_through() {
        let rows = cast_rows(&roster_fixture());
        let narrator = rows.iter().find(|r| r.character == "Narrator").unwrap();
        assert_eq!(narrator.gender, "male");
        assert_eq!(narrator.accent, "South");
        assert!(narrator.in_roster);
        assert!(narrator.allowed && !narrator.enrolled);
        // A voice the roster does not list carries no metadata at all, so the
        // table renders dashes rather than a fabricated accent.
        let ha = rows.iter().find(|r| r.character == "Hà").unwrap();
        assert!(!ha.in_roster);
        assert!(ha.accent.is_empty() && ha.gender.is_empty());
    }

    // --- rendering ----------------------------------------------------------

    /// Render one frame into an in-memory terminal and flatten it to text.
    ///
    /// The responsive tiers are pure layout, so they can be checked without a
    /// real terminal — which is also the only way to prove the size guard does
    /// not panic on a degenerate area.
    fn render_text(app: &mut App, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn the_log_pane_has_one_title_aliases_and_local_time() {
        let mut app = App::new("http://127.0.0.1:8901");
        app.log_at(Level::Ok, "[192.168.2.2] enrolled x");
        app.log_at(Level::Error, "localhost-99: render failed: boom");
        app.log_at(Level::Info, "reconcile: nothing certain to fold");
        let text = render_text(&mut app, 140, 44);
        assert!(text.contains("Logs"), "pane renamed:\n{text}");
        assert!(!text.contains("Events"), "no stale title anywhere:\n{text}");
        let (alias, _) = worker_alias("192.168.2.2");
        assert!(text.contains(&format!("[{alias}]")), "machine line aliased:\n{text}");
        let (walias, _) = worker_alias("localhost-99");
        assert!(text.contains(&format!("[{walias}]")), "worker line aliased:\n{text}");
        assert!(text.contains("reconcile: nothing certain"), "plain lines pass through:\n{text}");
    }

    #[test]
    fn the_size_guard_replaces_the_dashboard_below_the_floor() {        let mut app = App::new("http://127.0.0.1:8901");
        let text = render_text(&mut app, 60, 16);
        assert!(text.contains("too small"), "{text}");
        assert!(!text.contains("Machines"), "no clipped panes behind the notice:\n{text}");
        assert!(text.contains("60×16"), "the notice names the actual size:\n{text}");
        assert!(text.contains("76×20"), "and the requirement:\n{text}");
    }

    #[test]
    fn the_size_guard_does_not_panic_on_a_degenerate_area() {
        let mut app = App::new("http://127.0.0.1:8901");
        // Only the first has room for the full notice; the slivers must simply
        // not panic, and must never leak a clipped dashboard.
        for (w, h) in [(60u16, 16u16), (1, 1), (0, 0), (200, 3), (3, 200)] {
            let text = render_text(&mut app, w, h);
            assert!(!text.contains("Machines"), "{w}x{h} rendered panes:\n{text}");
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
        assert!(!text.contains("┌Tasks"), "the Tasks pane is collapsed:\n{text}");
        assert!(text.contains("tasks:"), "its roll-up takes its place:\n{text}");
    }

    #[test]
    fn the_full_tier_shows_every_pane_and_the_new_key() {
        let mut app = App::new("http://127.0.0.1:8901");
        let text = render_text(&mut app, 140, 44);
        for pane in ["Machines", "Workers", "Tasks", "Logs"] {
            assert!(text.contains(pane), "{pane} is missing:\n{text}");
        }
        assert!(text.contains("S cast"), "the cast key is advertised:\n{text}");
    }

    #[test]
    fn an_empty_cluster_says_what_to_do_in_every_tier() {
        let mut app = App::new("http://127.0.0.1:8901");
        for (w, h) in [(80u16, 24u16), (140, 44)] {
            let text = render_text(&mut app, w, h);
            assert!(text.contains("no machines in the cluster"), "{w}x{h}:\n{text}");
            assert!(text.contains("no workers connected"), "{w}x{h}:\n{text}");
            assert!(text.contains("nothing has happened yet"), "{w}x{h}:\n{text}");
        }
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
        assert!(text.contains("6 speakers"), "the summary counts them:\n{text}");
        assert!(text.contains("4 voices in use"), "{text}");
        assert!(text.contains("1 shared"), "only Adam is shared:\n{text}");
        assert!(text.contains("2 to fix"), "Lâm and Hà:\n{text}");
        assert!(text.contains("1 unassigned"), "Mới:\n{text}");
        assert!(text.contains("shared with 1 other"), "{text}");
        assert!(text.contains("accent policy concern"), "Lâm is flagged:\n{text}");
        assert!(text.contains("unknown voice — stale cast?"), "Hà is flagged:\n{text}");
        assert!(text.contains("unassigned — v fills gaps"), "Mới is flagged:\n{text}");
    }

    #[test]
    fn the_cast_overview_without_a_roster_offers_the_retry_key() {
        let mut app = App::new("http://127.0.0.1:8901");
        app.screen = Screen::Cast(CastView::new());
        let text = render_text(&mut app, 120, 32);
        assert!(text.contains("roster not loaded — press R"), "{text}");
    }

    /// A small ledger: a shelved digest carrying a real failure reason, a
    /// render mid-flight, and a finished crawl. Sorted as a snapshot would be.
    fn tasks_app() -> App {
        let mut app = App::new("http://127.0.0.1:8901");
        let mut shelved = Task::new(3, Stage::Digest);
        shelved.state = TaskState::Shelved;
        shelved.attempts = 3;
        shelved.assigned_to = Some("w2".into());
        shelved.detail =
            "opencode exited 1: model 'claude' unavailable\nsecond line of the report".into();
        let mut running = Task::new(3, Stage::Render);
        running.state = TaskState::Running;
        running.assigned_to = Some("w1".into());
        running.lease_until = Some(bm_proto::now_secs() + 120);
        running.detail = "rendering segment 12/40".into();
        let mut done = Task::new(4, Stage::Crawl);
        done.state = TaskState::Done;
        done.detail = "ok".into();
        app.tasks = vec![shelved, running, done];
        app.tasks.sort_by_key(|t| (t.chapter, t.stage));
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn the_tasks_screen_shows_every_task_and_the_failure_reason() {
        let mut app = tasks_app();
        app.screen = Screen::Tasks(TasksView::new());
        let text = render_text(&mut app, 140, 44);
        assert!(text.contains("3 tasks"), "{text}");
        assert!(text.contains("1 shelved"), "{text}");
        assert!(text.contains("digest:3"), "the offending task is named:\n{text}");
        assert!(text.contains("crawl"), "finished work is still listed:\n{text}");
        assert!(
            text.contains("opencode exited 1"),
            "the detail column carries the reason:\n{text}"
        );
        assert!(text.contains("u retry  ·  F force re-run"), "{text}");
        assert!(text.contains(worker_alias("w2").0), "the worker that failed:\n{text}");
    }

    #[test]
    fn filtering_the_ledger_matches_stage_state_and_chapter() {
        let app = tasks_app();
        let all = &app.tasks;
        assert_eq!(filtered_tasks(all, "").len(), 3, "no filter, everything");
        assert_eq!(filtered_tasks(all, "   ").len(), 3, "whitespace is not a filter");
        assert_eq!(filtered_tasks(all, "shelved").len(), 1);
        assert_eq!(filtered_tasks(all, "  SHELVED ").len(), 1, "case and space insensitive");
        assert_eq!(filtered_tasks(all, "render")[0].chapter, 3);
        assert_eq!(filtered_tasks(all, "digest:3").len(), 1);
        assert_eq!(filtered_tasks(all, "4").len(), 1, "a chapter number matches");
        assert!(filtered_tasks(all, "merge").is_empty());
        assert!(
            filtered_tasks(all, "shel").len() == 1,
            "a partial state name still matches"
        );
    }

    #[test]
    fn an_empty_ledger_says_what_to_do_instead_of_drawing_nothing() {
        let mut app = App::new("http://127.0.0.1:8901");
        app.screen = Screen::Tasks(TasksView::new());
        let text = render_text(&mut app, 140, 44);
        assert!(text.contains("no tasks in the ledger yet"), "{text}");
        assert!(text.contains("press t to enqueue"), "{text}");
    }

    #[tokio::test]
    async fn k_opens_the_ledger_and_esc_closes_it() {
        let http = reqwest::Client::new();
        let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = tasks_app();
        handle_key(&mut app, key(KeyCode::Char('K')), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Tasks(_)), "{:?}", app.screen);
        let text = render_text(&mut app, 140, 44);
        assert!(text.contains("Tasks —"), "the overlay is open:\n{text}");

        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "{:?}", app.screen);
    }

    #[test]
    fn the_footer_names_the_ledger_when_work_is_shelved() {
        let mut app = tasks_app();
        // Wide enough that the footer is not clipped: the point is that the
        // count and the key are *there*, not that they survive an 80-column
        // terminal (the compact tier keeps them, minus the em-dash detail).
        let text = render_text(&mut app, 200, 44);
        assert!(text.contains("1 shelved — K tasks"), "{text}");
    }

    #[tokio::test]
    async fn enter_opens_the_task_page_and_shows_the_whole_reason() {
        let http = reqwest::Client::new();
        let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = tasks_app();
        app.screen = Screen::Tasks(TasksView::new());

        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        match &app.screen {
            Screen::TaskDetail(d) => assert_eq!((d.stage, d.chapter), (Stage::Digest, 3)),
            other => panic!("expected the detail page, got {other:?}"),
        }

        let text = render_text(&mut app, 140, 44);
        assert!(
            text.contains("opencode exited 1: model 'claude' unavailable"),
            "the first line of the reason:\n{text}"
        );
        assert!(
            text.contains("second line of the report"),
            "the *rest* of the reason, which the pane never showed:\n{text}"
        );
        assert!(text.contains("3 of 3 before it is shelved"), "{text}");
        assert!(text.contains(worker_alias("w2").0), "the worker that failed:\n{text}");
        assert!(text.contains("lease"), "the lease is on the page:\n{text}");

        // Esc returns to the list — and to the same view of it.
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Tasks(_)), "{:?}", app.screen);
    }

    #[tokio::test]
    async fn retry_from_the_list_targets_the_highlighted_task_only() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = tasks_app();
        app.screen = Screen::Tasks(TasksView::new());

        // Type "shelved": letters filter, they never act.
        for c in "shelved".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
        }
        match &app.screen {
            Screen::Tasks(v) => assert_eq!(v.filter, "shelved"),
            other => panic!("typing must filter, got {other:?}"),
        }
        assert!(job_rx.try_recv().is_err(), "a filter keystroke must not dispatch");

        handle_key(&mut app, key(KeyCode::Char('u')), &http, &job_tx).await;
        match job_rx.try_recv() {
            Ok(Job::Op { req, .. }) => {
                assert_eq!(req.op, Op::RetryTask);
                assert_eq!(req.stage, Some(Stage::Digest));
                assert_eq!(req.chapter, Some(3), "the filtered row, not the visible one");
                assert_eq!(req.force, Some(false));
            }
            other => panic!("expected a retry-task op, got {other:?}"),
        }
        assert_eq!(app.tasks.len(), 3, "the ledger is untouched locally");
        assert!(app.status.text.contains("digest:3 requeued"), "{}", app.status.text);

        // F is a different job (force), so the duplicate guard lets it through.
        handle_key(&mut app, key(KeyCode::Char('F')), &http, &job_tx).await;
        match job_rx.try_recv() {
            Ok(Job::Op { req, .. }) => {
                assert_eq!(req.force, Some(true), "F asks for a forced re-run");
                assert_eq!(req.chapter, Some(3));
            }
            other => panic!("expected a forced retry-task op, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn m_asks_first_and_confirms_into_a_reconcile_op() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = App::new("http://127.0.0.1:8901");
        handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "m"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Confirm(_)), ":m opens a confirm, not an op");
        assert!(job_rx.try_recv().is_err(), "nothing dispatches before confirm");
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        match job_rx.try_recv() {
            Ok(Job::Op { req, .. }) => assert_eq!(req.op, Op::Reconcile),
            other => panic!("expected a reconcile op, got {other:?}"),
        }
        assert!(app.status.text.contains("reconciling"), "{}", app.status.text);

        // A bare `m` from Normal mode is a stray key: it must never reach the
        // confirm, let alone dispatch.
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app2 = App::new("http://127.0.0.1:8901");
        handle_key(&mut app2, key(KeyCode::Char('m')), &http, &tx2).await;
        assert!(matches!(app2.screen, Screen::Normal), "a stray m must not open confirm");
        assert!(app2.status.text.contains("command line"), "{}", app2.status.text);
        assert!(rx2.try_recv().is_err(), "a stray m must never dispatch");
    }

    #[tokio::test]
    async fn colon_opens_a_command_line_that_presses_keys() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = App::new("http://127.0.0.1:8901");
        handle_key(&mut app, key(KeyCode::Char(':')), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Text(_)), ": opens the command line");
        // `:r` refreshes: a state fetch against a dead inductor fails
        // quietly into the status line, dispatching no job.
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "r"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "submit closes the prompt");
        assert!(job_rx.try_recv().is_err(), "refresh dispatches no job");
        // `:frobnicate` stays an error, `:q` quits through the normal path.
        app.screen = Screen::Text(TextPrompt::new(TextKind::Command, ":", "", "frobnicate"));
        handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
        assert!(app.status.text.contains("unknown command"), "{}", app.status.text);
    }

    #[tokio::test]
    async fn ctrl_u_clears_the_filter_and_never_requeues() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = tasks_app();
        app.screen = Screen::Tasks(TasksView::new());
        for c in "render".chars() {
            handle_key(&mut app, key(KeyCode::Char(c)), &http, &job_tx).await;
        }
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &http,
            &job_tx,
        )
        .await;
        match &app.screen {
            Screen::Tasks(v) => assert!(v.filter.is_empty(), "Ctrl-U clears the filter"),
            other => panic!("{other:?}"),
        }
        assert!(
            job_rx.try_recv().is_err(),
            "Ctrl-U must not be read as a plain `u` — that would re-queue a task"
        );
    }

    #[tokio::test]
    async fn retry_from_the_detail_page_stays_on_the_page() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = tasks_app();
        app.screen = Screen::TaskDetail(TaskDetail {
            stage: Stage::Digest,
            chapter: 3,
            scroll: 0,
            list: TasksView::new(),
        });
        handle_key(&mut app, key(KeyCode::Char('u')), &http, &job_tx).await;
        match job_rx.try_recv() {
            Ok(Job::Op { req, .. }) => assert_eq!(
                (req.op, req.stage, req.chapter),
                (Op::RetryTask, Some(Stage::Digest), Some(3))
            ),
            other => panic!("expected a retry-task op, got {other:?}"),
        }
        assert!(
            matches!(app.screen, Screen::TaskDetail(_)),
            "the page stays open so the state field can be watched changing: {:?}",
            app.screen
        );
        // The wake-up keys are on the page's own title, since this is where the
        // reason was read.
        let text = render_text(&mut app, 140, 44);
        assert!(text.contains("F force re-run"), "{text}");
        assert!(text.contains("Task digest:3"), "{text}");
    }

    #[test]
    fn two_retries_of_different_chapters_do_not_suppress_each_other() {
        // The in-flight guard is keyed by what the op acts on: a blanket
        // per-op guard would silently refuse the second retry.
        let a = OpRequest {
            op: Op::RetryTask,
            stage: Some(Stage::Digest),
            chapter: Some(3),
            ..Default::default()
        };
        let b = OpRequest { chapter: Some(4), ..a.clone() };
        assert_ne!(op_key(&a), op_key(&b));
        assert_eq!(op_key(&a), op_key(&a.clone()), "the same job twice is a duplicate");
        let forced = OpRequest { force: Some(true), ..a.clone() };
        assert_ne!(op_key(&a), op_key(&forced), "force is a different job");
    }

    #[test]
    fn scheduler_events_reach_the_pane_exactly_once() {
        let mut app = App::new("http://x");
        let snapshot = serde_json::json!({
            "tasks": [], "machines": [], "beats": [],
            "events": [
                {"id": 0, "ts": 1, "level": "error",
                 "text": "[w2] digest:3 FAILED (shelved — press u to retry): opencode exited 1"},
                {"id": 1, "ts": 2, "level": "ok", "text": "[w1] render:2 done in 4.2s"},
            ],
        });
        app.apply_state(snapshot.clone());
        assert!(app.events.iter().any(|l| l.text.contains("digest:3 FAILED")), "{:?}", app.events);
        assert!(app.events.iter().any(|l| l.text.contains("done in 4.2s")));
        // Levels ride along, so a failure reads as a failure.
        assert!(app
            .events
            .iter()
            .any(|l| l.level == Level::Error && l.text.contains("FAILED")));

        // The poller resends the whole buffer every 800 ms: nothing may repeat.
        app.apply_state(snapshot.clone());
        app.apply_state(snapshot);
        assert_eq!(
            app.events.iter().filter(|l| l.text.contains("FAILED")).count(),
            1,
            "a repeated snapshot must not duplicate the log: {:?}",
            app.events
        );

        // Only a new id appends.
        app.apply_state(serde_json::json!({
            "events": [{"id": 2, "ts": 3, "level": "warn",
                        "text": "lease expired — requeued 1: render:3"}],
        }));
        assert!(app.events.iter().any(|l| l.text.contains("lease expired")));
        assert_eq!(app.events.iter().filter(|l| l.text.contains("FAILED")).count(), 1);

        // A snapshot without the key (an older inductor) must not panic or clear.
        let before = app.events.len();
        app.apply_state(serde_json::json!({ "tasks": [] }));
        assert_eq!(app.events.len(), before);
    }

    #[test]
    fn a_restarted_inductor_resets_the_event_cursor() {
        let mut app = App::new("http://x");
        app.apply_state(serde_json::json!({
            "events": [{"id": 7, "ts": 1, "level": "ok", "text": "seven"}],
        }));
        // A fresh inductor counts from zero again; without the reset its whole
        // history would look older than the last id we saw and be dropped.
        app.apply_state(serde_json::json!({
            "events": [
                {"id": 0, "ts": 2, "level": "info", "text": "after restart"},
                {"id": 1, "ts": 3, "level": "ok", "text": "and again"},
            ],
        }));
        assert!(app.events.iter().any(|l| l.text.contains("after restart")), "{:?}", app.events);
        assert!(app.events.iter().any(|l| l.text.contains("and again")));
        assert!(app.events.iter().any(|l| l.text.contains("event stream reset")));
    }

    #[test]
    fn both_key_lines_advertise_the_task_list() {
        assert!(KEYS_FULL.iter().any(|k| k.contains("K tasks")), "{KEYS_FULL:?}");
        assert!(KEYS_COMPACT.iter().any(|k| k.contains("K tasks")), "{KEYS_COMPACT:?}");
    }

    #[tokio::test]
    async fn pick_filter_accepts_letters_instead_of_moving() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = App::new("http://127.0.0.1:8901");
        app.roster = Some(roster_fixture());
        app.screen = Screen::Pick(Picker::new());
        // "Kiên" starts with K: if movement still owned letters, this would
        // move the cursor instead of typing.
        handle_key(&mut app, key(KeyCode::Char('j')), &http, &job_tx).await;
        match &app.screen {
            Screen::Pick(p) => {
                assert_eq!(p.filter, "j");
                assert_eq!(p.cursor, 0, "typing resets the cursor, it never moves it");
            }
            other => panic!("typing must filter, got {other:?}"),
        }
        assert!(job_rx.try_recv().is_err(), "a filter keystroke must not dispatch");
    }

    #[tokio::test]
    async fn cast_filter_accepts_every_letter_including_q() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = App::new("http://127.0.0.1:8901");
        app.roster = Some(roster_fixture());
        app.screen = Screen::Cast(CastView::new());
        handle_key(&mut app, key(KeyCode::Char('j')), &http, &job_tx).await;
        match &app.screen {
            Screen::Cast(v) => {
                assert_eq!(v.filter, "j");
                assert_eq!(v.cursor, 0, "typing resets the cursor, it never moves it");
            }
            other => panic!("typing must filter, got {other:?}"),
        }
        // `q` closes other screens but must type here: it is Esc that closes.
        handle_key(&mut app, key(KeyCode::Char('q')), &http, &job_tx).await;
        match &app.screen {
            Screen::Cast(v) => assert_eq!(v.filter, "jq"),
            other => panic!("q must type into the filter, got {other:?}"),
        }
        assert!(job_rx.try_recv().is_err(), "a filter keystroke must not dispatch");
    }

    #[tokio::test]
    async fn pick_esc_steps_back_from_voice_to_character_then_closes() {
        let http = reqwest::Client::new();
        let (job_tx, _job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = App::new("http://127.0.0.1:8901");
        let mut p = Picker::new();
        p.stage = PickStage::Voice;
        p.character = "Kiên".into();
        p.filter = "duc".into();
        app.screen = Screen::Pick(p);
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        match &app.screen {
            Screen::Pick(p) => {
                assert_eq!(p.stage, PickStage::Character);
                assert!(p.filter.is_empty(), "stepping back clears the voice filter");
            }
            other => panic!("first Esc must step back a stage, got {other:?}"),
        }
        handle_key(&mut app, key(KeyCode::Esc), &http, &job_tx).await;
        assert!(matches!(app.screen, Screen::Normal), "second Esc closes: {:?}", app.screen);
    }

    #[tokio::test]
    async fn tasks_filter_accepts_j_and_k_instead_of_moving() {
        let http = reqwest::Client::new();
        let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let mut app = tasks_app();
        app.screen = Screen::Tasks(TasksView::new());
        handle_key(&mut app, key(KeyCode::Char('j')), &http, &job_tx).await;
        match &app.screen {
            Screen::Tasks(v) => {
                assert_eq!(v.filter, "j");
                assert_eq!(v.cursor, 0, "typing resets the cursor, it never moves it");
            }
            other => panic!("typing must filter, got {other:?}"),
        }
        handle_key(&mut app, key(KeyCode::Char('k')), &http, &job_tx).await;
        match &app.screen {
            Screen::Tasks(v) => assert_eq!(v.filter, "jk"),
            other => panic!("typing must filter, got {other:?}"),
        }
        assert!(job_rx.try_recv().is_err(), "a filter keystroke must not dispatch");
    }

