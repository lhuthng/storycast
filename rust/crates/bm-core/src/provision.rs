//! Machine onboarding: probe, distribute, verify.
//!
//! New in the cluster version — the legacy `swarm` hardcoded one box in three
//! separate places. Here any machine is reachable by address, and the
//! provisioner answers the question that matters before doing any work:
//! *is this box already configured, or do we have to push to it?*
//!
//! Everything shells out to `ssh` and `rsync` rather than linking an SSH
//! library. That keeps the build small, reuses the user's existing keys and
//! `~/.ssh/config`, and makes the exact command visible in the TUI log.

use anyhow::{Context, Result};
use bm_proto::Machine;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// Directory under the remote `$HOME` that holds a worker's whole world.
pub const REMOTE_DIR: &str = "bm-worker";

/// Port the Python TTS sidecar listens on.
pub const TTS_PORT: u16 = 8818;

/// What a probe found on a machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    pub reachable: bool,
    pub hostname: String,
    pub nproc: u32,
    pub mem_mb: u64,
    pub disk_free_mb: u64,
    pub arch: String,
    /// Version string reported by the installed agent, if any.
    pub agent_version: Option<String>,
    /// A usable Python interpreter with the TTS deps installed.
    pub python_present: bool,
    /// Enrolled clone-voice names parsed from the voice store (no model load).
    #[serde(default)]
    pub voices: Vec<String>,
    pub tts_up: bool,
    pub note: String,
}

impl Probe {
    /// A machine is ready to take work when it is reachable, runs the exact
    /// agent build we are scheduling with, and has the TTS sidecar's Python.
    pub fn configured(&self, want_version: &str) -> bool {
        self.reachable
            && self.agent_version.as_deref() == Some(want_version)
            && self.python_present
    }

    /// One-line summary for the TUI machine pane.
    pub fn summary(&self) -> String {
        if !self.reachable {
            return format!("unreachable: {}", self.note);
        }
        format!(
            "{} · {} cpu · {} MB ram · {} · agent={} · python={} · voices={} · tts={}",
            self.hostname,
            self.nproc,
            self.mem_mb,
            self.arch,
            self.agent_version.as_deref().unwrap_or("absent"),
            if self.python_present { "yes" } else { "no" },
            if self.voices.is_empty() { "-".into() } else { self.voices.join(",") },
            if self.tts_up { "up" } else { "down" },
        )
    }
}

/// A resolved SSH connection to one machine.
pub struct Ssh {
    pub target: String,
    pub port: u16,
    pub key: Option<String>,
    /// The inductor's own machine: run commands directly instead of ssh-ing out.
    pub local: bool,
}

impl Ssh {
    pub fn for_machine(m: &Machine) -> Self {
        let local = matches!(m.addr.as_str(), "127.0.0.1" | "localhost" | "::1");
        Ssh {
            target: m.ssh_target(),
            port: m.ssh_port,
            key: m.ssh_key.clone(),
            local,
        }
    }

    fn ssh_args(&self) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
        ];
        if self.port != 22 {
            args.push("-p".into());
            args.push(self.port.to_string());
        }
        if let Some(key) = &self.key {
            args.push("-i".into());
            args.push(key.clone());
        }
        args.push(self.target.clone());
        args
    }

    /// Run a shell script on the machine. Returns `(exit_code, stdout, stderr)`.
    ///
    /// A transport failure is reported as exit code 255 (ssh's own convention)
    /// so callers can distinguish "box is down" from "the command failed".
    pub fn run(&self, script: &str, timeout_secs: u64) -> Result<(i32, String, String)> {
        let full = format!(
            "export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH\n{script}"
        );
        let mut cmd = if self.local {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&full);
            c
        } else {
            let mut c = Command::new("ssh");
            c.args(self.ssh_args()).arg(&full);
            c
        };
        let _ = timeout_secs; // ssh's own ConnectTimeout bounds the connect phase
        let out = cmd
            .output()
            .with_context(|| format!("spawning {} for {}", if self.local { "sh" } else { "ssh" }, self.target))?;
        Ok((
            out.status.code().unwrap_or(255),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ))
    }

    /// Ask a machine what it already has.
    pub fn probe(&self) -> Probe {
        let script = format!(
            r#"echo "hostname=$(hostname 2>/dev/null || echo unknown)"
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
echo "voices=$(for f in $HOME/{dir}/python/.venv/lib/*/site-packages/vieneu/assets/voices_v3_turbo.json; do [ -f "$f" ] && python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(chr(31).join(d.get('presets', d).keys()))" "$f"; done 2>/dev/null)"
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
                        "arch" => probe.arch = v.to_string(),
                        "nproc" => probe.nproc = v.parse().unwrap_or(0),
                        "mem_mb" => probe.mem_mb = v.parse().unwrap_or(0),
                        "disk_mb" => probe.disk_free_mb = v.parse().unwrap_or(0),
                        "agent" if v != "absent" => probe.agent_version = Some(v.to_string()),
                        "python" => probe.python_present = v == "present",
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

    fn rsync_e(&self) -> String {
        let mut e = format!("ssh -o BatchMode=yes -o ConnectTimeout=10 -p {}", self.port);
        if let Some(key) = &self.key {
            e.push_str(&format!(" -i {key}"));
        }
        e
    }

    /// Push a local path into the machine's worker root.
    pub fn rsync_push(&self, src: &Path, remote_rel: &str, delete: bool) -> Result<()> {
        if self.local {
            return self.rsync_push_local(src, remote_rel);
        }
        let dst = format!("{}:{}/{remote_rel}", self.target, REMOTE_DIR);
        let mut args: Vec<String> = vec!["-az".into(), "--no-perms".into()];
        if delete {
            args.push("--delete".into());
        }
        args.push("-e".into());
        args.push(self.rsync_e());
        // Directories sync their CONTENTS (trailing slash). Without it rsync
        // nests: bm-worker/assets/assets — the exact bug this comment prevents.
        let mut src_s = src.to_string_lossy().to_string();
        if src.is_dir() && !src_s.ends_with('/') {
            src_s.push('/');
        }
        args.push(src_s);
        args.push(dst);
        let out = Command::new("rsync")
            .args(&args)
            .output()
            .context("spawning rsync")?;
        if !out.status.success() {
            anyhow::bail!(
                "rsync push failed: {}",
                crate::util::head_chars(&String::from_utf8_lossy(&out.stderr), 300)
            );
        }
        Ok(())
    }

    /// Local shortcut: copy inside the inductor's own `~/{REMOTE_DIR}`.
    fn rsync_push_local(&self, src: &Path, remote_rel: &str) -> Result<()> {
        let home = std::env::var("HOME").context("HOME not set")?;
        let dst = Path::new(&home).join(REMOTE_DIR).join(remote_rel);
        if src.is_dir() {
            copy_dir(src, &dst)?;
        } else {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src, &dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
        }
        Ok(())
    }

    /// Pull a path from the machine's worker root into a local destination.
    pub fn rsync_pull(&self, remote_rel: &str, dst: &Path) -> Result<()> {
        if self.local {
            let home = std::env::var("HOME").context("HOME not set")?;
            let src = Path::new(&home).join(REMOTE_DIR).join(remote_rel);
            if src.is_dir() {
                return copy_dir(&src, dst);
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, dst)
                .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
            return Ok(());
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let src = format!("{}:{}/{remote_rel}", self.target, REMOTE_DIR);
        let out = Command::new("rsync")
            .args([
                "-az",
                "--no-perms",
                "-e",
                &self.rsync_e(),
                &src,
                &dst.to_string_lossy(),
            ])
            .output()
            .context("spawning rsync")?;
        if !out.status.success() {
            anyhow::bail!(
                "rsync pull failed: {}",
                crate::util::head_chars(&String::from_utf8_lossy(&out.stderr), 300)
            );
        }
        Ok(())
    }

    /// Create the worker root skeleton.
    pub fn ensure_root(&self) -> Result<()> {
        let script = format!(
            "mkdir -p $HOME/{d}/python $HOME/{d}/prompts $HOME/{d}/assets/ambience \
             $HOME/{d}/refs $HOME/{d}/data/chapters $HOME/{d}/data/audio $HOME/{d}/output \
             && echo READY",
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
        let script = format!("chmod +x $HOME/{d}/bm-agent && $HOME/{d}/bm-agent --version", d = REMOTE_DIR);
        let (code, stdout, _stderr) = self.run(&script, 30)?;
        if code != 0 {
            anyhow::bail!("installed agent would not run (exit {code})");
        }
        Ok(stdout.trim().to_string())
    }

    /// Push the source files the worker needs (never the inductor's state).
    pub fn install_sources(&self, repo_root: &Path) -> Result<()> {
        for rel in ["prompts", "assets", "refs"] {
            let src = repo_root.join(rel);
            if src.exists() {
                self.rsync_push(&src, rel, true)?;
            }
        }
        let py = repo_root.join("python");
        if py.exists() {
            self.rsync_push(&py, "python", false)?;
        }
        // Voice assignments travel with sources (additive only — a worker's
        // segment cache is keyed by voice, so clobbering mid-render would
        // strand it; the voices op is the writer, this is just transport).
        for rel in ["data/cast-vieneu.json", "data/cast.json"] {
            let src = repo_root.join(rel);
            if src.exists() {
                self.rsync_push(&src, rel, false)?;
            }
        }
        Ok(())
    }

    /// Create the TTS virtualenv and install the sidecar's dependencies.
    /// Only runs when the probe says the interpreter is missing, because the
    /// VieNeu weights are ~1.7 GB and this is the slow part of onboarding.
    pub fn ensure_python(&self, force: bool) -> Result<String> {
        let script = format!(
            r#"set -e
D="$HOME/{d}/python"
if [ {force} -eq 0 ] && [ -x "$D/.venv/bin/python" ]; then
  echo "PYTHON-OK (existing venv)"
  exit 0
fi
command -v python3 >/dev/null 2>&1 || {{ echo "python3 not found on this machine" >&2; exit 4; }}
python3 -m venv "$D/.venv"
"$D/.venv/bin/python" -m pip install -q --upgrade pip
"$D/.venv/bin/python" -m pip install -q -r "$D/requirements.txt"
echo "PYTHON-OK (fresh venv)"
"#,
            d = REMOTE_DIR,
            force = if force { 1 } else { 0 }
        );
        let (code, stdout, stderr) = self.run(&script, 3600)?;
        if code != 0 {
            anyhow::bail!(
                "python provisioning failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        Ok(stdout.trim().to_string())
    }

    /// Enroll the clone voices from `voices.json`. The voice store lives
    /// inside the venv, so a venv rebuild vaporizes it — this step re-enrolls
    /// whatever the probe did not find. Skips the model load entirely when
    /// everything is already enrolled.
    pub fn ensure_voices(&self, repo_root: &Path) -> Result<String> {
        let manifest_src = repo_root.join("voices.json");
        let manifest: std::collections::HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&manifest_src).with_context(|| {
                format!("reading {}", manifest_src.display())
            })?)
            .context("parsing voices.json (name -> refs/*.wav)")?;
        self.rsync_push(&repo_root.join("refs"), "refs", false)?;
        self.rsync_push(&manifest_src, "voices.json", false)?;
        let want: Vec<String> = {
            let mut w: Vec<String> = manifest
                .keys()
                .filter(|k| !k.starts_with('_')) // skip "_note" metadata keys
                .cloned()
                .collect();
            w.sort();
            w
        };
        let script = format!(
            r#"set -e
D="$HOME/{d}"
V="$D/python/.venv/bin/python"
STORE=$(ls $D/python/.venv/lib/*/site-packages/vieneu/assets/voices_v3_turbo.json 2>/dev/null | head -n 1)
[ -n "$STORE" ] || {{ echo "voice store not found (venv broken?)" >&2; exit 6; }}
HAVE=$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(chr(31).join(d.get('presets', d).keys()))" "$STORE")
HAVE_SP=" $(echo "$HAVE" | tr '\037' ' ') "
MISSING=""
for name in {want}; do
  case "$HAVE_SP" in *" $name "*) ;; *) MISSING="$MISSING $name";; esac
done
MISSING=$(echo "$MISSING" | sed 's/^ *//')
if [ -z "$MISSING" ]; then echo "VOICES-OK (already enrolled)"; exit 0; fi
echo "enrolling:$MISSING"
cd "$D"
PYTHONPATH="$D/python" "$V" -c "
import json
import tts_vieneu as vn
manifest = json.load(open('voices.json'))
tts = vn.engine()
for line in '''$MISSING'''.split():
    name = line.strip()
    if not name:
        continue
    tts.add_voice(name, manifest[name])
    print('enrolled', name, flush=True)
tts.save_voices()
"
echo "VOICES-OK (enrolled:$MISSING)"
"#,
            d = REMOTE_DIR,
            want = want.join(" ")
        );
        let (code, stdout, stderr) = self.run(&script, 1800)?;
        if code != 0 {
            anyhow::bail!(
                "voice enrollment failed (exit {code}): {}",
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
            r#"D="$HOME/{d}/python"
if curl -s -o /dev/null --max-time 3 http://127.0.0.1:{port}/health >/dev/null 2>&1; then
  echo "TTS-ALREADY-UP"; exit 0
fi
cd "$D" || exit 5
nohup "$D/.venv/bin/python" tts_server.py --port {port} --bind 0.0.0.0 > "$HOME/{d}/tts.log" 2>&1 &
echo $! > "$HOME/{d}/tts.pid"
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
            r#"if [ -f "$HOME/{d}/tts.pid" ]; then kill "$(cat "$HOME/{d}/tts.pid")" 2>/dev/null || true; rm -f "$HOME/{d}/tts.pid"; fi
pkill -f "tts_server.py" 2>/dev/null || true
echo stopped"#,
            d = REMOTE_DIR
        );
        let _ = self.run(&script, 30)?;
        Ok(())
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            // never copy a virtualenv or build cache between machines
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".venv" | "__pycache__" | "target") {
                continue;
            }
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Full onboarding for one machine: probe, then push only what is missing.
///
/// Returns the log lines the TUI should show, in order.
pub fn provision(
    m: &Machine,
    repo_root: &Path,
    agent_binary: &Path,
    agent_version: &str,
    force: bool,
) -> (Probe, Vec<String>) {
    let ssh = Ssh::for_machine(m);
    let mut log = Vec::new();

    log.push(format!("[{}] probing {}", m.id, ssh.target));
    let probe = ssh.probe();
    log.push(format!("[{}] {}", m.id, probe.summary()));
    if !probe.reachable {
        log.push(format!("[{}] unreachable — aborting provision", m.id));
        return (probe, log);
    }
    if probe.configured(agent_version) && !force {
        log.push(format!(
            "[{}] already configured (agent {} + python) — syncing sources only",
            m.id, agent_version
        ));
        // Sources still sync: cast/asset/prompt updates must reach workers
        // without a venv rebuild. Cheap rsync deltas when nothing changed.
        match ssh.install_sources(repo_root) {
            Ok(()) => log.push(format!("[{}] sources in sync", m.id)),
            Err(e) => log.push(format!("[{}] source sync failed: {e}", m.id)),
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

        match ssh.install_sources(repo_root) {
            Ok(()) => log.push(format!("[{}] prompts/assets/refs/python distributed", m.id)),
            Err(e) => log.push(format!("[{}] source distribution failed: {e}", m.id)),
        }

        if !probe.python_present || force {
            log.push(format!(
                "[{}] installing TTS venv (slow: downloads ~1.7 GB of weights on first use)",
                m.id
            ));
            match ssh.ensure_python(force) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] python provisioning failed: {e}", m.id));
                    return (probe, log);
                }
            }
        } else {
            log.push(format!("[{}] TTS venv already present — skipped", m.id));
        }
    }

    // Voice enrollment runs on every provision, configured or not: the store
    // lives inside the venv and any rebuild vaporizes it. Cheap no-op when
    // everything is already enrolled.
    match ssh.ensure_voices(repo_root) {
        Ok(v) => {
            for line in v.lines() {
                log.push(format!("[{}] {line}", m.id));
            }
        }
        Err(e) => log.push(format!("[{}] voice enrollment failed: {e}", m.id)),
    }

    match ssh.ensure_opencode() {
        Ok(v) => log.push(format!("[{}] {v}", m.id)),
        Err(e) => log.push(format!("[{}] opencode check failed: {e}", m.id)),
    }

    match ssh.start_tts() {
        Ok(v) => log.push(format!("[{}] tts: {v}", m.id)),
        Err(e) => log.push(format!("[{}] could not start tts: {e}", m.id)),
    }

    // Re-probe so the caller records the post-provision truth.
    let after = ssh.probe();
    log.push(format!("[{}] after provision: {}", m.id, after.summary()));
    (after, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localhost_is_detected_as_local() {
        for addr in ["127.0.0.1", "localhost", "::1"] {
            let m = Machine::new(addr, "me", 22, None, "worker");
            assert!(Ssh::for_machine(&m).local, "{addr} should be local");
        }
        let remote = Machine::new("192.168.2.2", "thang", 22, None, "worker");
        assert!(!Ssh::for_machine(&remote).local);
    }

    #[test]
    fn configured_requires_matching_version_and_python() {
        let mut p = Probe {
            reachable: true,
            agent_version: Some("0.2.0".into()),
            python_present: true,
            ..Default::default()
        };
        assert!(p.configured("0.2.0"));
        assert!(!p.configured("0.3.0"), "stale agent must trigger a redeploy");
        p.python_present = false;
        assert!(!p.configured("0.2.0"));
        p.python_present = true;
        p.reachable = false;
        assert!(!p.configured("0.2.0"));
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

    #[test]
    fn copy_dir_skips_venv_and_caches() {
        let src = std::env::temp_dir().join("bm-provision-src");
        let dst = std::env::temp_dir().join("bm-provision-dst");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(src.join(".venv")).unwrap();
        std::fs::create_dir_all(src.join("__pycache__")).unwrap();
        std::fs::write(src.join("tts_server.py"), "x").unwrap();
        std::fs::write(src.join(".venv/marker"), "x").unwrap();
        copy_dir(&src, &dst).unwrap();
        assert!(dst.join("tts_server.py").exists());
        assert!(!dst.join(".venv").exists(), "venv must never be copied");
        assert!(!dst.join("__pycache__").exists());
    }

    #[test]
    fn ssh_args_include_port_and_key_only_when_set() {
        let m = Machine::new("10.0.0.5", "pi", 2222, Some("/k/id".into()), "worker");
        let ssh = Ssh::for_machine(&m);
        let args = ssh.ssh_args();
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"2222".to_string()));
        assert!(args.contains(&"/k/id".to_string()));
        assert_eq!(args.last().unwrap(), "pi@10.0.0.5");

        let m2 = Machine::new("10.0.0.6", "pi", 22, None, "worker");
        let args2 = Ssh::for_machine(&m2).ssh_args();
        assert!(!args2.contains(&"-p".to_string()));
        assert!(!args2.contains(&"-i".to_string()));
    }
}
