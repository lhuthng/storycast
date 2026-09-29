use anyhow::{Context, Result};
use bm_proto::Machine;
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::ssh::{RsyncProgress, Ssh};
use super::stamp::{compute_provision_stamp, parse_stamp, ProvisionStamp};
use super::{REMOTE_DIR, TTS_PORT};

/// The line the box is asked to run, and the exit codes it answers with.
///
/// Split out of the ssh call so both halves are testable without one: the
/// script says *what* is asked for, [`classify_fetch`] says what an answer
/// means, and neither needs a box, a network or a `$HOME` to point somewhere
/// temporary.
///
/// The codes are **20 and 21, not 2 and 3**, because `clap` exits 2 on a usage
/// error — and the subcommand it does not recognise is exactly what an agent
/// predating `fetch-artifact` answers. That has to read as *absence* (fall back
/// to the push), never as *corrupt bytes* (stop), so the codes sit above
/// everything the toolchain emits on its own and anything unrecognised is
/// treated as absence.
const EXIT_LANDED: i32 = 0;
const EXIT_CORRUPT: i32 = 20;
const EXIT_UNREACHABLE: i32 = 21;

fn fetch_script(release: &crate::artifact::ModelsRelease, engine: &str) -> String {
    format!(
        "~/bm-worker/bm-agent fetch-artifact {url} ~/bm-worker/{rel}/models --expect {hash}",
        url = shell_quote(&release.url),
        rel = engine_rel(engine),
        hash = release.hash,
    )
}

/// The same call for a profile pack, which differs in exactly two things: it
/// lands `assets/` rather than the engine's tree, and it says which prefix of
/// the bundle to keep — because a pack bundle is a manifest **beside** an
/// `assets/` subtree, and swapping the worker root in to hold both would take
/// the prompts and the casts with it.
///
/// `--strip-prefix` rather than a second subcommand, because the delivery is
/// identical work: download beside the destination, verify against the manifest
/// that travelled in the same archive, swap in two renames. Only the shape of
/// the archive differs, and a second verb would be a second copy of the check
/// that has to be right.
fn pack_fetch_script(release: &crate::artifact::PackRelease) -> String {
    format!(
        "~/bm-worker/bm-agent fetch-artifact {url} ~/bm-worker/{dir} --expect {hash} --strip-prefix {dir}",
        url = shell_quote(&release.url),
        dir = crate::artifact::PACK_DIR,
        hash = release.hash,
    )
}

/// The engine's own tree on a worker, relative to the worker root:
/// `engines/<name>`.
///
/// One spelling, so the push, the release fetch, the checksum list and the
/// launch script cannot disagree about where the weights landed. It mirrors
/// `Layout::engine_dir` on the far side of the ssh, where there is no `Layout`
/// — the worker root is `$HOME/{REMOTE_DIR}` and the engine is a string.
fn engine_rel(engine: &str) -> String {
    format!("{}/{}", crate::paths::ENGINES_DIR, engine)
}

/// Read a fetch's exit code as the contract it is.
///
/// `0` landed, `20` is corruption and `21` is absence, and the difference is the
/// whole design: `21` falls back to the push, `20` never does. Anything else — a
/// box too old to have the subcommand, an ssh that died mid-run — is treated as
/// absence, because the push is a real answer to "I could not fetch it" and
/// only *wrong bytes* are a reason to stop.
fn classify_fetch(
    code: i32,
    stdout: &str,
    stderr: &str,
    tag: &str,
    what: &str,
) -> std::result::Result<String, FetchOutcome> {
    let said = |fallback: &str| {
        let t = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        crate::util::head_chars(if t.is_empty() { fallback } else { &t }, 200)
    };
    match code {
        EXIT_LANDED => Ok(format!(
            "{what} from the release {tag} ({})",
            crate::util::head_chars(stdout.trim(), 120)
        )),
        EXIT_CORRUPT => Err(FetchOutcome::Corrupt(said("no detail"))),
        EXIT_UNREACHABLE => Err(FetchOutcome::Unreachable(said("no detail"))),
        other => Err(FetchOutcome::Unreachable(format!(
            "bm-agent fetch-artifact exited {other} ({})",
            said("no detail")
        ))),
    }
}

/// The one member of `models/` a push must leave behind.
///
/// `models.tar.zst` is the *transfer* artifact: 380 MB that stands for the
/// 668 MB of weights next to it. A box that is being pushed the directory has
/// no use for a second copy of the same tree, and a box that fetched a bundle
/// has the extracted one — so the push skips it in both directions, which is
/// also what keeps `--delete` from putting one back.
const MODELS_PUSH_EXCLUDES: &[&str] = &["/models.tar.zst"];

/// How a release fetch ended, in the two shapes the caller must treat
/// differently.
#[derive(Debug)]
enum FetchOutcome {
    /// The artifact was not there. The push is the answer.
    Unreachable(String),
    /// Bytes arrived and are not the ones asked for. Stop.
    Corrupt(String),
}

/// Single-quote a value for the remote `sh -c`.
///
/// The release URL is built from a configured `owner/name` and a hash, so it is
/// already restricted to characters no shell would read as syntax — but the
/// command is assembled as a string and handed to `sh`, and a helper that only
/// works for the input it happens to get today is not a helper.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

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
    /// it dynamically, a binary without it dies on startup, while macOS
    /// links statically and never has it, so readiness only demands it there.
    #[serde(default)]
    pub tts_lib_present: bool,
    /// A baked `~/{REMOTE_DIR}/models/`, the Rust sidecar's weights.
    #[serde(default)]
    pub models_present: bool,
    /// `ffmpeg` on PATH. The merge stage shells out to it, so a box without it
    /// provisions cleanly and then fails every merge it is offered, a strike
    /// and a shelved chapter instead of a report. Reported, not gated: merge is
    /// a small share of the work and refusing the box outright would cost more
    /// than it saves.
    #[serde(default)]
    pub ffmpeg_present: bool,
    /// The sidecar's roster, as `/voices` sends it: **labels, not names**
    /// `"<name>, <description>"` for a voice with a description (every preset)
    /// and the bare name for one without (every enrolled clone).
    ///
    /// It was documented as "enrolled clone-voice names", which is a guarantee
    /// the transport does not keep, and the one consumer that compared these
    /// against a list of names read all 23 shipped presets as undeclared. Use
    /// [`crate::voices::voice_name`] before comparing anything here with a name.
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
            " · NO FFMPEG, merges will fail here"
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
/// install is bounded at ten minutes, spent on every `B` press of a healthy
/// cluster, which is what made a catch-up look like a hang. Yes otherwise,
/// including on `force`, which is the operator explicitly asking for the slow
/// path.
fn may_install(configured: bool, force: bool) -> bool {
    force || !configured
}

/// Whether the model/voice store must be pushed even when the worker already
/// has the right agent and sidecar.
///
/// **Two digests, because `models/` holds two kinds of thing.** The immutable
/// weights are `tts_hash`; the mutable voice roster, `models/voices.json`, the
/// file enrollment rewrites, is `voices_hash`. Checking only the first is what
/// once let an enrolled voice sit on this disk while every log said "in sync";
/// checking only the second would hide a re-bake behind a 492 KB file.
fn models_need_push(remote: Option<&ProvisionStamp>, local: &ProvisionStamp, force: bool) -> bool {
    force || !remote.is_some_and(|stamp| stamp.tts_in_sync(local) && stamp.voices_in_sync(local))
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

/// The `sha256sum -c` lines for a baked `models/`, taken from its own manifest.
///
/// **`models/voices.json` is excluded.** The manifest's entry for it is stale by
/// design, enrollment rewrites the file after the bake, and the recorded hash
/// describes the pre-enrollment bytes, so verifying it would fail every
/// provision of a box that had ever enrolled a voice. Excluding it is the same
/// immutable/mutable split the stamp's `tts_hash` makes, applied to the one
/// other place this file is described.
///
/// Empty means "nothing to verify": a manifest that is missing, unparseable, or
/// describing no files. Never a failure in itself, the `manifest.json`
/// existence check in `install_models` is what refuses an incomplete bake.
fn model_checksums(models_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(models_dir.join("manifest.json")) else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(files) = doc.get("files").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = files
        .iter()
        .filter(|(name, _)| name.as_str() != "voices.json")
        .filter_map(|(name, entry)| {
            let hash = entry.get("sha256")?.as_str()?;
            Some(format!("{hash}  {name}"))
        })
        .collect();
    out.sort();
    out
}

/// The `opencode` step, in two flavours.
///
/// Both start with the same `command -v`: a box that has it pays one round
/// trip either way. The difference is what happens when it does not. On a
/// fresh or forced provision the box is chased with `npm i -g` (bounded at ten
/// minutes, and genuinely needed for the digest lane). On a box that already
/// passed a full provision it is reported instead, the answer is not going to
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
echo "OPENCODE-SKIP (already configured, not reinstalling; force a re-provision to try again)""#
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
  echo "FFMPEG-SKIP (no known package manager, install ffmpeg by hand)"; exit 0
fi
if command -v ffmpeg >/dev/null 2>&1; then echo "FFMPEG-OK (installed)"; else echo "FFMPEG-SKIP (install refused, needs sudo? run: sudo apt-get install -y ffmpeg)"; fi
"#
            .into()
    } else {
        r#"if command -v ffmpeg >/dev/null 2>&1; then echo "FFMPEG-OK (present)"; exit 0; fi
echo "FFMPEG-SKIP (already configured, not reinstalling; force a re-provision to try again)""#
            .into()
    }
}

/// The `zstd` step: the sources bundle is `tar` + `zstd`, so every box that
/// takes sources needs the decompressor.
///
/// **The one install that is attempted on an already-configured box**, and the
/// exception is the point. What the `allow_install` rule protects is *minutes* —
/// `npm i -g` is bounded at ten of them, ffmpeg is tens of megabytes — and zstd
/// is a one-second, ~1 MB package that the very next push cannot proceed
/// without. A box provisioned before bundles existed would otherwise fail its
/// first re-provision with an error it can do nothing about, which is the
/// failure mode this whole module is written to avoid.
fn zstd_script() -> String {
    r#"export DEBIAN_FRONTEND=noninteractive
command -v zstd >/dev/null 2>&1 && { echo "ZSTD-OK (present)"; exit 0; }
install() { $1 >/dev/null 2>&1; }
if command -v apt-get >/dev/null 2>&1; then
  sudo -n apt-get install -y zstd >/dev/null 2>&1 || install "apt-get install -y zstd"
elif command -v dnf >/dev/null 2>&1; then
  sudo -n dnf install -y zstd >/dev/null 2>&1 || install "dnf install -y zstd"
elif command -v yum >/dev/null 2>&1; then
  sudo -n yum install -y zstd >/dev/null 2>&1 || install "yum install -y zstd"
elif command -v brew >/dev/null 2>&1; then
  install "brew install zstd"
else
  echo "ZSTD-SKIP (no known package manager, install zstd by hand)"; exit 0
fi
if command -v zstd >/dev/null 2>&1; then echo "ZSTD-OK (installed)"; else echo "ZSTD-SKIP (install refused, needs sudo? run: sudo apt-get install -y zstd)"; fi
"#
    .into()
}

impl Ssh {
    /// Ask a machine what it already has.
    pub fn probe(&self, engine: &str) -> Probe {
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
if [ -x "$HOME/{dir}/{rel}/bm-tts" ]; then
  echo "tts_bin=present"
else
  echo "tts_bin=absent"
fi
if [ -f "$HOME/{dir}/{rel}/libonnxruntime.so.1" ]; then
  echo "tts_lib=present"
else
  echo "tts_lib=absent"
fi
if [ -f "$HOME/{dir}/{rel}/models/manifest.json" ]; then
  echo "models=present"
else
  echo "models=absent"
fi
if command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg=present"
else
  echo "ffmpeg=absent"
fi
# The roster the sidecar is *serving*, asked of the process that owns the
# answer rather than re-parsed out of the file it loaded. Two things fall out
# of that: the list describes what a render will actually find, not what a file
# says it should; and the probe's last `python3` is gone, which is what used to
# let a stock box be described as needing Python. `-f` makes a 503 (still
# loading) an empty answer instead of the error body read as a voice name.
#
# The octet 037 separator is `chr(31)`, which the Rust side splits on.
echo "voices=$(curl -s -f --max-time 3 http://127.0.0.1:{port}/voices 2>/dev/null | tr -d '[]"' | tr ',' '\037')"
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
            rel = engine_rel(engine),
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
                            // Names contain spaces ("Minh Triết"), the probe
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

    /// Create the worker root skeleton, including the bound engine's own tree.
    pub fn ensure_root(&self, engine: &str) -> Result<()> {
        let script = format!(
            "mkdir -p $HOME/{d}/{rel} $HOME/{d}/prompts $HOME/{d}/assets/effects \
             $HOME/{d}/assets/music $HOME/{d}/assets/injects $HOME/{d}/data/chapters \
             $HOME/{d}/data/audio $HOME/{d}/crawl $HOME/{d}/output && echo READY",
            d = REMOTE_DIR,
            rel = engine_rel(engine)
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

    /// Push the files this box's policy needs, as one bundle.
    ///
    /// Replaces three whole-tree rsyncs (`prompts/`, `assets/`, `refs/`) — 202 MB
    /// per box on this repo, 144 MB of it `refs/`, which no worker reads. What
    /// travels instead is the set `super::sources` selects: the registries a
    /// stage opens, the clips they register, the prompts a digest reads, the
    /// crawlers a crawl resolves, and the cast files the legacy paths fall back
    /// to. See that module for the per-stage table and why `refs/` is in none of
    /// it.
    ///
    /// The bundle is **named by its own manifest digest**, which is also the
    /// stamp's `sources_hash`. That is what makes the cache safe to keep: a file
    /// at that name is by definition the artifact this plan would produce, so
    /// there is no mtime to compare and nothing to rebuild.
    ///
    /// Delivery is a *replacement*, not a merge: the box prunes the trees the
    /// archive owns and extracts over them, which is the `--delete` semantics
    /// the old rsyncs had, plus the one-time removal of the `refs/` tree earlier
    /// versions left on every configured box.
    pub fn install_sources(
        &self,
        layout: &crate::Layout,
        stages: &[bm_proto::Stage],
        pack: Option<&crate::artifact::PackRelease>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Vec<String>> {
        let plan = super::sources::Sources::plan_for(layout, stages, pack)?;
        let manifest = plan.manifest()?;
        let hash = super::sources::Sources::hash(&manifest);
        let dir = layout.root.join(".bm").join("sources");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let bundle = dir.join(format!("{hash}.tar.zst"));
        if !bundle.is_file() {
            plan.pack(&manifest, &bundle)?;
        }

        let mut lines = vec![format!(
            "sources: {} -> {}",
            plan.summary(),
            super::sources::BUNDLE_NAME
        )];
        // A registry naming a clip that is not on disk here is worth a line: the
        // merge degrades that one sound to silence with its own warning, and an
        // operator reading a provision log is the last person who can still fix
        // it cheaply. Bounded, because a pool with a moved directory would
        // otherwise fill the pane.
        //
        // With a fetched pack the warning is about **this** tree, not the box's,
        // and the box may well have the clip — so it says so rather than
        // sending the operator to re-push a bundle that no longer carries it.
        for missing in plan.missing.iter().take(5) {
            lines.push(match pack {
                Some(_) => format!(
                    "registry names a clip this checkout has lost (the release pack may still have it): {missing}"
                ),
                None => format!(
                    "registry names a clip that is not here (the merge goes silent for it): {missing}"
                ),
            });
        }
        if plan.missing.len() > 5 {
            lines.push(format!(
                "…and {} more missing clip(s)",
                plan.missing.len() - 5
            ));
        }

        self.rsync_push_plain(
            &bundle,
            super::sources::BUNDLE_NAME,
            progress(live, "sources"),
        )?;
        let (code, stdout, stderr) = self.run(&super::sources::extract_script(), 600)?;
        if code != 0 {
            let hint = if stderr.contains("zstd") || stdout.contains("zstd-missing") {
                " — install zstd on the box (apt install -y zstd)"
            } else {
                ""
            };
            anyhow::bail!(
                "unpacking the sources bundle failed (exit {code}){hint}: {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        if !stdout.contains("SOURCES-OK") {
            anyhow::bail!(
                "the box did not confirm the bundle: {}",
                crate::util::head_chars(stdout.trim(), 200)
            );
        }
        lines.push(format!(
            "sources in sync ({} @ {})",
            &hash[..12.min(hash.len())],
            if plan.stages.is_empty() {
                "no stage".to_string()
            } else {
                plan.stages
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("+")
            }
        ));

        // **After** the extract, never before. The bundle's own delivery is a
        // *replacement* — it prunes every tree it owns, `assets/` among them,
        // and that is the right thing for it to do. A pack landing in front of
        // it would be deleted by the step that follows, and the box would come
        // up with a profile and no assets, which is the one combination nothing
        // downstream reports as an error.
        if let Some(r) = pack {
            lines.push(self.install_pack(layout, r, live)?);
        }

        // One bundle per policy, kept as a cache; anything a day old goes. Age
        // rather than "everything but mine": provisions run one per machine and
        // several can be in flight, so a sweep by name could delete the artifact
        // another push is about to send.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let now = std::time::SystemTime::now();
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                if p == bundle || p.extension().and_then(|x| x.to_str()) != Some("zst") {
                    continue;
                }
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .map(|age| age.as_secs() > 24 * 3600)
                    .unwrap_or(false);
                if old {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(lines)
    }

    /// Get the profile pack onto the box: from its release, or over the push.
    ///
    /// **Why a pack is worth publishing at all.** It is 60-odd MB of mp3 and
    /// JSON that is byte-identical on every machine and changes only when the
    /// operator publishes a new profile — the same argument that took the
    /// weights off the uplink. Before this, a fresh box paid for the whole
    /// profile over the operator's connection, once per box, for content that
    /// was already on a CDN at a hash it could check.
    ///
    /// **The split of failures is the models one, and for the same reason.**
    /// *Unreachable* — no such release, no route, a GitHub incident, an agent
    /// too old to have the verb — falls back to the push and says so in the
    /// log. *Corrupt* stops: bytes arrived and disagree with the hash of the
    /// profile this cluster is running, and pushing the local tree over the top
    /// would paper over a disagreement that is exactly what the check is for.
    ///
    /// What the box ends up with is the same either way. The release is
    /// verified against the *live* tree's hash, so a pushed `assets/` and a
    /// fetched one are the same bytes — which is why the stamp records the
    /// release hash on a box that took the push, and why the next provision
    /// does not try to fetch what is already there.
    pub fn install_pack(
        &self,
        layout: &crate::Layout,
        release: &crate::artifact::PackRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        match self.fetch_pack(release, live) {
            Ok(line) => Ok(line),
            Err(FetchOutcome::Corrupt(e)) => {
                anyhow::bail!(
                    "the {} release artifact does not verify and was not put in place: {e} \
                     (nothing was changed on this box; this checkout's assets/ is still the live \
                     pack '{}', so re-publish it under the same tag with \
                     `tools/profile.sh pack {name} --version {version}` then \
                     `gh release upload {tag} profiles/pack/{name}.tar.zst --clobber`, \
                     or clear `packs_release` to push the directory)",
                    release.tag,
                    release.name,
                    name = release.name,
                    version = release.version,
                    tag = release.tag,
                )
            }
            Err(FetchOutcome::Unreachable(e)) => {
                if let Some(l) = live {
                    let _ = l.send(format!(
                        "[{}] release {} unreachable ({}), pushing assets/ instead",
                        self.target, release.tag, e
                    ));
                }
                self.push_pack(layout)?;
                Ok(format!(
                    "pack {} v{} over the push ({})",
                    release.name,
                    release.version,
                    crate::util::head_chars(&e, 80)
                ))
            }
        }
    }

    /// Push the TTS sidecar binary and the shared ONNX Runtime it links.
    ///
    /// Replaces `ensure_python`, which built a 647 MB virtualenv on every
    /// worker. Two files and a symlink do the same job now.
    ///
    /// The library is pushed under **every** name it is known by, the linker
    /// wants the plain `libonnxruntime.so`, the loader wants the SONAME
    /// `libonnxruntime.so.1`, and the versioned file is what those two point at.
    /// Shipping only one of them produces a failure that names none of this.
    ///
    /// `runtime_dir` is `None` where the sidecar is self-contained (macOS
    /// links its runtime statically, the native binary runs with no `.so`
    /// beside it), so only the binary travels.
    pub fn install_tts_runtime(
        &self,
        engine: &str,
        tts_binary: &Path,
        runtime_dir: Option<&Path>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        // Beside the engine's weights, not at the root: `bm-tts` is VieNeu's
        // binary, and a second engine ships its own under its own tree.
        let rel = engine_rel(engine);
        self.rsync_push(
            tts_binary,
            &format!("{rel}/bm-tts"),
            false,
            progress(live, "bm-tts"),
        )?;
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
                    "no libonnxruntime.so* in {}, run `make runtime` first",
                    runtime_dir.display()
                );
            }
            for lib in &libs {
                let name = lib.file_name().expect("filtered on a file name");
                let name = name.to_string_lossy();
                self.rsync_push(lib, &format!("{rel}/{name}"), false, progress(live, &name))?;
            }
        }

        // The SONAME has to be reachable *beside the binary* — the engine's
        // directory, which is what `LD_LIBRARY_PATH` names.
        let script = format!(
            r#"set -e
D="$HOME/{d}/{rel}"
cd "$D"
chmod +x bm-tts
LD_LIBRARY_PATH="$D" ./bm-tts --version >/dev/null 2>&1 || \
  {{ echo "bm-tts would not run, missing libonnxruntime.so.1 beside it?" >&2; exit 7; }}
echo "TTS-RUNTIME-OK ($(LD_LIBRARY_PATH="$D" ./bm-tts --version))"
"#,
            d = REMOTE_DIR,
            rel = rel,
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

    /// Ask the box to take the artifact itself, and read the exit code as the
    /// contract `bm-agent fetch-artifact` documents: `0` landed, `2` wrong
    /// bytes, `3` not there.
    ///
    /// The agent is what fetches, not a `curl | zstd | tar` line in a heredoc,
    /// for the reason the artifact exists at all: a shell pipeline cannot verify
    /// seventeen files and then swap a directory in two renames, and a
    /// half-extracted `models/` is the failure this was built to remove. The
    /// agent also carries the decompressor, so the box needs no `zstd` package
    /// and there is no second cross-built binary to version-gate.
    fn fetch_models(
        &self,
        engine: &str,
        release: &crate::artifact::ModelsRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> std::result::Result<String, FetchOutcome> {
        if let Some(l) = live {
            let _ = l.send(format!(
                "[{}] models: fetching {} from the release (a CDN, not this machine's uplink)",
                self.target, release.tag
            ));
        }
        // Bounded well past the transfer: 363 MB on a 200 KB/s link is half an
        // hour, and the timeout is here to catch a wedged ssh rather than to
        // second-guess a slow box.
        let (code, stdout, stderr) = self
            .run(&fetch_script(release, engine), 3600)
            .map_err(|e| FetchOutcome::Unreachable(e.to_string()))?;
        classify_fetch(code, &stdout, &stderr, &release.tag, "models")
    }

    /// Ask the box to fetch the profile pack, and read the answer the same way
    /// as the weights: `0` landed, `21` fall back to the push, `20` stop.
    ///
    /// The timeout is the models one for the same reason, and a pack is the
    /// *smaller* of the two artifacts, so it is not the transfer that is at
    /// risk here — it is an agent predating `fetch-artifact`, which answers
    /// with a usage error and has to read as absence.
    fn fetch_pack(
        &self,
        release: &crate::artifact::PackRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> std::result::Result<String, FetchOutcome> {
        if let Some(l) = live {
            let _ = l.send(format!(
                "[{}] pack: fetching {} v{} from the release (a CDN, not this machine's uplink)",
                self.target, release.name, release.version
            ));
        }
        let (code, stdout, stderr) = self
            .run(&pack_fetch_script(release), 1800)
            .map_err(|e| FetchOutcome::Unreachable(e.to_string()))?;
        classify_fetch(code, &stdout, &stderr, &release.tag, "pack")
    }

    /// Put the live `assets/` on the box, for a box that could not fetch the
    /// release.
    ///
    /// The push half of the same decision, and deliberately a **directory**
    /// push rather than more bundle members: a box whose fetch failed has to
    /// end up with the profile either way, and re-adding those files to
    /// `sources.tar.zst` would mean the operator's uplink pays for them on
    /// every box even when the release works for all the others.
    ///
    /// Excludes `assets/_extends/` for the same reason the release does: those
    /// are composition *inputs*, and a worker resolves a composed pack from the
    /// flattened tree — shipping them would re-fold into a bundle that is
    /// supposed to be content.
    pub fn push_pack(&self, layout: &crate::Layout) -> Result<()> {
        self.rsync_push_excluding(
            &layout.assets(),
            crate::artifact::PACK_DIR,
            true,
            None,
            &["/assets/_extends"],
        )
    }

    /// Put the baked `models/` directory on the box: from the release when one
    /// is configured and reachable, over the push otherwise.
    ///
    /// 668 MB, content-addressed by the stamp's `tts_hash`, so a re-provision
    /// with nothing changed costs nothing either way.
    ///
    /// **A release, when there is one.** The weights are the one payload that
    /// is identical on every machine and changes only when the operator
    /// re-bakes them, so they are the one payload worth naming: the box
    /// downloads `models.tar.zst` from a GitHub release tagged by the manifest
    /// hash and verifies it itself, and 363 MB comes off a CDN instead of this
    /// house's uplink, once per box instead of once per operator. The split of
    /// failures is what makes the fallback safe — *unreachable* (no such
    /// release, no route, a 5xx) falls back to the push below, and says so;
    /// *wrong bytes* stops, because pushing the same wrong bytes again would
    /// only hide the disagreement.
    ///
    /// **What arrives is verified, not assumed.** rsync exiting 0 says the
    /// *transfer* worked, which is weaker than "the weights are intact": a box
    /// that dies mid-push, a corrupt source file, or a `--delete` racing a
    /// writer all leave a directory rsync is happy with and the sidecar is
    /// not. So the bake's own `sha256` entries are written out as a
    /// `sha256sum -c` list and checked on the box, with `models/voices.json`
    /// excluded: the manifest's entry for it is stale by design, because
    /// enrollment rewrites the file after the bake.
    pub fn install_models(
        &self,
        engine: &str,
        src: &Path,
        release: Option<&crate::artifact::ModelsRelease>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        let rel = engine_rel(engine);
        if !src.is_dir() {
            anyhow::bail!(
                "no {}, run the bake first (`python3 tools/bake-models.py`)",
                src.display()
            );
        }
        if let Some(r) = release {
            match self.fetch_models(engine, r, live) {
                Ok(line) => {
                    // The checksum list is the box's own statement about its
                    // install, and the fetch has already verified every file
                    // against the same manifest, so it is written either way:
                    // a later push of a *different* bake then has a list to
                    // check against.
                    let sums = model_checksums(src);
                    if !sums.is_empty() {
                        self.write_model_checksums(engine, &sums)?;
                    }
                    return Ok(line);
                }
                Err(FetchOutcome::Corrupt(e)) => {
                    // Not a fallback trigger. The bytes disagreed with the
                    // manifest the operator's own bake declares, and pushing
                    // them would be the same bytes with a worse provenance.
                    anyhow::bail!(
                        "the release artifact does not verify and was not put in place: {e} \
                         (nothing was changed on this box; re-pack with tools/models.sh pack \
                         and re-publish, or clear `models_release` to push the directory)"
                    );
                }
                Err(FetchOutcome::Unreachable(e)) => {
                    if let Some(l) = live {
                        let _ = l.send(format!(
                            "[{}] release {} unreachable ({}), pushing models/ instead",
                            self.target, r.tag, e
                        ));
                    }
                }
            }
        }
        // `models.tar.zst` stays behind: it is the transfer artifact, and a box
        // that is being pushed the directory has no use for a second copy of
        // the same 668 MB. Excluding it also keeps `--delete` from putting one
        // back on a box that fetched a bundle.
        self.rsync_push_excluding(
            src,
            &format!("{rel}/models"),
            true,
            progress(live, "models"),
            MODELS_PUSH_EXCLUDES,
        )?;
        let sums = model_checksums(src);
        if !sums.is_empty() {
            self.write_model_checksums(engine, &sums)?;
        }
        // `sha256sum` is coreutils, so it is present on every platform these
        // workers run, but "present" is assumed rather than proved, and a box
        // without it is reported rather than silently reported as verified.
        let script = format!(
            r#"D="$HOME/{d}/{rel}/models"
[ -f "$D/manifest.json" ] || {{ echo "models/manifest.json missing, incomplete bake" >&2; exit 8; }}
n=$(ls "$D" | wc -l)
L="$HOME/{d}/{rel}/models.sha256"
if ! command -v sha256sum >/dev/null 2>&1; then
  echo "MODELS-OK ($n files, NOT verified, sha256sum absent)" >&2
  exit 0
fi
if [ -f "$L" ]; then
  if ! out=$(cd "$D" && sha256sum -c "$L" 2>&1); then
    echo "$out" | grep -v ': OK$' | head -n 5 >&2
    echo "MODELS-CORRUPT, the weights here do not match the bake; nothing was rendered with them" >&2
    exit 10
  fi
  echo "MODELS-OK ($(wc -l < "$L") files verified)"
else
  echo "MODELS-OK ($n files, no checksum list)" >&2
fi
"#,
            d = REMOTE_DIR,
            rel = rel,
        );
        let (code, stdout, stderr) = self.run(&script, 600)?;
        if code != 0 {
            anyhow::bail!(
                "installing models failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        Ok(stdout.trim().to_string())
    }

    /// Write the `sha256sum -c` list the verify step reads.
    ///
    /// Generated here, from the bake's own declaration, rather than derived on
    /// the box: the box has no JSON parser left (the probe's `python3` is gone),
    /// and a shell re-parse of a single-line 492 KB document is exactly the kind
    /// of thing that works until a voice is named with a brace in it.
    ///
    /// It lives at the worker root, not inside `models/`, because that push
    /// carries `--delete` and would eat it.
    fn write_model_checksums(&self, engine: &str, sums: &[String]) -> Result<()> {
        let body = sums.join("\n");
        let script = format!(
            "cat > $HOME/{d}/{rel}/models.sha256 << 'EOF'\n{body}\nEOF\n",
            d = REMOTE_DIR,
            rel = engine_rel(engine),
        );
        let (code, _, stderr) = self.run(&script, 10)?;
        if code != 0 {
            anyhow::bail!("failed to write the model checksum list: {}", stderr.trim());
        }
        Ok(())
    }

    /// Best-effort opencode install for the digest lane. Auth stays manual
    /// (browser login); without it remote digests fail loudly, never silently.
    pub fn ensure_opencode(&self, allow_install: bool) -> Result<String> {
        // `allow_install` is false on a box that already passed a full
        // provision. The check stays, it is one `command -v`, but the install
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
    /// platform's package manager when it is missing, never a hard failure:
    /// a refused install (no sudo, an offline mirror) only warns, and the
    /// worker's `merge` capability gate keeps merge off this box until ffmpeg
    /// appears. Returns a one-line verdict for the provision log.
    ///
    /// `allow_install` carries the same meaning as on [`Self::ensure_opencode`]:
    /// a present `ffmpeg` short-circuits either way, so the flag only decides
    /// whether a *missing* one is chased with a package manager this time.
    pub fn ensure_zstd(&self) -> Result<String> {
        let (code, stdout, stderr) = self.run(&zstd_script(), 300)?;
        if code != 0 {
            return Ok(format!(
                "ZSTD-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }

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
    /// answer yet. A short sleep there made the worker's first render spawn a
    /// **second** model on an 8 GiB box: the OOM this cluster kept taking.
    ///
    /// A ready server answers 200; a loading one answers 503, which `curl`
    /// reports as success unless told otherwise, so the check is on the
    /// *code*. The budget is deliberately under the ssh call's own timeout
    /// (240 s of polling, 300 s allowed).
    pub fn start_tts(&self, engine: &str) -> Result<String> {
        // The engine's tree holds the binary, its runtime and its weights; the
        // log and the pid stay at the worker root, where `stop_tts` reads the
        // pid and where an operator goes looking for the log.
        //
        // `--dict` is emitted only when the engine declares a lexicon, so a
        // second engine is never handed VieNeu's and asked to mispronounce
        // through it.
        let dict = match crate::voices::dictionary(engine) {
            Some(name) => format!("--dict \"$E/models/{name}\" "),
            None => String::new(),
        };
        let script = format!(
            r#"D="$HOME/{d}"
E="$D/{rel}"
if [ "$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/health)" = "200" ]; then
  echo "TTS-ALREADY-UP"; exit 0
fi
cd "$E" || exit 5
LD_LIBRARY_PATH="$E" nohup "$E/bm-tts" --models "$E/models" --codec "$E/models" \
  {dict}--voices "$E/models/voices.json" \
  --port {port} --bind 0.0.0.0 > "$D/tts.log" 2>&1 &
echo $! > "$D/tts.pid"
for _ in $(seq 1 120); do
  sleep 2
  [ "$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/health)" = "200" ] && {{ echo "TTS-STARTED"; exit 0; }}
done
echo "TTS-STARTING (not ready after 240s, check $D/tts.log)"
"#,
            d = REMOTE_DIR,
            rel = engine_rel(engine),
            dict = dict,
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
    /// `argv` is visible in `ps` on every box it was typed on, and it is
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
    /// file, but without it the worker cannot tell a complete profile from
    /// a half-rsynced one, which is exactly what `verify` refuses to run on.
    ///
    /// **The whole binding, not the pack alone.** It used to be the pack
    /// `Pointer` — the shape from before the split — and the consequence was
    /// silent: a box that read `{name, hash}` resolves `adapter` and `engine`
    /// to their defaults, so it keyed `cast-*` and `segments-*` under `default`
    /// while this inductor keyed them under `vi-VN`. Nothing noticed while
    /// segment files travelled by name, so the cost was a re-render nobody
    /// asked for; the day a stage reads a cast on the box, it is a chapter
    /// spoken from the wrong roster. The offer carries the binding too (it is
    /// the authority for the task in hand), and this is the same answer for the
    /// box's *own* runs, which have no offer to read.
    pub fn write_profile_pointer(&self, binding: &crate::profile::Binding) -> Result<()> {
        let json = serde_json::to_string_pretty(binding)?;
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
/// **Each store entry is a roster *label*, not a name**, because that is what
/// the sidecar's `/voices` sends: `"<name>, <description>"` for a voice that
/// has a description (every preset, this is what the operator reads in the
/// picker) and the bare name for one that does not (every enrolled clone). So
/// the label is split through [`crate::voices::voice_name`] before any of the
/// three comparisons, and the *name* is what comes back: "Thái Sơn" is the thing
/// to add to a manifest, not a 40-character description of it.
///
/// Reported, never deleted: erasing a voice the cast uses would break renders.
/// The fix is named in the warning, declare it in `voices.json` (with its
/// `refs/` clip) or drop it from the store.
pub fn undeclared_voices(
    store: &[String],
    manifest: &std::collections::HashMap<String, String>,
    pool: &crate::pool::Pool,
    catalogue: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = store
        .iter()
        .map(|label| crate::voices::voice_name(label))
        .filter(|name| {
            !name.starts_with('_')
                && !manifest.contains_key(name.as_str())
                && !pool.contains_key(name.as_str())
                && !catalogue.iter().any(|c| c == name)
        })
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
    // `owner/name` of the releases that host the model artifact. `None` reads
    // it from this workspace's `settings.json`, which is where the operator
    // sets it; a one-shot provision passes it to override.
    release_repo: Option<&str>,
) -> (Probe, Vec<String>) {
    let ssh = Ssh::for_machine(m);
    // Cloned, not moved: the install steps below borrow the original for
    // their byte-progress streams.
    let mut log = LiveLog::new(live.clone());

    let mut probe = match initial_probe {
        Some(p) => {
            log.push(format!("[{}] {}", m.id, p.summary()));
            p
        }
        None => {
            log.push(format!("[{}] probing {}", m.id, ssh.target));
            let p = ssh.probe(&layout.engine);
            log.push(format!("[{}] {}", m.id, p.summary()));
            p
        }
    };
    if !probe.reachable {
        log.push(format!("[{}] unreachable, aborting provision", m.id));
        return (probe, log.lines);
    }

    // Where the weights come from, resolved once: the release named by the
    // operator, or nothing. A workspace with no `models_release` and no
    // `--release-repo` keeps the push, which is the safe direction — a box
    // that cannot reach a release is still a box that can be provisioned.
    let repo = release_repo
        .map(str::to_string)
        .unwrap_or_else(|| crate::config::Settings::load(&layout.settings()).models_release);
    let release = crate::artifact::ModelsRelease::resolve(&layout.models_dir(), &repo);
    if repo.trim().is_empty() {
        log.push(format!(
            "[{}] no models release configured, the weights travel over the push",
            m.id
        ));
    }

    // And the profile pack, from the same shape of setting and for the same
    // reason. **Separate** on purpose: the two artifacts are published
    // independently and one is far more often absent — a checkout with a
    // released pack and an unreleased bake of the weights is the ordinary
    // case, and a setting that could only say both or neither would make the
    // operator choose a 668 MB push to save a 60 MB one.
    //
    // Resolved against the **load pointer**, not a hash of the tree, so a
    // release is only used when this checkout says which one it is running. A
    // pointer with no version resolves to nothing, which is the push: every
    // box provisioned before this existed keeps working.
    let settings = crate::config::Settings::load(&layout.settings());
    let pack = crate::artifact::PackRelease::resolve(&layout.root, &settings.packs_release);
    if !settings.packs_release.trim().is_empty() && pack.is_none() {
        let pointer = crate::profile::read_pointer(&layout.root)
            .map(|p| format!("{} (version {:?})", p.name, p.version))
            .unwrap_or_else(|e| format!("no profile pointer: {e}"));
        log.push(format!(
            "[{}] packs_release is set but {} names no versioned pack, so assets/ travels over the push \
             (re-publish it: tools/profile.sh pack <name> --version <v> && gh release create {}-pack-v<v> …)",
            m.id,
            pointer,
            crate::profile::read_pointer(&layout.root).map(|p| p.name).unwrap_or_default(),
        ));
    }

    // Before anything is pushed: the sources bundle is `tar` + `zstd`, so the
    // tool that opens it has to be here first. This is the one install attempted
    // on an already-configured box (see `zstd_script`); a box that cannot get it
    // still gets a line here, and the push below fails with the remedy in its
    // own message rather than a shell error naming nothing.
    match ssh.ensure_zstd() {
        Ok(v) => log.push(format!("[{}] {v}", m.id)),
        Err(e) => log.push(format!("[{}] zstd check failed: {e}", m.id)),
    }

    // Self-healing enrollment: the manifest may name clones the pushed store
    // lacks (added or swapped since the last bake). Merging them here, before
    // the stamp, means the hash drift pushes the fix to workers in this same
    // run, instead of warning forever no matter how often `:prov` runs.
    // Voices enrolled nowhere stay missing; the warning below still names
    // exactly those.
    let baked = crate::pool::bake_missing_voices(layout);
    if !baked.is_empty() {
        log.push(format!(
            "[{}] baked {} voice(s) into models/voices.json: {}",
            m.id,
            baked.len(),
            baked.join(", ")
        ));
    }

    // What this box's own policy says it may run decides what it must hold. A
    // box with no stored policy enables all four, so the ordinary case is the
    // full set and only an explicitly narrowed panel goes lean.
    let stages = super::sources::stages_of(&m.effective_task_policy());
    let local_stamp =
        match compute_provision_stamp(layout, &stages, agent_version, agent_binary, pack.as_ref())
        {
        Ok(s) => s,
        Err(e) => {
            probe.note = format!("cannot read the sources it would push: {e:#}");
            log.push(format!("[{}] {}", m.id, probe.note));
            return (probe, log.lines);
        }
    };
    let remote_stamp = probe.stamp.as_ref();

    // **And** the pack. With a release configured the bundle carries no
    // `assets/`, so two different packs produce the *same* `sources.tar.zst` —
    // the bundle cannot see the difference, and a gate that read only the
    // bundle would call a re-pointed profile "in sync" on every box and never
    // send it. The pack has its own stamp field for exactly this.
    let sources_match = !force
        && remote_stamp
            .map(|s| s.sources_in_sync(&local_stamp) && s.pack_in_sync(&local_stamp))
            .unwrap_or(false);
    // The voice store now travels inside `models/`, so `tts_hash` covers it and
    // there is no separate voices check. Also verify the remote names: a stamp
    // written by the old already-configured path could say “in sync” after it
    // skipped the model push, which is exactly how Narrator 2 stayed missing.
    let manifest = crate::pool::load_manifest(&layout.root);
    // An *unknown* roster is not a missing one. The probe cannot read the
    // roster when the sidecar is not answering, and reading that as "this box
    // knows none of the declared voices" would answer a down sidecar with a
    // 668 MB model push. `voices_hash` is the primary gate now, it compares
    // the content of `models/voices.json` against ours, and this check is the
    // backstop for a stamp that lies, so it only ever *adds* a push when it has
    // something to say.
    let remote_voice_store_complete =
        probe.voices.is_empty() || voice_store_covers(&probe.voices, &manifest);
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
    // And the same flag for the binary itself, which is the one that matters
    // most: a replaced `bm-tts` on disk does nothing while the old process is
    // still running it, so a redeploy that does not recycle the sidecar looks
    // like a successful provision and behaves like no provision at all.
    let mut tts_pushed = false;
    // Read here, beside `already`, so the two cannot disagree about what this
    // run is allowed to do, see [`may_install`].
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
        // The sidecar binary, on the same reasoning as the agent above.
        //
        // Without this branch a rebuilt `bm-tts` never reached a configured
        // box: `tts_hash` covers `models/`, not the binary, and the only push
        // site lived in the `else` below, which an already-configured box never
        // reaches. The box kept serving the old sidecar for ever, silently.
        if !remote_stamp
            .map(|s| s.tts_bin_in_sync(&local_stamp))
            .unwrap_or(false)
        {
            match ssh.install_tts_runtime(&layout.engine, tts_binary, tts_runtime, live.as_ref()) {
                Ok(v) => {
                    tts_pushed = true;
                    log.push(format!("[{}] sidecar drifted, redeployed, {v}", m.id));
                }
                Err(e) => log.push(format!("[{}] sidecar redeploy failed: {e}", m.id)),
            }
        }
        if sources_match {
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            // One artifact, pushed whole. The hash in the bundle's name is the
            // same digest `sources_match` just compared, so a box that reaches
            // this branch is one whose set really differs.
            match ssh.install_sources(layout, &stages, pack.as_ref(), live.as_ref()) {
                Ok(lines) => {
                    for l in lines {
                        log.push(format!("[{}] {l}", m.id));
                    }
                }
                Err(e) => log.push(format!("[{}] sources push failed: {e}", m.id)),
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
            match ssh.install_models(
                &layout.engine,
                &layout.models_dir(),
                release.as_ref(),
                live.as_ref(),
            ) {
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
        if let Err(e) = ssh.ensure_root(&layout.engine) {
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
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            match ssh.install_sources(layout, &stages, pack.as_ref(), live.as_ref()) {
                Ok(lines) => {
                    for l in lines {
                        log.push(format!("[{}] {l}", m.id));
                    }
                }
                Err(e) => log.push(format!("[{}] sources push failed: {e}", m.id)),
            }
        }

        if !probe.tts_bin_present || !probe.tts_runtime_ok() || force {
            log.push(format!(
                "[{}] installing the TTS sidecar binary + runtime",
                m.id
            ));
            match ssh.install_tts_runtime(&layout.engine, tts_binary, tts_runtime, live.as_ref()) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log.lines);
                }
            }
        } else {
            log.push(format!(
                "[{}] TTS sidecar binary already present, skipped",
                m.id
            ));
        }

        // 668 MB, and the reason `tts_hash` exists: a re-provision with nothing
        // changed must not re-send it.
        if models_match {
            log.push(format!("[{}] models in sync (cache match)", m.id));
        } else {
            log.push(format!("[{}] pushing models (~668 MB)", m.id));
            match ssh.install_models(
                &layout.engine,
                &layout.models_dir(),
                release.as_ref(),
                live.as_ref(),
            ) {
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
            "[{}] no cluster token on this inductor, `serve` generates one; a worker started with --serve-tasks will refuse to run until it does",
            m.id
        )),
    }

    // The worker's agent gate checks this pointer at startup: sources above
    // carry the profile content, the pointer says what it claims to be.
    // Written every provision (one small file) so a re-pointed inductor
    // cannot leave a worker verifying yesterday's profile.
    match crate::profile::read_binding(&layout.root) {
        Ok(binding) => match ssh.write_profile_pointer(&binding) {
            Ok(()) => log.push(format!(
                "[{}] profile pointer: {} ({})",
                m.id,
                crate::profile::label(&binding),
                &binding.pack.hash[..12.min(binding.pack.hash.len())]
            )),
            Err(e) => log.push(format!("[{}] profile pointer failed: {e}", m.id)),
        },
        Err(_) => log.push(format!(
            "[{}] no local profile pointer, load one first (`:profile` in the dashboard, or `tools/profile.sh unpack <name>`), or this worker will refuse to start",
            m.id
        )),
    }

    // Enrollment moved off the worker, it needs the encoder, which is not on a
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
                &std::fs::read_to_string(layout.tts_voices()).unwrap_or_default(),
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
                    "[{}] declared in voices.json but missing from models/voices.json: {}, enroll on this machine and re-bake, or a render naming one will fail here",
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
            "[{}] {v}, merge stays disabled on this box until ffmpeg is present (apt/dnf install ffmpeg), then force a re-provision",
            m.id
        )),        Err(e) => log.push(format!("[{}] ffmpeg install check failed: {e}", m.id)),
    }
    // The sidecar loads its voice roster at startup. A models push therefore
    // has to recycle an already-running sidecar; otherwise the new store is
    // present on disk but the process keeps serving the old 66-voice roster.

    if models_pushed || tts_pushed {
        match ssh.stop_tts() {
            Ok(()) if models_pushed && probe.tts_up => {
                log.push(format!("[{}] stopped TTS to reload the voice store", m.id))
            }
            Ok(()) if tts_pushed => log.push(format!(
                "[{}] stopped TTS so the new sidecar binary is the one that runs",
                m.id
            )),
            Ok(()) => log.push(format!("[{}] cleared stale TTS process", m.id)),
            Err(e) => log.push(format!(
                "[{}] could not stop TTS for voice reload: {e}",
                m.id
            )),
        }
    }

    // Waits for ready, so this line is a fact and not a hope, see `start_tts`.
    // A box still loading after the budget is *not* held back here: readiness
    // is about the binary and the weights (`configured`), and the worker's own
    // `ensure` now waits for a loading server instead of racing it. Making
    // `configured` depend on a live sidecar was considered and rejected: it
    // would deny a registered box over a sidecar restart and drag
    // `may_install` into re-running package installs on a healthy cluster.
    match ssh.start_tts(&layout.engine) {
        Ok(v) if v.starts_with("TTS-STARTING") => log.push(format!(
            "[{}] {v}, the worker will wait for it rather than start a second one; re-run the probe if renders are slow to begin",
            m.id
        )),
        Ok(v) => log.push(format!("[{}] tts: {v}", m.id)),
        Err(e) => log.push(format!("[{}] could not start tts: {e}", m.id)),
    }

    // Write provision stamp so subsequent runs can skip
    let _ = ssh.write_provision_stamp(&local_stamp);

    // Re-probe so the caller records the post-provision truth.
    let after = ssh.probe(&layout.engine);
    log.push(format!("[{}] after provision: {}", m.id, after.summary()));
    // Named on its own line, not just inside the summary: a merge offered to
    // this box fails after a full render lease, and the operator's next stop is
    // this log. The fix is one package manager away on every platform.
    if !after.ffmpeg_present {
        log.push(format!(
            "[{}] ffmpeg is not on PATH, this box can crawl/digest/render but every merge it is offered will fail; install it (apt install ffmpeg / dnf install ffmpeg) and force a re-provision",
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
                "[{}] voices in store but declared nowhere (not a preset, not in voices.json, not pooled): {}, add each with its refs/ clip to voices.json and provision again, or drop it from the store; remote renders 500 until then",
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
            tts_hash: "weights-v1".into(),
            voices_hash: "store-v1".into(),
            ..Default::default()
        };
        let same = local.clone();
        assert!(!models_need_push(Some(&same), &local, false));
        assert!(models_need_push(None, &local, false));
        assert!(models_need_push(Some(&same), &local, true));

        // A re-bake: the weights moved, the store did not.
        let rebaked = ProvisionStamp {
            tts_hash: "weights-v2".into(),
            ..local.clone()
        };
        assert!(
            models_need_push(Some(&rebaked), &local, false),
            "a re-bake must resync"
        );

        // An enrollment: the store moved, the weights did not. This is the case
        // that used to slip through the gate entirely, `tts_hash` was the whole
        // check, and it deliberately excludes `models/voices.json`, so a freshly
        // enrolled voice sat on the inductor while every log said "in sync".
        let enrolled = ProvisionStamp {
            voices_hash: "store-v2".into(),
            ..local.clone()
        };
        assert!(
            models_need_push(Some(&enrolled), &local, false),
            "an enrollment must reach the box even though the weights are unchanged"
        );
    }

    /// The verify list is the weights, and never the one mutable file among them.
    #[test]
    fn the_checksum_list_covers_the_weights_and_not_the_voice_store() {
        let dir = std::env::temp_dir().join("bm-model-checksums");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"files":{"sea_g2p.bin":{"bytes":1,"sha256":"aa11"},"voices.json":{"bytes":2,"sha256":"bb22"},"config.json":{"bytes":3,"sha256":"cc33"}}}"#,
        )
        .unwrap();
        assert_eq!(
            model_checksums(&dir),
            vec![
                "aa11  sea_g2p.bin".to_string(),
                "cc33  config.json".to_string()
            ],
            "sorted, both weights kept, and the store dropped"
        );

        // Absent or unparseable means "nothing to verify", not a failure:
        // `install_models` is what refuses an incomplete bake.
        std::fs::remove_file(dir.join("manifest.json")).unwrap();
        assert!(model_checksums(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
        // minutes in `npm i`, and it ran on *every* provision of *every* box
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

        // zstd is the deliberate exception to that rule: one second, ~1 MB, and
        // the very next push cannot proceed without it. Its script installs
        // unconditionally and still short-circuits when the tool is present.
        let zstd = zstd_script();
        assert!(
            zstd.contains(
                r#"command -v zstd >/dev/null 2>&1 && { echo "ZSTD-OK (present)"; exit 0; }"#
            ),
            "a box that has it must pay one command: {zstd}"
        );
        assert!(
            zstd.contains("install -y zstd"),
            "and one that does not must be able to get it: {zstd}"
        );
        assert!(
            !zstd.contains("force a re-provision"),
            "the exception is that this one does not wait for a forced box: {zstd}"
        );
        // A box that already has the tool short-circuits in *both* flavours
        // the flag only decides what happens when it is missing.
        for script in [opencode_script(true), opencode_script(false)] {
            assert!(script.contains(r#"command -v opencode >/dev/null 2>&1 && { echo "OPENCODE-OK (present)"; exit 0; }"#));
        }
    }

    /// The exit code is the whole contract between the box and this function,
    /// so it is pinned here rather than left to a comment in two crates: `3`
    /// falls back to the push, `2` does not, and nothing else is mistaken for
    /// corruption.
    #[test]
    fn a_fetch_exit_code_says_whether_the_push_may_try_again() {
        let tag = "models-vdda4efee13df";
        let ok = classify_fetch(EXIT_LANDED, "FETCH-OK (16 files, 667 MiB)", "", tag, "models");
        assert!(ok.unwrap().contains("models-vdda4efee13df"));

        // Absence: the artifact is not published, the network is down, the URL
        // 404s. The push is a real answer to any of those.
        let absent = classify_fetch(
            EXIT_UNREACHABLE,
            "",
            "FETCH-UNREACHABLE (HTTP 404)",
            tag,
            "models",
        );
        assert!(matches!(absent, Err(FetchOutcome::Unreachable(_))));

        // Wrong bytes: the one answer that must stop, because pushing the same
        // bytes again would only hide the disagreement.
        let wrong = classify_fetch(
            EXIT_CORRUPT,
            "",
            "FETCH-CORRUPT (tts.onnx: sha256 …)",
            tag,
            "models",
        );
        match wrong {
            Err(FetchOutcome::Corrupt(m)) => assert!(m.contains("tts.onnx"), "{m}"),
            other => panic!("corruption must not be a fallback: {other:?}"),
        }

        // Anything else is absence, not corruption: a box too old to have the
        // subcommand has not said anything about the bytes. `2` is in this list
        // deliberately — that is what `clap` exits with on an unrecognised
        // subcommand, so it is the exact shape of "the agent predates this".
        for code in [1_i32, 2, 126, 127, 255] {
            match classify_fetch(code, "", "", tag, "models") {
                Err(FetchOutcome::Unreachable(m)) => assert!(m.contains(&code.to_string()), "{m}"),
                other => panic!("exit {code} must be absence: {other:?}"),
            }
        }
    }

    /// A pack fetch is the same contract as a models fetch, and the reason it
    /// shares the codes is that a box answers them with **one** subcommand: a
    /// second verb would be a second `match` on exit codes, and the day one of
    /// them read `2` as corruption the cluster would stop provisioning.
    #[test]
    fn a_pack_is_fetched_the_same_way_and_read_the_same_way() {
        let release = crate::artifact::PackRelease::for_repo(
            "lhuthng/storycast",
            "xianxia",
            "0.1.0",
            "25e7ed5b07955cd15c97897b41bc4353cea2aa344da514f51ec585ac81897821",
        )
        .unwrap();
        assert_eq!(release.tag, "xianxia-pack-v0.1.0");
        let script = pack_fetch_script(&release);
        // The prefix and the destination are the same word on purpose: the
        // bundle holds `assets/…` and it lands at `assets/`, and a spelling
        // that let those two drift is a tree nothing can read.
        assert!(script.contains("--strip-prefix assets"), "{script}");
        assert!(script.contains("~/bm-worker/assets"), "{script}");
        assert!(script.contains(&release.hash), "{script}");
        assert!(script.contains(&shell_quote(&release.url)), "{script}");

        let ok = classify_fetch(
            EXIT_LANDED,
            "FETCH-OK (41 files, 62 MiB)",
            "",
            &release.tag,
            "pack",
        );
        let line = ok.unwrap();
        assert!(line.starts_with("pack from the release xianxia-pack-v0.1.0"), "{line}");

        // A pack that does not verify is still a stop, not a push.
        assert!(matches!(
            classify_fetch(
                EXIT_CORRUPT,
                "",
                "FETCH-CORRUPT (the bundle is a different pack)",
                &release.tag,
                "pack",
            ),
            Err(FetchOutcome::Corrupt(_))
        ));
        // …and an unreachable one is still a push.
        assert!(matches!(
            classify_fetch(
                EXIT_UNREACHABLE,
                "",
                "FETCH-UNREACHABLE (HTTP 404)",
                &release.tag,
                "pack",
            ),
            Err(FetchOutcome::Unreachable(_))
        ));
    }

    /// A pointer stamped before versions existed must not fetch anything, and
    /// a name that is not URL- and tag-safe must not build a URL at all.
    #[test]
    fn a_pack_release_only_resolves_from_a_versioned_pointer() {
        let root = std::env::temp_dir().join("bm-packrelease-pointer");
        let _ = std::fs::remove_dir_all(&root);

        // No pointer at all.
        assert!(crate::artifact::PackRelease::resolve(&root, "lhuthng/storycast").is_none());
        // No repo: the push, which is what every box has today.
        crate::profile::write_pointer(
            &root,
            &crate::profile::Pointer {
                name: "xianxia".into(),
                hash: "aa".into(),
                version: "0.1.0".into(),
            },
        )
        .unwrap();
        assert!(crate::artifact::PackRelease::resolve(&root, "  ").is_none());

        let resolved = crate::artifact::PackRelease::resolve(&root, "lhuthng/storycast").unwrap();
        assert_eq!(resolved.tag, "xianxia-pack-v0.1.0");
        assert_eq!(
            resolved.url,
            "https://github.com/lhuthng/storycast/releases/download/xianxia-pack-v0.1.0/xianxia.tar.zst"
        );

        // The pointer written before this field existed: no version, so no
        // release, so the push. This is every checkout in existence.
        crate::profile::write_pointer(
            &root,
            &crate::profile::Pointer {
                name: "xianxia".into(),
                hash: "aa".into(),
                version: String::new(),
            },
        )
        .unwrap();
        assert!(
            crate::artifact::PackRelease::resolve(&root, "lhuthng/storycast").is_none(),
            "an unversioned pointer must fall back to the push, not guess a tag"
        );

        // The name and the version go into a URL and a git tag, so they are
        // validated like the repo is.
        for (name, version) in [("", "0.1.0"), ("xianxia", ""), ("../etc", "0.1.0"), ("a/b", "0.1.0")] {
            assert!(
                crate::artifact::PackRelease::for_repo("o/n", name, version, "aa").is_err(),
                "`{name}`/`{version}` was accepted"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The line the box runs, and the one thing that could make it do something
    /// other than fetch: it is a string handed to `sh -c`.
    #[test]
    fn the_fetch_line_asks_for_the_tagged_bundle_and_quotes_the_url() {
        let hash = "dda4efee13df0eb2b30ef45eb548741b5af633f6d55712e30f4da574b357c552";
        let r = crate::artifact::ModelsRelease::for_repo("lhuthng/storycast", hash).unwrap();
        let script = fetch_script(&r, "vieneu");
        assert!(
            script.contains("~/bm-worker/bm-agent fetch-artifact"),
            "{script}"
        );
        assert!(script.contains(&format!("--expect {hash}")), "{script}");
        assert!(
            script.contains("~/bm-worker/engines/vieneu/models"),
            "the destination is the engine's own models dir: {script}"
        );
        // A second engine fetches into its own tree, never over VieNeu's.
        assert!(
            fetch_script(&r, "gemini").contains("~/bm-worker/engines/gemini/models"),
            "the engine name has to be in the fetch destination"
        );
        // Quoted, because the URL is a string in a `sh -c`. A repo the parser
        // accepts cannot produce a quote today; a helper that only works for
        // today's input is not a helper.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    /// The push must not carry the bundle it is standing in for.
    #[test]
    fn the_models_push_leaves_the_transfer_artifact_behind() {
        assert_eq!(
            MODELS_PUSH_EXCLUDES,
            &["/models.tar.zst"],
            "the bundle is 380 MB of the same 668 MB, and no box reads it"
        );
        assert_eq!(
            MODELS_PUSH_EXCLUDES[0].trim_start_matches('/'),
            crate::artifact::BUNDLE_NAME,
            "and it is the one file `tools/models.sh pack` writes"
        );
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
        // Exactly what the sidecar sends, and the difference is the whole test:
        // a preset carries a description and so arrives as a *label*, while a
        // clone has none and arrives bare.
        let store = vec![
            "Thái Sơn — Nam · Trung · Kể chuyện".to_string(),
            "Adam — Nam · Nam · Giọng đọc tự nhiên".to_string(),
            "Học Trò".to_string(),
            "Pool Sample".to_string(),
            "Suneo".to_string(),
            "Suneo".to_string(),
            "_note".to_string(),
        ];
        assert_eq!(
            undeclared_voices(&store, &manifest, &pool, &catalogue),
            vec!["Suneo"],
            "one hand-enrolled clone, named by its name and not by a label"
        );
        assert!(undeclared_voices(&[], &manifest, &pool, &catalogue).is_empty());
    }

    /// The false positive this shape caused, in the numbers it produced.
    ///
    /// It reported all 23 shipped presets on every single provision and told the
    /// operator to add each one with a `refs/` clip, ~20 MB of audio to clone
    /// voices already present, under names that already exist. Nothing about
    /// those 23 is undeclared: they are the catalogue itself.
    #[test]
    fn a_shipped_preset_is_never_reported_as_undeclared() {
        let catalogue: Vec<String> = crate::voices::offline_voices("vieneu")
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(catalogue.len(), 23, "the shipped ViNeu roster");
        // Built the way the sidecar builds them: name + description, verbatim.
        let store: Vec<String> = catalogue
            .iter()
            .map(|n| format!("{n} — Nam · Bắc · Kể chuyện"))
            .chain(["Suneo".to_string()])
            .collect();
        assert_eq!(
            undeclared_voices(
                &store,
                &std::collections::HashMap::new(),
                &crate::pool::Pool::new(),
                &catalogue,
            ),
            vec!["Suneo"],
            "23 declared presets, 1 genuine stray"
        );
        // And the same answer whatever the description says, or whether the
        // label uses an en dash or the ASCII fallback.
        let spelled = vec![
            "Thái Sơn – Nam · Trung".to_string(),
            "Adam - Nam".to_string(),
        ];
        assert!(undeclared_voices(
            &spelled,
            &std::collections::HashMap::new(),
            &crate::pool::Pool::new(),
            &catalogue
        )
        .is_empty());
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
        // merge, and shelves the chapter after a full render lease, with
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

    /// The sidecar needs *both* the binary and its weights, a binary with no
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
