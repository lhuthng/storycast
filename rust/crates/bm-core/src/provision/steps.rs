use anyhow::{Context, Result};
use bm_proto::Machine;
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::ssh::{RsyncProgress, Ssh};
use super::stamp::{compute_provision_stamp, parse_stamp, ProvisionStamp};
use super::{REMOTE_DIR, TTS_PORT};

/// One labelled rsync progress stream off the shared live sender: `None`
/// keeps the push silent.
fn progress<'a>(
    live: Option<&'a tokio::sync::mpsc::UnboundedSender<String>>,
    label: &'a str,
) -> Option<RsyncProgress<'a>> {
    live.map(|tx| RsyncProgress { tx, label })
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
    /// The ONNX Runtime `libonnxruntime.so.1` beside the binary. Linux links
    /// it dynamically — a binary without it dies on startup — while macOS
    /// links statically and never has it, so readiness only demands it there.
    #[serde(default)]
    pub tts_lib_present: bool,
    /// A baked `~/{REMOTE_DIR}/models/` — the Rust sidecar's weights.
    #[serde(default)]
    pub models_present: bool,
    /// `ffmpeg` on PATH. The merge stage shells out to it, so a box without it
    /// provisions cleanly and then fails every merge it is offered — a strike
    /// and a shelved chapter instead of a report. Reported, not gated: merge is
    /// a small share of the work and refusing the box outright would cost more
    /// than it saves.
    #[serde(default)]
    pub ffmpeg_present: bool,
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
        self.tts_bin_present && self.models_present && self.tts_runtime_ok()
    }

    /// The loader is satisfied. A binary-without-lib box reads as not-ready,
    /// so the next `:prov` repairs it (binary + lib + a verification run)
    /// instead of confirming a sidecar that dies on startup.
    pub fn tts_runtime_ok(&self) -> bool {
        self.os != "linux" || self.tts_lib_present
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
        ) + if self.ffmpeg_present {
            ""
        } else {
            " · NO FFMPEG — merges will fail here"
        }
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

/// Whether a provision may chase a missing tool with a package manager.
///
/// The rule lives here, once, because two call sites read it and a second copy
/// is how they drift. No on a box that already has everything and was not
/// forced to redo it: the answer will not change, and `ensure_opencode`'s
/// install is bounded at ten minutes — spent on every `B` press of a healthy
/// cluster, which is what made a catch-up look like a hang. Yes otherwise,
/// including on `force`, which is the operator explicitly asking for the slow
/// path.
fn may_install(configured: bool, force: bool) -> bool {
    force || !configured
}

/// Whether the model/voice store must be pushed even when the worker already
/// has the right agent and sidecar. The voice roster lives in
/// `models/voices.json`, so a clone addition is a model-store change.
fn models_need_push(remote: Option<&ProvisionStamp>, local: &ProvisionStamp, force: bool) -> bool {
    force || !remote.is_some_and(|stamp| stamp.tts_in_sync(local))
}

/// Whether the remote voice store actually contains every clone declared by the
/// manifest. The stamp is only a cache hint: an older provision bug could write
/// a fresh stamp after skipping the model push, leaving the box permanently
/// looking in sync while missing the newly added voice.
fn voice_store_covers(
    remote: &[String],
    manifest: &std::collections::BTreeMap<String, String>,
) -> bool {
    manifest
        .keys()
        .filter(|name| !name.starts_with('_'))
        .all(|name| remote.iter().any(|voice| voice == name))
}

/// The `opencode` step, in two flavours.
///
/// Both start with the same `command -v`: a box that has it pays one round
/// trip either way. The difference is what happens when it does not. On a
/// fresh or forced provision the box is chased with `npm i -g` (bounded at ten
/// minutes, and genuinely needed for the digest lane). On a box that already
/// passed a full provision it is reported instead — the answer is not going to
/// change because `B` was pressed again, and re-running a failing install on
/// every press is what made a healthy cluster's start take minutes.
fn opencode_script(allow_install: bool) -> String {
    if allow_install {
        r#"command -v opencode >/dev/null 2>&1 && { echo "OPENCODE-OK (present)"; exit 0; }
command -v npm >/dev/null 2>&1 || { echo "OPENCODE-SKIP (npm missing; install node first)"; exit 0; }
mkdir -p "$HOME/.local"
npm i -g --prefix "$HOME/.local" opencode-ai >/dev/null 2>&1 && echo "OPENCODE-OK (installed)" || echo "OPENCODE-SKIP (npm install failed)""#
            .into()
    } else {
        r#"command -v opencode >/dev/null 2>&1 && { echo "OPENCODE-OK (present)"; exit 0; }
echo "OPENCODE-SKIP (already configured — not reinstalling; force a re-provision to try again)""#
            .into()
    }
}

/// The `ffmpeg` step. Same split as [`opencode_script`], and here the cheap
/// half is most of the value: `command -v ffmpeg` is what a healthy box runs,
/// and the package-manager branch below is only reached on a box that has
/// neither ffmpeg nor a reason to be chased for it again.
fn ffmpeg_script(allow_install: bool) -> String {
    if allow_install {
        r#"export DEBIAN_FRONTEND=noninteractive
if command -v ffmpeg >/dev/null 2>&1; then echo "FFMPEG-OK (present)"; exit 0; fi
install() { $1 >/dev/null 2>&1; }
if command -v apt-get >/dev/null 2>&1; then
  sudo -n apt-get install -y ffmpeg >/dev/null 2>&1 || install "apt-get install -y ffmpeg"
elif command -v dnf >/dev/null 2>&1; then
  sudo -n dnf install -y ffmpeg >/dev/null 2>&1 || install "dnf install -y ffmpeg"
elif command -v yum >/dev/null 2>&1; then
  sudo -n yum install -y ffmpeg >/dev/null 2>&1 || install "yum install -y ffmpeg"
else
  echo "FFMPEG-SKIP (no known package manager — install ffmpeg by hand)"; exit 0
fi
if command -v ffmpeg >/dev/null 2>&1; then echo "FFMPEG-OK (installed)"; else echo "FFMPEG-SKIP (install refused — needs sudo? run: sudo apt-get install -y ffmpeg)"; fi
"#
            .into()
    } else {
        r#"if command -v ffmpeg >/dev/null 2>&1; then echo "FFMPEG-OK (present)"; exit 0; fi
echo "FFMPEG-SKIP (already configured — not reinstalling; force a re-provision to try again)""#
            .into()
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
if [ -f "$HOME/{dir}/libonnxruntime.so.1" ]; then
  echo "tts_lib=present"
else
  echo "tts_lib=absent"
fi
if [ -f "$HOME/{dir}/models/manifest.json" ]; then
  echo "models=present"
else
  echo "models=absent"
fi
if command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg=present"
else
  echo "ffmpeg=absent"
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
                        "tts_lib" => probe.tts_lib_present = v == "present",
                        "models" => probe.models_present = v == "present",
                        "ffmpeg" => probe.ffmpeg_present = v == "present",
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
    pub fn install_agent(
        &self,
        agent_binary: &Path,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        self.rsync_push(agent_binary, "bm-agent", false, progress(live, "agent"))?;
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
    pub fn install_sources(
        &self,
        layout: &crate::Layout,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<()> {
        for rel in ["prompts", "assets", "refs"] {
            let src = layout.root.join(rel);
            if src.exists() {
                self.rsync_push(&src, rel, true, progress(live, rel))?;
            }
        }
        for (engine, rel) in [
            ("vieneu", "data/cast-vieneu.json"),
            ("gemini", "data/cast.json"),
        ] {
            let src = layout.cast(engine);
            if src.exists() {
                self.rsync_push(&src, rel, false, progress(live, rel))?;
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
            self.rsync_push(
                &manifest,
                "voices.json",
                false,
                progress(live, "voices.json"),
            )?;
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
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        self.rsync_push(tts_binary, "bm-tts", false, progress(live, "bm-tts"))?;
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
                let name = name.to_string_lossy();
                self.rsync_push(lib, &name, false, progress(live, &name))?;
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
    pub fn install_models(
        &self,
        root: &Path,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        let src = root.join("models");
        if !src.is_dir() {
            anyhow::bail!(
                "no {} — run the bake first (`python3 tools/bake-models.py`)",
                src.display()
            );
        }
        self.rsync_push(&src, "models", true, progress(live, "models"))?;
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
    pub fn ensure_opencode(&self, allow_install: bool) -> Result<String> {
        // `allow_install` is false on a box that already passed a full
        // provision. The check stays — it is one `command -v` — but the install
        // does not: `npm i -g` is bounded at ten minutes, and on a box whose
        // answer will not change it was ten minutes of a `B` press that looked
        // like a hang. A deliberate re-provision still installs.
        let (code, stdout, stderr) = self.run(&opencode_script(allow_install), 600)?;
        if code != 0 {
            anyhow::bail!("opencode check failed: {}", stderr.trim());
        }
        Ok(stdout.trim().to_string())
    }

    /// Best-effort ffmpeg install for the merge lane.
    ///
    /// The merge stage shells out to `ffmpeg`, so a box without it takes every
    /// merge it is offered and fails each one. This installs it from the
    /// platform's package manager when it is missing — never a hard failure:
    /// a refused install (no sudo, an offline mirror) only warns, and the
    /// worker's `merge` capability gate keeps merge off this box until ffmpeg
    /// appears. Returns a one-line verdict for the provision log.
    ///
    /// `allow_install` carries the same meaning as on [`Self::ensure_opencode`]:
    /// a present `ffmpeg` short-circuits either way, so the flag only decides
    /// whether a *missing* one is chased with a package manager this time.
    pub fn ensure_ffmpeg(&self, allow_install: bool) -> Result<String> {
        let (code, stdout, stderr) = self.run(&ffmpeg_script(allow_install), 300)?;
        if code != 0 {
            // A failure to even run the check is itself a warning, never fatal:
            // the merge lane stays available to other boxes.
            return Ok(format!(
                "FFMPEG-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }

    /// Start the TTS sidecar detached, unless it is already answering, and
    /// **wait for it to be ready** before returning.
    ///
    /// Waiting is the point, not politeness. `bm-tts` binds its port before it
    /// loads ~2.85 GB of weights, so between the launch and the first ready
    /// `/health` there is a window where the box has a sidecar that cannot
    /// answer yet. The old 3-second sleep closed on "model loading" and moved
    /// on, and the worker's first render — unable to tell "not up yet" from
    /// "not there" — spawned a **second** model on an 8 GiB box. That is the
    /// OOM this cluster kept taking.
    ///
    /// A ready server answers 200; a loading one answers 503, which `curl`
    /// reports as success unless told otherwise, so the check is on the *code*.
    /// The budget is deliberately under the ssh call's own timeout (240 s of
    /// polling, 300 s allowed) — a sidecar that has not loaded in four minutes
    /// on a box this repo sizes for is reported, not waited on for ever.
    pub fn start_tts(&self) -> Result<String> {
        let script = format!(
            r#"D="$HOME/{d}"
if [ "$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/health)" = "200" ]; then
  echo "TTS-ALREADY-UP"; exit 0
fi
cd "$D" || exit 5
LD_LIBRARY_PATH="$D" nohup "$D/bm-tts" --models models --codec models \
  --dict models/sea_g2p.bin --voices models/voices.json \
  --port {port} --bind 0.0.0.0 > "$D/tts.log" 2>&1 &
echo $! > "$D/tts.pid"
for _ in $(seq 1 120); do
  sleep 2
  [ "$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/health)" = "200" ] && {{ echo "TTS-STARTED"; exit 0; }}
done
echo "TTS-STARTING (not ready after 240s — check $D/tts.log)"
"#,
            d = REMOTE_DIR,
            port = TTS_PORT
        );
        let (code, stdout, stderr) = self.run(&script, 300)?;
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

    /// Ship the cluster token to the worker's root.
    ///
    /// The inverted protocol makes a worker accept *instructions*, so it has to
    /// be able to tell its own inductor from anything else that can reach the
    /// port. The secret travels as a file rather than as an argument because
    /// `argv` is visible in `ps` on every box it was typed on — and it is
    /// rewritten on every provision so a rotated token reaches the box without
    /// a manual step.
    pub fn write_cluster_token(&self, token: &str) -> Result<()> {
        // A quoted heredoc, like the profile pointer beside it: the token is hex
        // today, but a hand-edited one must not be able to end the command early
        // or be re-split by the shell.
        let script = format!(
            "mkdir -p $HOME/{d}/.bm && cat > $HOME/{d}/.bm/{f} << 'EOF'\n{token}\nEOF\nchmod 600 $HOME/{d}/.bm/{f}\n",
            d = REMOTE_DIR,
            f = crate::token::FILE,
        );
        let (code, _, stderr) = self.run(&script, 10)?;
        if code != 0 {
            anyhow::bail!("failed to write the cluster token: {}", stderr.trim());
        }
        Ok(())
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

/// Provision log lines that also stream live: every `push` appends to the
/// returned vec *and* forwards a copy to the sender, so a slow provision
/// (models push, package installs) shows each step in the event pane as it
/// happens instead of dumping everything at the end. `None` keeps the old
/// collect-only behavior (CLI, tests).
pub struct LiveLog {
    pub lines: Vec<String>,
    live: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl LiveLog {
    pub fn new(live: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Self {
        LiveLog {
            lines: Vec::new(),
            live,
        }
    }

    pub fn push(&mut self, line: String) {
        if let Some(tx) = &self.live {
            let _ = tx.send(line.clone());
        }
        self.lines.push(line);
    }
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
    live: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> (Probe, Vec<String>) {
    let ssh = Ssh::for_machine(m);
    // Cloned, not moved: the install steps below borrow the original for
    // their byte-progress streams.
    let mut log = LiveLog::new(live.clone());

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
        return (probe, log.lines);
    }

    // Self-healing enrollment: the manifest may name clones the pushed store
    // lacks (added or swapped since the last bake). Merging them here — before
    // the stamp — means the hash drift pushes the fix to workers in this same
    // run, instead of warning forever no matter how often `:prov` runs.
    // Voices enrolled nowhere stay missing; the warning below still names
    // exactly those.
    let baked = crate::pool::bake_missing_voices(&layout.root);
    if !baked.is_empty() {
        log.push(format!(
            "[{}] baked {} voice(s) into models/voices.json: {}",
            m.id,
            baked.len(),
            baked.join(", ")
        ));
    }

    let local_stamp = compute_provision_stamp(&layout.root, agent_version, agent_binary);
    let remote_stamp = probe.stamp.as_ref();

    let sources_match = !force
        && remote_stamp
            .map(|s| s.sources_in_sync(&local_stamp))
            .unwrap_or(false);
    // The voice store now travels inside `models/`, so `tts_hash` covers it and
    // there is no separate voices check. Also verify the remote names: a stamp
    // written by the old already-configured path could say “in sync” after it
    // skipped the model push, which is exactly how Narrator 2 stayed missing.
    let manifest = crate::pool::load_manifest(&layout.root);
    let remote_voice_store_complete = voice_store_covers(&probe.voices, &manifest);
    let models_match =
        !models_need_push(remote_stamp, &local_stamp, force) && remote_voice_store_complete;

    // What the box already is, asked once. This gates the install steps at the
    // bottom of the function as well as the push steps here: on a box that has
    // everything, provisioning is a *verification*, and the two package
    // installs are attempts that can each take minutes and cannot succeed on
    // the second try any more than the first. That is the difference between a
    // catch-up that costs a round trip per step and one that costs minutes on a
    // machine that is already working.
    let already = probe.configured(agent_version) && !force;
    // A voice change is a model-store change. Remember that we pushed the
    // store so the already-running sidecar is restarted below; otherwise the
    // new `models/voices.json` is on disk while the old roster stays resident.
    let mut models_pushed = false;
    // Read here, beside `already`, so the two cannot disagree about what this
    // run is allowed to do — see [`may_install`].
    let installs = may_install(probe.configured(agent_version), force);

    if already {
        log.push(format!(
            "[{}] already configured (agent {} + tts sidecar)",
            m.id, agent_version
        ));
        // The version string cannot see a rebuild: every dev build between
        // releases reports the same one, so a same-version binary drift would
        // otherwise sit on the box for ever. The content hash catches it, and
        // rsync makes the no-op push cheap when the bytes never moved.
        if !remote_stamp
            .map(|s| s.agent_in_sync(&local_stamp))
            .unwrap_or(false)
        {
            match ssh.install_agent(agent_binary, live.as_ref()) {
                Ok(v) => log.push(format!(
                    "[{}] agent binary drifted, redeployed (reports version {v})",
                    m.id
                )),
                Err(e) => log.push(format!("[{}] agent redeploy failed: {e}", m.id)),
            }
        }
        if sources_match {
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            // Sources still sync: cast/asset/prompt updates must reach workers
            // without a venv rebuild. Cheap rsync deltas when nothing changed.
            match ssh.install_sources(layout, live.as_ref()) {
                Ok(()) => log.push(format!("[{}] sources in sync", m.id)),
                Err(e) => log.push(format!("[{}] source sync failed: {e}", m.id)),
            }
        }

        // The voice store lives in `models/voices.json`, not in `voices.json`.
        // An already-configured worker still needs the model directory pushed
        // when a new clone was baked; the old branch only synced sources, so
        // `:prov` could report success while the worker still answered
        // `unknown voice "Narrator 2"`.
        if models_match {
            log.push(format!("[{}] models in sync (cache match)", m.id));
        } else {
            log.push(format!("[{}] pushing models/voice store", m.id));
            match ssh.install_models(&layout.root, live.as_ref()) {
                Ok(v) => {
                    models_pushed = true;
                    log.push(format!("[{}] {v}", m.id));
                }
                Err(e) => {
                    log.push(format!("[{}] model sync failed: {e}", m.id));
                    return (probe, log.lines);
                }
            }
        }
    } else {
        if let Err(e) = ssh.ensure_root() {
            log.push(format!("[{}] ensure_root failed: {e}", m.id));
            return (probe, log.lines);
        }
        log.push(format!("[{}] worker root ready (~/{REMOTE_DIR})", m.id));

        match ssh.install_agent(agent_binary, live.as_ref()) {
            Ok(v) => log.push(format!("[{}] agent installed, reports version {v}", m.id)),
            Err(e) => {
                log.push(format!("[{}] agent install failed: {e}", m.id));
                return (probe, log.lines);
            }
        }

        if sources_match {
            log.push(format!(
                "[{}] prompts/assets/refs in sync (cache match)",
                m.id
            ));
        } else {
            match ssh.install_sources(layout, live.as_ref()) {
                Ok(()) => log.push(format!("[{}] prompts/assets/refs distributed", m.id)),
                Err(e) => log.push(format!("[{}] source distribution failed: {e}", m.id)),
            }
        }

        if !probe.tts_bin_present || !probe.tts_runtime_ok() || force {
            log.push(format!(
                "[{}] installing the TTS sidecar binary + runtime",
                m.id
            ));
            match ssh.install_tts_runtime(tts_binary, tts_runtime, live.as_ref()) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log.lines);
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
            match ssh.install_models(&layout.root, live.as_ref()) {
                Ok(v) => {
                    models_pushed = true;
                    log.push(format!("[{}] {v}", m.id));
                }
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log.lines);
                }
            }
        }
    }

    // The cluster token: what lets this worker tell its own inductor from
    // anything else that can reach its port. Shipped here rather than passed at
    // launch so the secret never appears in `argv` (and so a rotated token
    // reaches the box without a manual step). A worker started with
    // `--serve-tasks` refuses to run without it.
    match crate::token::read(&layout.root) {
        Some(token) => match ssh.write_cluster_token(&token) {
            Ok(()) => log.push(format!(
                "[{}] cluster token {} (owner-only on the box)",
                m.id,
                &token[..8.min(token.len())]
            )),
            Err(e) => log.push(format!("[{}] cluster token failed: {e}", m.id)),
        },
        None => log.push(format!(
            "[{}] no cluster token on this inductor — `serve` generates one; a worker started with --serve-tasks will refuse to run until it does",
            m.id
        )),
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
            "[{}] no local profile pointer — load one first (`:profile` in the dashboard, or `tools/profile.sh unpack <name>`), or this worker will refuse to start",
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

    match ssh.ensure_opencode(installs) {
        Ok(v) => log.push(format!("[{}] {v}", m.id)),
        Err(e) => log.push(format!("[{}] opencode check failed: {e}", m.id)),
    }

    // The merge stage's encoder. Installed here so a fresh box can merge; a
    // refusal is a warning, and the worker reports no `merge` capability
    // (so the scheduler simply never offers it one) rather than failing
    // three merges and shelving chapters.
    match ssh.ensure_ffmpeg(installs) {
        Ok(v) if v.starts_with("FFMPEG-OK") => log.push(format!("[{}] {v}", m.id)),
        Ok(v) => log.push(format!(
            "[{}] {v} — merge stays disabled on this box until ffmpeg is present (apt/dnf install ffmpeg), then force a re-provision",
            m.id
        )),        Err(e) => log.push(format!("[{}] ffmpeg install check failed: {e}", m.id)),
    }
    // The sidecar loads its voice roster at startup. A models push therefore
    // has to recycle an already-running sidecar; otherwise the new store is
    // present on disk but the process keeps serving the old 66-voice roster.

    if models_pushed {
        match ssh.stop_tts() {
            Ok(()) if probe.tts_up => {
                log.push(format!("[{}] stopped TTS to reload the voice store", m.id))
            }
            Ok(()) => log.push(format!("[{}] cleared stale TTS process", m.id)),
            Err(e) => log.push(format!(
                "[{}] could not stop TTS for voice reload: {e}",
                m.id
            )),
        }
    }

    // Waits for ready, so this line is a fact and not a hope — see `start_tts`.
    // A box still loading after the budget is *not* held back here: readiness
    // is about the binary and the weights (`configured`), and the worker's own
    // `ensure` now waits for a loading server instead of racing it. Making
    // `configured` depend on a live sidecar was considered and rejected: it
    // would deny a registered box over a sidecar restart and drag
    // `may_install` into re-running package installs on a healthy cluster.
    match ssh.start_tts() {
        Ok(v) if v.starts_with("TTS-STARTING") => log.push(format!(
            "[{}] {v} — the worker will wait for it rather than start a second one; re-run the probe if renders are slow to begin",
            m.id
        )),
        Ok(v) => log.push(format!("[{}] tts: {v}", m.id)),
        Err(e) => log.push(format!("[{}] could not start tts: {e}", m.id)),
    }

    // Write provision stamp so subsequent runs can skip
    let _ = ssh.write_provision_stamp(&local_stamp);

    // Re-probe so the caller records the post-provision truth.
    let after = ssh.probe();
    log.push(format!("[{}] after provision: {}", m.id, after.summary()));
    // Named on its own line, not just inside the summary: a merge offered to
    // this box fails after a full render lease, and the operator's next stop is
    // this log. The fix is one package manager away on every platform.
    if !after.ffmpeg_present {
        log.push(format!(
            "[{}] ffmpeg is not on PATH — this box can crawl/digest/render but every merge it is offered will fail; install it (apt install ffmpeg / dnf install ffmpeg) and force a re-provision",
            m.id
        ));
    }
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
    (after, log.lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_log_streams_a_copy_and_keeps_the_lines() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut log = LiveLog::new(Some(tx));
        log.push("first".into());
        log.push("second".into());
        assert_eq!(log.lines, vec!["first", "second"]);
        assert_eq!(rx.try_recv().unwrap(), "first");
        assert_eq!(rx.try_recv().unwrap(), "second");
        // Collect-only mode: no sender, no panic, lines still kept.
        let mut quiet = LiveLog::new(None);
        quiet.push("only".into());
        assert_eq!(quiet.lines, vec!["only"]);
    }

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
    fn a_new_voice_forces_a_model_store_push_on_an_already_configured_box() {
        let local = ProvisionStamp {
            tts_hash: "new-store".into(),
            ..Default::default()
        };
        let same = ProvisionStamp {
            tts_hash: "new-store".into(),
            ..Default::default()
        };
        let old = ProvisionStamp {
            tts_hash: "old-store".into(),
            ..Default::default()
        };
        assert!(!models_need_push(Some(&same), &local, false));
        assert!(models_need_push(Some(&old), &local, false));
        assert!(models_need_push(None, &local, false));
        assert!(models_need_push(Some(&same), &local, true));
    }

    #[test]
    fn a_matching_stamp_cannot_hide_a_missing_voice() {
        let manifest = [("Narrator".to_string(), "refs/narrator.wav".to_string())]
            .into_iter()
            .collect();
        assert!(voice_store_covers(
            &["Narrator".into(), "Đức Trí".into()],
            &manifest
        ));
        assert!(!voice_store_covers(&["Đức Trí".into()], &manifest));
    }

    #[test]
    fn only_a_fresh_or_forced_provision_may_install() {
        // The decision the two `ensure_*` call sites read. A configured box
        // that nobody forced is the case that made a healthy cluster's `B`
        // cost minutes: it re-ran package installs that could not change.
        assert!(may_install(false, false), "a fresh box installs");
        assert!(may_install(false, true), "force on a fresh box installs");
        assert!(may_install(true, true), "force re-installs on purpose");
        assert!(
            !may_install(true, false),
            "a box that already has everything is checked, not reinstalled"
        );
    }

    #[test]
    fn a_configured_box_is_checked_but_never_re_installed() {
        // The waste this exists to stop: `ensure_opencode` can spend ten
        // minutes in `npm i`, and it ran on *every* provision of *every* box —
        // including the ones whose answer was not going to change. A catch-up
        // on a working cluster is a verification, so the install half is
        // reserved for a fresh or forced provision.
        for script in [opencode_script(false), ffmpeg_script(false)] {
            assert!(
                script.contains("command -v"),
                "the check must survive: {script}"
            );
            assert!(
                !script.contains("npm i"),
                "a configured box must not re-run npm: {script}"
            );
            assert!(
                !script.contains("install -y"),
                "a configured box must not chase the package manager: {script}"
            );
            assert!(
                script.contains("force a re-provision"),
                "and it must name the way out: {script}"
            );
        }
        // The full path keeps both halves: a fresh box still gets them.
        assert!(opencode_script(true).contains("npm i -g"));
        assert!(ffmpeg_script(true).contains("install -y ffmpeg"));
        // A box that already has the tool short-circuits in *both* flavours —
        // the flag only decides what happens when it is missing.
        for script in [opencode_script(true), opencode_script(false)] {
            assert!(script.contains(r#"command -v opencode >/dev/null 2>&1 && { echo "OPENCODE-OK (present)"; exit 0; }"#));
        }
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
    fn a_box_without_ffmpeg_says_so_before_a_merge_fails() {
        // The failure this prevents: a box provisions cleanly, is offered a
        // merge, and shelves the chapter after a full render lease — with
        // nothing anywhere saying the tool was missing.
        let present = Probe {
            reachable: true,
            hostname: "box".into(),
            ffmpeg_present: true,
            ..Default::default()
        };
        assert!(
            !present.summary().contains("FFMPEG"),
            "{}",
            present.summary()
        );
        let absent = Probe {
            ffmpeg_present: false,
            ..present
        };
        assert!(
            absent.summary().contains("NO FFMPEG"),
            "{}",
            absent.summary()
        );
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

    #[test]
    fn a_linux_binary_without_its_runtime_is_not_ready() {
        // Wolf's box: binary + models present, libonnxruntime never landed.
        // The old gate read that as configured, so `:prov` confirmed a
        // sidecar that dies on startup instead of repairing it.
        let mut p = Probe {
            reachable: true,
            agent_version: Some("0.2.0".into()),
            tts_bin_present: true,
            models_present: true,
            os: "linux".into(),
            ..Default::default()
        };
        assert!(!p.tts_runtime_ok());
        assert!(!p.rust_ready());
        assert!(!p.configured("0.2.0"));
        assert_eq!(p.sidecar(), "none");
        p.tts_lib_present = true;
        assert!(p.configured("0.2.0"));
        // macOS links statically: no lib, still ready.
        p.os = "macos".into();
        p.tts_lib_present = false;
        assert!(p.configured("0.2.0"));
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
