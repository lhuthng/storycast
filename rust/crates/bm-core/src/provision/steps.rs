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
        self.reachable && self.agent_version.as_deref() == Some(want_version) && self.python_present
    }

    /// One-line summary for the TUI machine pane.
    pub fn summary(&self) -> String {
        if !self.reachable {
            return format!("unreachable: {}", self.note);
        }
        // A count, not the list: 50 enrolled names wrapped the Logs pane for
        // screens. Which voices enrolled is already on the `enrolled …` lines.
        format!(
            "{} · {} cpu · {} MB ram · {} · agent={} · python={} · voices={} · tts={}",
            self.hostname,
            self.nproc,
            self.mem_mb,
            self.arch,
            self.agent_version.as_deref().unwrap_or("absent"),
            if self.python_present { "yes" } else { "no" },
            if self.voices.is_empty() {
                "-".into()
            } else {
                format!("{} voices", self.voices.len())
            },
            if self.tts_up { "up" } else { "down" },
        )
    }
}

impl Ssh {
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
        let _manifest: std::collections::HashMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(&manifest_src)
                .with_context(|| format!("reading {}", manifest_src.display()))?,
        )
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
        && remote_stamp
            .map(|s| s.sources_in_sync(&local_stamp))
            .unwrap_or(false);
    let voices_match = !force
        && remote_stamp
            .map(|s| s.voices_in_sync(&local_stamp))
            .unwrap_or(false);

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
            log.push(format!(
                "[{}] prompts/assets/refs/python in sync (cache match)",
                m.id
            ));
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
        log.push(format!(
            "[{}] clone voices in sync (cache match, skipped PyTorch init)",
            m.id
        ));
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
    // Single-source-of-truth check, against the fresh probe: a store holding
    // voices nothing declares desyncs the cluster silently (renders pass here,
    // 500 everywhere else). Warn with the fix; never delete.
    {
        let layout = crate::Layout::new(repo_root);
        let engine = crate::config::Settings::load(&layout.settings()).engine;
        let manifest: std::collections::HashMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(repo_root.join("voices.json")).unwrap_or_default(),
        )
        .unwrap_or_default();
        let pool = crate::pool::load_pool(&repo_root.join("voice-pool.json"));
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
    fn configured_requires_matching_version_and_python() {
        let mut p = Probe {
            reachable: true,
            agent_version: Some("0.2.0".into()),
            python_present: true,
            ..Default::default()
        };
        assert!(p.configured("0.2.0"));
        assert!(
            !p.configured("0.3.0"),
            "stale agent must trigger a redeploy"
        );
        p.python_present = false;
        assert!(!p.configured("0.2.0"));
        p.python_present = true;
        p.reachable = false;
        assert!(!p.configured("0.2.0"));
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
}
