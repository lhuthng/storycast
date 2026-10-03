/// True when a heartbeat answer carries the shutdown command.
pub(crate) struct LoadProbe {
    sys: sysinfo::System,
    primed: bool,
    /// Whether the last sample already warned about a duplicate sidecar, so a
    warned_extra_sidecar: bool,
}

/// Everything one beat reports about the box: `(cpu_pct, mem_pct,
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
pub(crate) fn census_refresh_kind() -> sysinfo::ProcessRefreshKind {
    sysinfo::ProcessRefreshKind::nothing()
        .with_memory()
        .without_tasks()
}

/// `(count, total RSS bytes)` of the `bm-tts` processes on this box.
pub(crate) fn sidecar_processes(sys: &mut sysinfo::System) -> (u32, u64) {
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, census_refresh_kind());
    let (mut count, mut bytes) = (0u32, 0u64);
    for p in sys.processes().values() {
        // Belt and braces, and not redundant: the refresh above cannot yield a
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
pub(crate) fn wants_shutdown(body: &[u8]) -> bool {
    serde_json::from_slice::<bm_proto::HeartbeatAck>(body)
        .map(|a| a.shutdown)
        .unwrap_or(false)
}
