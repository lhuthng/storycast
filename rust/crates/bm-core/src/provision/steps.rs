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
fn pack_base(layout: &crate::Layout) -> std::path::PathBuf {
    layout
        .assets()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| layout.root.clone())
}

/// The line the box is asked to run, and the exit codes it answers with.
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
fn pack_fetch_script(release: &crate::artifact::PackRelease) -> String {
    format!(
        "~/bm-worker/bm-agent fetch-artifact {url} ~/bm-worker/{dir} --expect {hash} --strip-prefix {dir}",
        url = shell_quote(&release.url),
        dir = crate::artifact::PACK_DIR,
        hash = release.hash,
    )
}

/// The engine's own tree on a worker, relative to the worker root:
fn engine_rel(engine: &str) -> String {
    format!("{}/{}", crate::paths::ENGINES_DIR, engine)
}

/// Read a fetch's exit code as the contract it is.
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
const MODELS_PUSH_EXCLUDES: &[&str] = &["/models.tar.zst"];

/// How a release fetch ended, in the two shapes the caller must treat
#[derive(Debug)]
enum FetchOutcome {
    /// The artifact was not there. The push is the answer.
    Unreachable(String),
    /// Bytes arrived and are not the ones asked for. Stop.
    Corrupt(String),
}

/// Single-quote a value for the remote `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One labelled rsync progress stream off the shared live sender: `None`
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
    #[serde(default)]
    pub os: String,
    /// Version string reported by the installed agent, if any.
    pub agent_version: Option<String>,
    /// A usable Python interpreter with the TTS deps installed.
    pub python_present: bool,
    /// The Rust sidecar binary, `~/{REMOTE_DIR}/bm-tts`.
    #[serde(default)]
    pub tts_bin_present: bool,
    /// The ONNX Runtime `libonnxruntime.so.1` beside the binary. Linux links
    #[serde(default)]
    pub tts_lib_present: bool,
    /// A baked `~/{REMOTE_DIR}/models/`, the Rust sidecar's weights.
    #[serde(default)]
    pub models_present: bool,
    /// `ffmpeg` on PATH. The merge stage shells out to it, so a box without it
    #[serde(default)]
    pub ffmpeg_present: bool,
    /// `sox` on PATH. The merge now shells out to it for every voice
    #[serde(default)]
    pub sox_present: bool,
    /// The sidecar's roster, as `/voices` sends it: **labels, not names**
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
    pub fn configured(&self, want_version: &str) -> bool {
        self.reachable && self.agent_version.as_deref() == Some(want_version) && self.rust_ready()
    }

    /// The TTS sidecar is installed and has its weights.
    pub fn rust_ready(&self) -> bool {
        self.tts_bin_present && self.models_present && self.tts_runtime_ok()
    }

    /// The loader is satisfied. A binary-without-lib box reads as not-ready,
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

/// Whether `opencode` may chase a missing tool on a configured box: the one
fn may_install(configured: bool, force: bool) -> bool {
    force || !configured
}

/// Whether the model/voice store must be pushed even when the worker already
fn models_need_push(remote: Option<&ProvisionStamp>, local: &ProvisionStamp, force: bool) -> bool {
    force || !remote.is_some_and(|stamp| stamp.tts_in_sync(local) && stamp.voices_in_sync(local))
}

/// Whether the remote voice store actually contains every clone declared by the
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

/// The `ffmpeg` step: an ensure like [`zstd_script`], so a configured box that
fn ffmpeg_script() -> String {
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
}

/// The `sox` step, [`ffmpeg_script`]'s pair: merges need both, so both are
fn sox_script() -> String {
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
}

/// The `zstd` step: the sources bundle is `tar` + `zstd`, so every box that
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
        match self.run(&script, 10) {
            Ok((code, stdout, _)) => stamp_from(code, &stdout),
            Err(_) => None,
        }
    }

    /// Ship the cluster token to the worker's root.
    pub fn write_cluster_token(&self, token: &str) -> Result<()> {
        // A quoted heredoc, like the profile pointer beside it: the token is hex
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
