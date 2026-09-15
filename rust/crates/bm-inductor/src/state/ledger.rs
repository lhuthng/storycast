use super::{EVENT_CAP, EventRecord, Inner};
use anyhow::Result;
use bm_core::{Layout, config::Settings};
use bm_proto::{Machine, Task, TaskState, now_secs};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

impl Inner {
    pub fn new(layout: Layout, settings: Settings) -> Self {
        Inner {
            layout,
            settings,
            tasks: HashMap::new(),
            machines: HashMap::new(),
            workers: HashMap::new(),
            beats: HashMap::new(),
            started_at: now_secs(),
            events: VecDeque::new(),
            next_event_id: 0,
        }
    }

    pub(crate) fn push_event(&mut self, level: &str, text: String) {
        let id = self.next_event_id;
        self.next_event_id += 1;
        self.events.push_back(EventRecord {
            id,
            ts: now_secs(),
            level: level.to_string(),
            text,
        });
        while self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
    }

    /// The most recent `limit` events, newest last (ascending id order).
    pub fn recent_events(&self, limit: usize) -> Vec<&EventRecord> {
        let skip = self.events.len().saturating_sub(limit);
        self.events.iter().skip(skip).collect()
    }

    fn ledger_path(&self) -> std::path::PathBuf {
        self.layout.bm_state().join("ledger.json")
    }

    pub fn save(&self) {
        let doc = json!({"tasks": self.tasks.values().collect::<Vec<_>>(),
                         "machines": self.machines.values().collect::<Vec<_>>(),
                         "workers": self.workers});
        let _ = bm_core::write_json(&self.ledger_path(), &doc);
    }

    pub fn load_ledger(&mut self) {
        let Ok(doc): Result<Value, _> =
            bm_core::read_json(&self.ledger_path()).map_err(|_| anyhow::anyhow!("none"))
        else {
            return;
        };
        if let Some(tasks) = doc.get("tasks").and_then(|t| t.as_array()) {
            for t in tasks {
                if let Ok(task) = serde_json::from_value::<Task>(t.clone()) {
                    self.tasks.insert(task.id(), task);
                }
            }
        }
        if let Some(ms) = doc.get("machines").and_then(|m| m.as_array()) {
            for m in ms {
                if let Ok(mac) = serde_json::from_value::<Machine>(m.clone()) {
                    self.machines.insert(mac.addr.clone(), mac);
                }
            }
        }
        // Worker identity survives restarts: without it, completions filed
        // while the map is cold get attributed to the wrong machine (and
        // merge affinity strands tasks on machines that never rendered).
        if let Some(w) = doc.get("workers").and_then(|w| w.as_object()) {
            for (k, v) in w {
                if let Some(addr) = v.as_str() {
                    self.workers.insert(k.clone(), addr.to_string());
                }
            }
        }
    }

    /// Expired leases return to the pool with no strike. So do tasks stranded
    /// on dead workers (no live beat) — automatically, every 10s, with no
    /// keypress and no lease wait. Returns their ids.
    pub fn reap(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        let mut expired = Vec::new();
        for t in self.tasks.values_mut() {
            if matches!(t.state, TaskState::Assigned | TaskState::Running)
                && t.lease_until.map(|l| l < now).unwrap_or(false)
            {
                Self::release(t, now, "lease expired");
                expired.push(t.id());
                out.push(t.id());
            }
        }
        // Orphan pass: assigned to a worker with no live beat (90s, the same
        // window the ETA calls live). Workers beat every 2s, so a live one is
        // never caught here — and the boot grace in `started_at` means a
        // reboot never mistakes grinding workers for dead ones either.
        let mut orphaned = Vec::new();
        if now.saturating_sub(self.started_at) > 120 {
            let live: std::collections::HashSet<&str> = self
                .beats
                .values()
                .filter(|b| now.saturating_sub(b.ts) < 90)
                .map(|b| b.worker_id.as_str())
                .collect();
            for t in self.tasks.values_mut() {
                if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                    continue;
                }
                let orphan = match &t.assigned_to {
                    None => true,
                    Some(w) => !live.contains(w.as_str()),
                };
                if orphan && !out.contains(&t.id()) {
                    Self::release(t, now, "worker gone");
                    orphaned.push(t.id());
                    out.push(t.id());
                }
            }
        }
        // Both passes are silent by design (no strikes), which used to mean an
        // operator saw a task flip back to Pending with no explanation. The
        // events are pushed after the loops: the loops hold `tasks` mutably.
        if !expired.is_empty() {
            expired.sort();
            self.push_event(
                "warn",
                format!(
                    "lease expired — requeued {}: {}",
                    expired.len(),
                    expired.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
                ),
            );
        }
        if !orphaned.is_empty() {
            orphaned.sort();
            self.push_event(
                "warn",
                format!(
                    "worker gone — requeued {}: {}",
                    orphaned.len(),
                    orphaned.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
                ),
            );
        }
        if !out.is_empty() {
            self.save();
        }
        out
    }

    /// Return one task to the pool. Attempts are kept — this unsticks, it
    /// does not forgive strikes.
    pub(crate) fn release(t: &mut Task, now: u64, why: &str) {
        t.state = TaskState::Pending;
        t.assigned_to = None;
        t.lease_until = None;
        t.detail = format!("requeued: {why}");
        t.updated = now;
    }

    /// Refuse voice/cache surgery while workers are mid-play: acting then
    /// mixes voices and marks stale mp3s done. Only *fresh* evidence counts
    /// (30s) — stale beats and ghost assignments are the reaper's job, and an
    /// offline Inner (empty beats) always passes.
    pub(crate) fn ensure_idle(&self) -> anyhow::Result<()> {
        let now = now_secs();
        let fresh = |ts: u64| now.saturating_sub(ts) < 30;
        let mut busy: Vec<String> = Vec::new();
        for t in self.tasks.values() {
            if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                continue;
            }
            let live_holder = t
                .assigned_to
                .as_deref()
                .and_then(|w| self.beats.get(w))
                .map(|b| fresh(b.ts))
                .unwrap_or(false);
            if live_holder {
                busy.push(format!("{} on {}", t.id(), t.assigned_to.as_deref().unwrap_or("?")));
            }
        }
        for (w, b) in &self.beats {
            if fresh(b.ts) {
                if let Some(tid) = &b.task_id {
                    let s = format!("{tid} on {w}");
                    if !busy.contains(&s) {
                        busy.push(s);
                    }
                }
            }
        }
        if !busy.is_empty() {
            busy.sort();
            anyhow::bail!(
                "workers mid-play ({}) — X stops everything, then swap",
                busy.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
            );
        }
        Ok(())
    }
}
