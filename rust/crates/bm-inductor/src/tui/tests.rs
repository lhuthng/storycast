//! Key and render tests, moved as one file.
use super::app::App;
use super::audio::Player;
use super::audition::AuditionLine;
use super::draw::draw;
use super::input::command::{busy_summary, command_key, do_command, split_args, Command, WORDS};
use super::input::runconfig::{
    parse_mix_config, parse_render_batch, parse_run_config, run_preview, save_app_setting,
    save_render_batch, save_run_config,
};
use super::input::submit::submit_text;
use super::input::{handle_key, op_key, urlencode};
use super::jobs::{
    job_segment, run_job, set_machine_state, unreachable_verdict, verdict_after_failed_provision,
    BackgroundJob, DoneKind, Ev, Job, ProfileReq, Res, WorkspaceReq,
};
use super::layout::{
    cols, size_class, width_of, Size, COMPACT_EVENTS_MIN_H, COMPACT_FOOTER_H,
    COMPACT_MACHINES_MAX_H, COMPACT_MACHINES_MIN_H, COMPACT_MACHINE_COLS, COMPACT_TASKS_MAX_H,
    COMPACT_TASKS_MIN_H, COMPACT_WORKERS_MAX_H, COMPACT_WORKERS_MIN_H, COMPACT_WORKER_COLS,
    FULL_EVENTS_MIN_H, FULL_FOOTER_H, FULL_H, FULL_HEADER_H, FULL_MACHINES_MAX_H,
    FULL_MACHINES_MIN_H, FULL_TASKS_MAX_H, FULL_TASKS_MIN_H, FULL_W, FULL_WORKERS_MAX_H,
    FULL_WORKERS_MIN_H, KEYS_COMPACT, KEYS_FULL, MIN_H, MIN_W,
};
use super::model::*;
use super::screen::*;
use super::sound::{self, SoundView};
use super::style::*;
use super::*;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bm_proto::{
    Heartbeat, Machine, MachineState, Op, OpRequest, Roster, Stage, Task, TaskPref, TaskState,
    VoiceInfo,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use std::collections::BTreeMap;

fn roster_fixture() -> Roster {
    let voice = |name: &str, gender: &str, accent: &str, enrolled: bool| {
        VoiceInfo {
            // Keys come from the real catalogue, so the fixture cannot drift
            key: bm_core::voices::key_for_name("vieneu", name).unwrap_or_default(),
            name: name.to_string(),
            gender: gender.to_string(),
            accent: accent.to_string(),
            language: "vi-VN".to_string(),
            style: "tin tức".to_string(),
            pool_tags: Vec::new(),
            enrolled,
        }
    };
    // A pooled sample, as `voice-pool.json` describes it: tags and all. It is
    let pooled = |name: &str, tags: &[&str]| VoiceInfo {
        key: String::new(),
        name: name.to_string(),
        gender: "unknown".into(),
        accent: "unknown".into(),
        language: "vi-VN".to_string(),
        style: format!("pool: {}", tags.join(", ")),
        pool_tags: tags.iter().map(|t| t.to_string()).collect(),
        enrolled: true,
    };
    Roster {
        engine: "vieneu".into(),
        source: "live".into(),
        voices: vec![
            voice("Đức Trí", "male", "South", false),
            voice("Adam", "male", "unknown", true),
            voice("Bắc Kỳ", "male", "Northern", false),
            pooled("young-male-10", &["young", "male"]),
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
    }
}
/// Render one frame into an in-memory terminal and flatten it to text.
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
/// Whether `hint` is on the rendered screen, **as a reader would see it**.
fn hint_visible(text: &str, hint: &str) -> bool {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.contains(hint)
}
/// A beat with the fields the panes read, fresh unless told otherwise.
fn beat(id: &str, addr: &str, age_secs: u64, alias: &str) -> Heartbeat {
    Heartbeat {
        worker_id: id.into(),
        addr: addr.into(),
        task_id: None,
        stage: None,
        chapter: None,
        progress: 0.0,
        activity: "idle".into(),
        eta_secs: None,
        ts: bm_proto::now_secs().saturating_sub(age_secs),
        hostname: format!("host-{id}"),
        alias: alias.into(),
        cpu_pct: None,
        mem_pct: None,
        mem_gb: None,
        sidecars: None,
        sidecar_gb: None,
        capabilities: vec![],
        sources_stages: Vec::new(),
        sidecar_keep: None,
        tts_threads: None,
        cores: None,
    }
}
fn named_machine(addr: &str, name: &str) -> Machine {
    let mut m = Machine::new(addr, "thang", 22, None, "worker");
    m.name = name.into();
    m
}
fn stats_app() -> App {
    // One worker mid-render (half done), one idle; history says a render
    let mut app = App::new("http://127.0.0.1:8901");
    app.machines = vec![named_machine("192.168.2.2", "hawk")];
    let mut busy = beat("thang-marmot", "192.168.2.2", 2, "marmot");
    busy.stage = Some(Stage::Render);
    busy.chapter = Some(7);
    busy.progress = 0.5;
    busy.cpu_pct = Some(25.0);
    busy.mem_pct = Some(40.0);
    busy.mem_gb = Some(4.5);
    let idle = beat("localhost-caracal", "127.0.0.1", 2, "caracal");
    app.beats = vec![busy, idle];
    app.stats = super::model::parse_stats(Some(&serde_json::json!({
        "counts": {"thang-marmot": {"render": 3, "digest": 1}},
        "avg_task_secs": {"render": 100.0, "digest": 40.0},
    })));
    app
}
/// A small ledger: a shelved digest carrying a real failure reason, a
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
/// A ledger with one row out with a box that has gone quiet, one out with a box
fn ledger_with_a_silent_box() -> (App, std::collections::BTreeSet<String>) {
    let mut app = App::new("http://127.0.0.1:8901");
    let mut orphaned = Task::new(7, Stage::Merge);
    orphaned.state = TaskState::Assigned;
    orphaned.assigned_to = Some("hcm-1".into());
    let mut working = Task::new(8, Stage::Render);
    working.state = TaskState::Running;
    working.assigned_to = Some("hcm-2".into());
    let mut queued = Task::new(9, Stage::Render);
    queued.state = TaskState::Pending;
    app.tasks = vec![orphaned, working, queued];
    app.tasks.sort_by_key(|t| (t.chapter, t.stage));
    // Only the second box is beating. An empty set would make every assigned
    let live: std::collections::BTreeSet<String> = ["hcm-2".to_string()].into_iter().collect();
    (app, live)
}
/// A `:` line, one keypress at a time, then Enter on it.
async fn type_command(
    app: &mut App,
    http: &reqwest::Client,
    job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
    word: &str,
) {
    handle_key(app, key(KeyCode::Char(':')), http, job_tx).await;
    for c in word.chars() {
        handle_key(app, key(KeyCode::Char(c)), http, job_tx).await;
    }
    handle_key(app, key(KeyCode::Enter), http, job_tx).await;
}
/// Long enough to clear `MIN_LINE_CHARS`, so the chooser prefers it over
fn audition_line(tag: &str) -> String {
    format!("{tag} — một câu đủ dài để làm mẫu thử giọng đọc cho nhân vật này nhé")
}
/// A picker at step 2 for Narrator (cast to Đức Trí), filtered to a single
fn audition_app() -> App {
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster_fixture());
    let mut p = Picker::new();
    p.stage = PickStage::Voice;
    p.character = "Narrator".into();
    p.filter = "adam".into();
    app.screen = Screen::Pick(p);
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("một"), audition_line("hai")],
    )]));
    app
}
/// Pull the `OpRequest` a keypress dispatched, if it dispatched one.
fn last_op(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Job>) -> Option<OpRequest> {
    match rx.try_recv().ok()?.into_bare() {
        Job::Op { req, .. } => Some(req),
        other => panic!("expected an Op job, got {other:?}"),
    }
}
/// Release the in-flight audition slot the way a completed op would.
fn finish_audition(app: &mut App, voice: &str) {
    app.apply(Ev::Done(DoneKind::Op {
        op: Op::PreviewVoice,
        key: op_key(&OpRequest {
            op: Op::PreviewVoice,
            ..Default::default()
        }),
        ok: true,
        voice: Some(voice.to_string()),
        audio_b64: None,
        line_speaker: None,
        line_text: None,
    }));
}
fn cast_app() -> App {
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster_fixture());
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("n")],
    )]));
    app.screen = Screen::Cast(CastView::new());
    app
}
fn bind_app() -> App {
    let mut app = App::new("http://x");
    app.settings = Some(serde_json::json!({"ssh": {"user": "op", "port": 2222}}));
    app
}
fn bind_machine(app: &mut App, buf: &str) -> Machine {
    let p = TextPrompt::new(TextKind::AddMachine, "t", "h", buf);
    match submit_text(app, &p) {
        Ok(Job::AddMachine { m, .. }) => m,
        other => panic!("bind {buf:?} must dispatch AddMachine, got {other:?}"),
    }
}
/// A checkout holding the fixture scene map and registries, with every clip
fn sound_layout(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    bm_core::profile::install_fixture(dir.path()).expect("fixture profile");
    for kind in bm_core::audio_pool::PoolKind::ALL {
        let pool = bm_core::audio_pool::load_pool(&layout.pool(kind));
        assert!(!pool.is_empty(), "{} empty", kind.registry());
        for sound in pool.values() {
            for f in &sound.files {
                let p = layout.assets().join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"").unwrap();
            }
        }
    }
    let _ = tag;
    (dir, layout.root.clone())
}
/// An app parked on the sound editor with the pools loaded, as the screen
fn sound_app(root: &std::path::Path) -> App {
    let mut app = App::new("http://unused");
    let layout = bm_core::Layout::new(root);
    app.sound = Some(sound::load(&layout).expect("the fixture loads"));
    app.layout = layout;
    app.screen = Screen::Sound(SoundView::new());
    app
}
/// A checkout with one chapter's script and one rendered segment in it, for
fn local_cache_layout() -> (tempfile::TempDir, bm_core::Layout) {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    layout.ensure().unwrap();
    std::fs::write(
        layout.script(1),
        serde_json::json!({"segments": [
            {"speaker": "Narrator", "text": "Nar nói."},
        ]})
        .to_string(),
    )
    .unwrap();
    let seg = layout.seg_dir("vieneu", 1);
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0000_Đức Trí.wav"), b"RIFF-fake-local").unwrap();
    (dir, layout)
}
fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}
fn job_channel() -> tokio::sync::mpsc::UnboundedSender<Job> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    tx
}
/// A roster with two pool groups and two unique voices, and a cast that
fn pooled_roster() -> Roster {
    let mut r = roster_fixture();
    let sample = |name: &str, tags: &[&str]| VoiceInfo {
        key: String::new(),
        name: name.to_string(),
        gender: "unknown".into(),
        accent: "unknown".into(),
        language: "vi-VN".into(),
        style: format!("pool: {}", tags.join(", ")),
        pool_tags: tags.iter().map(|t| t.to_string()).collect(),
        enrolled: true,
    };
    r.voices = vec![
        sample("young-male-10", &["young", "male"]),
        sample("young-male-3", &["male", "young"]),
        sample("old-female-2", &["female", "old"]),
        VoiceInfo {
            key: String::new(),
            name: "Võ Tắc Thiên".into(),
            gender: "male".into(),
            accent: "South".into(),
            language: "vi-VN".into(),
            style: "trầm".into(),
            pool_tags: Vec::new(),
            enrolled: false,
        },
        VoiceInfo {
            key: String::new(),
            name: "Bắc Kỳ".into(),
            gender: "male".into(),
            accent: "Northern".into(),
            language: "vi-VN".into(),
            style: "tin tức".into(),
            pool_tags: Vec::new(),
            enrolled: false,
        },
    ];
    // `young-male-10` is the busiest of the two male+young samples, so it must
    r.cast.insert("Kiên".into(), "young-male-10".into());
    r.cast.insert("Vũ".into(), "young-male-10".into());
    r.cast.insert("Bé Mắt".into(), "old-female-2".into());
    r
}
fn voice_app(roster: Roster) -> App {
    let mut app = App::new("http://127.0.0.1:8901");
    app.conn = Conn::Up;
    app.roster = Some(roster);
    let mut p = Picker::new();
    p.stage = PickStage::Voice;
    p.character = "Narrator".into();
    app.screen = Screen::Pick(p);
    // Auditioning reads the scripts; without a line index `T` has nothing to
    app.lines = Some(std::collections::HashMap::from([(
        "Narrator".to_string(),
        vec![audition_line("một"), audition_line("hai")],
    )]));
    app
}
/// The list as `(kind, label, name)` triples, so a test can read the whole
fn listed(app: &App) -> Vec<(Option<VoiceKind>, String, Option<String>)> {
    filtered_voices(app, "")
        .iter()
        .map(|r| match r {
            VoiceRow::Group { kind, tags, .. } => (Some(*kind), tags.clone(), None),
            VoiceRow::Voice { voice, .. } => (None, String::new(), Some(voice.name.clone())),
        })
        .collect()
}
/// The one thing the main loop does with every event, verbatim from `tui.rs`:
async fn pump_like_the_main_loop(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<Ev>) {
    // `relayout` re-requests the caches it just dropped, and a receiver that
    let (dead_tx, _dead_rx) = tokio::sync::mpsc::unbounded_channel();
    while let Ok(ev) = rx.try_recv() {
        let relayout = matches!(ev, Ev::Done(DoneKind::Relayout));
        app.apply(ev);
        if relayout {
            app.relayout(&dead_tx, &reqwest::Client::new());
        }
    }
}
/// The workspace job, with its `cluster_busy` guard left out and everything
async fn the_workspace_job_without_its_cluster_guard(
    job: Job,
    tx: tokio::sync::mpsc::UnboundedSender<Ev>,
) {
    match job {
        Job::Workspace { layout, req, .. } => {
            let root = layout.root.clone();
            let listing = matches!(req, WorkspaceReq::List);
            let cmd = match req {
                WorkspaceReq::List => crate::WorkspaceCmd::List,
                WorkspaceReq::Use(name) => crate::WorkspaceCmd::Use { name },
                WorkspaceReq::New {
                    name,
                    profile,
                    crawler,
                } => crate::WorkspaceCmd::New {
                    name,
                    profile,
                    crawler,
                },
            };
            let out = tokio::task::spawn_blocking(move || crate::workspace_cmd(&root, cmd))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("workspace task failed: {e}")));
            let switched = out.is_ok() && !listing;
            let (level, lines) = match out {
                Ok(lines) => (Level::Info, lines),
                Err(e) => (Level::Error, vec![format!("workspace: {e:#}")]),
            };
            for text in lines {
                let _ = tx.send(Ev::Log(LogLine {
                    level,
                    wall: 0,
                    text,
                }));
            }
            let _ = tx.send(Ev::Done(if switched {
                DoneKind::Relayout
            } else {
                DoneKind::Other
            }));
        }
        other => run_job(other, tx).await,
    }
}
/// A checkout with two books, one of which is not one, and the pointer on the
fn two_workspaces_one_bad(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("bm-ws-pick-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    let book = root.join("workspaces/book-a");
    std::fs::create_dir_all(book.join("data/chapters")).unwrap();
    std::fs::create_dir_all(book.join("data/script")).unwrap();
    bm_core::config::Settings::default()
        .save(&book.join("settings.json"))
        .unwrap();
    std::fs::write(book.join("data/chapters/ch01.txt"), "x").unwrap();
    std::fs::write(book.join("data/script/01.json"), "{}").unwrap();
    // A directory somebody left under workspaces/, with no settings at all.
    std::fs::create_dir_all(root.join("workspaces/scratch")).unwrap();
    std::fs::create_dir_all(root.join(".bm")).unwrap();
    std::fs::write(bm_core::Layout::active_workspace_file(&root), "book-a\n").unwrap();
    root
}

mod app;
mod audition;
mod cast;
mod cloud;
mod crawl;
mod digest_manager;
mod guided_create;
mod hints;
mod jobs;
mod machines;
mod panes;
mod prompts;
mod responsive;
mod run;
mod runconfig;
mod samples;
mod script_window;
mod settings;
mod sound_editor;
mod task_page;
mod tasks;
mod theme;
mod voices;
mod workspaces;
