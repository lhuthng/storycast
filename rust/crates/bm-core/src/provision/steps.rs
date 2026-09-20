use anyhow::{Context, Result};
use bm_proto::Machine;
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::ssh::Ssh;
use super::stamp::{compute_provision_stamp, parse_stamp, ProvisionStamp};
use super::{REMOTE_DIR, TTS_PORT};

/// What a probe found on a machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    pub reachable: bool,
    pub hostname: String,
    pub nproc: u32,
    pub mem_mb: u64,
    pub disk_free_mb: u64,
    pub arch: String,
    /// Remote OS in Rust's spelling (`std::env::consts::OS`): `linux`,
    /// `macos`. Probed via `uname -s`; `#[serde(default)]` so a record
    /// written before this field still reads (the summary then shows the
    /// arch alone).
    #[serde(default)]
    pub os: String,
    /// Version string reported by the installed agent, if any.
    pub agent_version: Option<String>,
    /// A usable Python interpreter with the TTS deps installed.
    pub python_present: bool,
    /// The Rust sidecar binary, `~/{REMOTE_DIR}/bm-tts`.
    ///
    /// Both sidecars can be present at once, and the two flags are separate on
    /// purpose: which one a box *runs* is a provisioning choice, not something
    /// to infer from what happens to be on disk. `#[serde(default)]` so a
    /// machine record written before this field still reads.
    #[serde(default)]
    pub tts_bin_present: bool,
    /// A baked `~/{REMOTE_DIR}/models/` — the Rust sidecar's weights.
    #[serde(default)]
    pub models_present: bool,
    /// Enrolled clone-voice names parsed from the voice store (no model load).
    #[serde(default)]
    pub voices: Vec<String>,
    pub tts_up: bool,
    pub note: String,
    /// Manifest stamp found on the remote box from the previous provision, if any.
    #[serde(default)]
    pub stamp: Option<ProvisionStamp>,
}

impl Probe {
    /// A machine is ready to take work when it is reachable, runs the exact
    /// agent build we are scheduling with, and has the TTS sidecar.
    ///
    /// There is one sidecar now. `python_present` is still reported by the probe
    /// but is no longer consulted: a box that happens to have a virtualenv is not
    /// thereby able to render.
    pub fn configured(&self, want_version: &str) -> bool {
        self.reachable && self.agent_version.as_deref() == Some(want_version) && self.rust_ready()
    }

    /// The TTS sidecar is installed and has its weights.
    pub fn rust_ready(&self) -> bool {
        self.tts_bin_present && self.models_present
    }

    /// Which sidecar this box would run, for the TUI's summary line.
    pub fn sidecar(&self) -> &'static str {
        if self.rust_ready() {
            "rust"
        } else {
            "none"
        }
    }

    /// One-line summary for the TUI machine pane.
    pub fn summary(&self) -> String {
        if !self.reachable {
            return format!("unreachable: {}", self.note);
        }
        // ponytail: one platform string, not two fields to keep in sync.
        let platform = if self.os.is_empty() {
            self.arch.clone()
        } else {
            format!("{}/{}", self.os, self.arch)
        };
        // A count, not the list: 50 enrolled names wrapped the Logs pane for
        // screens. Which voices enrolled is already on the `enrolled …` lines.
        format!(
            "{} · {} cpu · {} MB ram · {} · agent={} · sidecar={} · voices={} · tts={}",
            self.hostname,
            self.nproc,
            self.mem_mb,
            platform,
            self.agent_version.as_deref().unwrap_or("absent"),
            self.sidecar(),
            if self.voices.is_empty() {
                "-".into()
            } else {
                format!("{} voices", self.voices.len())
            },
            if self.tts_up { "up" } else { "down" },
        )
    }
}

/// `uname -m` spelling → Rust's (`std::env::consts::ARCH`): `arm64` (macOS)
/// and `aarch64` (Linux) are the same chip; `amd64` is `x86_64`. Unknown
/// spellings pass through so a new platform reads as its own name, not as
/// another platform's binary.
pub fn normalize_arch(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "aarch64" | "arm64" => "aarch64".into(),
        "x86_64" | "amd64" => "x86_64".into(),
        other => other.into(),
    }
}

/// `uname -s` spelling → Rust's (`std::env::consts::OS`).
pub fn normalize_os(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "darwin" => "macos".into(),
        other => other.into(),
    }
}

impl Ssh {
    /// Ask a machine what it already has.
    pub fn probe(&self) -> Probe {
        let script = format!(
            r#"echo "hostname=$(hostname 2>/dev/null || echo unknown)"
echo "os=$(uname -s 2>/dev/null || echo unknown)"
echo "arch=$(uname -m 2>/dev/null || echo unknown)"
echo "nproc=$(nproc 2>/dev/null || echo 0)"
echo "mem_mb=$(awk '/MemTotal/{{printf "%d", $2/1024}}' /proc/meminfo 2>/dev/null || echo 0)"
echo "disk_mb=$(df -Pm "$HOME" 2>/dev/null | awk 'NR==2{{print $4}}' || echo 0)"
if [ -x "$HOME/{dir}/bm-agent" ]; then
  echo "agent=$("$HOME/{dir}/bm-agent" --version 2>/dev/null | awk '{{print $NF}}' || echo unknown)"
else
  echo "agent=absent"
fi
if [ -x "$HOME/{dir}/python/.venv/bin/python" ]; then
  echo "python=present"
else
  echo "python=absent"
fi
if [ -x "$HOME/{dir}/bm-tts" ]; then
  echo "tts_bin=present"
else
  echo "tts_bin=absent"
fi
if [ -f "$HOME/{dir}/models/manifest.json" ]; then
  echo "models=present"
else
  echo "models=absent"
fi
# Whichever layout is here. The Rust bake puts the store beside the weights; the
# Python one keeps it inside the venv, so a rebuild vaporizes it.
STORE="$HOME/{dir}/models/voices.json"
[ -f "$STORE" ] || STORE=$(ls $HOME/{dir}/python/.venv/lib/*/site-packages/vieneu/assets/voices_v3_turbo.json 2>/dev/null | head -n 1)
echo "voices=$([ -n "$STORE" ] && [ -f "$STORE" ] && python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(chr(31).join(d.get('presets', d).keys()))" "$STORE" 2>/dev/null)"
if [ -f "$HOME/{dir}/.provision_stamp.json" ]; then
  echo "stamp=$(tr '\n' ' ' < "$HOME/{dir}/.provision_stamp.json" 2>/dev/null)"
fi
if command -v curl >/dev/null 2>&1 && curl -s --max-time 3 http://127.0.0.1:{port}/health >/dev/null 2>&1; then
  echo "tts=up"
else
  echo "tts=down"
fi
echo "probe=done"
"#,
            dir = REMOTE_DIR,
            port = TTS_PORT
        );

        let mut probe = Probe::default();
        match self.run(&script, 30) {
            Ok((code, stdout, stderr)) => {
                if code != 0 {
                    probe.note = format!(
                        "ssh exit {code}: {}",
                        crate::util::head_chars(stderr.trim(), 160)
                    );
                    return probe;
                }
                probe.reachable = true;
                for line in stdout.lines() {
                    let Some((k, v)) = line.split_once('=') else {
                        continue;
                    };
                    let v = v.trim();
                    match k.trim() {
                        "hostname" => probe.hostname = v.to_string(),
                        "os" => probe.os = normalize_os(v),
                        "arch" => probe.arch = normalize_arch(v),
                        "nproc" => probe.nproc = v.parse().unwrap_or(0),
                        "mem_mb" => probe.mem_mb = v.parse().unwrap_or(0),
                        "disk_mb" => probe.disk_free_mb = v.parse().unwrap_or(0),
                        "agent" if v != "absent" => probe.agent_version = Some(v.to_string()),
                        "python" => probe.python_present = v == "present",
                        "tts_bin" => probe.tts_bin_present = v == "present",
                        "models" => probe.models_present = v == "present",
                        "voices" => {
                            // Names contain spaces ("Minh Triết") — the probe
                            // joins them with \x1f, never whitespace.
                            probe.voices = v
                                .split('\u{1f}')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect()
                        }
                        "stamp" => {
                            probe.stamp = parse_stamp(v);
                        }
                        "tts" => probe.tts_up = v == "up",
                        _ => {}
                    }
                }
                probe.note = "ok".into();
            }
            Err(e) => probe.note = format!("{e}"),
        }
        probe
    }

    /// Create the worker root skeleton.
    pub fn ensure_root(&self) -> Result<()> {
        let script = format!(
            "mkdir -p $HOME/{d}/models $HOME/{d}/prompts $HOME/{d}/assets/effects \
             $HOME/{d}/assets/music $HOME/{d}/refs $HOME/{d}/data/chapters \
             $HOME/{d}/data/audio $HOME/{d}/output && echo READY",
            d = REMOTE_DIR
        );
        let (code, stdout, stderr) = self.run(&script, 30)?;
        if code != 0 || !stdout.contains("READY") {
            anyhow::bail!("ensure_root failed (exit {code}): {}", stderr.trim());
        }
        Ok(())
    }

    /// Install or upgrade the agent binary, then verify it runs.
    pub fn install_agent(&self, agent_binary: &Path) -> Result<String> {
        self.rsync_push(agent_binary, "bm-agent", false)?;
        let script = format!(
            "chmod +x $HOME/{d}/bm-agent && $HOME/{d}/bm-agent --version",
            d = REMOTE_DIR
        );
        let (code, stdout, _stderr) = self.run(&script, 30)?;
        if code != 0 {
            anyhow::bail!("installed agent would not run (exit {code})");
        }
        Ok(stdout.trim().to_string())
    }

    /// Push the source files the worker needs (never the inductor's state).
    ///
    /// Two halves, from two different places. `prompts`, `assets` and `refs`
    /// are profile content and live at the root. The cast files are the
    /// *book's* — `data/` is in the active workspace — so they are read
    /// through the layout; naming them root-relative shipped no cast at all
    /// the moment a workspace was selected, and the worker then rendered with
    /// the catalogue's default voices.
    pub fn install_sources(&self, layout: &crate::Layout) -> Result<()> {
        for rel in ["prompts", "assets", "refs"] {
            let src = layout.root.join(rel);
            if src.exists() {
                self.rsync_push(&src, rel, true)?;
            }
        }
        for (engine, rel) in [
            ("vieneu", "data/cast-vieneu.json"),
            ("gemini", "data/cast.json"),
        ] {
            let src = layout.cast(engine);
            if src.exists() {
                self.rsync_push(&src, rel, false)?;
            }
        }
        // `python/` used to be pushed here. It is not any more: the sidecar is
        // `bm-tts`, and the one thing a worker still needed Python for —
        // enrolling a clone — now happens on the inductor, whose store travels
        // inside `models/voices.json`.
        //
        // Voice assignments travel with sources (additive only — a worker's
        // segment cache is keyed by voice, so clobbering mid-render would
        // strand it; the voices op is the writer, this is just transport).
        // The clone manifest travels too: nothing on a worker reads it yet,
        // but the inductor's warnings are computed against it, so a box
        // holding a different declaration is a silent desync.
        let manifest = layout.root.join("voices.json");
        if manifest.exists() {
            self.rsync_push(&manifest, "voices.json", false)?;
        }
        Ok(())
    }

    /// Push the TTS sidecar binary and the shared ONNX Runtime it links.
    ///
    /// Replaces `ensure_python`, which built a 647 MB virtualenv on every
    /// worker. Two files and a symlink do the same job now.
    ///
    /// The library is pushed under **every** name it is known by — the linker
    /// wants the plain `libonnxruntime.so`, the loader wants the SONAME
    /// `libonnxruntime.so.1`, and the versioned file is what those two point at.
    /// Shipping only one of them produces a failure that names none of this.
    ///
    /// `runtime_dir` is `None` where the sidecar is self-contained (macOS
    /// links its runtime statically — the native binary runs with no `.so`
    /// beside it), so only the binary travels.
    pub fn install_tts_runtime(
        &self,
        tts_binary: &Path,
        runtime_dir: Option<&Path>,
    ) -> Result<String> {
        self.rsync_push(tts_binary, "bm-tts", false)?;
        if let Some(runtime_dir) = runtime_dir {
            // Whatever the make target staged, rather than a version hardcoded here:
            // the pin lives in the Makefile, and two copies of it would drift.
            let mut libs: Vec<std::path::PathBuf> = std::fs::read_dir(runtime_dir)
                .with_context(|| format!("reading the runtime dir {}", runtime_dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("libonnxruntime.so"))
                })
                .collect();
            libs.sort();
            if libs.is_empty() {
                anyhow::bail!(
                    "no libonnxruntime.so* in {} — run `make runtime` first",
                    runtime_dir.display()
                );
            }
            for lib in &libs {
                let name = lib.file_name().expect("filtered on a file name");
                self.rsync_push(lib, &name.to_string_lossy(), false)?;
            }
        }

        let script = format!(
            r#"set -e
D="$HOME/{d}"
cd "$D"
chmod +x bm-tts
LD_LIBRARY_PATH="$D" ./bm-tts --version >/dev/null 2>&1 || \
  {{ echo "bm-tts would not run — missing libonnxruntime.so.1 beside it?" >&2; exit 7; }}
echo "TTS-RUNTIME-OK ($(LD_LIBRARY_PATH="$D" ./bm-tts --version))"
"#,
            d = REMOTE_DIR
        );
        let (code, stdout, stderr) = self.run(&script, 60)?;
        if code != 0 {
            anyhow::bail!(
                "installing the TTS runtime failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        Ok(stdout.trim().to_string())
    }

    /// Push the baked `models/` directory — the weights the sidecar reads.
    ///
    /// 668 MB, and content-addressed by the stamp's `tts_hash`, so a re-provision
    /// with nothing changed costs one rsync delta rather than a transfer.
    pub fn install_models(&self, root: &Path) -> Result<String> {
        let src = root.join("models");
        if !src.is_dir() {
            anyhow::bail!(
                "no {} — run the bake first (`python3 tools/bake-models.py`)",
                src.display()
            );
        }
        self.rsync_push(&src, "models", true)?;
        let script = format!(
            r#"D="$HOME/{d}/models"
n=$(ls "$D" | wc -l)
[ -f "$D/manifest.json" ] || {{ echo "models/manifest.json missing — incomplete bake" >&2; exit 8; }}
echo "MODELS-OK ($n files)"
"#,
            d = REMOTE_DIR
        );
        let (code, stdout, stderr) = self.run(&script, 60)?;
        if code != 0 {
            anyhow::bail!(
                "installing models failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        Ok(stdout.trim().to_string())
    }

    /// Best-effort opencode install for the digest lane. Auth stays manual
    /// (browser login); without it remote digests fail loudly, never silently.
    pub fn ensure_opencode(&self) -> Result<String> {
        let script = r#"command -v opencode >/dev/null 2>&1 && { echo "OPENCODE-OK (present)"; exit 0; }
command -v npm >/dev/null 2>&1 || { echo "OPENCODE-SKIP (npm missing; install node first)"; exit 0; }
mkdir -p "$HOME/.local"
npm i -g --prefix "$HOME/.local" opencode-ai >/dev/null 2>&1 && echo "OPENCODE-OK (installed)" || echo "OPENCODE-SKIP (npm install failed)""#;
        let (code, stdout, stderr) = self.run(script, 600)?;
        if code != 0 {
            anyhow::bail!("opencode check failed: {}", stderr.trim());
        }
        Ok(stdout.trim().to_string())
    }

    /// Start the TTS sidecar detached, unless it is already answering.
    pub fn start_tts(&self) -> Result<String> {
        let script = format!(
            r#"D="$HOME/{d}"
if curl -s -o /dev/null --max-time 3 http://127.0.0.1:{port}/health >/dev/null 2>&1; then
  echo "TTS-ALREADY-UP"; exit 0
fi
cd "$D" || exit 5
LD_LIBRARY_PATH="$D" nohup "$D/bm-tts" --models models --codec models \
  --dict models/sea_g2p.bin --voices models/voices.json \
  --port {port} --bind 0.0.0.0 > "$D/tts.log" 2>&1 &
echo $! > "$D/tts.pid"
sleep 3
curl -s -o /dev/null --max-time 10 http://127.0.0.1:{port}/health >/dev/null 2>&1 && echo "TTS-STARTED" || echo "TTS-STARTING (model loading)"
"#,
            d = REMOTE_DIR,
            port = TTS_PORT
        );
        let (code, stdout, stderr) = self.run(&script, 60)?;
        if code != 0 {
            anyhow::bail!(
                "starting TTS failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 200)
            );
        }
        Ok(stdout.trim().to_string())
    }

    pub fn stop_tts(&self) -> Result<()> {
        let script = format!(
            // `pkill -x`, never `-f`: the pattern for a command-line match is
            // contained in the ssh wrapper's own argv, so `-f` kills the wrapper
            // running this script. `-x` matches the process name only.
            r#"if [ -f "$HOME/{d}/tts.pid" ]; then kill "$(cat "$HOME/{d}/tts.pid")" 2>/dev/null || true; rm -f "$HOME/{d}/tts.pid"; fi
pkill -x bm-tts 2>/dev/null || true
echo stopped"#,
            d = REMOTE_DIR
        );
        let _ = self.run(&script, 30)?;
        Ok(())
    }

    /// The stamp this box wrote at the end of its last provision, if any.
    ///
    /// `probe` already reads it inside its single ssh round trip, so provisioning
    /// never pays for a second connection. This exists for callers that want the
    /// stamp *without* a full probe (and for the tests that pin the round-trip
    /// behaviour): a truncated or absent file reads as `None`, never as an error.
    pub fn read_provision_stamp(&self) -> Option<ProvisionStamp> {
        let script = format!("cat \"$HOME/{d}/.provision_stamp.json\"", d = REMOTE_DIR);
        match self.run(&script, 10) {
            Ok((0, stdout, _)) => parse_stamp(&stdout),
            _ => None,
        }
    }

    /// Write the load pointer the worker's agent gate checks at startup.
    ///
    /// The profile content already travels inside `install_sources`
    /// (prompts + assets ride the sources sync), so this is one small JSON
    /// file — but without it the worker cannot tell a complete profile from
    /// a half-rsynced one, which is exactly what `verify` refuses to run on.
    pub fn write_profile_pointer(&self, pointer: &crate::profile::Pointer) -> Result<()> {
        let json = serde_json::to_string_pretty(pointer)?;
        let script = format!(
            "mkdir -p $HOME/{d}/.bm && cat > $HOME/{d}/.bm/profile << 'EOF'\n{json}\nEOF\n",
            d = REMOTE_DIR
        );
        let (code, _, stderr) = self.run(&script, 10)?;
        if code != 0 {
            anyhow::bail!("failed to write profile pointer: {}", stderr.trim());
        }
        Ok(())
    }

    pub fn write_provision_stamp(&self, stamp: &ProvisionStamp) -> Result<()> {
        let json = serde_json::to_string(stamp)?;
        let script = format!(
            "mkdir -p $HOME/{d} && cat > $HOME/{d}/.provision_stamp.json << 'EOF'\n{json}\nEOF\n",
            d = REMOTE_DIR
        );
        let (code, _, stderr) = self.run(&script, 10)?;
        if code != 0 {
            anyhow::bail!("failed to write provision stamp: {}", stderr.trim());
        }
        Ok(())
    }
}

/// Voices a box's store holds that are declared nowhere: not a shipped
/// preset, not in the clone manifest, not a pool sample. Assigning one works
/// on that box and 500s everywhere else (the Suneo outage: hand-enrolled
/// locally, offered by the picker, unknown to every other worker).
///
/// Reported, never deleted: erasing a voice the cast uses would break renders.
/// The fix is named in the warning — declare it in `voices.json` (with its
/// `refs/` clip) or drop it from the store.
pub fn undeclared_voices(
    store: &[String],
    manifest: &std::collections::HashMap<String, String>,
    pool: &crate::pool::Pool,
    catalogue: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = store
        .iter()
        .filter(|v| {
            !v.starts_with('_')
                && !manifest.contains_key(v.as_str())
                && !pool.contains_key(v.as_str())
                && !catalogue.iter().any(|c| c == *v)
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Full onboarding for one machine: probe, then push only what is missing.
///
/// Returns the log lines the TUI should show, in order.
#[allow(clippy::too_many_arguments)]
pub fn provision(
    m: &Machine,
    layout: &crate::Layout,
    agent_binary: &Path,
    tts_binary: &Path,
    tts_runtime: Option<&Path>,
    agent_version: &str,
    force: bool,
    initial_probe: Option<Probe>,
) -> (Probe, Vec<String>) {
    let ssh = Ssh::for_machine(m);
    let mut log = Vec::new();

    let probe = match initial_probe {
        Some(p) => {
            log.push(format!("[{}] {}", m.id, p.summary()));
            p
        }
        None => {
            log.push(format!("[{}] probing {}", m.id, ssh.target));
            let p = ssh.probe();
            log.push(format!("[{}] {}", m.id, p.summary()));
            p
        }
    };
    if !probe.reachable {
        log.push(format!("[{}] unreachable — aborting provision", m.id));
        return (probe, log);
    }

    let local_stamp = compute_provision_stamp(&layout.root, agent_version);
    let remote_stamp = probe.stamp.as_ref();

    let sources_match = !force
        && remote_stamp
            .map(|s| s.sources_in_sync(&local_stamp))
            .unwrap_or(false);
    // The voice store now travels inside `models/`, so `tts_hash` covers it and
    // there is no separate voices check.
    let models_match = !force
        && remote_stamp
            .map(|s| s.tts_in_sync(&local_stamp))
            .unwrap_or(false);

    if probe.configured(agent_version) && !force {
        log.push(format!(
            "[{}] already configured (agent {} + tts sidecar)",
            m.id, agent_version
        ));
        if sources_match {
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            // Sources still sync: cast/asset/prompt updates must reach workers
            // without a venv rebuild. Cheap rsync deltas when nothing changed.
            match ssh.install_sources(layout) {
                Ok(()) => log.push(format!("[{}] sources in sync", m.id)),
                Err(e) => log.push(format!("[{}] source sync failed: {e}", m.id)),
            }
        }
    } else {
        if let Err(e) = ssh.ensure_root() {
            log.push(format!("[{}] ensure_root failed: {e}", m.id));
            return (probe, log);
        }
        log.push(format!("[{}] worker root ready (~/{REMOTE_DIR})", m.id));

        match ssh.install_agent(agent_binary) {
            Ok(v) => log.push(format!("[{}] agent installed, reports version {v}", m.id)),
            Err(e) => {
                log.push(format!("[{}] agent install failed: {e}", m.id));
                return (probe, log);
            }
        }

        if sources_match {
            log.push(format!(
                "[{}] prompts/assets/refs in sync (cache match)",
                m.id
            ));
        } else {
            match ssh.install_sources(layout) {
                Ok(()) => log.push(format!("[{}] prompts/assets/refs distributed", m.id)),
                Err(e) => log.push(format!("[{}] source distribution failed: {e}", m.id)),
            }
        }

        if !probe.tts_bin_present || force {
            log.push(format!(
                "[{}] installing the TTS sidecar binary + runtime",
                m.id
            ));
            match ssh.install_tts_runtime(tts_binary, tts_runtime) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log);
                }
            }
        } else {
            log.push(format!(
                "[{}] TTS sidecar binary already present — skipped",
                m.id
            ));
        }

        // 668 MB, and the reason `tts_hash` exists: a re-provision with nothing
        // changed must not re-send it.
        if models_match {
            log.push(format!("[{}] models in sync (cache match)", m.id));
        } else {
            log.push(format!("[{}] pushing models (~668 MB)", m.id));
            match ssh.install_models(&layout.root) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log);
                }
            }
        }
    }

    // The worker's agent gate checks this pointer at startup: sources above
    // carry the profile content, the pointer says what it claims to be.
    // Written every provision (one small file) so a re-pointed inductor
    // cannot leave a worker verifying yesterday's profile.
    match crate::profile::read_pointer(&layout.root) {
        Ok(pointer) => match ssh.write_profile_pointer(&pointer) {
            Ok(()) => log.push(format!(
                "[{}] profile pointer: {} ({})",
                m.id,
                pointer.name,
                &pointer.hash[..12.min(pointer.hash.len())]
            )),
            Err(e) => log.push(format!("[{}] profile pointer failed: {e}", m.id)),
        },
        Err(_) => log.push(format!(
            "[{}] no local profile pointer — `profile.sh unpack <name>` first, or this worker will refuse to start",
            m.id
        )),
    }

    // Enrollment moved off the worker — it needs the encoder, which is not on a
    // worker any more. A clone declared in `voices.json` but absent from the
    // pushed store therefore cannot render anywhere, and the old flow would
    // have quietly enrolled it on first use. Say so instead.
    {
        let manifest: std::collections::HashMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(layout.root.join("voices.json")).unwrap_or_default(),
        )
        .unwrap_or_default();
        if !manifest.is_empty() {
            let store: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(layout.root.join("models/voices.json"))
                    .unwrap_or_default(),
            )
            .unwrap_or(serde_json::Value::Null);
            let presets = store.get("presets").and_then(|v| v.as_object());
            let missing: Vec<&str> = manifest
                .keys()
                .filter(|n| !n.starts_with('_'))
                .filter(|n| presets.is_none_or(|p| !p.contains_key(*n)))
                .map(|s| s.as_str())
                .collect();
            if !missing.is_empty() {
                log.push(format!(
                    "[{}] declared in voices.json but missing from models/voices.json: {} — enroll on this machine and re-bake, or a render naming one will fail here",
                    m.id,
                    missing.join(", ")
                ));
            }
        }
    }

    match ssh.ensure_opencode() {
        Ok(v) => log.push(format!("[{}] {v}", m.id)),
        Err(e) => log.push(format!("[{}] opencode check failed: {e}", m.id)),
    }

    match ssh.start_tts() {
        Ok(v) => log.push(format!("[{}] tts: {v}", m.id)),
        Err(e) => log.push(format!("[{}] could not start tts: {e}", m.id)),
    }

    // Write provision stamp so subsequent runs can skip
    let _ = ssh.write_provision_stamp(&local_stamp);

    // Re-probe so the caller records the post-provision truth.
    let after = ssh.probe();
    log.push(format!("[{}] after provision: {}", m.id, after.summary()));
    // Single-source-of-truth check, against the fresh probe: a store holding
    // voices nothing declares desyncs the cluster silently (renders pass here,
    // 500 everywhere else). Warn with the fix; never delete.
    {
        let engine = crate::config::Settings::load(&layout.settings()).engine;
        let manifest: std::collections::HashMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(layout.root.join("voices.json")).unwrap_or_default(),
        )
        .unwrap_or_default();
        let pool = crate::pool::load_pool(&layout.root.join("voice-pool.json"));
        let catalogue: Vec<String> = crate::voices::offline_voices(&engine)
            .iter()
            .map(|v| v.name.clone())
            .collect();
        let strays = undeclared_voices(&after.voices, &manifest, &pool, &catalogue);
        if !strays.is_empty() {
            log.push(format!(
                "[{}] voices in store but declared nowhere (not a preset, not in voices.json, not pooled): {} — add each with its refs/ clip to voices.json and provision again, or drop it from the store; remote renders 500 until then",
                m.id,
                strays.join(", ")
            ));
        }
    }
    (after, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_requires_a_matching_agent_and_the_tts_sidecar() {
        let mut p = Probe {
            reachable: true,
            agent_version: Some("0.2.0".into()),
            tts_bin_present: true,
            models_present: true,
            ..Default::default()
        };
        assert!(p.configured("0.2.0"));
        assert!(
            !p.configured("0.3.0"),
            "stale agent must trigger a redeploy"
        );
        p.reachable = false;
        assert!(!p.configured("0.2.0"));

        // A Python virtualenv is no longer a reason to call a box ready. This
        // is the assertion that changed: the field is still probed and
        // reported, but it decides nothing.
        p.reachable = true;
        p.tts_bin_present = false;
        p.models_present = false;
        p.python_present = true;
        assert!(!p.configured("0.2.0"), "a venv cannot render");
    }

    #[test]
    fn undeclared_voices_names_only_the_strays() {
        // The Suneo outage in one assertion: a hand-enrolled clone the
        // manifest never learned must be reported; presets, manifest clones
        // and pool samples must not be.
        let manifest: std::collections::HashMap<String, String> =
            [("Học Trò".to_string(), "refs/hoc-tro.mp3".to_string())]
                .into_iter()
                .collect();
        let mut pool = crate::pool::Pool::new();
        pool.insert(
            "Pool Sample".to_string(),
            crate::pool::PoolEntry {
                file: "refs/pool.wav".into(),
                tags: vec![],
            },
        );
        let catalogue = vec!["Thái Sơn".to_string(), "Adam".to_string()];
        let store = vec![
            "Thái Sơn".to_string(),
            "Học Trò".to_string(),
            "Pool Sample".to_string(),
            "Suneo".to_string(),
            "Suneo".to_string(),
            "_note".to_string(),
        ];
        assert_eq!(
            undeclared_voices(&store, &manifest, &pool, &catalogue),
            vec!["Suneo"]
        );
        assert!(undeclared_voices(&[], &manifest, &pool, &catalogue).is_empty());
    }

    #[test]
    fn probe_summary_counts_voices_instead_of_listing_them() {
        // Fifty enrolled names wrapped the Logs pane for screens. The count
        // carries the signal; which voices enrolled rides the enroll lines.
        let p = Probe {
            reachable: true,
            hostname: "box".into(),
            voices: vec!["Adam".into(), "Suneo".into()],
            ..Default::default()
        };
        let s = p.summary();
        assert!(s.contains("2 voices"), "{s}");
        assert!(!s.contains("Suneo"), "names stay out of the summary: {s}");
    }

    #[test]
    fn probe_normalizes_uname_spellings_to_rust_platforms() {
        // `arm64` (macOS) and `aarch64` (Linux) are the same chip; `Darwin`
        // is Rust's `macos`. Anything unknown passes through so a new
        // platform reads as its own name, never as another platform's binary.
        assert_eq!(super::normalize_arch("arm64"), "aarch64");
        assert_eq!(super::normalize_arch("aarch64"), "aarch64");
        assert_eq!(super::normalize_arch("amd64"), "x86_64");
        assert_eq!(super::normalize_arch("riscv64"), "riscv64");
        assert_eq!(super::normalize_os("Darwin"), "macos");
        assert_eq!(super::normalize_os("Linux"), "linux");
        let p = Probe {
            reachable: true,
            hostname: "mac".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            ..Default::default()
        };
        assert!(p.summary().contains("macos/aarch64"), "{}", p.summary());
    }

    #[test]
    fn unreachable_probe_explains_itself() {
        let p = Probe {
            reachable: false,
            note: "ssh exit 255".into(),
            ..Default::default()
        };
        assert!(p.summary().contains("unreachable"));
        assert!(p.summary().contains("ssh exit 255"));
    }

    /// The sidecar needs *both* the binary and its weights — a binary with no
    /// models cannot render, and reads as ready right up until the first task.
    #[test]
    fn the_tts_sidecar_needs_the_binary_and_the_weights() {
        let mut p = Probe {
            reachable: true,
            agent_version: Some("0.2.0".into()),
            ..Default::default()
        };
        assert!(!p.configured("0.2.0"), "nothing installed is not ready");
        assert_eq!(p.sidecar(), "none");

        p.tts_bin_present = true;
        assert!(!p.rust_ready(), "a binary with no models cannot render");
        assert!(!p.configured("0.2.0"));

        p.models_present = true;
        assert!(p.configured("0.2.0"));
        assert_eq!(p.sidecar(), "rust");

        assert!(!p.configured("0.3.0"), "a stale agent is never configured");
    }

    /// The two flags are independent, so a box can report both without either
    /// implying the other.
    #[test]
    fn both_sidecars_can_be_present_at_once() {
        let p = Probe {
            reachable: true,
            agent_version: Some("0.2.0".into()),
            python_present: true,
            tts_bin_present: true,
            models_present: true,
            ..Default::default()
        };
        assert!(p.configured("0.2.0"));
        // Rust wins the summary when it is ready, because that is what a switch
        // would put in charge.
        assert_eq!(p.sidecar(), "rust");
    }
}
