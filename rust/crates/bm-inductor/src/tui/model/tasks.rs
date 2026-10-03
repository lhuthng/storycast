use super::*;

/// The one thing the ledger is narrowed to, besides the typed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Facet {
    #[default]
    All,
    Crawl,
    Digest,
    Render,
    Merge,
    /// Nothing has taken it: `pending`. "Queued" is the word an operator uses
    Queued,
    /// A box has it and is on it: `assigned` or `running`.
    Active,
    Done,
    Shelved,
    Failed,
    /// Held by a worker that has stopped beating — the row somebody comes to
    Abandoned,
}

impl Facet {
    pub(crate) const ALL: [Facet; 11] = [
        Facet::All,
        Facet::Crawl,
        Facet::Digest,
        Facet::Render,
        Facet::Merge,
        Facet::Queued,
        Facet::Active,
        Facet::Done,
        Facet::Shelved,
        Facet::Failed,
        Facet::Abandoned,
    ];

    /// The chip's word, drawn on the bar and said in the status line.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Facet::All => "all",
            Facet::Crawl => "crawl",
            Facet::Digest => "digest",
            Facet::Render => "render",
            Facet::Merge => "merge",
            Facet::Queued => "queued",
            Facet::Active => "active",
            Facet::Done => "done",
            Facet::Shelved => "shelved",
            Facet::Failed => "failed",
            Facet::Abandoned => "abandoned",
        }
    }

    /// The next chip along, wrapping at both ends.
    pub(crate) fn step(self, forward: bool) -> Facet {
        let n = Self::ALL.len();
        let i = self as usize;
        Self::ALL[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }

    pub(crate) fn matches(self, t: &Task, live: &BTreeSet<String>) -> bool {
        match self {
            Facet::All => true,
            Facet::Crawl => t.stage == Stage::Crawl,
            Facet::Digest => t.stage == Stage::Digest,
            Facet::Render => t.stage == Stage::Render,
            Facet::Merge => t.stage == Stage::Merge,
            Facet::Queued => t.state == TaskState::Pending,
            Facet::Active => matches!(t.state, TaskState::Assigned | TaskState::Running),
            Facet::Done => t.state == TaskState::Done,
            Facet::Shelved => t.state == TaskState::Shelved,
            Facet::Failed => t.state == TaskState::Failed,
            Facet::Abandoned => abandoned(t, live),
        }
    }
}

/// Whether every box holding this row has gone quiet.
pub(crate) fn abandoned(t: &Task, live: &BTreeSet<String>) -> bool {
    if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
        return false;
    }
    // Written out rather than over `holders()`, which allocates: this runs once
    if matches!(&t.assigned_to, Some(w) if live.contains(w)) {
        return false;
    }
    !t.racers.iter().any(|r| live.contains(r))
}

/// The worker ids the ledger should treat as alive: a beat inside the 90s
pub(crate) fn live_worker_ids(
    beats: &[Heartbeat],
    machines: &[Machine],
    now: u64,
) -> BTreeSet<String> {
    live_beats(beats, now)
        .into_iter()
        .filter(|b| beat_backed(machines, b))
        .map(|b| b.worker_id.clone())
        .collect()
}

/// Tasks matching the facet and then the filter, in the ledger's own order
pub(crate) fn filtered_tasks<'a>(
    tasks: &'a [Task],
    filter: &str,
    facet: Facet,
    live: &BTreeSet<String>,
) -> Vec<&'a Task> {
    let f = filter.trim().to_lowercase();
    tasks
        .iter()
        .filter(|t| facet.matches(t, live))
        .filter(|t| {
            if f.is_empty() {
                return true;
            }
            let id = t.id().to_lowercase();
            let chapter = t.chapter.to_string();
            id.contains(&f)
                || t.stage.as_str().contains(&f)
                || t.state.as_str().contains(&f)
                || chapter.contains(&f)
        })
        .collect()
}

/// Beats inside the liveness window (90s — the same window the reaper and the
pub(crate) fn live_beats(beats: &[Heartbeat], now: u64) -> Vec<&Heartbeat> {
    beats
        .iter()
        .filter(|b| now.saturating_sub(b.ts) < 90)
        .collect()
}

/// Whether a beat's box still backs it. A machine stamped `Offline` after
pub(crate) fn beat_backed(machines: &[Machine], beat: &Heartbeat) -> bool {
    match machines.iter().find(|m| m.addr == beat.addr) {
        Some(m) => {
            m.state != MachineState::Offline || m.state_since == 0 || beat.ts >= m.state_since
        }
        None => true,
    }
}

/// A worker's self-reported display name, when any beat carries one for this
pub(crate) fn reported_alias<'a>(beats: &'a [Heartbeat], id: &str) -> Option<&'a str> {
    beats
        .iter()
        .find(|b| b.worker_id == id && !b.alias.is_empty())
        .map(|b| b.alias.as_str())
}

/// Every address one EC2 instance can be reached at, public first.
pub(crate) fn instance_addresses(instances: &[bm_core::provision::AwsInstance]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in instances {
        for a in [&i.public_ip, &i.private_ip] {
            if !a.is_empty() && !out.contains(a) {
                out.push(a.clone());
            }
        }
    }
    out
}

/// Whether an instance is worth terminating: the three states that still cost
pub(crate) fn is_live_state(state: &str) -> bool {
    matches!(state, "pending" | "running" | "stopping")
}

/// Workers mid-task on any of `addrs` — the in-flight guard for `:down`.
pub(crate) fn busy_on(
    beats: &[Heartbeat],
    tasks: &[Task],
    addrs: &[String],
    now: u64,
) -> Vec<String> {
    let on_it = |a: &str| addrs.iter().any(|x| x == a);
    let live: Vec<&Heartbeat> = beats
        .iter()
        .filter(|b| now.saturating_sub(b.ts) < 90)
        .collect();
    let addr_of = |worker: &str| {
        live.iter()
            .find(|b| b.worker_id == worker)
            .map(|b| b.addr.as_str())
    };
    let mut busy: Vec<String> = Vec::new();
    for t in tasks {
        if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
            continue;
        }
        let here = t.affinity.as_deref().map(on_it).unwrap_or(false)
            || t.holders().into_iter().filter_map(addr_of).any(on_it);
        if here {
            busy.push(t.id());
        }
    }
    for b in &live {
        if b.stage.is_some() && b.progress < 0.999 && on_it(&b.addr) {
            let id = b.worker_id.clone();
            if !busy.contains(&id) {
                busy.push(id);
            }
        }
    }
    busy.sort();
    busy.dedup();
    busy
}

/// Display name for a beat's box: the registry handle the provision log
pub(crate) fn machine_name<'a>(machines: &'a [Machine], beat: &'a Heartbeat) -> &'a str {
    machines
        .iter()
        .find(|m| m.addr == beat.addr)
        .map(|m| {
            if m.name.is_empty() {
                if beat.hostname.is_empty() {
                    beat.addr.as_str()
                } else {
                    beat.hostname.as_str()
                }
            } else {
                m.name.as_str()
            }
        })
        .unwrap_or_else(|| {
            if beat.hostname.is_empty() {
                beat.addr.as_str()
            } else {
                beat.hostname.as_str()
            }
        })
}

/// Workers `activity` without the `{stage} ch{n}` the stage/ch columns already
pub(crate) fn short_activity(b: &Heartbeat) -> String {
    let a = b.activity.trim();
    if a.is_empty() {
        return "—".into();
    }
    if let (Some(st), Some(ch)) = (b.stage, b.chapter) {
        let prefix = format!("{} ch{}", st.as_str(), ch);
        if let Some(rest) = a.strip_prefix(&prefix) {
            let rest = rest.trim();
            return if rest.is_empty() {
                "—".into()
            } else {
                rest.to_string()
            };
        }
    }
    a.to_string()
}

/// The `threads` column: `{eff}/{cores}` (`?` where either is unknown).
pub(crate) fn threads_label(m: &Machine, beats: &[Heartbeat]) -> String {
    let cores = beats
        .iter()
        .filter(|b| b.addr == m.addr && b.cores.filter(|c| *c > 0).is_some())
        .max_by_key(|b| b.ts)
        .and_then(|b| b.cores);
    match (m.tts_threads.map(u32::from), cores) {
        (Some(eff), Some(c)) => format!("{eff}/{c}"),
        (Some(eff), None) => format!("{eff}/?"),
        (None, Some(c)) => format!("{}/{c}", (c / 2).clamp(1, 8)),
        (None, None) => "?/?".into(),
    }
}

/// Per-stage chapter progress in pipeline order: done chapters over the chapters
pub(crate) fn pipeline_counts(tasks: &[Task]) -> Vec<(Stage, usize, usize)> {
    fn done_chapters(tasks: &[Task], stage: Stage) -> BTreeSet<u32> {
        tasks
            .iter()
            .filter(|t| t.stage == stage)
            .map(|t| t.chapter)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|ch| {
                tasks
                    .iter()
                    .filter(|t| t.stage == stage && t.chapter == *ch)
                    .all(|t| t.state == TaskState::Done)
            })
            .collect()
    }
    let chapters: BTreeSet<u32> = tasks.iter().map(|t| t.chapter).collect();
    let crawl = done_chapters(tasks, Stage::Crawl);
    let digest = done_chapters(tasks, Stage::Digest);
    let render = done_chapters(tasks, Stage::Render);
    let merge = done_chapters(tasks, Stage::Merge);
    vec![
        (Stage::Crawl, crawl.len(), chapters.len()),
        (Stage::Digest, digest.len(), crawl.len()),
        (Stage::Render, render.len(), digest.len()),
        (Stage::Merge, merge.len(), render.len()),
    ]
}
