use super::{EventRecord, Inner, EVENT_CAP};
use anyhow::Result;
use bm_core::provision::{join_all, load_boxes, save_box, split_machine};
use bm_core::{config::Settings, Layout};
use bm_proto::{now_secs, Machine, Task, TaskState};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

impl Inner {
    pub fn new(layout: Layout, settings: Settings) -> Self {
        Inner {
            layout,
            settings,
            tasks: HashMap::new(),
            ledger_profile: None,
            machines: HashMap::new(),
            workers: HashMap::new(),
            caps: HashMap::new(),
            beats: HashMap::new(),
            started_at: now_secs(),
            events: VecDeque::new(),
            next_event_id: 0,
            shutdown_requested: false,
            shutdown_when_idle: false,
            stats: super::StatsAgg::default(),
            unreadable_tasks: Vec::new(),
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

    /// Workspace/profile gate: a ledger holding another profile's tasks
    /// refuses to run here rather than mixing two genres' output. Empty or
    /// unstamped ledgers pass and adopt the workspace profile at reconcile.
    pub fn check_profile(&self) -> Result<()> {
        // Preserved-but-unreadable rows count as *held work*: they came from a
        // ledger, and calling a ledger we could not fully read "empty" would let
        // reconcile stamp this workspace's profile over rows that may belong to
        // another book — the mixing this gate exists to prevent.
        if self.tasks.is_empty() && self.unreadable_tasks.is_empty() {
            return Ok(());
        }
        match &self.ledger_profile {
            Some(stamped) if *stamped != self.settings.profile => anyhow::bail!(
                "ledger holds {} task(s) for profile '{}' but this workspace runs '{}' — switch back (`workspace use` / `profile load`) or clear the ledger; refusing to mix",
                self.tasks.len() + self.unreadable_tasks.len(),
                stamped.name,
                self.settings.profile.name,
            ),
            _ => Ok(()),
        }
    }

    fn ledger_path(&self) -> std::path::PathBuf {
        self.layout.ledger()
    }

    /// Registry handle for an address: the stored box name, else the
    /// fallback. Display only — keys stay addresses everywhere.
    pub fn box_name(&self, addr: &str, fallback: &str) -> String {
        load_boxes(&self.layout.machines())
            .iter()
            .find(|b| b.addr == addr)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Persist one in-memory machine's connection config to `machines.json`.
    /// `fallback_name` (hostname, address) applies only when the box has no
    /// stored name yet. Runtime still goes through `save()`.
    pub fn persist_box(&self, addr: &str, fallback_name: &str) {
        let Some(m) = self.machines.get(addr) else {
            return;
        };
        let name = self.box_name(addr, fallback_name);
        let (bxo, _) = split_machine(m, &name);
        let _ = save_box(&self.layout.machines(), &bxo);
    }

    pub fn save(&self) {
        // Runtime only: connection config lives in machines.json and is
        // written at bind time, never on this hot path.
        let state: HashMap<String, Value> = self
            .machines
            .iter()
            .map(|(a, m)| {
                (
                    a.clone(),
                    serde_json::to_value(split_machine(m, "").1).unwrap_or(Value::Null),
                )
            })
            .collect();
        // Every row this build understands, **plus every row it does not**.
        // Appending the raw values is what stops a deserialisation problem from
        // becoming permanent: without them this write is the moment the library
        // shrinks. See `Inner::unreadable_tasks`.
        let mut tasks: Vec<Value> = self
            .tasks
            .values()
            .filter_map(|t| serde_json::to_value(t).ok())
            .collect();
        tasks.extend(self.unreadable_tasks.iter().cloned());
        let doc = json!({"tasks": tasks,
                         "machine_state": state,
                         "workers": self.workers,
                         "caps": self.caps,
                         "profile": self.ledger_profile});
        let _ = bm_core::write_json(&self.ledger_path(), &doc);
    }

    pub fn load_ledger(&mut self) {
        let Ok(doc): Result<Value, _> =
            bm_core::read_json(&self.ledger_path()).map_err(|_| anyhow::anyhow!("none"))
        else {
            return;
        };
        // One-way migration: the old shape stored full Machines under
        // `machines`. Split it once, snapshot both files first, then load
        // the new shape (the recursive call terminates — the key is gone).
        if doc.get("machines").and_then(|m| m.as_array()).is_some() {
            match self.migrate_ledger(&doc) {
                Ok(n) => self.push_event(
                    "info",
                    format!(
                        "ledger migrated: {n} machines split into machines.json + machine_state"
                    ),
                ),
                Err(e) => self.push_event("error", format!("ledger migration failed: {e:#}")),
            }
            self.load_ledger();
            return;
        }
        self.load_new_shape(&doc);
    }

    fn load_new_shape(&mut self, doc: &Value) {
        // Cleared first, not appended to: `load_ledger` can run twice (the
        // migration path recurses), and a second pass over the same file must
        // not double every preserved row.
        self.unreadable_tasks.clear();
        if let Some(tasks) = doc.get("tasks").and_then(|t| t.as_array()) {
            for t in tasks {
                match serde_json::from_value::<Task>(t.clone()) {
                    Ok(task) => {
                        self.tasks.insert(task.id(), task);
                    }
                    // **Kept, not dropped.** A row this build cannot read is
                    // carried through `save()` verbatim, so a schema change can
                    // never silently delete library rows — the failure mode this
                    // used to have, where `save()` wrote the shortened ledger
                    // back over the full one. The event is what makes it visible
                    // rather than merely harmless.
                    Err(e) => {
                        let named = t
                            .get("stage")
                            .and_then(|s| s.as_str())
                            .zip(t.get("chapter").and_then(|c| c.as_u64()))
                            .map(|(s, c)| format!("{s}:{c}"))
                            .unwrap_or_else(|| "(no stage/chapter)".into());
                        self.push_event(
                            "error",
                            format!(
                                "ledger row {named} could not be read ({e}) — kept verbatim, \
                                 not dropped; nothing else about it is changed"
                            ),
                        );
                        self.unreadable_tasks.push(t.clone());
                    }
                }
            }
        }
        if !self.unreadable_tasks.is_empty() {
            self.push_event(
                "error",
                format!(
                    "{} ledger row(s) preserved unread — this build is older than the ledger; \
                     upgrade the inductor before trusting the task counts",
                    self.unreadable_tasks.len()
                ),
            );
        }
        self.ledger_profile = doc
            .get("profile")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let boxes = load_boxes(&self.layout.machines());
        let empty = serde_json::Map::new();
        let rt = doc
            .get("machine_state")
            .and_then(|v| v.as_object())
            .unwrap_or(&empty);
        self.machines = join_all(boxes, rt)
            .into_iter()
            .map(|m| (m.addr.clone(), m))
            .collect();
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
        if let Some(c) = doc.get("caps").and_then(|c| c.as_object()) {
            for (k, v) in c {
                if let Some(list) = v.as_array() {
                    self.caps.insert(
                        k.clone(),
                        list.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect(),
                    );
                }
            }
        }
    }

    /// Split an old-shape ledger (`machines` array of full `Machine`s):
    /// config fields into `machines.json` for any addr not already there,
    /// runtime into `machine_state`. Idempotent — a second run finds no
    /// `machines` key and is a no-op. Returns the migrated machine count.
    fn migrate_ledger(&mut self, doc: &Value) -> Result<usize> {
        let ledger_path = self.ledger_path();
        let boxes_path = self.layout.machines();
        // The one pass that can lose a credential: snapshot first.
        if ledger_path.exists() {
            std::fs::copy(&ledger_path, ledger_path.with_extension("json.bak"))?;
        }
        if boxes_path.exists() {
            std::fs::copy(&boxes_path, boxes_path.with_extension("json.bak"))?;
        }
        let mut n = 0;
        if let Some(ms) = doc.get("machines").and_then(|m| m.as_array()) {
            for m in ms {
                let Ok(mac) = serde_json::from_value::<Machine>(m.clone()) else {
                    continue;
                };
                n += 1;
                if !load_boxes(&boxes_path).iter().any(|b| b.addr == mac.addr) {
                    // No names exist yet: the address doubles as the handle
                    // until `link` renames the box.
                    let (bxo, _) = split_machine(&mac, &mac.addr);
                    save_box(&boxes_path, &bxo)?;
                }
            }
        }
        // Tasks and workers ride along untouched; `machine_state` is built
        // here so the file is complete without a second pass.
        let mut state = serde_json::Map::new();
        if let Some(ms) = doc.get("machines").and_then(|m| m.as_array()) {
            for m in ms {
                let Ok(mac) = serde_json::from_value::<Machine>(m.clone()) else {
                    continue;
                };
                state.insert(
                    mac.addr.clone(),
                    serde_json::to_value(split_machine(&mac, "").1)?,
                );
            }
        }
        let mut new_doc = json!({"machine_state": state});
        if let Some(t) = doc.get("tasks") {
            new_doc["tasks"] = t.clone();
        }
        if let Some(w) = doc.get("workers") {
            new_doc["workers"] = w.clone();
        }
        bm_core::write_json(&ledger_path, &new_doc)?;
        Ok(n)
    }

    /// Expired leases return to the pool with no strike. So do tasks stranded
    /// on dead workers (no live beat) — automatically, every 10s, with no
    /// keypress and no lease wait. Returns their ids.
    pub fn reap(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        // **One liveness set for both passes**, because the two passes need the
        // same answer to "is the holder still there" — and the expiry pass needs
        // it for a question the orphan pass never asks: an expiry on a worker
        // that is *still beating* is a stuck worker, not a lost one, and the
        // strike-free rule was written for the second of those.
        let live: std::collections::HashSet<String> = self
            .beats
            .values()
            .filter(|b| now.saturating_sub(b.ts) < 90)
            .map(|b| b.worker_id.clone())
            .collect();
        let mut expired = Vec::new();
        // A row that expired while its worker was still answering:
        // `(row, worker, how many times now)`.
        let mut stuck: Vec<(String, String, u32)> = Vec::new();
        for t in self.tasks.values_mut() {
            if matches!(t.state, TaskState::Assigned | TaskState::Running)
                && t.lease_until.map(|l| l < now).unwrap_or(false)
            {
                let id = t.id();
                // Counted **only** when the holder was still beating, so the
                // number means "expired while its worker was alive" — the hang
                // signature — rather than a tally of every requeue, which is what
                // the orphan pass above is for. `release` leaves the count alone,
                // so it survives the requeue it just described.
                if let Some(w) = t.assigned_to.clone().filter(|w| live.contains(w.as_str())) {
                    t.expiries = t.expiries.saturating_add(1);
                    stuck.push((id.clone(), w, t.expiries));
                }
                Self::release(t, now, "lease expired");
                expired.push(id.clone());
                out.push(id);
            }
        }
        // Orphan pass: assigned to a worker with no live beat (90s, the same
        // window the ETA calls live). Workers beat every 2s, so a live one is
        // never caught here — and the boot grace in `started_at` means a
        // reboot never mistakes grinding workers for dead ones either.
        let mut orphaned = Vec::new();
        if now.saturating_sub(self.started_at) > 120 {
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
                    expired
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
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
                    orphaned
                        .iter()
                        .take(8)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        // **The one that was missing.** An expiry on a *dead* worker is the case
        // the strike-free rule was written for, and the aggregate line above
        // covers it. An expiry on a worker that is still beating is a different
        // event entirely: nothing is lost, the box is healthy, and the task is
        // simply never going to finish — so it gets its own line, in its own
        // words, escalating once the loop is established.
        //
        // This is what the operator could not see on 2026-09-22: two digest rows
        // re-queued silently for ~80 minutes while the TUI showed a percentage
        // that never moved, and the only visible evidence was a `warn` per
        // expiry that read like routine housekeeping.
        for (id, worker, count) in &stuck {
            let level = if *count >= 2 { "error" } else { "warn" };
            let escalating = if *count >= 2 {
                format!(" — it has now done so {count}×, so this is a hang, not a hiccup")
            } else {
                String::new()
            };
            self.push_event(
                level,
                format!(
                    "{id} expired while {worker} was still beating (live worker, unfinished \
                     task){escalating}. The worker's log tail names the step it is in; a stage \
                     that hangs with no output is usually a child process with no deadline"
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
                busy.push(format!(
                    "{} on {}",
                    t.id(),
                    t.assigned_to.as_deref().unwrap_or("?")
                ));
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
