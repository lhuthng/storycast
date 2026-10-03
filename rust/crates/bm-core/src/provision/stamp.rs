use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Manifest stamp recorded on a target machine to detect whether sources/voices changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProvisionStamp {
    pub agent_version: String,
    pub sources_hash: String,
    pub voices_hash: String,
    /// The sidecar's own artifacts: the baked `models/` directory and the
    #[serde(default)]
    pub tts_hash: String,
    /// SHA-256 of the `bm-agent` binary bytes the inductor would push.
    #[serde(default)]
    pub agent_hash: String,
    /// SHA-256 of the `bm-tts` bytes the inductor would push.
    #[serde(default)]
    pub tts_bin_hash: String,
    /// The `(stage, adapter)` slots the bundle on this box covers, from the
    #[serde(default)]
    pub sources_stages: Vec<String>,
    /// The profile pack's release identity, when the box takes it from one:
    #[serde(default)]
    pub pack_release: String,
}

impl ProvisionStamp {
    /// Whether the baked voice store on this box still matches the one we would
    pub fn voices_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.voices_hash == want.voices_hash
    }

    /// Whether the worker's sources (prompts, requirements, casts, assets, and
    pub fn sources_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.sources_hash == want.sources_hash && self.agent_version == want.agent_version
    }

    /// Whether the Rust sidecar's artifacts still match ours.
    pub fn tts_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.tts_hash == want.tts_hash
    }

    /// Whether the worker's `bm-agent` binary still matches ours.
    pub fn agent_in_sync(&self, want: &ProvisionStamp) -> bool {
        want.agent_hash.is_empty() || self.agent_hash == want.agent_hash
    }

    /// Whether the box's profile pack is the one we would hand it.
    pub fn pack_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.pack_release == want.pack_release
    }

    /// Whether the worker's `bm-tts` binary still matches ours.
    pub fn tts_bin_in_sync(&self, want: &ProvisionStamp) -> bool {
        want.tts_bin_hash.is_empty() || self.tts_bin_hash == want.tts_bin_hash
    }
}

/// Compute manifest stamp for detecting changes to sources, voices and sidecars.
pub fn compute_provision_stamp(
    layout: &crate::Layout,
    stages: &[bm_proto::Stage],
    agent_version: &str,
    agent_binary: &Path,
    pack: Option<&crate::artifact::PackRelease>,
) -> anyhow::Result<ProvisionStamp> {
    let repo_root = layout.root.as_path();
    // The engine's own tree — `engines/<name>/models` — not `root/models`: the
    let models = layout.models_dir();
    // Named in the digest as it reads relative to the root, so the hash both
    let models_rel = models
        .strip_prefix(repo_root)
        .unwrap_or(&models)
        .display()
        .to_string();
    // What the push would send, hashed as a set. The plan is the same call the
    // push makes — with the same `pack`, which is what makes "the bundle does
    // not carry `assets/`" true of the digest as well as of the tar — so the
    let plan = super::sources::Sources::plan_for(layout, stages, pack)?;
    let sources_manifest = plan.manifest()?;
    let mut sources = Sha256::new();
    sources.update(agent_version.as_bytes());
    sources.update([0]);
    sources.update(super::sources::Sources::hash(&sources_manifest).as_bytes());
    sources.update([0]);
    // The Rust sidecar's *weights*, by directory signature rather than content:
    let mut tts = Sha256::new();
    if let Ok(bytes) = std::fs::read(models.join("manifest.json")) {
        tts.update(format!("{models_rel}/manifest.json").as_bytes());
        tts.update([0]);
        tts.update(&bytes);
        tts.update([0]);
    }
    tts.update(signature_of_dir_skipping(&models, &[VOICE_STORE]).as_bytes());
    tts.update([0]);

    // The sidecar *binary*, by content, in a digest of its own.
    let mut tts_bin = Sha256::new();
    let mut tts_bin_staged = false;
    for p in tts_bin_candidates(repo_root) {
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        tts_bin_staged = true;
        let rel = p
            .strip_prefix(repo_root)
            .unwrap_or(&p)
            .display()
            .to_string();
        tts_bin.update(rel.as_bytes());
        tts_bin.update([0]);
        tts_bin.update(hex_digest(Sha256::digest(&bytes)).as_bytes());
        tts_bin.update([0]);
    }

    // The store the sidecar loads at startup — and nothing else, because the
    let mut voices = Sha256::new();
    let store = models.join(VOICE_STORE);
    if let Ok(bytes) = std::fs::read(&store) {
        voices.update(format!("{models_rel}/voices.json").as_bytes());
        voices.update([0]);
        voices.update(&bytes);
        voices.update([0]);
    }

    Ok(ProvisionStamp {
        agent_version: agent_version.to_string(),
        sources_stages: sources_manifest.slots.clone(),
        pack_release: pack.map(|p| p.hash.clone()).unwrap_or_default(),
        sources_hash: hex_digest(sources.finalize()),
        voices_hash: hex_digest(voices.finalize()),
        tts_hash: hex_digest(tts.finalize()),
        // Empty, not the digest of nothing, when no sidecar is staged: this
        tts_bin_hash: if tts_bin_staged {
            hex_digest(tts_bin.finalize())
        } else {
            String::new()
        },
        // Content, not signature: the binary is ~100 MB and hashing it costs
        agent_hash: std::fs::read(agent_binary)
            .map(|bytes| hex_digest(Sha256::digest(&bytes)))
            .unwrap_or_default(),
    })
}

/// The one mutable file inside the otherwise immutable `models/` bake: the
const VOICE_STORE: &str = "voices.json";

/// The sidecar binaries a provision could push, relative to the repo root.
fn tts_bin_candidates(repo_root: &Path) -> Vec<std::path::PathBuf> {
    [
        "rust/target/x86_64-unknown-linux-gnu/release/bm-tts",
        "rust/target/aarch64-unknown-linux-gnu/release/bm-tts",
        "rust/target/release/bm-tts",
    ]
    .into_iter()
    .map(|rel| repo_root.join(rel))
    .collect()
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
fn signature_of_dir_skipping(dir: &Path, extra: &[&str]) -> String {
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
        if SKIP.contains(&name.as_str()) || extra.contains(&name.as_str()) {
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
            out.push_str(&signature_of_dir_skipping(&path, extra));
        } else {
            out.push_str(&format!("f {} {} {}\n", rel.display(), meta.len(), mtime));
        }
    }
    out
}

/// Parse a stamp payload.
pub(crate) fn parse_stamp(text: &str) -> Option<ProvisionStamp> {
    serde_json::from_str(text).ok()
}

/// [`parse_stamp`], plus the exit code the command that produced the text returned.
pub(crate) fn stamp_from(code: i32, stdout: &str) -> Option<ProvisionStamp> {
    if code != 0 {
        return None;
    }
    parse_stamp(stdout)
}

#[cfg(test)]
mod tests;
