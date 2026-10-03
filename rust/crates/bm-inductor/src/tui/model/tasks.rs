use super::*;

/// The one thing the ledger is narrowed to, besides the typed text.
///
/// **A cycle, not a key per facet**, and deliberately so. Every letter on this
/// screen already belongs to the filter — that is why seeing only one stage
/// meant typing `crawl` and nothing shorter — so a facet key would have to be
/// an arrow, a digit or a chord. An arrow then has one obvious meaning, and the
/// bar drawn under the counts always says where the cycle is, the way the
/// palette cycle's status line does. `←/→` steps it; nothing else changes hands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Facet {
    #[default]
    All,
    Crawl,
    Digest,
    Render,
    Merge,
    /// Nothing has taken it: `pending`. "Queued" is the word an operator uses
    /// and no state is spelled with it, which is the whole reason this chip
    /// exists rather than being typed.
    Queued,
    /// A box has it and is on it: `assigned` or `running`.
    Active,
    Done,
    Shelved,
    Failed,
    /// Held by a worker that has stopped beating — the row somebody comes to
    /// this screen to unstick, and the reason the filter is not enough on its
    /// own: no word in the ledger's own vocabulary names it.
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
///
/// Only rows that are *out* count: a `pending` row has no holder to have lost,
/// and reporting it as abandoned would make the chip useless — on a stalled
/// book almost everything is pending, and the one row worth finding would be
/// lost in it.
///
/// Any live holder is enough to keep the row alive, racers included, so a digest
/// row two boxes are grinding stays out of the list while either one answers.
pub(crate) fn abandoned(t: &Task, live: &BTreeSet<String>) -> bool {
    if !matches!(t.state, TaskState::Assigned | TaskState::Running) {
        return false;
    }
    // Written out rather than over `holders()`, which allocates: this runs once
    // per row per frame for the facet bar's counts, and a Vec per row per frame
    // on a five-thousand-row ledger is a cost with nothing to show for it.
    if matches!(&t.assigned_to, Some(w) if live.contains(w)) {
        return false;
    }
    !t.racers.iter().any(|r| live.contains(r))
}

/// The worker ids the ledger should treat as alive: a beat inside the 90s
/// window, on a box that has not since been declared offline.
///
/// Both halves matter and both are already the panes' rule ([`live_beats`],
/// [`beat_backed`]): a beat alone would list a ghost whose machine has been
/// stamped `Offline`, and the `abandoned` chip would then quietly disagree with
/// the machines pane about who is gone.
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
/// (chapter, then stage).
///
/// The two narrow differently and both apply: the **facet** is a kind (this
/// stage, this state, "held by a box that stopped answering"), the **text** is a
/// name — a term in the task id (`digest:3`), the stage name, the state name, or
/// the chapter number. So `render` alone is every render row and a facet of
/// render plus `41` is chapter 41's. Substring rather than prefix, so `shel`,
/// `shelv` and `shelved` all work; the hint line says so, because a filter
/// nobody can predict is a filter nobody uses.
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
/// ETA use). The panes hide the rest: a dead worker rendered as an idle row
/// is how two hares happen.
pub(crate) fn live_beats(beats: &[Heartbeat], now: u64) -> Vec<&Heartbeat> {
    beats
        .iter()
        .filter(|b| now.saturating_sub(b.ts) < 90)
        .collect()
}

/// Whether a beat's box still backs it. A machine stamped `Offline` after
/// the beat landed has declared the worker silent — the row is a ghost and
/// listing it as live contradicts the machines pane. A beat newer than the
/// verdict (or an unstamped box) still counts, so one slow poll flickers
/// the dot without deleting the row.
pub(crate) fn beat_backed(machines: &[Machine], beat: &Heartbeat) -> bool {
    match machines.iter().find(|m| m.addr == beat.addr) {
        Some(m) => {
            m.state != MachineState::Offline || m.state_since == 0 || beat.ts >= m.state_since
        }
        None => true,
    }
}

/// A worker's self-reported display name, when any beat carries one for this
/// id. The name is drawn once at worker startup and kept in `worker.alias`;
/// the panes show it verbatim so one worker never wears two names on one
/// screen. `None` means fall back to hashing the id (older agents).
pub(crate) fn reported_alias<'a>(beats: &'a [Heartbeat], id: &str) -> Option<&'a str> {
    beats
        .iter()
        .find(|b| b.worker_id == id && !b.alias.is_empty())
        .map(|b| b.alias.as_str())
}

/// Every address one EC2 instance can be reached at, public first.
///
/// The registry keys a launched box on its public address
/// (`machine_from_instance`), while the agent on that box reports whatever
/// address it binds — usually the private one. The `:down` guard treats either
/// as the same box, so the two halves agree about what "that machine" is.
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
/// money and can still run work.
pub(crate) fn is_live_state(state: &str) -> bool {
    matches!(state, "pending" | "running" | "stopping")
}

/// Workers mid-task on any of `addrs` — the in-flight guard for `:down`.
///
/// A box killed here loses that render, and TTS is stochastic: the same inputs
/// do not reproduce the same audio, so the loss is not recoverable from the
/// ledger. Returned as task ids (plus worker ids for a render not yet handed a
/// task record) so the refusal can name exactly what is in flight.
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
/// used (`hawk`), so the workers pane agrees with the events pane. Falls
/// back to the reported OS hostname, then the address — a box the registry
/// never saw still renders something true.
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
/// say (`render ch91 Aria (3/12)` → `Aria (3/12)`, `merge ch9` → `—`).
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
/// the stage can draw on (crawl: every chapter; digest: crawled; render:
/// digested; merge: rendered). A chapter is done when it has rows and every
/// row for the stage is Done (render runs one row per take).
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
