use super::*;

/// The share of this box's RAM the TTS sidecar may hold before it is recycled.
pub(crate) const SIDECAR_RSS_FRACTION: f64 = 0.5;

/// Renders one sidecar process may serve before it is recycled regardless of
pub(crate) const SIDECAR_MAX_RENDERS: u64 = 200;

/// The shortest life a sidecar may have before the guard will recycle it
pub(crate) const SIDECAR_MIN_LIFETIME_SECS: u64 = 300;

/// The guard's thresholds, resolved **once per process**.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// An absolute cap in MiB, or `None` to use [`SIDECAR_RSS_FRACTION`] of
    pub(crate) rss_cap_mib: Option<f64>,
    pub(crate) max_renders: u64,
    pub(crate) min_lifetime_secs: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            rss_cap_mib: None,
            max_renders: SIDECAR_MAX_RENDERS,
            min_lifetime_secs: SIDECAR_MIN_LIFETIME_SECS,
        }
    }
}

impl Budget {
    /// The compiled defaults with any per-box override applied, read from the
    pub(crate) fn from_env() -> Budget {
        // Two typed readers rather than one generic: a closure's type is fixed
        let num_f = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
        };
        let num_u = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let d = Budget::default();
        Budget {
            rss_cap_mib: num_f("BM_TTS_MAX_RSS_MB"),
            max_renders: num_u("BM_TTS_MAX_RENDERS").unwrap_or(d.max_renders),
            min_lifetime_secs: num_u("BM_TTS_MIN_LIFETIME_SECS").unwrap_or(d.min_lifetime_secs),
        }
    }

    /// The cap for a box with `total_bytes` of RAM, in MiB, `None` when the
    pub(crate) fn cap_mib(&self, total_bytes: u64) -> Option<f64> {
        if let Some(cap) = self.rss_cap_mib {
            return Some(cap);
        }
        (total_bytes > 0).then(|| total_bytes as f64 / 1_048_576.0 * SIDECAR_RSS_FRACTION)
    }

    /// One line naming the budget in force, for the startup log.
    pub(crate) fn describe(&self, total_bytes: u64) -> String {
        match self.cap_mib(total_bytes) {
            Some(cap) => format!(
                "TTS sidecar budget: recycle at {cap:.0} MiB resident{}{}, or {} renders, at most once per {}s",
                if self.rss_cap_mib.is_some() { " (BM_TTS_MAX_RSS_MB)" } else { "" },
                if self.rss_cap_mib.is_some() {
                    String::new()
                } else {
                    format!(" ({:.0}% of this box)", SIDECAR_RSS_FRACTION * 100.0)
                },
                self.max_renders,
                self.min_lifetime_secs,
            ),
            None => format!(
                "TTS sidecar budget: {} renders, at most once per {}s (no RSS cap — this box reports no memory)",
                self.max_renders, self.min_lifetime_secs
            ),
        }
    }
}

/// A render refused because the operator's policy turns render off for this
#[derive(Debug)]
pub(crate) struct PolicyRefusal(pub(crate) &'static str);

impl std::fmt::Display for PolicyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PolicyRefusal {}

pub(crate) struct Sidecar {
    pub(crate) tts_url: String,
    pub(crate) child: Option<tokio::process::Child>,
    /// Renders this worker has spoken through the process **currently on the
    pub(crate) served: u64,
    /// When the process currently serving was first used, in Unix seconds.
    pub(crate) serving_since: Option<u64>,
    /// This guard's own `sysinfo` handle. Separate from the heartbeat's
    pub(crate) sys: sysinfo::System,
    /// The thresholds in force **on this box**, resolved once at construction
    pub(crate) budget: Budget,
    /// The `threads` value the model on the port was last *asked* to open with,
    pub(crate) applied_threads: Option<u32>,
}

/// The memory guard's decision, as a pure function of what was measured.
pub(crate) fn budget_verdict(
    rss_bytes: u64,
    total_bytes: u64,
    served: u64,
    serving_for: Option<u64>,
    budget: &Budget,
) -> Option<String> {
    if matches!(serving_for, Some(life) if life < budget.min_lifetime_secs) {
        return None;
    }
    if served >= budget.max_renders {
        return Some(format!(
            "has served {served} renders since it started (budget {})",
            budget.max_renders
        ));
    }
    if let Some(cap_mib) = budget.cap_mib(total_bytes) {
        let rss_mib = rss_bytes as f64 / 1_048_576.0;
        if rss_mib >= cap_mib {
            return Some(format!(
                "holds {rss_mib:.0} MiB, at or over the {cap_mib:.0} MiB budget{}",
                if budget.rss_cap_mib.is_some() {
                    " (BM_TTS_MAX_RSS_MB)"
                } else {
                    " (half this box's RAM)"
                }
            ));
        }
    }
    None
}

impl Sidecar {
    pub(crate) fn new(tts_url: &str) -> Self {
        Sidecar {
            tts_url: tts_url.trim_end_matches('/').to_string(),
            child: None,
            served: 0,
            serving_since: None,
            sys: sysinfo::System::new(),
            budget: Budget::from_env(),
            applied_threads: None,
        }
    }

    pub(crate) fn tts(&self) -> Tts {
        Tts::new(&self.tts_url)
    }

    fn port(&self) -> u16 {
        self.tts_url
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_end_matches('/').parse().ok())
            .unwrap_or(SIDECAR_PORT)
    }

    /// Whether this worker is holding a sidecar child it started.
    pub(crate) fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Count the takes a render actually spoke. The guard's coarse trigger.
    pub(crate) fn note_renders(&mut self, units: u64) {
        if units == 0 {
            return;
        }
        self.served = self.served.saturating_add(units);
        self.serving_since.get_or_insert_with(bm_proto::now_secs);
    }

    /// Forget the work counted against the process on the port. Called wherever
    pub(crate) fn forget_work(&mut self) {
        self.served = 0;
        self.serving_since = None;
    }

    /// Why the sidecar on this port should be recycled before the next render,
    pub(crate) fn over_budget(&mut self) -> Option<String> {
        let (_, bytes) = sidecar_processes(&mut self.sys);
        let total = self.sys.total_memory();
        let serving_for = self
            .serving_since
            .map(|t| bm_proto::now_secs().saturating_sub(t));
        budget_verdict(bytes, total, self.served, serving_for, &self.budget)
    }

    /// Ensure the sidecar answers, starting it if needed. Idempotent.
    pub(crate) async fn ensure(
        &mut self,
        layout: &Layout,
        desired_threads: Option<u32>,
    ) -> Result<()> {
        if self.serving_current().await {
            if self.applied_threads == desired_threads {
                return Ok(());
            }
            // The count changed (or a value was pushed where the box had
            self.reap_all().await;
        }
        // Not ready, but two different things can be in the way, and they want
        match self.tts().probe().await {
            Health::Loading => {
                if self.wait_ready().await {
                    return Ok(());
                }
                anyhow::bail!(
                    "TTS sidecar is still loading after {}s",
                    SIDECAR_STARTUP_TICKS as u64 * SIDECAR_STARTUP_TICK.as_secs()
                );
            }
            Health::Up => self.reap_all().await,
            Health::Absent => self.stop(),
        }
        let resolved = desired_threads
            .map(|n| n as usize)
            .unwrap_or_else(bm_core::config::tts_threads);
        let (bin, args) = layout.sidecar_command(self.port(), resolved);
        if !bin.is_file() {
            anyhow::bail!(
                "no TTS sidecar at {} — build it (`make build`) or provision this box (`make provision BOX=…`)",
                bin.display()
            );
        }
        let mut child = tokio::process::Command::new(&bin)
            .args(&args)
            // The SONAME is `libonnxruntime.so.1`, and it sits beside the binary
            // at the worker root. Without this the spawn dies with "error while
            // loading shared libraries", which reads as a missing model.
            .env("LD_LIBRARY_PATH", layout.tts_lib_dir())
            .stdout(std::process::Stdio::null())
            // Piped, not nulled: when the child dies mid-startup its last
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning TTS sidecar {}", bin.display()))?;
        let mut stderr = child.stderr.take();
        for _ in 0..SIDECAR_STARTUP_TICKS {
            tokio::time::sleep(SIDECAR_STARTUP_TICK).await;
            if self.serving_current().await {
                self.child = Some(child);
                // A fresh process opened with this count: record it so a later
                self.applied_threads = desired_threads;
                // A fresh process: the renders counted against the previous one
                self.forget_work();
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                // Our child exited. If an instance that is *starting* holds the
                if self.tts().probe().await == Health::Loading && self.wait_ready().await {
                    return Ok(());
                }
                anyhow::bail!(
                    "TTS sidecar exited during startup{}",
                    child_stderr_tail(&mut stderr).await
                );
            }
        }
        let _ = child.kill().await;
        anyhow::bail!("TTS sidecar never answered /health")
    }

    /// Poll until a server answers ready, or the startup budget runs out.
    async fn wait_ready(&self) -> bool {
        for _ in 0..SIDECAR_STARTUP_TICKS {
            tokio::time::sleep(SIDECAR_STARTUP_TICK).await;
            if self.serving_current().await {
                return true;
            }
        }
        false
    }

    /// Health plus capability: the server must serve the policy endpoint
    async fn serving_current(&self) -> bool {
        if !self.tts().health().await {
            return false;
        }
        match self.tts().policy().await {
            Ok(p) => p.get("allowed_voices").and_then(|v| v.as_array()).is_some(),
            Err(_) => false,
        }
    }

    /// Signal our own child, if we have one, and forget the work counted
    pub(crate) fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        self.forget_work();
    }

    /// Stop **every** sidecar on this box: the child we spawned, and a server
    pub(crate) async fn reap_all(&mut self) {
        self.stop();
        let tts = self.tts();
        if tts.probe().await == Health::Absent {
            return;
        }
        if let Err(e) = tts.shutdown().await {
            // An older sidecar has no `/shutdown`. Say so and move on: one is
            eprintln!("could not ask the sidecar to exit ({e}) — merge proceeds with it resident");
            return;
        }
        // The model is dropped by the process returning from `main`; give the
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if tts.probe().await == Health::Absent {
                return;
            }
        }
        eprintln!("sidecar still answering 5s after shutdown — merge proceeds anyway");
    }

    /// Recycle the model if it has outgrown its budget, **the memory guard**.
    pub(crate) async fn recycle_if_over_budget(&mut self) {
        let Some(why) = self.over_budget() else {
            return;
        };
        println!("sidecar {why} — recycling it so the next render starts from a clean model");
        self.reap_all().await;
    }
}

/// What a dead sidecar said as it died, or nothing when it said nothing.
async fn child_stderr_tail(stderr: &mut Option<tokio::process::ChildStderr>) -> String {
    let Some(s) = stderr else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = tokio::io::AsyncReadExt::read_to_string(s, &mut buf).await;
    let t = buf.trim();
    if t.is_empty() {
        return String::new();
    }
    let tail: String = t
        .chars()
        .rev()
        .take(300)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!(": {tail}")
}

// ---------------------------------------------------------------------------
