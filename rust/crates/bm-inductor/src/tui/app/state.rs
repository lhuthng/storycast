use super::*;

impl App {
    /// The settings actually in force: the live ones while the inductor
    pub(crate) fn effective_settings(&self) -> serde_json::Value {
        if let Some(live) = &self.settings {
            return live.clone();
        }
        if !self.layout.root.as_os_str().is_empty() {
            // A missing *or* malformed file both land on the defaults, the same
            if let Ok(v) = bm_core::read_json::<serde_json::Value>(&self.layout.settings()) {
                return v;
            }
        }
        serde_json::to_value(bm_core::config::Settings::default()).unwrap_or_default()
    }

    pub(crate) fn setting_u32(&self, key: &str, default: u32) -> u32 {
        self.effective_settings()
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(default)
    }

    pub(crate) fn setting_f64(&self, key: &str, default: f64) -> f64 {
        self.effective_settings()
            .get(key)
            .and_then(|v| v.as_f64())
            .unwrap_or(default)
    }

    pub(crate) fn setting_str(&self, key: &str, default: &str) -> String {
        self.effective_settings()
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    }

    /// App-wide ssh defaults from [`App::effective_settings`] (see
    pub(crate) fn ssh_defaults(&self) -> bm_core::config::SshDefaults {
        self.effective_settings()
            .get("ssh")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    pub(crate) fn selected_machine(&self) -> Option<Machine> {
        self.machines.get(self.selected).cloned()
    }

    /// Machines to act on for B/R/X: the live registry when the inductor
    pub(crate) fn effective_machines(&self) -> Vec<Machine> {
        if !self.machines.is_empty() {
            return self.machines.clone();
        }
        registry_machines(&self.layout)
    }

    /// Any screen other than the dashboard. Used by the size guard to say when
    pub(crate) fn dialog_open(&self) -> bool {
        !matches!(self.screen, Screen::Normal)
    }

    /// The cast overview's rows, or an empty list before the roster arrives.
    pub(crate) fn cast_rows(&self) -> Vec<CastRow> {
        self.roster.as_ref().map(cast_rows).unwrap_or_default()
    }

    /// Kick off a roster fetch unless one is already in flight.
    pub(crate) fn load_roster(
        &mut self,
        job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
        http: &reqwest::Client,
    ) {
        // Singleton: every R press queues a ~35s job behind the serial worker,
        if self.roster_loading {
            self.set_status(Level::Warn, "roster reload already running — watch events");
            return;
        }
        self.roster_loading = true;
        self.roster_error = None;
        dispatch(
            self,
            job_tx,
            Job::LoadRoster {
                api: self.api.clone(),
                http: http.clone(),
                layout: self.layout.clone(),
            },
        );
    }

    /// Re-read everything that depends on *which* workspace or profile is
    pub(crate) fn relayout(
        &mut self,
        job_tx: &tokio::sync::mpsc::UnboundedSender<Job>,
        http: &reqwest::Client,
    ) {
        if self.layout.root.as_os_str().is_empty() {
            return;
        }
        let (layout, problem) = bm_core::Layout::resolve_or_root(&self.layout.root);
        self.layout = layout;
        self.profile = bm_core::profile::in_force(&self.layout).ok();
        // Cached file indexes: all of them belong to the workspace that was.
        self.lines = None;
        self.lines_loading = false;
        self.sound = None;
        self.sound_loading = false;
        self.roster = None;
        self.roster_loading = false;
        self.locked_lines.clear();
        // The polled view is per-workspace too, and a switch happens with the
        self.tasks.clear();
        self.counts = serde_json::Value::Null;
        self.settings = None;
        match &problem {
            Some(e) => self.log_at(Level::Warn, format!("workspace pointer: {e}")),
            None => self.log_at(
                Level::Info,
                format!(
                    "now on workspace {} · profile {}",
                    crate::tui::model::workspace_label(&self.layout),
                    crate::tui::model::profile_label(self.profile.as_ref()),
                ),
            ),
        }
        self.load_roster(job_tx, http);
    }

    pub(crate) fn machine_by_addr(&self, addr: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.addr == addr)
    }

    /// One blocking snapshot. Only the startup path and the `r` key use this;
    pub(crate) async fn refresh(&mut self, http: &reqwest::Client) {
        let outcome = fetch_state(http, &self.api).await;
        match outcome {
            Ok(v) => self.apply_state(v),
            Err(e) => self.state_failed(e),
        }
    }

    /// Fold one `/api/state` payload into the screen.
    pub(crate) fn apply_state(&mut self, v: serde_json::Value) {
        let mut machines: Vec<Machine> =
            serde_json::from_value(v.get("machines").cloned().unwrap_or_default())
                .unwrap_or_default();
        let mut beats: Vec<Heartbeat> =
            serde_json::from_value(v.get("beats").cloned().unwrap_or_default()).unwrap_or_default();
        let mut tasks: Vec<Task> =
            serde_json::from_value(v.get("tasks").cloned().unwrap_or_default()).unwrap_or_default();
        // The API serialises HashMaps, whose iteration order is not
        machines.sort_by(|a, b| a.addr.cmp(&b.addr));
        beats.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        tasks.sort_by_key(|t| (t.chapter, t.stage));
        // Which launched boxes are waiting to be onboarded, read off the note
        self.onboarded.retain(|addr| {
            machines
                .iter()
                .any(|m| &m.addr == addr && bm_core::provision::awaiting_onboard(&m.note))
        });
        self.pending_onboard = machines
            .iter()
            .filter(|m| {
                bm_core::provision::awaiting_onboard(&m.note) && !self.onboarded.contains(&m.addr)
            })
            .cloned()
            .collect();
        self.machines = machines;
        self.beats = beats;
        self.tasks = tasks;
        self.counts = v.get("counts").cloned().unwrap_or_default();
        self.stats = parse_stats(v.get("stats"));
        self.dispatch = parse_dispatch(v.get("dispatch"));
        self.settings = v.get("settings").cloned();
        self.ingest_events(v.get("events"));
        if self.selected >= self.machines.len() {
            self.selected = self.machines.len().saturating_sub(1);
        }
        self.refreshed = Some(Instant::now());
        if self.conn != Conn::Up {
            if matches!(self.conn, Conn::Down(_)) {
                self.log_at(Level::Ok, "inductor reachable again");
            }
            self.conn = Conn::Up;
        }
    }

    /// Record a failed poll. The message is only logged on the *transition* into
    pub(crate) fn state_failed(&mut self, e: String) {
        if self.conn != Conn::Down(e.clone()) {
            self.log_at(Level::Warn, e.clone());
        }
        self.conn = Conn::Down(e);
    }

    /// Append scheduler events the inductor has not shown us yet.
    pub(crate) fn ingest_events(&mut self, events: Option<&serde_json::Value>) {
        let Some(list) = events.and_then(|v| v.as_array()) else {
            return;
        };
        let mut fresh: Vec<crate::state::EventRecord> = Vec::new();
        for item in list {
            match serde_json::from_value::<crate::state::EventRecord>(item.clone()) {
                Ok(rec) => fresh.push(rec),
                Err(_) => continue,
            }
        }
        let newest = fresh.iter().map(|e| e.id).max();
        // A restarted inductor begins its ids at 0 again. Without this reset the
        if let (Some(newest), Some(last)) = (newest, self.last_event_id) {
            if newest < last {
                self.last_event_id = None;
                self.log_at(Level::Info, "inductor restarted — event stream reset");
            }
        }
        for rec in fresh {
            if self
                .last_event_id
                .map(|last| rec.id <= last)
                .unwrap_or(false)
            {
                continue;
            }
            self.last_event_id = Some(rec.id);
            // The record's own `ts` is epoch seconds on the inductor's clock;
            self.push_log(LogLine {
                level: level_from_str(&rec.level),
                wall: rec.ts,
                text: rec.text,
            });
        }
    }

    pub(crate) fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::JobStarted(id) => {
                if let Some(job) = self.background_jobs.iter_mut().find(|job| job.id == id) {
                    job.started.get_or_insert_with(Instant::now);
                    job.activity = "running".into();
                }
            }
            Ev::JobProgress { id, text } => {
                if let Some(job) = self.background_jobs.iter_mut().find(|job| job.id == id) {
                    job.activity = text;
                }
            }
            Ev::JobFinished(id) => {
                self.background_jobs.retain(|job| job.id != id);
                // The last catch-up provision finishing is what ends the `B`
                if let Some(i) = self.catchup_jobs.iter().position(|j| *j == id) {
                    self.catchup_jobs.remove(i);
                    if self.catchup_jobs.is_empty() {
                        self.backend_start_outstanding = false;
                    }
                }
            }
            Ev::Log(l) => self.push_log(l),
            Ev::Roster(Ok(r)) => {
                self.roster_loading = false;
                self.roster_error = None;
                self.log_at(
                    Level::Ok,
                    format!(
                        "roster: {} voices · {} speakers · {} ({})",
                        r.voices.len(),
                        r.characters.len(),
                        r.cast.len(),
                        r.source
                    ),
                );
                self.roster = Some(r);
            }
            Ev::Roster(Err(e)) => {
                self.roster_loading = false;
                self.roster_error = Some(e.clone());
                self.log_at(Level::Error, format!("roster: {e}"));
            }
            // The inductor's answer to a manual digest, shown either way. The
            Ev::ManualDigest(Ok(line)) => {
                self.log_at(Level::Ok, format!("manual digest: {line}"));
            }
            Ev::ManualDigest(Err(e)) => {
                self.log_at(Level::Error, format!("manual digest: {e}"));
            }
            Ev::DigestPolicy(Ok(msg)) => self.log_at(Level::Ok, msg),
            Ev::DigestPolicy(Err(e)) => self.log_at(Level::Error, format!("digest policy: {e}")),
            // One provider's model list, for the `L` screen's picker. Stored
            Ev::LlmModels { provider, result } => match result {
                Ok(models) => {
                    self.llm_models = models;
                    self.llm_models_for = provider.clone();
                    if let Screen::Llm(v) = &mut self.screen {
                        v.note = format!(
                            "{} model(s) for {provider} — ↑↓ move · Enter saves",
                            self.llm_models.len()
                        );
                    }
                    self.set_status(
                        Level::Ok,
                        format!(
                            "{provider}: {} model(s) — Enter picks, Esc leaves",
                            self.llm_models.len()
                        ),
                    );
                }
                Err(e) => {
                    if let Screen::Llm(v) = &mut self.screen {
                        v.note = e.clone();
                    }
                    self.set_status(Level::Error, e);
                }
            },
            // The `B` job started a backend: run this range on the first live
            Ev::BackendLive { start, count } => {
                self.pending_enqueue = Some((start, count));
                self.log_at(
                    Level::Info,
                    format!("ch{start}×{count} will enqueue once live"),
                );
            }
            // The `B` job stopped at the backend and handed us the boxes that
            Ev::CatchUp { machines, cancel } => {
                self.pending_catchup = Some((machines, cancel));
            }
            Ev::MachineUpdate { addr, state, note } => {
                if let Some(m) = self.machines.iter_mut().find(|m| m.addr == addr) {
                    m.state = state;
                    m.note = note;
                }
            }
            Ev::Lines(res) => {
                self.lines_loading = false;
                match res {
                    Ok(index) => {
                        self.log_at(
                            Level::Info,
                            format!("{} speaker(s) indexed for audition lines", index.len()),
                        );
                        self.lines = Some(index);
                    }
                    // Not fatal: the sample audition still works, only the
                    Err(e) => {
                        self.set_status(
                            Level::Warn,
                            format!("audition lines unavailable: {e} — :current (rendered segments) still plays"),
                        );
                    }
                }
            }
            Ev::Sounds(res) => {
                self.sound_loading = false;
                match res {
                    Ok(data) => {
                        let counts: Vec<String> = bm_core::audio_pool::PoolKind::ALL
                            .iter()
                            .map(|k| format!("{} {}", data.pools[k].len(), k.label()))
                            .collect();
                        self.log_at(
                            Level::Info,
                            format!("sound design loaded: {}", counts.join(" · ")),
                        );
                        self.sound_error = None;
                        self.sound = Some(*data);
                    }
                    // Not fatal to the TUI, fatal to the editor: the screen
                    Err(e) => {
                        self.sound = None;
                        self.sound_error = Some(e.clone());
                        self.log_at(Level::Error, format!("sound design: {e}"));
                    }
                }
            }
            // A fresh account listing. A failed read clears the rows and
            Ev::Cloud(Ok(instances)) => {
                self.cloud_error = None;
                self.cloud = instances;
            }
            Ev::Cloud(Err(e)) => {
                self.cloud.clear();
                self.cloud_error = Some(e);
            }
            // A poller snapshot, applied the moment it arrives: nothing here
            Ev::State(Ok(v)) => self.apply_state(v),
            Ev::State(Err(e)) => self.state_failed(e),
            Ev::Done(kind) => {
                self.pending = self.pending.saturating_sub(1);
                match kind {
                    DoneKind::StartDone => {
                        // The start *job* is over; the sequence it began may not
                        self.backend_start_outstanding = !self.catchup_jobs.is_empty();
                    }
                    DoneKind::RosterDone => self.roster_loading = false,
                    DoneKind::LinesDone => self.lines_loading = false,
                    DoneKind::SoundsDone => self.sound_loading = false,
                    DoneKind::Op {
                        op,
                        key,
                        ok,
                        voice,
                        audio_b64,
                        line_speaker,
                        line_text,
                    } => {
                        self.inflight.retain(|k| *k != key);
                        if op == Op::PreviewVoice || op == Op::Segment {
                            self.audition = None;
                            if ok {
                                if let Screen::Pick(p) = &mut self.screen {
                                    if let Some(v) = voice {
                                        if !p.previewed.contains(&v) {
                                            p.previewed.push(v);
                                        }
                                    }
                                }
                                // A served segment names its sentence: hold it
                                if op == Op::Segment {
                                    if let (Some(speaker), Some(text)) = (line_speaker, line_text) {
                                        let line = crate::tui::audition::AuditionLine {
                                            character: speaker,
                                            text,
                                        };
                                        match &mut self.screen {
                                            Screen::Cast(v) => v.line = Some(line),
                                            Screen::Pick(p) => p.line = Some(line),
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            self.play_audition(ok, audio_b64);
                        }
                        // A successful swap rewrites the cast, so the picker's
                        if op == Op::SwapVoice && ok {
                            self.roster = None;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
