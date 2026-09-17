use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

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

/// Compute manifest stamp for detecting changes to sources and clone voices.
///
/// Two SHA-256 digests, each over a canonical (sorted, newline-joined) view of
/// its inputs, so the same inputs produce the same hex string on any machine:
///
/// * `sources_hash` — `prompts/` by signature, plus the *content* of the small
///   manifests the worker must match exactly (`requirements.txt`, the cast
///   files, the scene map and the three clip-pool registries), plus the effect,
///   music and inject clip directories by signature, plus the agent version so
///   a rebuild redeploys.
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
        // The pools are manifests, not media: the registry decides which clip
        // answers a scene, so a worker left holding a stale one would mix a
        // different chapter than the inductor previewed — same script, same
        // seed, different audio, and nothing on either side to say why.
        "assets/effect-pool.json",
        "assets/music-pool.json",
        "assets/inject-pool.json",
    ] {
        let p = repo_root.join(rel);
        if let Ok(bytes) = std::fs::read(&p) {
            sources.update(rel.as_bytes());
            sources.update([0]);
            sources.update(&bytes);
            sources.update([0]);
        }
    }
    // …and the clips themselves by signature, exactly like `refs/`: a pool
    // registry is only as good as the files it names, so adding a clip has to
    // resync even though no manifest changed.
    for rel in ["assets/effects", "assets/music", "assets/injects"] {
        sources.update(rel.as_bytes());
        sources.update([0]);
        sources.update(signature_of_dir(&repo_root.join(rel)).as_bytes());
        sources.update([0]);
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
pub(crate) fn parse_stamp(text: &str) -> Option<ProvisionStamp> {
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::super::ssh::Ssh;
    use super::*;

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
        assert_ne!(
            a.sources_hash, c.sources_hash,
            "a cast edit must resync sources"
        );
        assert_eq!(
            a.voices_hash, c.voices_hash,
            "…and must not re-enroll voices"
        );

        // A version bump redeploys the agent even when every file is identical.
        let d = compute_provision_stamp(&root, "0.3.0");
        assert!(!a.sources_in_sync(&d), "a new agent build must redeploy");
        assert!(
            a.voices_in_sync(&d),
            "the agent version says nothing about voices"
        );
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
        assert!(
            base.sources_in_sync(&renamed),
            "voices.json is not a source"
        );

        // A new clip changes the refs signature without touching the manifest.
        std::fs::write(root.join("refs/m.wav"), vec![2u8; 64]).unwrap();
        let added = compute_provision_stamp(&root, "0.2.0");
        assert!(!renamed.voices_in_sync(&added), "a new clip must re-enroll");
        assert!(
            base.sources_in_sync(&added),
            "refs/ is not part of the sources hash"
        );
    }

    #[test]
    fn the_clip_pools_are_sources_and_resync_when_a_clip_is_added() {
        let root = stamp_fixture("pools");
        std::fs::create_dir_all(root.join("assets/effects")).unwrap();
        std::fs::create_dir_all(root.join("assets/music")).unwrap();
        std::fs::write(root.join("assets/effects/rain-1.mp3"), vec![1u8; 32]).unwrap();
        std::fs::write(
            root.join("assets/music-pool.json"),
            r#"{"soft-1":{"file":"assets/music/soft-1.mp3","tags":["soft"]}}"#,
        )
        .unwrap();
        let base = compute_provision_stamp(&root, "0.2.0");

        // Re-running with nothing touched must not resync — otherwise every
        // provision would push the clip directories for no reason.
        assert!(base.sources_in_sync(&compute_provision_stamp(&root, "0.2.0")));

        // The registry is a manifest: editing it changes what a scene means,
        // so the worker has to receive it.
        std::fs::write(
            root.join("assets/music-pool.json"),
            r#"{"soft-1":{"file":"assets/music/soft-1.mp3","tags":["calm"]}}"#,
        )
        .unwrap();
        let edited = compute_provision_stamp(&root, "0.2.0");
        assert!(
            !base.sources_in_sync(&edited),
            "a pool edit must resync sources"
        );

        // A new clip resyncs too, even though no manifest moved.
        std::fs::write(root.join("assets/music/soft-1.mp3"), vec![2u8; 32]).unwrap();
        assert!(
            !edited.sources_in_sync(&compute_provision_stamp(&root, "0.2.0")),
            "a new clip must resync sources"
        );
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
}
