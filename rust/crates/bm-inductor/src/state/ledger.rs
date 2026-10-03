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
            // Held at birth: see `Inner::dispatch_held`. A process has to be
            dispatch_held: true,
            stats: super::StatsAgg::default(),
            unreadable_tasks: Vec::new(),
            exclusive: Vec::new(),
        }
    }

    /// The same state, already distributing: a **fixture**, and it exists
    #[cfg(test)]
    pub(crate) fn distributing(layout: Layout, settings: Settings) -> Self {
        let mut inner = Inner::new(layout, settings);
        inner.dispatch_held = false;
        inner
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

    /// Workspace/profile gate: a ledger holding another binding's tasks
    pub fn check_profile(&self) -> Result<()> {
        // Preserved-but-unreadable rows count as *held work*: they came from a
        if self.tasks.is_empty() && self.unreadable_tasks.is_empty() {
            return Ok(());
        }
        match &self.ledger_profile {
            Some(stamped) if *stamped != self.settings.profile => {
                // Name the pieces that moved. "another profile" sends an
                let which: Vec<String> =
                    bm_core::profile::pieces_differing(stamped, &self.settings.profile)
                        .iter()
                        .map(|piece| {
                            format!(
                                "{} '{}' -> '{}'",
                                piece.noun(),
                                stamped.get(*piece).name,
                                self.settings.profile.get(*piece).name,
                            )
                        })
                        .collect();
                anyhow::bail!(
                    "ledger holds {} task(s) for profile {} but this workspace runs {} — {}; switch back (`workspace use` / `profile load`) or clear the ledger; refusing to mix",
                    self.tasks.len() + self.unreadable_tasks.len(),
                    bm_core::profile::label(stamped),
                    bm_core::profile::label(&self.settings.profile),
                    which.join(", "),
                )
            }
            _ => Ok(()),
        }
    }

    /// Bring a pre-split cache into the `(adapter, engine)` shape.
    pub fn migrate_cache_keys(&mut self) {
        let mut moved = 0;
        for engine in bm_core::paths::LEGACY_CACHE_ENGINES {
            match self.layout.migrate_cache_keys(engine) {
                Ok(m) => moved += m.len(),
                Err(e) => {
                    self.push_event("error", format!("cache re-key failed: {e:#}"));
                    return;
                }
            }
        }
        if moved > 0 {
            self.push_event(
                "info",
                format!(
                    "cache re-keyed for adapter '{}': {moved} path(s) moved into the (adapter, engine) shape",
                    self.layout.adapter
                ),
            );
        }
    }

    /// Bring a pre-engine-tree checkout into the `engines/<name>/` shape.
    pub fn migrate_engine_tree(&mut self) {
        let target = self
            .layout
            .root
            .join(bm_core::paths::ENGINES_DIR)
            .join(bm_core::paths::LEGACY_ENGINE);
        match self.layout.migrate_engine_tree() {
            Ok(moved) if moved.is_empty() => {}
            Ok(moved) => self.push_event(
                "info",
                format!(
                    "engine tree: {} path(s) moved into {} — the sidecar, its runtime and the weights are the engine's own files now",
                    moved.len(),
                    target.display(),
                ),
            ),
            Err(e) => self.push_event("error", format!("engine tree move failed: {e:#}")),
        }
    }

    fn ledger_path(&self) -> std::path::PathBuf {
        self.layout.ledger()
    }

    /// Registry handle for an address: the stored box name, else the
    pub fn box_name(&self, addr: &str, fallback: &str) -> String {
        load_boxes(&self.layout.machines())
            .iter()
            .find(|b| b.addr == addr)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Persist one in-memory machine's connection config to `machines.json`.
    pub fn persist_box(&self, addr: &str, fallback_name: &str) {
        let Some(m) = self.machines.get(addr) else {
            return;
        };
        let name = self.box_name(addr, fallback_name);
        let (bxo, _) = split_machine(m, &name);
        let _ = save_box(&self.layout.machines(), &bxo);
    }

    /// Record the range **this process** was told to work on.
    /// `settings.start/count` is where the range lives (it is the same "chapter
    /// range the cluster is currently working on" the run-config prompt saves),
    pub fn set_authored_range(&mut self, start: u32, count: u32) {
        self.settings.start = start;
        self.settings.count = count;
    }

    /// Where the authored range has actually got to: the first chapter of it
    pub fn remaining(&self) -> Option<(u32, u32, u32)> {
        let (start, count) = (self.settings.start, self.settings.count);
        if count == 0 {
            return None;
        }
        let end = start.saturating_add(count - 1);
        let merged: std::collections::BTreeSet<u32> = self
            .tasks
            .values()
            .filter(|t| t.stage == bm_proto::Stage::Merge && t.state == TaskState::Done)
            .map(|t| t.chapter)
            .collect();
        let first = (start..=end).find(|n| !merged.contains(n))?;
        let done = (start..=end).filter(|n| merged.contains(n)).count() as u32;
        Some((first, end, done))
    }

    /// The same answer as one operator-facing line: `ch4..100 · 3 done, 97 to
    pub fn remaining_line(&self) -> String {
        let (start, count) = (self.settings.start, self.settings.count);
        if count == 0 {
            return "range is empty".to_string();
        }
        match self.remaining() {
            Some((from, to, done)) => format!(
                "ch{from}..{to} · {done} done, {} to go",
                (to - from + 1) as u64
            ),
            None => format!("ch{start}..{} · every chapter merged", start + count - 1),
        }
    }

    pub fn save(&self) {
        // Runtime only: connection config lives in machines.json and is
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
                         "profile": self.ledger_profile,
                         "exclusive": self.exclusive});
        let _ = bm_core::write_json(&self.ledger_path(), &doc);
    }

    pub fn load_ledger(&mut self) {
        let Ok(doc): Result<Value, _> =
            bm_core::read_json(&self.ledger_path()).map_err(|_| anyhow::anyhow!("none"))
        else {
            return;
        };
        // One-way migration: the old shape stored full Machines under
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
        self.unreadable_tasks.clear();
        if let Some(tasks) = doc.get("tasks").and_then(|t| t.as_array()) {
            for t in tasks {
                match serde_json::from_value::<Task>(t.clone()) {
                    Ok(task) => {
                        self.tasks.insert(task.id(), task);
                    }
                    // **Kept, not dropped.** A row this build cannot read is
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
        // One-way migration: no row is ever pinned — takes are independent
        let released = self.release_pins();
        if released > 0 {
            self.push_event(
                "info",
                format!("released {released} pin(s) — every row is offerable to any box now"),
            );
        }
        self.ledger_profile = doc
            .get("profile")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        // The exclusive-write queue survives restarts the way tasks do: an
        self.exclusive = doc
            .get("exclusive")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
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
                    let (bxo, _) = split_machine(&mac, &mac.addr);
                    save_box(&boxes_path, &bxo)?;
                }
            }
        }
        // Tasks and workers ride along untouched; `machine_state` is built
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
    pub fn reap(&mut self) -> Vec<String> {
        let now = now_secs();
        let mut out = Vec::new();
        // **One liveness set for both passes**, because the two passes need the
        let live: std::collections::HashSet<String> = self
            .beats
            .values()
            .filter(|b| now.saturating_sub(b.ts) < 90)
            .map(|b| b.worker_id.clone())
            .collect();
        let mut expired = Vec::new();
        // A row that expired while its worker was still answering:
        let mut stuck: Vec<(String, String, u32)> = Vec::new();
        for t in self.tasks.values_mut() {
            if matches!(t.state, TaskState::Assigned | TaskState::Running)
                && t.lease_until.map(|l| l < now).unwrap_or(false)
            {
                let id = t.id();
                // Racing rows keep every live holder: drop the dead ones, and
                let live_holders: Vec<String> = t
                    .holders()
                    .into_iter()
                    .filter(|w| live.contains(*w))
                    .map(str::to_string)
                    .collect();
                if !t.racers.is_empty() && !live_holders.is_empty() {
                    let dead: Vec<String> = t
                        .holders()
                        .into_iter()
                        .filter(|w| !live.contains(*w))
                        .map(str::to_string)
                        .collect();
                    for w in &dead {
                        t.remove_holder(w);
                    }
                    t.lease_until = Some(now + super::lease_for(t.stage));
                    t.detail = "requeued: lease extended, still racing".to_string();
                    t.updated = now;
                    expired.push(id.clone());
                    out.push(id);
                    continue;
                }
                // Counted **only** when the holder was still beating, so the
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
        let mut orphaned = Vec::new();
        if now.saturating_sub(self.started_at) > 120 {
            for t in self.tasks.values_mut() {
                if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                    continue;
                }
                if !t.racers.is_empty() {
                    let dead: Vec<String> = t
                        .holders()
                        .into_iter()
                        .filter(|w| !live.contains(*w))
                        .map(str::to_string)
                        .collect();
                    if dead.is_empty() {
                        continue;
                    }
                    for w in &dead {
                        t.remove_holder(w);
                    }
                    if t.assigned_to.is_some() {
                        t.detail = "requeued: holder gone, still racing".to_string();
                        t.updated = now;
                        continue;
                    }
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

    /// Release every row a render offer covered, strike-free, because the
    pub(crate) fn release_render_rows(&mut self, task_id: &str, why: &str) -> String {
        let now = now_secs();
        let rows = self.covered_rows(task_id);
        for id in &rows {
            if let Some(t) = self.tasks.get_mut(id) {
                // Only rows this offer actually holds: a row moved on since
                if matches!(t.state, TaskState::Assigned | TaskState::Running) {
                    Self::release(t, now, why);
                }
            }
        }
        self.save();
        format!("released {} row(s) back to the pool", rows.len())
    }

    /// Return one task to the pool. Attempts are kept — this unsticks, it
    pub(crate) fn release(t: &mut Task, now: u64, why: &str) {
        t.state = TaskState::Pending;
        t.clear_holders();
        t.lease_until = None;
        t.detail = format!("requeued: {why}");
        t.updated = now;
    }

    /// Drop every row's affinity pin. No row is ever pinned: takes are
    pub(crate) fn release_pins(&mut self) -> usize {
        let mut released = 0;
        for t in self.tasks.values_mut() {
            if t.affinity.take().is_some() {
                released += 1;
            }
        }
        released
    }

    /// Refuse voice/cache surgery while workers are mid-play: acting then
    pub(crate) fn ensure_idle(&self) -> anyhow::Result<()> {
        let now = now_secs();
        let fresh = |ts: u64| now.saturating_sub(ts) < 30;
        let mut busy: Vec<String> = Vec::new();
        for t in self.tasks.values() {
            if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
                continue;
            }
            let live_holders: Vec<&str> = t
                .holders()
                .into_iter()
                .filter(|w| self.beats.get(*w).map(|b| fresh(b.ts)).unwrap_or(false))
                .collect();
            if !live_holders.is_empty() {
                busy.push(format!("{} on {}", t.id(), live_holders.join(",")));
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
                "workers mid-play ({}) — X stops everything, then retry (the live API queues this instead)",
                busy.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
            );
        }
        Ok(())
    }
}
