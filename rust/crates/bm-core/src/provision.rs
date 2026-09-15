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
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

/// Directory under the remote `$HOME` that holds a worker's whole world.
pub const REMOTE_DIR: &str = "bm-worker";

/// Port the Python TTS sidecar listens on.
pub const TTS_PORT: u16 = 8818;

/// One linked machine: how to reach a box plus everything `provision` needs to
/// prepare it, stored in `.bm/machines.json` (see `Layout::machines`) so it is
/// local-only by construction. A name, not an address, is the handle —
/// addresses change, the box does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedBox {
    pub name: String,
    pub addr: String,
    #[serde(default = "default_ssh_user")]
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_ssh_user() -> String {
    "thang".into()
}

fn default_ssh_port() -> u16 {
    22
}

fn default_role() -> String {
    "worker".into()
}

impl LinkedBox {
    /// The runtime machine `provision` and the scheduler speak.
    pub fn machine(&self) -> Machine {
        let mut m = Machine::new(&self.addr, &self.user, self.port, self.key.clone(), &self.role);
        m.tts_url = Some(format!("http://127.0.0.1:{TTS_PORT}"));
        m
    }
}

/// Read the linked boxes, or an empty list when nothing is linked yet. A
/// missing file is not an error — it just means `link` has never run.
pub fn load_boxes(path: &Path) -> Vec<LinkedBox> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Insert or replace one box by name. Writes are atomic; the file stays valid
/// if the process dies mid-save.
pub fn save_box(path: &Path, bxo: &LinkedBox) -> Result<()> {
    let mut boxes = load_boxes(path);
    if let Some(slot) = boxes.iter_mut().find(|b| b.name == bxo.name) {
        *slot = bxo.clone();
    } else {
        boxes.push(bxo.clone());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::atomic_write(path, &serde_json::to_string_pretty(&boxes)?)?;
    Ok(())
}

/// Manifest stamp recorded on a target machine to detect whether sources/voices changed.
///
/// Written to `~/{REMOTE_DIR}/.provision_stamp.json` at the end of every
/// provision, and read back by the *next* probe. When both hashes still match,
/// the slow work is skipped: `ensure_voices` (which boots Python and imports
/// PyTorch) and the redundant source sync. A stale or missing stamp is never an
/// error — it just means the full path runs, which is what it did before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProvisionStamp {
    pub agent_version: String,
    pub sources_hash: String,
    pub voices_hash: String,
}

impl ProvisionStamp {
    /// Whether the voices enrolled on this box still match the ones we would
    /// push. A mismatch means `ensure_voices` must run — that is the step that
    /// costs seconds, so it is the one worth skipping.
    pub fn voices_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.voices_hash == want.voices_hash
    }

    /// Whether the worker's sources (prompts, requirements, casts, assets, and
    /// the agent build itself) still match ours.
    pub fn sources_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.sources_hash == want.sources_hash && self.agent_version == want.agent_version
    }
}

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
    /// Manifest stamp found on the remote box from the previous provision, if any.
    #[serde(default)]
    pub stamp: Option<ProvisionStamp>,
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
            // Never prompt, never linger: every use is scripted, and a stalled
            // connection must die instead of hanging a TUI job forever.
            "-n".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "ServerAliveInterval=5".into(),
            "-o".into(),
            "ServerAliveCountMax=2".into(),
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
            let stderr = String::from_utf8_lossy(&out.stderr);
            let hint = if stderr.contains("command not found") {
                " — rsync is not on this box; install it, e.g. sudo apt install -y rsync"
            } else {
                ""
            };
            anyhow::bail!(
                "rsync push failed: {}{hint}",
                crate::util::head_chars(&stderr, 300)
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
    /// A missing `python3` (or venv module) is named with its fix, not left as
    /// a bare remote traceback: provisioning never installs python itself.
    pub fn ensure_python(&self, force: bool) -> Result<String> {
        let script = format!(
            r#"set -e
D="$HOME/{d}/python"
if [ {force} -eq 0 ] && [ -x "$D/.venv/bin/python" ]; then
  echo "PYTHON-OK (existing venv)"
  exit 0
fi
command -v python3 >/dev/null 2>&1 || {{ echo "python3 missing on this box — install it, e.g. sudo apt install -y python3 python3-venv" >&2; exit 4; }}
set +e
python3 -m venv "$D/.venv" 2>"$D/venv.err"
VENV_RC=$?
set -e
[ $VENV_RC -eq 0 ] || {{ echo "venv build failed: $(head -1 "$D/venv.err") — install the venv module, e.g. sudo apt install -y python3-venv" >&2; exit 5; }}
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
    ///
    /// `voices.json` and `refs/` are personal and git-ignored (see
    /// `.gitignore`), so a fresh clone legitimately has neither. Absence means
    /// "this machine has no clones", not "provisioning failed" — and there is
    /// nothing to enroll without the reference clips anyway.
    pub fn ensure_voices(&self, repo_root: &Path) -> Result<String> {
        let manifest_src = repo_root.join("voices.json");
        if !manifest_src.exists() {
            return Ok("VOICES-OK (no voices.json — no clones to enroll)".to_string());
        }
        // Parsed (and discarded) as validation: a malformed manifest must fail
        // here with the file named, not deep inside the remote python.
        let _manifest: std::collections::HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&manifest_src).with_context(|| {
                format!("reading {}", manifest_src.display())
            })?)
            .context("parsing voices.json (name -> refs/*.wav)")?;
        self.rsync_push(&repo_root.join("refs"), "refs", false)?;
        self.rsync_push(&manifest_src, "voices.json", false)?;
        // The missing-set is computed in python, not the shell: voice names
        // contain spaces ("Châu Tinh Trì"), and every shell word-split turned
        // them into fragments that matched nothing — enrollment died on the
        // first multi-word name with `manifest[name]` KeyError, enrolling zero
        // voices. `voices.json` is already on the target, so python reads the
        // want-list straight from it; no name list crosses the shell at all.
        let script = format!(
            r#"set -e
D="$HOME/{d}"
V="$D/python/.venv/bin/python"
STORE=$(ls $D/python/.venv/lib/*/site-packages/vieneu/assets/voices_v3_turbo.json 2>/dev/null | head -n 1)
[ -n "$STORE" ] || {{ echo "voice store not found (venv broken?)" >&2; exit 6; }}
cd "$D"
PYTHONPATH="$D/python" "$V" -c "
import json
import tts_vieneu as vn
manifest = json.load(open('voices.json'))
store = json.load(open('$STORE'))
have = set(store.get('presets', store).keys())
missing = sorted(n for n in manifest if not n.startswith('_') and n not in have)
if not missing:
    print('VOICES-OK (already enrolled)')
else:
    print('enrolling: ' + ', '.join(missing), flush=True)
    tts = vn.engine()
    failed = []
    for name in missing:
        try:
            tts.add_voice(name, manifest[name])
            print('enrolled', name, flush=True)
        except Exception as e:
            # One bad clip (missing/corrupt ref file) must not vaporize the
            # rest: report it, keep going, save whoever enrolled.
            print('SKIP', name, type(e).__name__, str(e)[:160], flush=True)
            failed.append(name)
    tts.save_voices()
    if failed:
        print('VOICES-PARTIAL (saved the rest, failed: ' + ', '.join(failed) + ')', flush=True)
        raise SystemExit('voice enrollment failed for: ' + ', '.join(failed))
    print('VOICES-OK (all enrolled)')
"
"#,
            d = REMOTE_DIR,
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
            // Skip copy if destination file exists and has identical size & mtime
            if to.is_file() {
                if let (Ok(m_from), Ok(m_to)) = (from.metadata(), to.metadata()) {
                    if m_from.len() == m_to.len() {
                        if let (Ok(t_from), Ok(t_to)) = (m_from.modified(), m_to.modified()) {
                            if t_from == t_to {
                                continue;
                            }
                        }
                    }
                }
            }
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Compute manifest stamp for detecting changes to sources and clone voices.
///
/// Two SHA-256 digests, each over a canonical (sorted, newline-joined) view of
/// its inputs, so the same inputs produce the same hex string on any machine:
///
/// * `sources_hash` — `prompts/` by signature, plus the *content* of the small
///   manifests the worker must match exactly (`requirements.txt`, the cast
///   files, the scene map), plus the agent version so a rebuild redeploys.
/// * `voices_hash` — `voices.json` by content (a rename with identical clips
///   must re-enroll) and `refs/` by signature only: those clips are megabytes,
///   and reading them would cost more than the enrollment we are avoiding.
pub fn compute_provision_stamp(repo_root: &Path, agent_version: &str) -> ProvisionStamp {
    let mut sources = Sha256::new();
    sources.update(agent_version.as_bytes());
    sources.update([0]);
    sources.update(signature_of_dir(&repo_root.join("prompts")).as_bytes());
    for rel in [
        "python/requirements.txt",
        "data/cast-vieneu.json",
        "data/cast.json",
        "assets/scene-map.json",
    ] {
        let p = repo_root.join(rel);
        if let Ok(bytes) = std::fs::read(&p) {
            sources.update(rel.as_bytes());
            sources.update([0]);
            sources.update(&bytes);
            sources.update([0]);
        }
    }

    let mut voices = Sha256::new();
    if let Ok(bytes) = std::fs::read(repo_root.join("voices.json")) {
        voices.update(&bytes);
    }
    voices.update([0]);
    voices.update(signature_of_dir(&repo_root.join("refs")).as_bytes());

    ProvisionStamp {
        agent_version: agent_version.to_string(),
        sources_hash: hex_digest(sources.finalize()),
        voices_hash: hex_digest(voices.finalize()),
    }
}

/// Hex-encode a digest by hand: the repo takes no hex dependency for one call.
fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for b in bytes.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A cheap, deterministic signature for a directory tree: sorted names plus
/// each file's length and mtime, recursively. Contents are never read — this
/// runs over `refs/`, where a single clip is megabytes and mtime+size is
/// exactly the test `copy_dir` and rsync already use to decide "unchanged".
fn signature_of_dir(dir: &Path) -> String {
    const SKIP: [&str; 3] = [".venv", "__pycache__", "target"];
    let mut out = String::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        let (Ok(meta), Ok(rel)) = (path.metadata(), path.strip_prefix(dir)) else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if meta.is_dir() {
            out.push_str(&format!("d {} {}\n", rel.display(), mtime));
            out.push_str(&signature_of_dir(&path));
        } else {
            out.push_str(&format!("f {} {} {}\n", rel.display(), meta.len(), mtime));
        }
    }
    out
}

/// Parse a stamp payload.
///
/// The probe reads the file inside its own ssh round trip (one connection, not
/// two) and hands the text here; `read_provision_stamp` fetches it on its own.
/// Both go through this so they can never disagree.
fn parse_stamp(text: &str) -> Option<ProvisionStamp> {
    serde_json::from_str(text).ok()
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

    let local_stamp = compute_provision_stamp(repo_root, agent_version);
    let remote_stamp = probe.stamp.as_ref();

    let sources_match = !force
        && remote_stamp.map(|s| s.sources_in_sync(&local_stamp)).unwrap_or(false);
    let voices_match = !force
        && remote_stamp.map(|s| s.voices_in_sync(&local_stamp)).unwrap_or(false);

    if probe.configured(agent_version) && !force {
        log.push(format!(
            "[{}] already configured (agent {} + python)",
            m.id, agent_version
        ));
        if sources_match {
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            // Sources still sync: cast/asset/prompt updates must reach workers
            // without a venv rebuild. Cheap rsync deltas when nothing changed.
            match ssh.install_sources(repo_root) {
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
            log.push(format!("[{}] prompts/assets/refs/python in sync (cache match)", m.id));
        } else {
            match ssh.install_sources(repo_root) {
                Ok(()) => log.push(format!("[{}] prompts/assets/refs/python distributed", m.id)),
                Err(e) => log.push(format!("[{}] source distribution failed: {e}", m.id)),
            }
        }

        if !probe.python_present || force {
            log.push(format!(
                "[{}] installing TTS venv (slow: downloads ~1.7 GB of weights on first use)",
                m.id
            ));
            match ssh.ensure_python(force) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log);
                }
            }
        } else {
            log.push(format!("[{}] TTS venv already present — skipped", m.id));
        }
    }

    // Voice enrollment runs only when voices changed, or on first venv build / force.
    if voices_match && probe.python_present {
        log.push(format!("[{}] clone voices in sync (cache match, skipped PyTorch init)", m.id));
    } else {
        match ssh.ensure_voices(repo_root) {
            Ok(v) => {
                for line in v.lines() {
                    log.push(format!("[{}] {line}", m.id));
                }
            }
            Err(e) => log.push(format!("[{}] voice enrollment failed: {e}", m.id)),
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
    (after, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_never_prompts_never_lingers() {
        // Every ssh use is scripted: no stdin, no password prompts, and a
        // stalled connection must die instead of hanging a TUI job forever.
        let args = Ssh::for_machine(&Machine::new("192.168.2.2", "thang", 22, None, "worker"))
            .ssh_args()
            .join(" ");
        for flag in ["-n", "BatchMode=yes", "ConnectTimeout=10", "ServerAliveInterval=5", "ServerAliveCountMax=2"] {
            assert!(args.contains(flag), "{args}");
        }
    }

    #[test]
    fn localhost_is_detected_as_local() {        for addr in ["127.0.0.1", "localhost", "::1"] {
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
    fn a_missing_voices_manifest_is_not_a_failure() {
        // `voices.json` and `refs/` are personal and git-ignored, so a fresh
        // clone has neither. Provisioning must read that as "no clones" rather
        // than as a failure — and it must decide that without reaching for ssh.
        // The target below is TEST-NET-1, so any attempt to connect fails.
        let ssh = Ssh {
            target: "nobody@192.0.2.1".into(),
            port: 22,
            key: None,
            local: false,
        };
        let empty = std::env::temp_dir().join("bm-provision-no-voices");
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();

        let out = ssh
            .ensure_voices(&empty)
            .expect("a missing manifest is a valid state, not an error");
        assert!(out.contains("no voices.json"), "got: {out}");
    }

    #[test]
    fn linked_boxes_round_trip_and_upsert_by_name() {
        let dir = std::env::temp_dir().join("bm-provision-boxes");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("machines.json");
        assert!(super::load_boxes(&path).is_empty(), "missing file, not an error");

        let bxo = super::LinkedBox {
            name: "box-1".into(),
            addr: "192.168.2.2".into(),
            user: "thang".into(),
            port: 22,
            key: Some("/k/id".into()),
            role: "worker".into(),
        };
        super::save_box(&path, &bxo).unwrap();
        let again = super::LinkedBox { addr: "10.0.0.9".into(), ..bxo.clone() };
        super::save_box(&path, &again).unwrap();

        let boxes = super::load_boxes(&path);
        assert_eq!(boxes.len(), 1, "same name replaces, never duplicates");
        assert_eq!(boxes[0].addr, "10.0.0.9");

        let m = boxes[0].machine();
        assert_eq!(m.ssh_target(), "thang@10.0.0.9");
        assert_eq!(m.tts_url.as_deref(), Some("http://127.0.0.1:8818"));
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

    /// A throwaway repo root holding only the files the stamp looks at.
    fn stamp_fixture(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("bm-stamp-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["prompts", "refs", "python", "data", "assets"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("prompts/digest.md"), "prompt v1").unwrap();
        std::fs::write(root.join("python/requirements.txt"), "torch\n").unwrap();
        std::fs::write(root.join("data/cast.json"), r#"{"Narrator":"Đức Trí"}"#).unwrap();
        std::fs::write(root.join("voices.json"), r#"{"Narrator":"refs/n.wav"}"#).unwrap();
        std::fs::write(root.join("refs/n.wav"), vec![1u8; 64]).unwrap();
        root
    }

    #[test]
    fn a_stamp_is_stable_and_content_addressed() {
        let root = stamp_fixture("stable");
        let a = compute_provision_stamp(&root, "0.2.0");
        let b = compute_provision_stamp(&root, "0.2.0");
        assert_eq!(a, b, "nothing changed, so the stamp must not either");
        assert_eq!(a.sources_hash.len(), 64, "sha-256 hex is 64 chars");
        assert_eq!(a.voices_hash.len(), 64);
        assert!(a.sources_in_sync(&b) && a.voices_in_sync(&b));

        // A cast edit is a source change and nothing else.
        std::fs::write(root.join("data/cast.json"), r#"{"Narrator":"Adam"}"#).unwrap();
        let c = compute_provision_stamp(&root, "0.2.0");
        assert_ne!(a.sources_hash, c.sources_hash, "a cast edit must resync sources");
        assert_eq!(a.voices_hash, c.voices_hash, "…and must not re-enroll voices");

        // A version bump redeploys the agent even when every file is identical.
        let d = compute_provision_stamp(&root, "0.3.0");
        assert!(!a.sources_in_sync(&d), "a new agent build must redeploy");
        assert!(a.voices_in_sync(&d), "the agent version says nothing about voices");
    }

    #[test]
    fn voices_hash_tracks_the_manifest_and_the_reference_clips() {
        let root = stamp_fixture("voices");
        let base = compute_provision_stamp(&root, "0.2.0");

        // A rename in voices.json must re-enroll even though the clip is
        // identical: enrollment is keyed by name, not by file.
        std::fs::write(root.join("voices.json"), r#"{"Storyteller":"refs/n.wav"}"#).unwrap();
        let renamed = compute_provision_stamp(&root, "0.2.0");
        assert!(!base.voices_in_sync(&renamed), "a rename must re-enroll");
        assert!(base.sources_in_sync(&renamed), "voices.json is not a source");

        // A new clip changes the refs signature without touching the manifest.
        std::fs::write(root.join("refs/m.wav"), vec![2u8; 64]).unwrap();
        let added = compute_provision_stamp(&root, "0.2.0");
        assert!(!renamed.voices_in_sync(&added), "a new clip must re-enroll");
        assert!(base.sources_in_sync(&added), "refs/ is not part of the sources hash");
    }

    #[test]
    fn a_stamp_payload_parses_and_garbage_does_not() {
        let s = ProvisionStamp {
            agent_version: "0.2.0".into(),
            sources_hash: "a".repeat(64),
            voices_hash: "b".repeat(64),
        };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(parse_stamp(&text).unwrap(), s, "a real payload round-trips");
        assert!(parse_stamp("").is_none());
        assert!(
            parse_stamp("not json").is_none(),
            "a truncated file is a cache miss, never a crash"
        );
        // TEST-NET-1: any attempt to connect fails, so this box has no stamp.
        let ssh = Ssh {
            target: "nobody@192.0.2.1".into(),
            port: 22,
            key: None,
            local: false,
        };
        assert!(ssh.read_provision_stamp().is_none());
    }

    #[test]
    fn copy_dir_leaves_an_unchanged_signature_alone() {
        // The local fast path compares size+mtime, exactly like rsync. To prove
        // the skip (a real identical file cannot be told apart anyway), the
        // destination is given different *content* with the same signature: if
        // it is copied over, the comparison did not happen.
        let src = std::env::temp_dir().join("bm-copy-skip-src");
        let dst = std::env::temp_dir().join("bm-copy-skip-dst");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "same").unwrap();
        copy_dir(&src, &dst).unwrap();

        std::fs::write(dst.join("a.txt"), "diff").unwrap();
        let t = std::fs::metadata(src.join("a.txt")).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(dst.join("a.txt"))
            .unwrap()
            .set_modified(t)
            .unwrap();
        copy_dir(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("a.txt")).unwrap(),
            "diff",
            "an identical size+mtime must not be re-copied"
        );

        // A changed size is a real change and must be copied.
        std::fs::write(src.join("a.txt"), "a longer body").unwrap();
        copy_dir(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "a longer body");
    }
}
