use super::*;

///
/// The `ip` column, which is not always an address.
///
/// A launched box whose public address the account has not assigned yet is keyed
/// by its instance id — that is the registry key and the handle the account read
/// repairs it by, not something ssh can answer on. Printing it under `ip` put a
/// plausible-looking non-address where an operator looks for one, which is the
/// confusion the `awaiting-ip` state exists to end. The id is not lost: it is
/// the row's name (see [`machine_label`]) and it is in the note.
pub(crate) fn addr_label(m: &Machine) -> String {
    if m.state.dialable() {
        m.addr.clone()
    } else {
        "—".into()
    }
}

/// The `state` column, which is sometimes about intent instead.
///
/// A parked box reads `relaxed`, because that is the answer to the question the
/// column is asked ("why is nothing happening on this box") — and because its
/// real state is `online`, which would say the opposite.
///
/// A verdict still outranks the park. `offline` and `error` are news about a box
/// and are not made less true by the operator having parked it; showing
/// `relaxed` over them would hide the one fact worth acting on, on the box the
/// operator is least likely to look at because they already dealt with it.
pub(crate) fn work_label(m: &Machine) -> String {
    match m.state {
        MachineState::Offline | MachineState::Error => m.state.as_str().to_string(),
        _ if m.relaxed() => "relaxed".to_string(),
        _ => m.state.as_str().to_string(),
    }
}

/// The one-character state mark the Machines **graph** puts on a box.
///
/// The graph is lean by design — a word per node costs the row the node needs to
/// say what it is doing — so the state travels as a mark and a colour. That is
/// only honest because the marks are distinct without the colour: `●` working,
/// `◐` on its way up, `○` parked, `✗` broken, `?` never contacted. Mono mode and
/// a colour-blind read still tell those five apart, which is the rule the table's
/// `state` column keeps by spelling the word out.
///
/// A fault outranks a park, exactly as in [`work_label`]: the box broke, and that
/// is the fact worth seeing.
pub(crate) fn graph_mark(m: &Machine) -> &'static str {
    match m.state {
        MachineState::Offline | MachineState::Error => "✗",
        MachineState::Unknown if m.relaxed() => "○",
        MachineState::Unknown => "?",
        _ if m.relaxed() => "○",
        MachineState::Online => "●",
        _ => "◐",
    }
}

/// What one box is doing right now, for a graph node's second line.
///
/// The busiest live worker wins, not the first: a box with two workers and one
/// render at 60% is a box that is rendering. Nothing live reads `—` rather than
/// `idle`, because "no worker" and "a worker with nothing to do" are different
/// answers and the graph has room for only one of them.
///
/// `×N` is prefixed only when more than one worker is live on the box — the
/// cluster is one worker per box today, so the prefix stays out of the way until
/// it is not.
/// The busiest live worker on `addr` that is actually working on a task.
///
/// The one place "what is this box busy with" is decided, so the rack's label
/// and the rack's *colour* cannot answer differently about the same box.
fn busiest_task<'a>(
    machines: &[Machine],
    beats: &'a [Heartbeat],
    addr: &str,
    now: u64,
) -> Option<&'a Heartbeat> {
    live_beats(beats, now)
        .into_iter()
        .filter(|b| b.addr == addr && beat_backed(machines, b) && b.task_id.is_some())
        .max_by(|a, b| a.progress.total_cmp(&b.progress))
}

/// The stage a box is working on, for the hue of its art in the rack.
///
/// `None` is a real answer and not a gap: the box is up and has nothing to do,
/// or has never been contacted. The caller tells those two apart by state.
pub(crate) fn node_stage(
    machines: &[Machine],
    beats: &[Heartbeat],
    addr: &str,
    now: u64,
) -> Option<&'static str> {
    busiest_task(machines, beats, addr, now).and_then(|b| b.stage.map(|s| s.as_str()))
}

/// The name a box is known by on the Machines **rack**.
///
/// The animal its worker reports — the same word the Workers pane, the event
/// log and Stats already use, so one box has one name on every screen — falling
/// back to the registry handle when nothing is beating on it, because a box with
/// no worker is still a box and needs a name.
pub(crate) fn machine_alias(
    machines: &[Machine],
    beats: &[Heartbeat],
    addr: &str,
    now: u64,
) -> String {
    if let Some(b) = live_beats(beats, now)
        .into_iter()
        .find(|b| b.addr == addr && beat_backed(machines, b))
    {
        return reported_alias(beats, &b.worker_id)
            .unwrap_or_else(|| worker_alias(&b.worker_id).0)
            .to_string();
    }
    machines
        .iter()
        .find(|m| m.addr == addr)
        .map(machine_label)
        .unwrap_or_else(|| addr.to_string())
}

/// The inductor's own name, for the rack's console.
///
/// **Never an address.** `127.0.0.1:8901` is where this dashboard happens to be
/// pointed — a fact about the session, not about the machine — and a picture
/// that labels the coordinator by its socket teaches the reader nothing they can
/// use. The local box's registry handle when there is one, else the word.
pub(crate) fn inductor_label(machines: &[Machine]) -> String {
    machines
        .iter()
        .find(|m| bm_core::is_local_node(&m.addr))
        .map(machine_label)
        .filter(|l| l != "local")
        .unwrap_or_else(|| "inductor".into())
}

pub(crate) fn current_work(
    machines: &[Machine],
    beats: &[Heartbeat],
    addr: &str,
    now: u64,
) -> String {
    let live: Vec<&Heartbeat> = live_beats(beats, now)
        .into_iter()
        .filter(|b| b.addr == addr && beat_backed(machines, b))
        .collect();
    let Some(first) = live.first() else {
        return "—".into();
    };
    let busy = live
        .iter()
        .filter(|b| b.task_id.is_some())
        .max_by(|a, b| a.progress.total_cmp(&b.progress));
    let line = match busy {
        Some(b) => format!(
            "{} {} {:.0}%",
            b.stage.map(|s| s.as_str()).unwrap_or("task"),
            b.chapter.unwrap_or_default(),
            b.progress * 100.0
        ),
        None if !first.activity.is_empty() => first.activity.clone(),
        None => "idle".into(),
    };
    if live.len() > 1 {
        format!("×{} {line}", live.len())
    } else {
        line
    }
}

/// What kind of box this is, for the Machines pane's `kind` column.
///
/// `local` is the inductor's own node; `aws` is an EC2-launched box, told apart
/// by the instance id stamped in its note; `rmt` is anything reached by ssh.
/// The distinction matters because `(aws)` tells an operator the address can
/// rotate under them — the reason relink exists.
pub(crate) fn machine_kind(m: &Machine) -> &'static str {
    if bm_core::is_local_node(&m.addr) {
        "local"
    } else if bm_core::provision::ec2_id_from_note(&m.note).is_some() {
        "aws"
    } else {
        "rmt"
    }
}

/// The one-word handle for a box in the Machines pane: the registry name when
/// it has one (`box-1`, `thang`); `local` for the inductor's own node; the ssh
/// user for a hand-linked remote; the address otherwise. Pairs with
/// [`machine_kind`] and the raw address so a row reads `box-1 · aws · 18.1.2.3`.
pub(crate) fn machine_label(m: &Machine) -> String {
    if !m.name.is_empty() {
        return m.name.clone();
    }
    if bm_core::is_local_node(&m.addr) {
        return "local".into();
    }
    if bm_core::provision::ec2_id_from_note(&m.note).is_some() {
        return m.addr.clone();
    }
    if !m.ssh_user.is_empty() && m.ssh_user != "unknown" && m.ssh_user != "local" {
        return m.ssh_user.clone();
    }
    m.addr.clone()
}

/// A one-glance encoding of a machine's work policy for the tables:
/// `M>R>D>C`, most-preferred first, enabled stages upper-case and disabled
/// ones lower-case (`m>R>D>C` is "merge is switched off here").
pub(crate) fn policy_summary(m: &Machine) -> String {
    m.effective_task_policy()
        .iter()
        .map(|p| {
            let c = match p.stage {
                Stage::Merge => 'M',
                Stage::Render => 'R',
                Stage::Digest => 'D',
                Stage::Crawl => 'C',
            };
            if p.enabled {
                c.to_string()
            } else {
                c.to_ascii_lowercase().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(">")
}

/// `(state, count)` in `TaskState::ALL` order, zeroes skipped.
pub(crate) fn task_state_counts(tasks: &[Task]) -> Vec<(TaskState, usize)> {
    TaskState::ALL
        .into_iter()
        .map(|s| (s, tasks.iter().filter(|t| t.state == s).count()))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// How long ago a task last changed, in seconds. The ledger stores epoch seconds.
pub(crate) fn age_secs(updated: u64) -> u64 {
    bm_proto::now_secs().saturating_sub(updated)
}

/// One-line task roll-up, rendered in the footer when the terminal is too short
/// for the Tasks pane. Collapsing the pane must not lose the numbers.
pub(crate) fn task_rollup(counts: &serde_json::Value, colour: bool) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let Some(obj) = counts.as_object() else {
        return Line::from(Span::styled("tasks: waiting for the inductor…", dim));
    };
    if obj.is_empty() {
        return Line::from(Span::styled(
            "tasks: none queued — :t enqueues a chapter range",
            dim,
        ));
    }
    let (mut done, mut total, mut failed, mut shelved) = (0u64, 0u64, 0u64, 0u64);
    for c in obj.values() {
        let get = |k: &str| c.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        done += get("done");
        failed += get("failed");
        shelved += get("shelved");
        total += c
            .as_object()
            .map(|m| m.values().filter_map(|v| v.as_u64()).sum::<u64>())
            .unwrap_or(0);
    }
    let open = total.saturating_sub(done).saturating_sub(shelved);
    let mut spans = vec![
        Span::styled("tasks: ", dim),
        Span::styled(
            format!("{done}/{total} done"),
            style_of(colour, Color::Green),
        ),
        Span::styled(format!("  · {open} open"), dim),
    ];
    if failed > 0 {
        spans.push(Span::styled(
            format!("  · {failed} failed"),
            style_of(colour, Color::Yellow),
        ));
    }
    if shelved > 0 {
        spans.push(Span::styled(
            format!("  · {shelved} shelved"),
            style_of(colour, Color::Red),
        ));
    }
    spans.push(Span::styled("   · resize for per-stage detail", dim));
    Line::from(spans)
}

/// Stats pane data: completed-task counts per worker per stage, plus the
/// median task seconds per stage the TUI-side ETA averages. Parsed from the
/// inductor's `stats` key; a missing or partial payload parses to empty —
/// a fresh backend has no history yet, and the pane shows zeroes and
/// dashes, not errors.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkerStats {
    pub counts: HashMap<String, HashMap<String, u64>>,
    pub avg_task_secs: HashMap<String, f64>,
}

pub(crate) fn parse_stats(v: Option<&serde_json::Value>) -> WorkerStats {
    let mut out = WorkerStats::default();
    let Some(obj) = v.and_then(|v| v.as_object()) else {
        return out;
    };
    if let Some(counts) = obj.get("counts").and_then(|c| c.as_object()) {
        for (worker, stages) in counts {
            if let Some(stages) = stages.as_object() {
                for (stage, n) in stages {
                    if let Some(n) = n.as_u64() {
                        out.counts
                            .entry(worker.clone())
                            .or_default()
                            .insert(stage.clone(), n);
                    }
                }
            }
        }
    }
    if let Some(avg) = obj.get("avg_task_secs").and_then(|a| a.as_object()) {
        for (stage, secs) in avg {
            if let Some(secs) = secs.as_f64() {
                out.avg_task_secs.insert(stage.clone(), secs);
            }
        }
    }
    out
}

/// Whether the inductor is handing work out at all, and where the authored
/// range stands — `/api/state`'s `dispatch` key.
///
/// `None` means the payload carried no `dispatch` at all, which is an inductor
/// older than the gate; the footer then says nothing about distribution rather
/// than inventing a hold nobody set.
#[derive(Debug, Clone, Default)]
pub(crate) struct Dispatch {
    pub held: bool,
    /// The inductor's own wording: `ch4..100 · 3 done, 97 to go`.
    pub span: String,
}

pub(crate) fn parse_dispatch(v: Option<&serde_json::Value>) -> Option<Dispatch> {
    let obj = v?.as_object()?;
    Some(Dispatch {
        held: obj.get("held").and_then(|h| h.as_bool()).unwrap_or(false),
        span: obj
            .get("span")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// Seconds left on one task, measured TUI-side: the stage's median task
/// duration scaled by the unworked fraction. `None` means print a dash —
/// no history for the stage yet, or nothing running on the worker.
pub(crate) fn task_eta(avg_task_secs: Option<f64>, progress: f32) -> Option<u64> {
    let avg = avg_task_secs.filter(|a| *a > 0.0)?;
    let left = 1.0 - progress.clamp(0.0, 1.0) as f64;
    if left <= 0.0 {
        return Some(0);
    }
    Some((avg * left).round() as u64)
}
