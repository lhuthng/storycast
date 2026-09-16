//! Scheduler state: the ledger the orchestrator owns.
//!
//! Workers report facts; every transition below is a decision. The ledger is
//! persisted on each mutation, and reconciled from artifacts on startup, so a
//! restart resumes instead of restarting.

use bm_core::{config::Settings, Layout};
use bm_proto::{Machine, Stage, Task};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

mod ledger;
mod offer;
mod ops;
mod reconcile;

const LEASE_SECS: [(Stage, u64); 4] = [
    (Stage::Crawl, 600),
    (Stage::Digest, 1200),
    (Stage::Render, 5400),
    (Stage::Merge, 1800),
];

fn lease_for(stage: Stage) -> u64 {
    LEASE_SECS
        .iter()
        .find(|(s, _)| *s == stage)
        .map(|(_, l)| *l)
        .unwrap_or(600)
}

/// Maximum number of events kept in memory. Older entries fall off the front.
const EVENT_CAP: usize = 200;

/// An event from the scheduler: task completion/failure, lease expiry, operator
/// actions, etc. Surfaced in `/api/state` so the TUI can show them live.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: u64,
    pub ts: u64,
    /// `"info"` | `"ok"` | `"warn"` | `"error"`
    pub level: String,
    pub text: String,
}

pub struct Inner {
    pub layout: Layout,
    pub settings: Settings,
    pub tasks: HashMap<String, Task>,
    pub machines: HashMap<String, Machine>,
    pub workers: HashMap<String, String>,
    /// Advertised capabilities per worker, refreshed at every registration.
    /// Drives the render gate: only workers with `render-segments` are
    /// offered render tasks (they upload units; older agents keep files
    /// locally, which the completion gate would fail anyway).
    pub caps: HashMap<String, Vec<String>>,
    pub beats: HashMap<String, bm_proto::Heartbeat>,
    /// Boot time: the orphan pass in `reap` stays quiet for the first 120s
    /// so a reboot never mistakes still-grinding workers (whose beats arrive
    /// within seconds) for dead ones.
    pub started_at: u64,
    /// Ring buffer of scheduler events surfaced to the TUI.
    pub events: VecDeque<EventRecord>,
    next_event_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bm_core::config::Settings;
    use bm_proto::{now_secs, Complete, Stage, Task, TaskState};
    use serde_json::Value;

    fn fixture() -> (tempfile::TempDir, Inner) {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        std::fs::create_dir_all(layout.data()).unwrap();
        std::fs::create_dir_all(layout.output()).unwrap();
        std::fs::write(layout.bible(), r#"{"characters":[]}"#).unwrap();
        let inner = Inner::new(layout, Settings::default());
        (d, inner)
    }

    fn old_shape_doc() -> Value {
        let mut m1 = bm_proto::Machine::new(
            "192.168.2.2",
            "thang",
            22,
            Some("~/.ssh/ssh-key-my-wsl".into()),
            "worker",
        );
        m1.state = bm_proto::MachineState::Online;
        m1.last_seen = 123;
        m1.note = "provisioned".into();
        m1.capabilities = vec!["gpu".into()];
        m1.tts_url = Some("http://127.0.0.1:8818".into());
        let m2 = bm_proto::Machine::new("127.0.0.1", "thang", 22, None, "worker");
        serde_json::json!({
            "tasks": [],
            "machines": [m1, m2],
            "workers": {"w1": "192.168.2.2"},
        })
    }

    #[test]
    fn ledger_splits_config_from_runtime_and_migrates_old_shape() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let ledger = layout.bm_state().join("ledger.json");
        bm_core::write_json(&ledger, &old_shape_doc()).unwrap();

        inner.load_ledger();
        // Joined view: config and runtime both survive the split.
        assert_eq!(inner.machines.len(), 2);
        let m1 = &inner.machines["192.168.2.2"];
        assert_eq!(m1.ssh_key.as_deref(), Some("~/.ssh/ssh-key-my-wsl"));
        assert_eq!(m1.state, bm_proto::MachineState::Online);
        assert_eq!((m1.last_seen, m1.note.as_str()), (123, "provisioned"));
        assert_eq!(m1.capabilities, vec!["gpu".to_string()]);
        assert_eq!(inner.machines["127.0.0.1"].ssh_key, None);
        assert_eq!(
            inner.workers.get("w1").map(String::as_str),
            Some("192.168.2.2")
        );

        // Config file: both boxes, keys byte-for-byte, names default to addr.
        let boxes = bm_core::provision::load_boxes(&layout.machines());
        assert_eq!(boxes.len(), 2);
        let b1 = boxes.iter().find(|b| b.addr == "192.168.2.2").unwrap();
        assert_eq!(
            (b1.name.as_str(), b1.key.as_deref()),
            ("192.168.2.2", Some("~/.ssh/ssh-key-my-wsl"))
        );

        // Ledger file: new shape, and the pre-migration snapshot is kept.
        let disk: Value = bm_core::read_json(&ledger).unwrap();
        assert!(disk.get("machines").is_none(), "old array is gone");
        let st = disk["machine_state"].as_object().unwrap();
        assert_eq!(st.len(), 2);
        assert_eq!(st["192.168.2.2"]["state"], serde_json::json!("online"));
        assert!(
            ledger.with_extension("json.bak").exists(),
            "pre-migration snapshot"
        );

        // Idempotent: a second load over the migrated file changes nothing.
        let mut again = Inner::new(layout.clone(), Settings::default());
        again.load_ledger();
        assert_eq!(again.machines.len(), 2);
        assert_eq!(
            again.machines["192.168.2.2"].ssh_key.as_deref(),
            Some("~/.ssh/ssh-key-my-wsl")
        );
        let disk2: Value = bm_core::read_json(&ledger).unwrap();
        assert_eq!(disk, disk2, "reload must not rewrite");
    }

    #[test]
    fn save_writes_runtime_only() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let (bxo, rt) = bm_core::provision::split_machine(
            &bm_proto::Machine::new("10.0.0.9", "thang", 22, Some("/k".into()), "worker"),
            "box-9",
        );
        bm_core::provision::save_box(&layout.machines(), &bxo).unwrap();
        inner.machines.insert(
            "10.0.0.9".into(),
            bm_core::provision::join_machine(&bxo, Some(&rt)),
        );
        inner.save();

        let disk: Value = bm_core::read_json(&layout.bm_state().join("ledger.json")).unwrap();
        assert!(
            disk.get("machines").is_none(),
            "config never lands in the ledger"
        );
        assert_eq!(
            disk["machine_state"]["10.0.0.9"]["note"],
            serde_json::json!("")
        );
    }

    #[test]
    fn migration_holds_for_the_live_ledger() {
        // Local-only gate (not CI): point at a copy of the real ledger and
        // prove the migration loses nothing at real scale.
        let src = std::env::var("BM_REAL_LEDGER").unwrap_or_default();
        if src.is_empty() {
            return;
        }
        let raw = std::fs::read_to_string(&src).expect("BM_REAL_LEDGER readable");
        let doc: Value = serde_json::from_str(&raw).unwrap();
        let tasks_n = doc["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
        assert!(
            tasks_n > 100,
            "this gate wants the real ledger, got {tasks_n} tasks"
        );

        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.bm_state().join("ledger.json"), &raw).unwrap();
        let mut inner = Inner::new(layout, Settings::default());
        inner.load_ledger();

        assert_eq!(inner.tasks.len(), tasks_n, "no task lost");
        assert_eq!(
            inner.machines["192.168.2.2"].ssh_key.as_deref(),
            Some("~/.ssh/ssh-key-my-wsl"),
            "the live key survives"
        );
        assert_eq!(inner.machines["127.0.0.1"].ssh_key, None);

        let snap: Vec<(String, String)> = {
            let mut v: Vec<_> = inner
                .machines
                .values()
                .map(|m| (m.addr.clone(), serde_json::to_string(m).unwrap()))
                .collect();
            v.sort();
            v
        };
        inner.load_ledger();
        let snap2: Vec<(String, String)> = {
            let mut v: Vec<_> = inner
                .machines
                .values()
                .map(|m| (m.addr.clone(), serde_json::to_string(m).unwrap()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(snap, snap2, "migration is idempotent at real scale");
    }

    #[test]
    fn swap_invalidates_only_the_characters_files() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 1);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0002_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(1), vec![0u8; 2000]).unwrap();

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(msg.contains("Đức Trí -> Minh Triết"), "{msg}");
        assert!(
            !seg.join("0000-0001_Đức Trí.wav").exists(),
            "stale run file must go"
        );
        assert!(
            seg.join("0002_Adam.wav").exists(),
            "other voices keep cache"
        );
        assert!(!layout.final_mp3(1).exists(), "stale product goes away");
        assert_eq!(inner.tasks["render:1"].state, TaskState::Pending);
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Pending);
        // The swap's *meaning* is checked through the reader (which resolves
        // keys back to names), and the stored form is checked directly — the
        // file persists keys now, so both assertions matter.
        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["A"], "Minh Triết");
        assert_eq!(cast["B"], "Adam", "untouched speakers survive");
        let disk: std::collections::HashMap<String, String> =
            bm_core::read_json(&layout.cast("vieneu")).unwrap();
        assert_eq!(disk["A"], "minh-triet", "persisted as a key, not a name");
        assert_eq!(disk["B"], "adam");
    }

    #[test]
    fn merge_affinity_is_local_after_a_remote_render() {
        // Step 13: segments live on the inductor now, so the merge runs where
        // they are — never where the render happened to run.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(7),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 7);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0001_Adam.wav"), vec![0u8; 2000]).unwrap();
        let mut t = Task::new(7, Stage::Render);
        t.state = TaskState::Running;
        t.assigned_to = Some("remote-w".into());
        inner.tasks.insert("render:7".into(), t);

        inner.complete(&completion(
            "remote-w",
            "render:7",
            true,
            "render ch7 (2 calls)",
        ));
        assert_eq!(
            inner.tasks["render:7"].state,
            TaskState::Done,
            "gate passes: files are home"
        );
        let m = inner.tasks.get("merge:7").expect("merge task exists");
        assert_eq!(
            m.affinity.as_deref(),
            Some("127.0.0.1"),
            "merge runs where the segments are"
        );
    }

    #[test]
    fn merge_without_a_payload_needs_the_file_on_disk() {
        // Local nodes ship no mp3_b64; the file itself is the evidence. A
        // report with neither is a failure, not a silent Done.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        for ch in [8u32, 9] {
            let mut t = Task::new(ch, Stage::Merge);
            t.state = TaskState::Running;
            t.assigned_to = Some("w1".into());
            inner.tasks.insert(format!("merge:{ch}"), t);
        }
        std::fs::create_dir_all(layout.final_mp3(8).parent().unwrap()).unwrap();
        std::fs::write(layout.final_mp3(8), vec![0u8; 2000]).unwrap();

        inner.complete(&completion("w1", "merge:8", true, "merge ch8 -> out.mp3"));
        assert_eq!(
            inner.tasks["merge:8"].state,
            TaskState::Done,
            "file present: done"
        );
        let msg = inner.complete(&completion("w1", "merge:9", true, "merge ch9 -> out.mp3"));
        assert!(
            msg.contains("failed"),
            "no payload and no file fails: {msg}"
        );
        assert!(msg.contains("no file"), "the absence is named: {msg}");
        assert_eq!(inner.tasks["merge:9"].state, TaskState::Pending);
    }

    #[test]
    fn render_offer_requires_the_upload_capability() {
        // Staged rollout: an agent without `render-segments` keeps every
        // other stage but never renders (its report would fail the gate
        // anyway). Unknown workers are allowed — failing closed would strand
        // anything that never registered.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(5),
            r#"{"segments":[{"speaker":"A","text":"a full sentence for synthesis here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
        ] {
            let mut t = Task::new(5, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:5"), t);
        }
        let mut t = Task::new(5, Stage::Render);
        t.state = TaskState::Pending;
        inner.tasks.insert("render:5".into(), t);
        inner.workers.insert("old-w".into(), "192.168.2.2".into());
        inner.caps.insert(
            "old-w".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "merge".into(),
            ],
        );
        inner.workers.insert("new-w".into(), "192.168.2.2".into());
        inner.caps.insert(
            "new-w".into(),
            vec![
                "crawl".into(),
                "digest".into(),
                "render".into(),
                "merge".into(),
                "render-segments".into(),
            ],
        );

        assert!(inner.offer("old-w").is_none(), "old agent never renders");
        let offer = inner.offer("new-w").expect("capable worker renders");
        assert_eq!(offer.task_id, "render:5");
        assert!(!offer.local_node, "192.168.2.2 is not the local node");
        assert!(offer.render_units.is_some(), "planned, not legacy");
    }

    #[test]
    fn offer_marks_the_local_node() {
        // The provisioner's `Ssh.local` and the offer's `local_node` run on
        // the same predicate — one fact, checked on both sides of the wire.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(5),
            r#"{"segments":[{"speaker":"A","text":"a full sentence for synthesis here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        for (stage, state) in [
            (Stage::Crawl, TaskState::Done),
            (Stage::Digest, TaskState::Done),
        ] {
            let mut t = Task::new(5, stage);
            t.state = state;
            inner.tasks.insert(format!("{stage}:5"), t);
        }
        let mut t = Task::new(5, Stage::Render);
        t.state = TaskState::Pending;
        inner.tasks.insert("render:5".into(), t);
        inner.workers.insert("w-local".into(), "127.0.0.1".into());

        let offer = inner.offer("w-local").expect("local worker renders");
        assert!(offer.local_node);
        for addr in ["127.0.0.1", "localhost", "::1"] {
            assert!(bm_core::is_local_node(addr), "{addr}");
        }
        assert!(!bm_core::is_local_node("192.168.2.2"));
    }

    #[test]
    fn swap_skips_a_chapter_whose_store_is_complete_and_current() {
        // Doc test 10, the narrowing: A speaks here and the local store is
        // already complete for the post-swap cast, with no stale files to
        // delete (the files got ahead of the cast file — a previous render
        // landed but the assignment never updated). Requeueing would re-speak
        // units that are already correct, so the chapter is left alone.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(11),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 11);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Minh Triết.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(11), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(11, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:11"), t);
        }

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(
            !msg.contains("11"),
            "complete store, no stale files: untouched: {msg}"
        );
        assert!(layout.final_mp3(11).exists(), "product stays");
        assert!(
            seg.join("0000-0001_Minh Triết.wav").exists(),
            "current files stay"
        );
        assert_eq!(inner.tasks["render:11"].state, TaskState::Done);
        assert_eq!(inner.tasks["merge:11"].state, TaskState::Done);
    }

    #[test]
    fn digest_completion_with_a_changed_script_invalidates_render() {
        // A re-digest rewrites run boundaries and voices: the kept render
        // would speak the old dramatization under the new one. Segments, mp3
        // and both tasks go; attempts reset because this is new work.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(12),
            r#"{"segments":[{"speaker":"A","text":"old line here"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 12);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(12), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(12, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:12"), t);
        }
        let mut t = Task::new(12, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:12".into(), t);

        let mut c = completion("w1", "digest:12", true, "digest ch12");
        c.script =
            Some(serde_json::json!({"segments":[{"speaker":"A","text":"a rewritten line here"}]}));
        inner.complete(&c);

        assert_eq!(inner.tasks["render:12"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["render:12"].attempts, 0,
            "new work, not a retry"
        );
        assert_eq!(inner.tasks["merge:12"].state, TaskState::Pending);
        assert!(!seg.join("0000_Adam.wav").exists(), "stale segments go");
        assert!(!layout.final_mp3(12).exists(), "stale product goes");
    }

    #[test]
    fn digest_completion_with_an_identical_script_invalidates_nothing() {
        // Duplicate reports are free: same bytes, no surgery.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        let script = r#"{"segments":[{"speaker":"A","text":"old line here"}]}"#;
        std::fs::write(layout.script(12), script).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 12);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(12), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(12, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:12"), t);
        }
        let mut t = Task::new(12, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:12".into(), t);

        let mut c = completion("w1", "digest:12", true, "digest ch12");
        c.script = Some(serde_json::from_str(script).unwrap());
        inner.complete(&c);

        assert_eq!(inner.tasks["render:12"].state, TaskState::Done);
        assert_eq!(inner.tasks["merge:12"].state, TaskState::Done);
        assert!(seg.join("0000_Adam.wav").exists(), "nothing touched");
        assert!(layout.final_mp3(12).exists(), "product stays");
    }

    #[test]
    fn retag_rewrites_laughs_and_requeues_only_edited_runs() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(13),
            r#"{"segments":[{"speaker":"A","text":"Ha ha ha!"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam","B":"Đức Trí"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 13);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(13), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(13, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:13"), t);
        }

        let msg = inner.op_retag(false).unwrap();
        assert!(msg.contains("13"), "touched chapter listed: {msg}");
        let back: serde_json::Value = bm_core::read_json(&layout.script(13)).unwrap();
        assert_eq!(back["segments"][0]["text"], "[cười]");
        assert_eq!(back["segments"][1]["text"], "z", "untouched text kept");
        assert!(
            !seg.join("0000_Adam.wav").exists(),
            "edited run's file goes"
        );
        assert!(
            seg.join("0001_Đức Trí.wav").exists(),
            "other runs keep cache"
        );
        assert!(!layout.final_mp3(13).exists(), "stale product goes");
        assert_eq!(inner.tasks["render:13"].state, TaskState::Pending);
        assert_eq!(inner.tasks["merge:13"].state, TaskState::Pending);

        // Second run is a no-op: deterministic convergence.
        let msg2 = inner.op_retag(false).unwrap();
        assert!(msg2.contains("already tags"), "{msg2}");
    }

    #[test]
    fn retag_dry_run_reports_without_writing() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(15),
            r#"{"segments":[{"speaker":"A","text":"Haizz, x"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 15);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();

        let msg = inner.op_retag(true).unwrap();
        assert!(msg.contains("15"), "report names the chapter: {msg}");
        let back: serde_json::Value = bm_core::read_json(&layout.script(15)).unwrap();
        assert!(back["segments"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Haizz"));
        assert!(seg.join("0000_Adam.wav").exists(), "nothing deleted");
        assert!(!inner.tasks.contains_key("render:15"), "nothing requeued");
    }

    #[test]
    fn missing_units_omits_what_the_store_already_holds() {
        // Doc test 1: a store holding 29 of 32 units yields an offer of 3.
        let (_d, inner) = fixture();
        let layout = inner.layout.clone();
        let mut segs = Vec::new();
        for i in 0..32u32 {
            let sp = if i % 2 == 0 { "A" } else { "B" };
            segs.push(
                serde_json::json!({"speaker": sp, "text": format!("line {i} spoken aloud here")}),
            );
        }
        std::fs::write(
            layout.script(9),
            serde_json::json!({"segments": segs}).to_string(),
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 9);
        std::fs::create_dir_all(&seg).unwrap();
        // Alternating speakers = 32 single-line runs: 0000..0031.
        for i in 0..32u32 {
            if [5, 17, 30].contains(&i) {
                continue;
            }
            let voice = if i % 2 == 0 { "Đức Trí" } else { "Adam" };
            std::fs::write(seg.join(format!("{i:04}_{voice}.wav")), vec![0u8; 2000]).unwrap();
        }

        let units = inner.missing_units(9).expect("plannable chapter");
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["0005_Adam.wav", "0017_Adam.wav", "0030_Đức Trí.wav"],
            "exactly the missing three, in order: {names:?}"
        );
        assert_eq!(units[0].speaker, "B");
        assert!(
            !units[0].text.is_empty(),
            "the worker gets text, not a lookup key"
        );

        // Full store → Some([]): report ok/0, not a replan.
        for i in [5u32, 17, 30] {
            let voice = if i % 2 == 0 { "Đức Trí" } else { "Adam" };
            std::fs::write(seg.join(format!("{i:04}_{voice}.wav")), vec![0u8; 2000]).unwrap();
        }
        assert_eq!(inner.missing_units(9).unwrap().len(), 0);
        // No script → None: the worker plans from its own copy instead.
        assert!(inner.missing_units(77).is_none());
    }

    #[test]
    fn swap_requeues_a_chapter_with_no_local_segments() {
        // The remote-render case: a worker's segment cache never comes home,
        // so no stale file exists locally — but the chapter still speaks with
        // the old voice and must re-render. Gating on deleted files skipped
        // it silently and its mp3 kept the old voice forever.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(1),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(layout.final_mp3(1), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(1, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:1"), t);
        }

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(msg.contains("[1]"), "chapter 1 must be listed: {msg}");
        assert!(!layout.final_mp3(1).exists(), "stale product goes away");
        assert_eq!(inner.tasks["render:1"].state, TaskState::Pending);
        assert_eq!(inner.tasks["merge:1"].state, TaskState::Pending);
    }

    #[test]
    fn swap_leaves_a_chapter_the_speaker_never_enters() {
        // Narrowness in the other direction: chapters without the speaker
        // keep their product and their Done tasks.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(2),
            r#"{"segments":[{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"A":"Đức Trí","B":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(layout.final_mp3(2), vec![0u8; 2000]).unwrap();
        for stage in [Stage::Render, Stage::Merge] {
            let mut t = Task::new(2, stage);
            t.state = TaskState::Done;
            inner.tasks.insert(format!("{stage}:2"), t);
        }

        let msg = inner.op_swap_voice("A", "Minh Triết").unwrap();
        assert!(!msg.contains('2'), "chapter 2 speaks nothing of A: {msg}");
        assert!(layout.final_mp3(2).exists(), "untouched product stays");
        assert_eq!(inner.tasks["render:2"].state, TaskState::Done);
        assert_eq!(inner.tasks["merge:2"].state, TaskState::Done);
    }

    #[test]
    fn reconcile_folds_bible_cast_scripts_and_requeues_renders() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.bible(),
            serde_json::to_string(&serde_json::json!({"characters": [
                {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
                 "proper_aliases": ["Huyền Vũ"], "first_seen": "10", "chapters_seen": ["10"]},
                {"name": "Huyền Vũ lão tổ", "personality": "", "voice_hint": "",
                 "proper_aliases": ["Huyền Vũ lão tổ"], "first_seen": "25", "chapters_seen": ["25"]}
            ]}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"Huyền Vũ":"Đức Trí","Huyền Vũ lão tổ":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(
            layout.script(25),
            r#"{"roster":["Huyền Vũ lão tổ"],"segments":[{"speaker":"Huyền Vũ lão tổ","text":"Ừ."}]}"#,
        )
        .unwrap();
        let seg = layout.seg_dir("vieneu", 25);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000_Adam.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(layout.final_mp3(25), vec![0u8; 2000]).unwrap();

        let msg = inner
            .apply_reconcile(&[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])])
            .unwrap();
        assert!(msg.contains("Huyền Vũ <= Huyền Vũ lão tổ"), "{msg}");

        let bible: Value = bm_core::read_json(&layout.bible()).unwrap();
        let chars = bible["characters"].as_array().unwrap();
        assert_eq!(chars.len(), 1);
        assert!(chars[0]["proper_aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "Huyền Vũ lão tổ"));

        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert_eq!(cast["Huyền Vũ"], "Đức Trí", "canonical keeps its voice");
        assert!(
            !cast.contains_key("Huyền Vũ lão tổ"),
            "absorbed key disappears"
        );

        let script: Value = bm_core::read_json(&layout.script(25)).unwrap();
        assert_eq!(
            script["segments"][0]["speaker"],
            serde_json::json!("Huyền Vũ")
        );
        assert_eq!(script["roster"], serde_json::json!(["Huyền Vũ"]));

        assert!(!seg.join("0000_Adam.wav").exists(), "loser's cache goes");
        assert!(!layout.final_mp3(25).exists(), "stale product goes away");
        assert_eq!(inner.tasks["render:25"].state, TaskState::Pending);
        assert_eq!(inner.tasks["merge:25"].state, TaskState::Pending);
        assert!(
            inner.events.iter().any(|e| e.text.contains("reconcile")),
            "logged"
        );
    }

    #[test]
    fn reconcile_with_no_merges_changes_nothing() {
        let (_d, mut inner) = fixture();
        let msg = inner.apply_reconcile(&[]).unwrap();
        assert!(msg.contains("nothing to fold"), "{msg}");
    }

    #[test]
    fn reconcile_folds_cast_only_variants_missing_from_the_bible() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.bible(),
            serde_json::to_string(&serde_json::json!({"characters": [
                {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
                 "proper_aliases": ["Huyền Vũ"], "first_seen": "10", "chapters_seen": ["10"]}
            ]}))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            layout.cast("vieneu"),
            r#"{"Huyền Vũ":"Đức Trí","Huyền Vũ lão tổ":"Adam","Narrator":"Đức Trí"}"#,
        )
        .unwrap();
        std::fs::write(
            layout.script(25),
            r#"{"roster":["Huyền Vũ lão tổ"],"segments":[{"speaker":"Huyền Vũ lão tổ","text":"Ừ."}]}"#,
        )
        .unwrap();

        let msg = inner
            .apply_reconcile(&[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])])
            .unwrap();
        assert!(msg.contains("Huyền Vũ <= Huyền Vũ lão tổ"), "{msg}");

        let cast = bm_core::cast::read_cast("vieneu", &layout.cast("vieneu"));
        assert!(
            !cast.contains_key("Huyền Vũ lão tổ"),
            "absorbed key disappears"
        );
        let script: Value = bm_core::read_json(&layout.script(25)).unwrap();
        assert_eq!(
            script["segments"][0]["speaker"],
            serde_json::json!("Huyền Vũ")
        );
    }

    #[test]
    fn swap_admits_anything_until_the_operator_narrows_it() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();

        // No local roster, so the shipped catalogue applies — and it restricts
        // nothing. A Northern preset that the old hardcoded Central/South policy
        // refused is assignable, which is the whole point of the split.
        assert!(inner.op_swap_voice("A", "Minh Đức").is_ok());

        // Narrow it the way an operator would, and the same voice is refused.
        std::fs::write(
            layout.roster(),
            r#"{"version":1,"engines":{"vieneu":{"policy":{"excluded_accents":["Northern"]}}}}"#,
        )
        .unwrap();
        let err = inner
            .op_swap_voice("A", "Minh Đức")
            .unwrap_err()
            .to_string();
        assert!(err.contains("neither an admitted preset"), "{err}");

        // An admitted voice still swaps.
        let msg = inner.op_swap_voice("A", "Quang Sơn").unwrap();
        assert!(msg.contains("->"), "{msg}");

        // A malformed roster is refused outright rather than silently ignored:
        // falling back to the catalogue would re-admit every excluded voice.
        std::fs::write(layout.roster(), "{ this is not json").unwrap();
        let err = inner.op_swap_voice("A", "Đức Trí").unwrap_err().to_string();
        assert!(err.contains("parsing"), "{err}");
    }

    #[test]
    fn swap_admits_a_fresh_pool_sample_but_nothing_else_undeclared() {
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.bm_state()).unwrap();
        // Narrow the policy so the trust rule actually bites: without this the
        // shipped catalogue admits everything and the test proves nothing.
        std::fs::write(
            layout.roster(),
            r#"{"version":1,"engines":{"vieneu":{"policy":{"excluded_accents":["Northern"]}}}}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        std::fs::write(
            layout.root.join("voice-pool.json"),
            r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]}}"#,
        )
        .unwrap();

        // Pooled samples are vetted at adding: assignable with nobody on them.
        assert!(inner.op_swap_voice("A", "young-female-1").is_ok());
        // Anything else undeclared still needs a prior assignment to be trusted.
        let err = inner
            .op_swap_voice("A", "Chưa Từng Có")
            .unwrap_err()
            .to_string();
        assert!(err.contains("neither an admitted preset"), "{err}");
    }

    fn busy_inner() -> (tempfile::TempDir, Inner) {
        // One chapter mid-render on w1: assigned task + fresh beat with task.
        let (d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí"}"#).unwrap();
        let mut t = Task::new(1, Stage::Render);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(now_secs() + 5000);
        inner.tasks.insert("render:1".into(), t);
        inner.beats.insert(
            "w1".into(),
            bm_proto::Heartbeat {
                worker_id: "w1".into(),
                addr: "127.0.0.1".into(),
                task_id: Some("render:1".into()),
                stage: Some(Stage::Render),
                chapter: Some(1),
                progress: 0.5,
                activity: "render".into(),
                eta_secs: None,
                ts: now_secs(),
                hostname: "box".into(),
                alias: String::new(),
            },
        );
        (d, inner)
    }

    #[test]
    fn swap_refuses_while_workers_are_mid_play() {
        let (_d, mut inner) = busy_inner();
        let err = inner
            .op_swap_voice("A", "Quang Sơn")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("mid-play") && err.contains("render:1"),
            "{err}"
        );
        assert!(err.contains('X'), "names the way out: {err}");
    }

    #[test]
    fn swap_proceeds_on_stale_or_idle_evidence() {
        let (_d, mut inner) = busy_inner();
        // Stale beats + ghost assignment: the reaper's business, not a block.
        for b in inner.beats.values_mut() {
            b.ts = now_secs().saturating_sub(3600);
            b.task_id = None;
        }
        for t in inner.tasks.values_mut() {
            t.assigned_to = Some("ghost".into());
        }
        assert!(inner.op_swap_voice("A", "Quang Sơn").is_ok());
    }

    #[test]
    fn enqueue_seeds_crawl_done_so_the_digest_is_offerable() {
        // Empty boot (B reconciles nothing) + text on disk + no script: the
        // digest must become offerable, which needs crawl:1 Done, not missing.
        // Before the fix the digest waited forever while workers idled.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(1), "Chương 1: X\n\nbody\n").unwrap();

        let (crawls, digests) = inner.enqueue_translate(1, 1);
        assert_eq!((crawls, digests), (0, 1));
        assert_eq!(inner.tasks["crawl:1"].state, TaskState::Done);
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Pending);

        inner.workers.insert("w1".into(), "127.0.0.1".into());
        let offer = inner.offer("w1").expect("digest:1 must be offered");
        assert_eq!(offer.task_id, "digest:1");
    }

    #[test]
    fn translate_heals_a_range_missing_downstream_tasks() {
        // Ledger holds only digest:done (hand-reset, older builds): after a
        // translate the full chain exists again instead of idling with nothing
        // offerable.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(layout.chapter_txt(1), "Chương 1: X\n\nbody\n").unwrap();
        std::fs::write(
            layout.script(1),
            r#"{"roster":["Narrator"],"segments":[{"speaker":"Narrator","text":"x"}]}"#,
        )
        .unwrap();
        inner.reconcile(1, 1);
        assert!(inner.tasks.contains_key("render:1"), "render recreated");
        assert!(inner.tasks.contains_key("merge:1"), "merge recreated");
        assert_eq!(inner.tasks["digest:1"].state, TaskState::Done);
        assert_eq!(inner.tasks["render:1"].state, TaskState::Pending);
    }

    #[test]
    fn eta_reports_a_total_even_with_no_measurements() {
        let (_d, inner) = fixture();
        let msg = inner.op_eta(1, 10);
        assert!(msg.contains("total"), "{msg}");
        assert!(msg.contains("(guess)"), "{msg}");
    }

    #[test]
    fn reap_frees_dead_workers_tasks_but_not_after_a_reboot() {
        let (_d, mut inner) = fixture();
        let now = now_secs();
        let mut t = Task::new(2, Stage::Digest);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("ghost".into());
        t.lease_until = Some(now + 5000);
        inner.tasks.insert("digest:2".into(), t);

        // Fresh boot: beats haven't arrived yet — hands off.
        assert!(inner.reap().is_empty(), "boot grace must hold");
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Assigned);

        // Long after boot with still no beat: the worker is gone, free it.
        inner.started_at = now.saturating_sub(1000);
        let freed = inner.reap();
        assert_eq!(freed, vec!["digest:2".to_string()]);
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Pending);

        // A live beat protects the assignment again.
        let mut t = Task::new(3, Stage::Digest);
        t.state = TaskState::Assigned;
        t.assigned_to = Some("w-live".into());
        t.lease_until = Some(now + 5000);
        inner.tasks.insert("digest:3".into(), t);
        inner.beats.insert(
            "w-live".into(),
            bm_proto::Heartbeat {
                worker_id: "w-live".into(),
                addr: "127.0.0.1".into(),
                task_id: Some("digest:3".into()),
                stage: Some(Stage::Digest),
                chapter: Some(3),
                progress: 0.5,
                activity: "digest".into(),
                eta_secs: None,
                ts: now_secs(),
                hostname: "box".into(),
                alias: String::new(),
            },
        );
        assert!(inner.reap().is_empty(), "live worker untouched");
    }

    #[test]
    fn requeue_orphans_frees_dead_workers_tasks_only() {
        let (_d, mut inner) = fixture();
        let now = now_secs();
        // Ghost-held task, live-held task, and an unassigned one.
        for (id, ch, who) in [("digest:2", 2, "ghost"), ("digest:3", 3, "w-live")] {
            let mut t = Task::new(ch, Stage::Digest);
            t.state = TaskState::Assigned;
            t.assigned_to = Some(who.into());
            t.lease_until = Some(now + 5000);
            inner.tasks.insert(id.into(), t);
        }
        inner
            .tasks
            .insert("digest:4".into(), Task::new(4, Stage::Digest));
        inner.beats.insert(
            "w-live".into(),
            bm_proto::Heartbeat {
                worker_id: "w-live".into(),
                addr: "127.0.0.1".into(),
                task_id: Some("digest:3".into()),
                stage: Some(Stage::Digest),
                chapter: Some(3),
                progress: 0.5,
                activity: "digest".into(),
                eta_secs: None,
                ts: now,
                hostname: "box".into(),
                alias: String::new(),
            },
        );
        let msg = inner.op_requeue_orphans();
        assert!(msg.contains("digest:2"), "{msg}");
        assert!(!msg.contains("digest:3"), "live worker untouched: {msg}");
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:2"].attempts, 0,
            "strikes untouched (none here)"
        );
        assert_eq!(inner.tasks["digest:3"].state, TaskState::Assigned);
        assert!(
            inner.op_requeue_orphans().contains("no orphaned"),
            "second run is a no-op"
        );
    }

    #[test]
    fn retry_shelved_resets_strikes_and_reoffers_the_chapter() {
        let (_d, mut inner) = fixture();
        let mut t = Task::new(2, Stage::Digest);
        t.state = TaskState::Shelved;
        t.attempts = 3;
        inner.tasks.insert("digest:2".into(), t);
        let mut c = Task::new(2, Stage::Crawl);
        c.state = TaskState::Done;
        inner.tasks.insert("crawl:2".into(), c);
        inner.workers.insert("w1".into(), "127.0.0.1".into());

        let msg = inner.op_retry_shelved();
        assert!(msg.contains("digest:2"), "{msg}");
        assert_eq!(inner.tasks["digest:2"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:2"].attempts, 0,
            "manual retry forgives strikes"
        );
        // The chapter-level shelve gate lifts, so the task is offerable again.
        assert!(inner.offer("w1").is_some(), "retried task must be offered");
        assert!(
            inner.op_retry_shelved().contains("no shelved"),
            "second run is a no-op"
        );
    }

    fn completion(worker: &str, task: &str, ok: bool, detail: &str) -> Complete {
        Complete {
            worker_id: worker.into(),
            task_id: task.into(),
            ok,
            detail: detail.into(),
            duration_secs: 12.5,
            bible_delta: None,
            units: 0,
            script: None,
            text: None,
            mp3_b64: None,
        }
    }

    #[test]
    fn a_render_report_with_missing_files_is_rejected_by_name() {
        // The completion gate: the worker's word is not evidence. Mutate one
        // wav below the completeness threshold and the `ok` report must fail
        // naming exactly that file, so the next offer repeats it.
        let (_d, mut inner) = fixture();
        let layout = inner.layout.clone();
        std::fs::write(
            layout.script(6),
            r#"{"segments":[{"speaker":"A","text":"x"},{"speaker":"A","text":"y"},{"speaker":"B","text":"z"}]}"#,
        )
        .unwrap();
        std::fs::write(layout.cast("vieneu"), r#"{"A":"Đức Trí","B":"Adam"}"#).unwrap();
        let seg = layout.seg_dir("vieneu", 6);
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(seg.join("0000-0001_Đức Trí.wav"), vec![0u8; 2000]).unwrap();
        std::fs::write(seg.join("0002_Adam.wav"), vec![0u8; 500]).unwrap();
        let mut t = Task::new(6, Stage::Render);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("render:6".into(), t);

        let msg = inner.complete(&completion("w1", "render:6", true, "render ch6 (2 calls)"));
        assert!(
            msg.contains("failed"),
            "an incomplete ok-report fails: {msg}"
        );
        assert!(
            msg.contains("0002_Adam.wav"),
            "the missing file is named: {msg}"
        );
        assert_eq!(inner.tasks["render:6"].state, TaskState::Pending);
        assert_eq!(inner.tasks["render:6"].attempts, 1);
    }

    #[test]
    fn a_failed_task_lands_in_the_event_stream_with_its_cause() {
        // The whole point of the event buffer: a digest that dies on a worker
        // must say *why* in the TUI, not just flip a row back to Pending.
        let (_d, mut inner) = fixture();
        let mut t = Task::new(4, Stage::Digest);
        t.state = TaskState::Running;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:4".into(), t);

        inner.complete(&completion(
            "w1",
            "digest:4",
            false,
            "opencode exited 1: model 'claude' unavailable",
        ));

        let events = inner.recent_events(10);
        let last = events.last().expect("a failure must record an event");
        assert_eq!(
            last.level, "warn",
            "a first failure retries, so it is a warning"
        );
        assert!(
            last.text.contains("w1") && last.text.contains("digest:4"),
            "{}",
            last.text
        );
        assert!(
            last.text.contains("model 'claude' unavailable"),
            "the worker's reason must survive: {}",
            last.text
        );
        assert_eq!(
            inner.tasks["digest:4"].state,
            TaskState::Pending,
            "one strike, not shelved"
        );
    }

    #[test]
    fn the_third_failure_escalates_to_error_and_names_the_retry_key() {
        let (_d, mut inner) = fixture();
        let mut t = Task::new(4, Stage::Digest);
        t.state = TaskState::Running;
        t.attempts = 2;
        t.assigned_to = Some("w1".into());
        inner.tasks.insert("digest:4".into(), t);

        inner.complete(&completion(
            "w1",
            "digest:4",
            false,
            "digest returned no segments",
        ));

        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(
            last.level, "error",
            "three strikes is an error, not a warning"
        );
        assert!(last.text.contains("shelved"), "{}", last.text);
        assert!(
            last.text.contains('u'),
            "the way out must be named: {}",
            last.text
        );
        assert_eq!(inner.tasks["digest:4"].state, TaskState::Shelved);
    }

    #[test]
    fn events_are_capped_and_ids_stay_in_order() {
        let (_d, mut inner) = fixture();
        for i in 0..(EVENT_CAP + 25) {
            inner.push_event("info", format!("event {i}"));
        }
        let all = inner.recent_events(EVENT_CAP * 2);
        assert_eq!(
            all.len(),
            EVENT_CAP,
            "the buffer must not grow without bound"
        );
        let ids: Vec<u64> = all.iter().map(|e| e.id).collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids must ascend: {ids:?}"
        );
        assert_eq!(*ids.last().unwrap(), (EVENT_CAP + 24) as u64);
        // `limit` is a tail window, not a reordering.
        let tail = inner.recent_events(3);
        assert_eq!(
            tail.iter().map(|e| e.text.clone()).collect::<Vec<_>>(),
            vec![
                format!("event {}", EVENT_CAP + 22),
                format!("event {}", EVENT_CAP + 23),
                format!("event {}", EVENT_CAP + 24),
            ]
        );
        assert!(inner.recent_events(0).is_empty());
    }

    #[test]
    fn reap_explains_a_requeue_instead_of_flipping_a_row_silently() {
        let (_d, mut inner) = fixture();
        let mut t = Task::new(5, Stage::Render);
        t.state = TaskState::Running;
        t.assigned_to = Some("w9".into());
        t.lease_until = Some(now_secs().saturating_sub(5));
        inner.tasks.insert("render:5".into(), t);

        let requeued = inner.reap();
        assert_eq!(requeued, vec!["render:5".to_string()]);
        assert_eq!(inner.tasks["render:5"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["render:5"].attempts, 0,
            "silence is not a strike"
        );

        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(last.level, "warn");
        assert!(last.text.contains("lease expired"), "{}", last.text);
        assert!(last.text.contains("render:5"), "{}", last.text);

        // A quiet reap stays quiet: no event, nothing to re-announce every 10s.
        let before = inner.events.len();
        assert!(inner.reap().is_empty());
        assert_eq!(
            inner.events.len(),
            before,
            "nothing happened, nothing logged"
        );
    }

    #[test]
    fn retry_task_targets_one_chapter_and_force_clears_its_artifact() {
        let (_d, mut inner) = fixture();
        let script = inner.layout.script(3);
        std::fs::write(&script, r#"{"segments":[]}"#).unwrap();
        let mut t = Task::new(3, Stage::Digest);
        t.state = TaskState::Shelved;
        t.attempts = 3;
        t.assigned_to = Some("w1".into());
        t.lease_until = Some(now_secs() + 60);
        t.detail = "digest failed: no such model".into();
        inner.tasks.insert("digest:3".into(), t);
        let mut other = Task::new(4, Stage::Digest);
        other.state = TaskState::Shelved;
        inner.tasks.insert("digest:4".into(), other);

        // Without force the artifact is left alone: reconcile may still see it.
        let msg = inner.op_retry_task(Stage::Digest, 3, false);
        assert!(msg.contains("digest:3"), "{msg}");
        assert_eq!(inner.tasks["digest:3"].state, TaskState::Pending);
        assert_eq!(
            inner.tasks["digest:3"].attempts, 0,
            "a manual retry forgives strikes"
        );
        assert!(inner.tasks["digest:3"].assigned_to.is_none());
        assert!(inner.tasks["digest:3"].lease_until.is_none());
        assert!(inner.tasks["digest:3"].detail.contains("requeued"));
        assert!(script.exists(), "a plain retry keeps the artifact");
        assert_eq!(
            inner.tasks["digest:4"].state,
            TaskState::Shelved,
            "only the named chapter is touched"
        );

        // Forced: the stale script goes, so the stage really re-runs.
        let msg = inner.op_retry_task(Stage::Digest, 3, true);
        assert!(msg.contains("forced"), "{msg}");
        assert!(!script.exists(), "force must delete what made it look done");

        // And the operator action is in the stream, with the state it replaced.
        let last = inner.recent_events(1).into_iter().next().unwrap().clone();
        assert_eq!(last.level, "ok");
        assert!(last.text.contains("digest:3"), "{}", last.text);
        assert!(last.text.contains("was pending"), "{}", last.text);

        assert!(inner
            .op_retry_task(Stage::Merge, 99, false)
            .contains("not found"));
    }
}
