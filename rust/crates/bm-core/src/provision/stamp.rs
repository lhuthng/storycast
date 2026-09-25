use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Manifest stamp recorded on a target machine to detect whether sources/voices changed.
///
/// Written to `~/{REMOTE_DIR}/.provision_stamp.json` at the end of every
/// provision, and read back by the *next* probe. When the hashes still match,
/// the slow work is skipped: pushing `models/` (668 MB) and the redundant source
/// sync. A stale or missing stamp is never an error — it just means the full
/// path runs, which is what it did before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProvisionStamp {
    pub agent_version: String,
    pub sources_hash: String,
    pub voices_hash: String,
    /// The sidecar's own artifacts: the baked `models/` directory and the
    /// `bm-tts` binary. Separate from `sources_hash` so a cast or prompt edit
    /// does not look like a reason to re-send 668 MB of weights.
    ///
    /// This is also what covers the **voice store**, which now travels inside
    /// `models/voices.json` — enrollment moved off the worker, so there is no
    /// separate voices step to skip.
    ///
    /// `#[serde(default)]` so a stamp written before this field existed still
    /// parses — an unreadable stamp would force the full slow path on every
    /// probe, which is the failure this whole mechanism exists to avoid.
    #[serde(default)]
    pub tts_hash: String,
    /// SHA-256 of the `bm-agent` binary bytes the inductor would push.
    ///
    /// The version *string* alone cannot detect a rebuild: every dev build
    /// between releases reports the same `agent_version`, so `:prov` kept
    /// calling the box "already configured" and never pushed the new binary.
    /// `#[serde(default)]` so pre-existing stamps parse as "" (drift → one
    /// reinstall, then the fresh stamp records the hash).
    #[serde(default)]
    pub agent_hash: String,
}

impl ProvisionStamp {
    /// Whether the voice store on this box still matches the one we would push.
    ///
    /// Kept for the stamp's own round-trip, but **no longer consulted by
    /// provisioning**: the store travels inside `models/`, so [`Self::tts_in_sync`]
    /// is what decides. Removing the field would invalidate every stamp already
    /// on a box for no gain.
    pub fn voices_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.voices_hash == want.voices_hash
    }

    /// Whether the worker's sources (prompts, requirements, casts, assets, and
    /// the agent build itself) still match ours.
    pub fn sources_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.sources_hash == want.sources_hash && self.agent_version == want.agent_version
    }

    /// Whether the Rust sidecar's artifacts still match ours.
    ///
    /// An empty hash on either side means "this box does not use them", so a
    /// worker on the Python path never reports drift here and never gets the
    /// models pushed at it.
    pub fn tts_in_sync(&self, want: &ProvisionStamp) -> bool {
        self.tts_hash == want.tts_hash
    }

    /// Whether the worker's `bm-agent` binary still matches ours.
    ///
    /// An empty `want` means the inductor could not hash its own binary, so it
    /// has no opinion — never drift on that, or every provision would reinstall.
    pub fn agent_in_sync(&self, want: &ProvisionStamp) -> bool {
        want.agent_hash.is_empty() || self.agent_hash == want.agent_hash
    }
}

/// Compute manifest stamp for detecting changes to sources and clone voices.
///
/// Two SHA-256 digests, each over a canonical (sorted, newline-joined) view of
/// its inputs, so the same inputs produce the same hex string on any machine:
///
/// * `sources_hash` — `prompts/` by signature, plus the *content* of the small
///   manifests the worker must match exactly (`requirements.txt`, the cast
///   files, the clone manifest `voices.json`, the scene map and the three
///   clip-pool registries), plus the effect, music and inject clip
///   directories by signature, plus the agent version so a release bump
///   redeploys. (A rebuild under the *same* version is `agent_hash`'s job.)
/// * `voices_hash` — `voices.json` by content (a rename with identical clips
///   must re-enroll) and `refs/` by signature only: those clips are megabytes,
///   and reading them would cost more than the enrollment we are avoiding.
///   Kept alongside `sources_hash` (which also covers the manifest) because
///   the stamp payload round-trips it and older stamps are still out there.
pub fn compute_provision_stamp(
    repo_root: &Path,
    agent_version: &str,
    agent_binary: &Path,
) -> ProvisionStamp {
    let mut sources = Sha256::new();
    sources.update(agent_version.as_bytes());
    sources.update([0]);
    sources.update(signature_of_dir(&repo_root.join("prompts")).as_bytes());
    for rel in [
        "python/requirements.txt",
        "data/cast-vieneu.json",
        "data/cast.json",
        "voices.json",
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
    // The active workspace's cast files: voice assignments travel to workers
    // via install_sources, so a swap must drift the stamp — otherwise the
    // next :prov reports "in sync (cache match)" and the new voices never
    // reach any box while renders naming them are already queued.
    if let Ok(ws) = std::fs::read_to_string(repo_root.join(".bm").join("active-workspace")) {
        let ws = ws.trim();
        if !ws.is_empty() {
            for name in ["cast-vieneu.json", "cast.json"] {
                let p = repo_root
                    .join("workspaces")
                    .join(ws)
                    .join("data")
                    .join(name);
                if let Ok(bytes) = std::fs::read(&p) {
                    sources.update(name.as_bytes());
                    sources.update([0]);
                    sources.update(&bytes);
                    sources.update([0]);
                }
            }
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

    // The Rust sidecar's artifacts. Hashed by *signature*, not content: `models/`
    // is 668 MB and a single clip-sized read of it would cost more than the
    // provisioning this is meant to skip. `manifest.json` is read by content
    // because it is the authoritative statement of what the models *are* — a
    // swapped file under an unchanged manifest is exactly the drift worth
    // catching, and the directory signature catches it too, but only if the
    // mtime moved.
    let mut tts = Sha256::new();
    if let Ok(bytes) = std::fs::read(repo_root.join("models/manifest.json")) {
        tts.update(b"models/manifest.json");
        tts.update([0]);
        tts.update(&bytes);
        tts.update([0]);
    }
    tts.update(signature_of_dir(&repo_root.join("models")).as_bytes());
    tts.update([0]);
    // The binary, if it has been built here. Absent is not an error: the models
    // can be baked on a machine that never builds the Rust sidecar.
    for rel in ["rust/target/release/bm-tts", "rust/target/debug/bm-tts"] {
        let p = repo_root.join(rel);
        if let Ok(meta) = std::fs::metadata(&p) {
            tts.update(rel.as_bytes());
            tts.update([0]);
            tts.update(meta.len().to_string().as_bytes());
            tts.update([0]);
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            tts.update(mtime.to_string().as_bytes());
            tts.update([0]);
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
        tts_hash: hex_digest(tts.finalize()),
        // Content, not signature: the binary is ~100 MB and hashing it costs
        // ~0.1 s locally, while a stale binary on a worker is silent drift.
        // Absent locally is not an error (see `agent_in_sync`).
        agent_hash: std::fs::read(agent_binary)
            .map(|bytes| hex_digest(Sha256::digest(&bytes)))
            .unwrap_or_default(),
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

    /// Stand-in for the cross-built agent binary the stamp hashes.
    fn agent_bin(root: &std::path::Path) -> std::path::PathBuf {
        let p = root.join("bm-agent");
        if !p.exists() {
            std::fs::write(&p, b"agent-bytes-v1").unwrap();
        }
        p
    }

    #[test]
    fn a_stamp_is_stable_and_content_addressed() {
        let root = stamp_fixture("stable");
        let a = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        let b = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_eq!(a, b, "nothing changed, so the stamp must not either");
        assert_eq!(a.sources_hash.len(), 64, "sha-256 hex is 64 chars");
        assert_eq!(a.voices_hash.len(), 64);
        assert!(a.sources_in_sync(&b) && a.voices_in_sync(&b));

        // A cast edit is a source change and nothing else.
        std::fs::write(root.join("data/cast.json"), r#"{"Narrator":"Adam"}"#).unwrap();
        let c = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_ne!(
            a.sources_hash, c.sources_hash,
            "a cast edit must resync sources"
        );
        assert_eq!(
            a.voices_hash, c.voices_hash,
            "…and must not re-enroll voices"
        );

        // A version bump redeploys the agent even when every file is identical.
        let d = compute_provision_stamp(&root, "0.3.0", &agent_bin(&root));
        assert!(!a.sources_in_sync(&d), "a new agent build must redeploy");
        assert!(
            a.voices_in_sync(&d),
            "the agent version says nothing about voices"
        );
    }

    #[test]
    fn a_workspace_cast_swap_drifts_the_stamp() {
        // The swap writes the workspace cast, not the repo-root one — if the
        // stamp only watched the root, :prov would report "in sync" and the
        // new voices would never reach any box.
        let root = stamp_fixture("wscast");
        std::fs::create_dir_all(root.join(".bm")).unwrap();
        std::fs::write(root.join(".bm/active-workspace"), "book\n").unwrap();
        std::fs::create_dir_all(root.join("workspaces/book/data")).unwrap();
        std::fs::write(
            root.join("workspaces/book/data/cast-vieneu.json"),
            r#"{"A":"Đức Trí"}"#,
        )
        .unwrap();
        let a = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        std::fs::write(
            root.join("workspaces/book/data/cast-vieneu.json"),
            r#"{"A":"Quang Sơn"}"#,
        )
        .unwrap();
        let b = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_ne!(
            a.sources_hash, b.sources_hash,
            "a workspace voice swap must force a resync"
        );
        assert_eq!(a.voices_hash, b.voices_hash, "…and must not re-enroll");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn voices_hash_tracks_the_manifest_and_the_reference_clips() {
        let root = stamp_fixture("voices");
        let base = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));

        // A rename in voices.json must re-enroll even though the clip is
        // identical: enrollment is keyed by name, not by file. The manifest
        // also rides the sources sync now, so the worker's copy never drifts
        // behind the declaration the inductor's warnings are computed against.
        std::fs::write(root.join("voices.json"), r#"{"Storyteller":"refs/n.wav"}"#).unwrap();
        let renamed = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(!base.voices_in_sync(&renamed), "a rename must re-enroll");
        assert!(
            !base.sources_in_sync(&renamed),
            "a rename must resync the manifest"
        );

        // A new clip changes the refs signature without touching the manifest.
        std::fs::write(root.join("refs/m.wav"), vec![2u8; 64]).unwrap();
        let added = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(!renamed.voices_in_sync(&added), "a new clip must re-enroll");
        assert!(
            renamed.sources_in_sync(&added),
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
        let base = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));

        // Re-running with nothing touched must not resync — otherwise every
        // provision would push the clip directories for no reason.
        assert!(base.sources_in_sync(&compute_provision_stamp(&root, "0.2.0", &agent_bin(&root))));

        // The registry is a manifest: editing it changes what a scene means,
        // so the worker has to receive it.
        std::fs::write(
            root.join("assets/music-pool.json"),
            r#"{"soft-1":{"file":"assets/music/soft-1.mp3","tags":["calm"]}}"#,
        )
        .unwrap();
        let edited = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(
            !base.sources_in_sync(&edited),
            "a pool edit must resync sources"
        );

        // A new clip resyncs too, even though no manifest moved.
        std::fs::write(root.join("assets/music/soft-1.mp3"), vec![2u8; 32]).unwrap();
        assert!(
            !edited.sources_in_sync(&compute_provision_stamp(&root, "0.2.0", &agent_bin(&root))),
            "a new clip must resync sources"
        );
    }

    /// A rebuild under the same version redeploys the agent and nothing else.
    ///
    /// This is the gap the field exists for: the version string cannot see a
    /// rebuild, so `sources_in_sync` stays true while the bytes moved.
    #[test]
    fn a_rebuild_under_the_same_version_redeploys_the_agent_only() {
        let root = stamp_fixture("agent-drift");
        let bin = agent_bin(&root);
        let base = compute_provision_stamp(&root, "0.2.0", &bin);
        assert!(base.agent_in_sync(&compute_provision_stamp(&root, "0.2.0", &bin)));

        // Same version string, different bytes: drift.
        std::fs::write(&bin, b"agent-bytes-v2").unwrap();
        let rebuilt = compute_provision_stamp(&root, "0.2.0", &bin);
        assert!(
            !base.agent_in_sync(&rebuilt),
            "a rebuild must redeploy the agent"
        );
        assert!(
            base.sources_in_sync(&rebuilt),
            "…but must not resync sources"
        );
        assert!(base.tts_in_sync(&rebuilt), "…or touch the sidecar");

        // No local binary to hash means no opinion, never a reinstall loop.
        let nobin = compute_provision_stamp(&root, "0.2.0", &root.join("no-such-binary"));
        assert!(nobin.agent_hash.is_empty());
        assert!(base.agent_in_sync(&nobin));
    }

    /// A stamp written before `tts_hash` existed must still parse. Forcing the
    /// full slow path on every probe is the exact failure the stamp prevents.
    #[test]
    fn an_older_stamp_without_a_tts_hash_still_parses() {
        let old = r#"{"agent_version":"0.2.0","sources_hash":"aa","voices_hash":"bb"}"#;
        let s = parse_stamp(old).expect("an older payload must not read as garbage");
        assert_eq!(s.agent_version, "0.2.0");
        assert_eq!(s.tts_hash, "", "an absent field means 'this box has none'");
        assert_eq!(
            s.agent_hash, "",
            "ditto: drift once, then the fresh stamp records it"
        );
    }

    /// The Rust sidecar's artifacts are their own hash: a box on the Python path
    /// must not be re-provisioned because the models were re-baked.
    #[test]
    fn the_tts_artifacts_are_tracked_separately_from_the_sources() {
        let root = stamp_fixture("tts");
        let without = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_eq!(without.tts_hash.len(), 64);
        assert!(
            without.sources_in_sync(&without),
            "a stamp is always in sync with itself"
        );

        // Baking the models moves only the TTS hash.
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(root.join("models/manifest.json"), r#"{"files":{}}"#).unwrap();
        let baked = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(
            without.sources_in_sync(&baked),
            "baking models must not resync the Python path's sources"
        );
        assert!(
            !without.tts_in_sync(&baked),
            "baking models must be visible to a box that uses them"
        );

        // …and swapping a model file under an unchanged manifest is drift too.
        std::fs::write(root.join("models/vieneu_prefill.onnx"), vec![1u8; 64]).unwrap();
        let swapped = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(!baked.tts_in_sync(&swapped), "a swapped model must resync");
        assert!(
            baked.sources_in_sync(&swapped),
            "…and must not touch sources"
        );
    }

    #[test]
    fn a_stamp_payload_parses_and_garbage_does_not() {
        let s = ProvisionStamp {
            agent_version: "0.2.0".into(),
            sources_hash: "a".repeat(64),
            voices_hash: "b".repeat(64),
            tts_hash: "c".repeat(64),
            agent_hash: "d".repeat(64),
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
