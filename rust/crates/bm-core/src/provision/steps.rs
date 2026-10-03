use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::ssh::{RsyncProgress, Ssh};
use super::stamp::{stamp_from, ProvisionStamp};
use super::REMOTE_DIR;

mod bootstrap;
mod models;
mod pack;
mod provision;

pub use self::provision::provision;

/// The directory a pack's `assets/…` paths are relative to: the `assets/` tree
/// **in force's** parent — the workspace's when it composes its own, the
/// checkout's otherwise. `push_pack` rsyncs that tree and the receipt is keyed
/// against it, so a delta must be addressed the same way, or it pushes a
/// workspace's changed paths out of the checkout's directory.
fn pack_base(layout: &crate::Layout) -> std::path::PathBuf {
    layout
        .assets()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| layout.root.clone())
}

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
    /// `sox` on PATH. The merge now shells out to it for every voice
    /// treatment (a room, a character, a decay), so a box with ffmpeg but no
    /// sox would take merges and fail every one of them. Reported and, unlike
    /// ffmpeg, **gated**: the worker advertises no `merge` without it, because
    /// a voice treatment is not optional in the effect pass.
    #[serde(default)]
    pub sox_present: bool,
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
        ) + if self.ffmpeg_present && self.sox_present {
            ""
        } else if !self.ffmpeg_present && !self.sox_present {
            " · NO FFMPEG/SOX, merges will fail here"
        } else if !self.ffmpeg_present {
            " · NO FFMPEG, merges will fail here"
        } else {
            " · NO SOX, merges will fail here"
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

/// The `sox` step. Same shape as [`ffmpeg_script`], and the same split: a
/// present `sox` short-circuits either way, so `allow_install` only decides
/// whether a *missing* one is chased this time. SoX is the second engine the
/// merge shells out to, and unlike ffmpeg it is a hard gate on `merge`.
fn sox_script(allow_install: bool) -> String {
    if allow_install {
        r#"export DEBIAN_FRONTEND=noninteractive
if command -v sox >/dev/null 2>&1; then echo "SOX-OK (present)"; exit 0; fi
install() { $1 >/dev/null 2>&1; }
if command -v apt-get >/dev/null 2>&1; then
  sudo -n apt-get install -y sox >/dev/null 2>&1 || install "apt-get install -y sox"
elif command -v dnf >/dev/null 2>&1; then
  sudo -n dnf install -y sox >/dev/null 2>&1 || install "dnf install -y sox"
elif command -v yum >/dev/null 2>&1; then
  sudo -n yum install -y sox >/dev/null 2>&1 || install "yum install -y sox"
elif command -v brew >/dev/null 2>&1; then
  install "brew install sox"
else
  echo "SOX-SKIP (no known package manager, install sox by hand)"; exit 0
fi
if command -v sox >/dev/null 2>&1; then echo "SOX-OK (installed)"; else echo "SOX-SKIP (install refused, needs sudo? run: sudo apt-get install -y sox)"; fi
"#
            .into()
    } else {
        r#"if command -v sox >/dev/null 2>&1; then echo "SOX-OK (present)"; exit 0; fi
echo "SOX-SKIP (already configured, not reinstalling; force a re-provision to try again)""#
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
    pub fn read_provision_stamp(&self) -> Option<ProvisionStamp> {
        let script = format!("cat \"$HOME/{d}/.provision_stamp.json\"", d = REMOTE_DIR);
        // A failed `cat` is a cache miss, not a stamp: `stamp_from` is what says
        // so, because it is also what the probe's own payload goes through.
        match self.run(&script, 10) {
            Ok((code, stdout, _)) => stamp_from(code, &stdout),
            Err(_) => None,
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

#[cfg(test)]
mod tests;
