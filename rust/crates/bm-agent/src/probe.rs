/// True when a heartbeat answer carries the shutdown command.
///
/// Box load for the heartbeat: CPU % plus RAM % and used GiB, sampled on
/// the beat. sysinfo needs two CPU refreshes to form a delta, so the first
/// beat reports `None` (the pane shows a dash) and every beat after is a
/// ~2s average, the cadence heartbeats already run at, no extra timer.
///
/// The same sample counts the TTS sidecars, because that is the quantity that
/// actually kills these boxes: one model is ~2.85 GB and an 8 GiB box cannot
/// hold two, so `> 1` is not a curiosity to log and move past, it is the OOM
/// warming up, and nothing in the cluster could see it before.
pub(crate) struct LoadProbe {
    sys: sysinfo::System,
    primed: bool,
    /// Whether the last sample already warned about a duplicate sidecar, so a
    /// box that stays wrong is reported once through its transition instead of
    /// on every beat. sysinfo is shared with the CPU delta: one timer, one
    /// refresh, no second sampling loop.
    warned_extra_sidecar: bool,
}

/// Everything one beat reports about the box: `(cpu_pct, mem_pct,
/// mem_used_gib, sidecar_count, sidecar_rss_gib)`.
///
/// A named alias because it is a five-tuple threaded from the sampler to the
/// heartbeat builder and back through the status endpoint, positional and
/// easy to transpose, so the names belong somewhere.
pub(crate) type Load = (
    Option<f32>,
    Option<f32>,
    Option<f32>,
    Option<u32>,
    Option<f32>,
);

impl LoadProbe {
    pub(crate) fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_all();
        sys.refresh_memory();
        Self {
            sys,
            primed: false,
            warned_extra_sidecar: false,
        }
    }

    /// One sample. `None` CPU/RAM until the second call: a CPU delta needs two
    /// refreshes, and reporting 0.0 would read as idle rather than unknown. The
    /// sidecar count is available immediately, it is a census, not a delta.
    pub(crate) fn sample(&mut self) -> Load {
        self.sys.refresh_cpu_all();
        self.sys.refresh_memory();
        let (count, rss_gb) = self.sidecars();
        if count > 1 && !self.warned_extra_sidecar {
            self.warned_extra_sidecar = true;
            eprintln!(
                "WARNING: {count} bm-tts processes are alive ({rss_gb:.1} GB resident) — this box is a duplicate model away from the OOM killer; the cluster sweep (`X`) clears it"
            );
        } else if count <= 1 {
            self.warned_extra_sidecar = false;
        }
        if !self.primed {
            self.primed = true;
            return (None, None, None, Some(count), Some(rss_gb));
        }
        let total = self.sys.total_memory() as f64;
        let used = self.sys.used_memory() as f64;
        let mem_pct = if total > 0.0 {
            Some((100.0 * used / total) as f32)
        } else {
            None
        };
        let mem_gb = Some((used / 1_073_741_824.0) as f32);
        (
            Some(self.sys.global_cpu_usage()),
            mem_pct,
            mem_gb,
            Some(count),
            Some(rss_gb),
        )
    }

    /// `(count, total RSS GiB)` of the sidecars on this box.
    fn sidecars(&mut self) -> (u32, f32) {
        let (count, bytes) = sidecar_processes(&mut self.sys);
        (count, bytes as f32 / 1_073_741_824.0)
    }
}

/// The refresh the census runs under: RSS, and **no tasks**.
///
/// `System::refresh_processes` ends in `.with_tasks()`, and sysinfo's own
/// `Default` sets `tasks: true`, because on Linux it lists every *task*
/// (thread) as a process in its own right, each one carrying its parent's
/// `/proc/<pid>` and therefore the parent's whole RSS. A sidecar with an
/// 8-thread ONNX pool was therefore counted as **8 processes holding eight
/// times its memory**: the Linux worker reported `8× 19.5G` for a 2.4 GiB
/// model on a box that simultaneously reported 3.8 GB used. Every task's
/// `/proc/<pid>/task/<tid>/statm` is byte-identical to the leader's, which is
/// what made the multiplication exact.
///
/// macOS enumerates no tasks, `Process::thread_kind` is `None` off
/// Linux/Android, so only the Linux workers were ever wrong, which is why
/// the pane looked sane on the inductor's own box.
///
/// Named and separate so a test can pin the one flag whose *default* is the
/// wrong answer. `nothing()` is not enough on its own: it is `Default`, and
/// `Default` is where `tasks: true` lives.
pub(crate) fn census_refresh_kind() -> sysinfo::ProcessRefreshKind {
    sysinfo::ProcessRefreshKind::nothing()
        .with_memory()
        .without_tasks()
}

/// `(count, total RSS bytes)` of the `bm-tts` processes on this box.
///
/// Matched on the process *name* containing `bm-tts`, which covers both
/// spellings a worker can run, the provisioned `~/bm-worker/bm-tts` and a
/// repo build at `rust/target/{debug,release}/bm-tts`, and deliberately not
/// on argv, which would also match an `ssh … bm-tts` wrapper on the
/// inductor's own box. `name` comes from the process's own `stat` parse, not
/// from a refresh flag, so the narrow [`census_refresh_kind`] keeps it.
///
/// One definition, two callers: the heartbeat's load sample, and the sidecar's
/// own memory guard. Two copies would let the number the panes show and the
/// number the guard acts on disagree, which is the worst version of this,
/// because the guard would be recycling on a reading nobody could see.
pub(crate) fn sidecar_processes(sys: &mut sysinfo::System) -> (u32, u64) {
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, census_refresh_kind());
    let (mut count, mut bytes) = (0u32, 0u64);
    for p in sys.processes().values() {
        // Belt and braces, and not redundant: the refresh above cannot yield a
        // task, but `tasks: true` is the *default*, so one future call to
        // `refresh_processes` or `everything()` restores the 8× silently. A
        // thread is not a model, whatever the refresh asked for.
        if p.thread_kind().is_some() {
            continue;
        }
        if p.name().to_string_lossy().contains("bm-tts") {
            count += 1;
            bytes = bytes.saturating_add(p.memory());
        }
    }
    (count, bytes)
}

/// Tolerant by design: an old inductor answers just `{"ok": true}` (no
/// `shutdown` key, so the default keeps us running), and a non-JSON answer
/// is ignored rather than acted on.
pub(crate) fn wants_shutdown(body: &[u8]) -> bool {
    serde_json::from_slice::<bm_proto::HeartbeatAck>(body)
        .map(|a| a.shutdown)
        .unwrap_or(false)
}
