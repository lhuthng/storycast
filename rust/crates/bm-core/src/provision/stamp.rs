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
    /// SHA-256 of the `bm-tts` bytes the inductor would push.
    ///
    /// The sidecar had no such field, and the consequence was the same silent
    /// staleness `agent_hash` exists to prevent, one binary over: `tts_hash`
    /// covers `models/` and not the binary, and `install_tts_runtime` only ran
    /// on a box that was *not* already configured. So rebuilding the sidecar
    /// and re-provisioning a working worker changed nothing — the new binary
    /// never left this disk while the box kept answering with the old one.
    ///
    /// `#[serde(default)]` for the same reason as the two above: an older
    /// stamp parses as "", which reads as drift once, redeploys, and then
    /// records the real hash.
    #[serde(default)]
    pub tts_bin_hash: String,
    /// The stages the bundle on this box covers, from the plan it was built
    /// for.
    ///
    /// Not a gate — a policy change drifts `sources_hash`, because the stage
    /// list is part of the manifest — but the one thing that lets a report say
    /// *which* stages a box holds, and so answer the question a widened policy
    /// raises: this box is being offered merge work and was handed no clips.
    ///
    /// `#[serde(default)]`: a stamp written before the field existed reads as
    /// an empty list, which is "unknown", not "nothing shipped".
    #[serde(default)]
    pub sources_stages: Vec<String>,
}

impl ProvisionStamp {
    /// Whether the baked voice store on this box still matches the one we would
    /// push.
    ///
    /// **Consulted again**, with the field's meaning narrowed to make that
    /// workable: it is `models/voices.json` by content, and nothing else.
    ///
    /// It was once computed over the clone manifest and `refs/` and then never
    /// read — which is how an edited reference clip shipped nowhere while every
    /// gate reported "in sync". Both of those are now *inputs to a bake* rather
    /// than files a box needs: the clone manifest travels in the bundle
    /// (`sources_hash`) and a reference clip reaches a box encoded, in the store
    /// this method gates. This field covers the one file the bundle does not
    /// carry: the store the sidecar loads at startup.
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

    /// Whether the worker's `bm-tts` binary still matches ours.
    ///
    /// Same "no opinion" rule as [`Self::agent_in_sync`]: an empty `want` means
    /// this inductor has no sidecar staged, so it never claims drift. Without
    /// that, a host that only bakes models would try to push a binary that does
    /// not exist on every provision.
    pub fn tts_bin_in_sync(&self, want: &ProvisionStamp) -> bool {
        want.tts_bin_hash.is_empty() || self.tts_bin_hash == want.tts_bin_hash
    }
}

/// Compute manifest stamp for detecting changes to sources, voices and sidecars.
///
/// Four SHA-256 digests, each over a canonical (sorted, newline-joined) view of
/// its inputs, so the same inputs produce the same hex string on any machine.
/// Each one gates a *different push*, and the split is what keeps one kind of
/// change from paying for another:
///
/// * `sources_hash` — the **manifest of the bundle** the push would send: one
///   sha256 per file, keyed by where it lands on the worker, plus the stage
///   list it was selected for, plus the agent version so a release bump
///   redeploys. (A rebuild under the *same* version is `agent_hash`'s job.)
///   This replaced a walk of `prompts/`, `refs/`, the clip directories and the
///   crawl scripts: 202 MB of directory signatures on every probe, for a
///   question that is now answered by the digest of the artifact itself. The
///   stage list is in there deliberately — widening a box's policy changes
///   what it must hold, and that has to be drift or the narrowing is silent.
///   `refs/` is no longer in any digest: no worker reads it (enrollment runs on
///   the inductor and ships encoded, inside `models/voices.json`), so a new
///   reference clip reaches a box as a *bake*, through `voices_hash`.
/// * `tts_hash` — the baked `models/` directory **minus `models/voices.json`**,
///   by signature, plus `manifest.json` by content. Excluding the store is what
///   lets a newly enrolled voice ship without re-sending 668 MB of weights, and
///   the set that remains is exactly the immutable one, so this digest is also
///   the natural name for a published artifact (see `docs/ARTIFACTS.md`).
/// * `voices_hash` — `models/voices.json` by content: the store the sidecar
///   loads at startup, which nothing else covers.
/// * `tts_bin_hash` — the `bm-tts` bytes, by content, so a rebuild reaches a box
///   that already has the right models.
pub fn compute_provision_stamp(
    layout: &crate::Layout,
    stages: &[bm_proto::Stage],
    agent_version: &str,
    agent_binary: &Path,
) -> anyhow::Result<ProvisionStamp> {
    let repo_root = layout.root.as_path();
    // What the push would send, hashed as a set. The plan is the same call the
    // push makes, so the digest and the artifact cannot describe different
    // files — which is the one property this gate exists to have.
    let plan = super::sources::Sources::plan(layout, stages)?;
    let sources_manifest = plan.manifest()?;
    let mut sources = Sha256::new();
    sources.update(agent_version.as_bytes());
    sources.update([0]);
    sources.update(super::sources::Sources::hash(&sources_manifest).as_bytes());
    sources.update([0]);
    // The Rust sidecar's *weights*, by directory signature rather than content:
    // `models/` is 668 MB and reading it would cost more than the provisioning
    // this digest exists to skip. `manifest.json` is read by content because it
    // is the authoritative statement of what the models *are* — a swapped file
    // under an unchanged manifest is exactly the drift worth catching, and the
    // directory signature only catches it if the mtime moved.
    //
    // `models/voices.json` is **excluded by name**, and that exclusion is the
    // point of this digest rather than a detail: it is the cluster's voice
    // roster, enrollment rewrites it, it is 492 KB of a 668 MB directory, and
    // folding it in here would make a single new voice re-send every weight in
    // the bake. `voices_hash` covers it instead.
    let mut tts = Sha256::new();
    if let Ok(bytes) = std::fs::read(repo_root.join("models/manifest.json")) {
        tts.update(b"models/manifest.json");
        tts.update([0]);
        tts.update(&bytes);
        tts.update([0]);
    }
    tts.update(signature_of_dir_skipping(&repo_root.join("models"), &[VOICE_STORE]).as_bytes());
    tts.update([0]);

    // The sidecar *binary*, by content, in a digest of its own.
    //
    // Separate from the weights above because the two drift independently: a
    // rebuild of `bm-tts` has to reach a box that already holds the right
    // models, and folding the binary in with them would answer a nine-megabyte
    // binary change with a 668 MB re-push. Absent is not an error — the models
    // can be baked on a machine that never builds the sidecar — and an empty
    // `want` reads as "no opinion" (see `tts_bin_in_sync`).
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
    // clone manifest and `refs/` are both `install_sources`' business and live
    // in `sources_hash` above. Content, not signature: it is 492 KB, and a
    // roster that changed at all has to reach the box, mtime or not.
    let mut voices = Sha256::new();
    let store = repo_root.join("models").join(VOICE_STORE);
    if let Ok(bytes) = std::fs::read(&store) {
        voices.update("models/voices.json".as_bytes());
        voices.update([0]);
        voices.update(&bytes);
        voices.update([0]);
    }

    Ok(ProvisionStamp {
        agent_version: agent_version.to_string(),
        sources_stages: sources_manifest.stages.clone(),
        sources_hash: hex_digest(sources.finalize()),
        voices_hash: hex_digest(voices.finalize()),
        tts_hash: hex_digest(tts.finalize()),
        // Empty, not the digest of nothing, when no sidecar is staged: this
        // field is the one whose "empty means no opinion" rule has to be able
        // to fire, and a hash of zero inputs is still a hash that no remote
        // box can match — which would read as permanent drift and push a binary
        // that does not exist here.
        tts_bin_hash: if tts_bin_staged {
            hex_digest(tts_bin.finalize())
        } else {
            String::new()
        },
        // Content, not signature: the binary is ~100 MB and hashing it costs
        // ~0.1 s locally, while a stale binary on a worker is silent drift.
        // Absent locally is not an error (see `agent_in_sync`).
        agent_hash: std::fs::read(agent_binary)
            .map(|bytes| hex_digest(Sha256::digest(&bytes)))
            .unwrap_or_default(),
    })
}

/// The one mutable file inside the otherwise immutable `models/` bake: the
/// cluster's voice roster, rewritten by enrollment. Named once, so the digest
/// that must avoid it and the digest that must cover it cannot drift apart.
const VOICE_STORE: &str = "voices.json";

/// The sidecar binaries a provision could push, relative to the repo root.
///
/// **Release builds, every target.** A debug binary is never pushed, and the
/// list this replaces named `debug/bm-tts` while missing both cross paths — so
/// on the one machine shape that actually provisions a Linux box from a Mac,
/// the binary that gets pushed was not hashed at all and the field would have
/// been decorative.
///
/// Hashing every target rather than only the one this host serves is coarse on
/// purpose: `compute_provision_stamp` takes no platform, because one inductor
/// can drive a mixed pool and does not know the target when it hashes. The
/// coarseness can only cause an *extra* redeploy — a rebuild of a target this
/// box does not use — never a missed one, and a missed one is the bug.
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
/// each file's length and mtime, recursively. Contents are never read — the one
/// caller left is `models/`, where 668 MB of weights would cost more to hash
/// than the provisioning the digest exists to skip, and mtime+size is exactly
/// the test `copy_dir` and rsync already use to decide "unchanged".
///
/// It was also how `prompts/`, the clip directories, the crawl scripts and
/// `refs/` were covered, before they became a bundle whose manifest hashes each
/// file by content. This is now the exception, not the rule.
///
/// Any entry whose file name is in `extra` is ignored.
///
/// `models/` needs it: that directory is a 668 MB immutable bake with exactly
/// one mutable file inside it, and the two have opposite requirements. Skipping
/// by *name* rather than by content or by listing what to include keeps the
/// exclusion honest when the bake grows — a new weight is covered by default,
/// and only `voices.json` is special.
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

    /// The stage list the fixture is stamped for: the two whose files it holds.
    const STAGES: [bm_proto::Stage; 2] = [bm_proto::Stage::Digest, bm_proto::Stage::Merge];

    /// The stamp for a plain root. The plan is built through the same call the
    /// push makes, so a fixture that cannot be planned is a test failure rather
    /// than a silently empty digest.
    fn stamp(root: &std::path::Path) -> ProvisionStamp {
        compute_provision_stamp(
            &crate::Layout::new(root),
            &STAGES,
            "0.2.0",
            &agent_bin(root),
        )
        .expect("the fixture plan must build")
    }

    fn stamp_v(root: &std::path::Path, version: &str) -> ProvisionStamp {
        compute_provision_stamp(
            &crate::Layout::new(root),
            &STAGES,
            version,
            &agent_bin(root),
        )
        .expect("the fixture plan must build")
    }

    /// The stamp with an explicit agent binary, for the rebuild cases that swap
    /// the bytes under it.
    fn stamp_bin(root: &std::path::Path, bin: &std::path::Path) -> ProvisionStamp {
        compute_provision_stamp(&crate::Layout::new(root), &STAGES, "0.2.0", bin)
            .expect("the fixture plan must build")
    }

    fn stamp_for(root: &std::path::Path, stages: &[bm_proto::Stage]) -> ProvisionStamp {
        compute_provision_stamp(&crate::Layout::new(root), stages, "0.2.0", &agent_bin(root))
            .expect("the fixture plan must build")
    }

    /// The layout a real provision uses on a machine with a workspace selected:
    /// the root carries the profile, the workspace carries the book. The stamp
    /// has to follow *that* split, because the cast files it ships are the
    /// book's.
    fn workspace_layout(root: &std::path::Path, name: &str) -> crate::Layout {
        crate::Layout {
            root: root.to_path_buf(),
            work: root.join("workspaces").join(name),
        }
    }

    #[test]
    fn a_stamp_is_stable_and_content_addressed() {
        let root = stamp_fixture("stable");
        let a = stamp(&root);
        let b = stamp(&root);
        assert_eq!(a, b, "nothing changed, so the stamp must not either");
        assert_eq!(a.sources_hash.len(), 64, "sha-256 hex is 64 chars");
        assert_eq!(a.voices_hash.len(), 64);
        assert!(a.sources_in_sync(&b) && a.voices_in_sync(&b));

        // A cast edit is a source change and nothing else.
        std::fs::write(root.join("data/cast.json"), r#"{"Narrator":"Adam"}"#).unwrap();
        let c = stamp(&root);
        assert_ne!(
            a.sources_hash, c.sources_hash,
            "a cast edit must resync sources"
        );
        assert_eq!(
            a.voices_hash, c.voices_hash,
            "…and must not re-enroll voices"
        );

        // A version bump redeploys the agent even when every file is identical.
        let d = stamp_v(&root, "0.3.0");
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
        let a = compute_provision_stamp(
            &workspace_layout(&root, "book"),
            &STAGES,
            "0.2.0",
            &agent_bin(&root),
        )
        .unwrap();
        std::fs::write(
            root.join("workspaces/book/data/cast-vieneu.json"),
            r#"{"A":"Quang Sơn"}"#,
        )
        .unwrap();
        let b = compute_provision_stamp(
            &workspace_layout(&root, "book"),
            &STAGES,
            "0.2.0",
            &agent_bin(&root),
        )
        .unwrap();
        assert_ne!(
            a.sources_hash, b.sources_hash,
            "a workspace voice swap must force a resync"
        );
        assert_eq!(a.voices_hash, b.voices_hash, "…and must not re-enroll");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The clone manifest travels in the bundle; a reference clip does not
    /// travel at all.
    ///
    /// This test replaces one that asserted `refs/` was part of the sources
    /// hash. That was true of the *push* — `install_sources` rsynced the whole
    /// directory, 144 MB of it — and it was the bug rather than the design: no
    /// worker reads a reference clip, because enrollment runs on the inductor
    /// (it needs the encoder, which a worker does not have) and what crosses is
    /// the encoded store in `models/voices.json`. A new or edited clip is an
    /// input to the next *bake*, so it drifts `voices_hash` when an enrollment
    /// rewrites the store, and drifts nothing until then. The one file that
    /// still has to reach a box is the declaration itself, which is why
    /// `voices.json` is in the bundle.
    #[test]
    fn the_clone_manifest_is_a_source_and_a_reference_clip_is_not() {
        let root = stamp_fixture("voices");
        let base = stamp(&root);

        // A rename in voices.json has to reach the worker's copy, so it is a
        // source change — and because the bundle is what ships the manifest, it
        // must NOT be a reason to re-push the model store.
        std::fs::write(root.join("voices.json"), r#"{"Storyteller":"refs/n.wav"}"#).unwrap();
        let renamed = stamp(&root);
        assert!(
            !base.sources_in_sync(&renamed),
            "a rename must resync the manifest"
        );
        assert!(
            base.voices_in_sync(&renamed),
            "…and must not look like a voice-store change"
        );
        assert!(base.tts_in_sync(&renamed), "…nor re-send 668 MB of weights");

        // A new reference clip changes nothing a box holds: it is this machine's
        // input to the next bake, and nothing in the manifest names it.
        std::fs::write(root.join("refs/m.wav"), vec![2u8; 64]).unwrap();
        let added = stamp(&root);
        assert!(
            renamed.sources_in_sync(&added),
            "a clip no cell of the manifest names must not resync anything"
        );
        assert!(renamed.tts_in_sync(&added) && renamed.voices_in_sync(&added));
    }

    /// Widening a box's policy is drift, because the stage list is part of the
    /// manifest.
    ///
    /// This is the guard the narrowed set rests on. A render-only box is sent
    /// no clips at all; without this assertion it could be handed merge work
    /// later and merge *silently silent* — the merge degrades a missing clip to
    /// one warning — while every log said the sources were in sync.
    #[test]
    fn a_wider_policy_is_sources_drift() {
        let root = stamp_fixture("policy");
        std::fs::create_dir_all(root.join("assets/effects")).unwrap();
        std::fs::write(
            root.join("assets/effect-pool.json"),
            r#"{"wind":{"tags":["wind"],"files":["effects/wind-1.mp3"]}}"#,
        )
        .unwrap();
        std::fs::write(root.join("assets/effects/wind-1.mp3"), vec![1u8; 32]).unwrap();

        let render_only = stamp_for(&root, &[bm_proto::Stage::Render]);
        let with_merge = stamp_for(&root, &[bm_proto::Stage::Render, bm_proto::Stage::Merge]);
        assert!(
            !render_only.sources_in_sync(&with_merge),
            "a box given merge must be re-provisioned for the clips it now needs"
        );
        assert_eq!(render_only.sources_stages, vec!["render".to_string()]);
        assert_eq!(
            with_merge.sources_stages,
            vec!["render".to_string(), "merge".to_string()],
            "the stages travel in canonical order, not the policy's own"
        );
        // The order the operator happens to list them in is not content.
        let reordered = stamp_for(&root, &[bm_proto::Stage::Merge, bm_proto::Stage::Render]);
        assert_eq!(with_merge.sources_hash, reordered.sources_hash);
        assert_eq!(with_merge.sources_stages, reordered.sources_stages);
    }

    /// The store the sidecar loads is its own gate: a newly enrolled voice must
    /// ship, and must not cost a re-push of the weights it sits among.
    ///
    /// This is the split `docs/ARTIFACTS.md` depends on. `models/` is 668 MB of
    /// immutable bake with one mutable 492 KB file inside it, and before this
    /// the two were one digest: enrolling a voice re-sent every weight, while
    /// excluding the store entirely would have hidden the enrollment instead.
    #[test]
    fn the_baked_voice_store_is_its_own_gate_and_not_a_model_change() {
        let root = stamp_fixture("store");
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(root.join("models/manifest.json"), r#"{"files":{}}"#).unwrap();
        std::fs::write(root.join("models/sea_g2p.bin"), vec![1u8; 64]).unwrap();
        std::fs::write(root.join("models/voices.json"), r#"{"presets":{"A":{}}}"#).unwrap();
        let base = stamp(&root);

        // Enrolling a voice rewrites the store and nothing else.
        std::fs::write(
            root.join("models/voices.json"),
            r#"{"presets":{"A":{},"B":{}}}"#,
        )
        .unwrap();
        let enrolled = stamp(&root);
        assert!(
            !base.voices_in_sync(&enrolled),
            "an enrollment must reach the box"
        );
        assert!(
            base.tts_in_sync(&enrolled),
            "…and must not re-send 668 MB of weights for a 492 KB file"
        );
        assert!(base.sources_in_sync(&enrolled));

        // The weights themselves still drift `tts_hash`, so excluding the store
        // did not exclude the directory. The size changes because a *signature*
        // is size+mtime, and a same-size rewrite inside one second is invisible
        // to it — the limit `manifest.json`-by-content and the publish gate
        // (`bake-models.py --check`, `16/16 files match`) exist to cover.
        std::fs::write(root.join("models/sea_g2p.bin"), vec![2u8; 128]).unwrap();
        let swapped = stamp(&root);
        assert!(
            !enrolled.tts_in_sync(&swapped),
            "a swapped weight must still resync the store"
        );
        assert!(
            enrolled.voices_in_sync(&swapped),
            "…without looking like an enrollment"
        );
    }

    /// A rebuilt sidecar reaches a box that already has the right models.
    ///
    /// The gap this closes: `tts_hash` covers `models/`, not the binary, and
    /// the sidecar push only ran on a box that was *not* already configured. So
    /// a rebuilt `bm-tts` never left this disk on a working worker, and the box
    /// kept serving the old one indefinitely — the `agent_hash` bug, one binary
    /// over.
    #[test]
    fn a_rebuilt_sidecar_drifts_only_the_binary_hash() {
        let root = stamp_fixture("sidecar-drift");
        let bin = root.join("rust/target/x86_64-unknown-linux-gnu/release/bm-tts");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"sidecar-bytes-v1").unwrap();
        let base = stamp(&root);
        assert!(!base.tts_bin_hash.is_empty());
        assert!(base.tts_bin_in_sync(&stamp(&root)));

        std::fs::write(&bin, b"sidecar-bytes-v2").unwrap();
        let rebuilt = stamp(&root);
        assert!(
            !base.tts_bin_in_sync(&rebuilt),
            "a rebuilt sidecar must redeploy"
        );
        assert!(
            base.sources_in_sync(&rebuilt) && base.tts_in_sync(&rebuilt),
            "…and must not drag the sources or the weights along"
        );
        assert!(base.agent_in_sync(&rebuilt), "…nor the agent");

        // The cross path is the one that actually gets pushed, and the list
        // this replaced hashed only `target/release` and `target/debug` — so on
        // a Mac provisioning a Linux box the field would have been empty.
        assert!(
            !rebuilt.tts_bin_hash.is_empty(),
            "the cross build is what provisioning pushes; it must be hashed"
        );

        // No sidecar on this host means no opinion, never a push of nothing.
        let none = compute_provision_stamp(
            &crate::Layout::new(root.join("elsewhere")),
            &STAGES,
            "0.2.0",
            &root.join("a"),
        )
        .unwrap();
        assert!(base.tts_bin_in_sync(&none));
    }

    /// A registered clip is a source; a clip nobody registers is not.
    ///
    /// The registry is the pool, so the selection is closed by construction:
    /// the old stamp hashed the clip *directories* by signature, which meant a
    /// file copied in and never registered drifted every box — and travelled to
    /// it, 25 MB at a time — while being unreachable by the merge forever.
    #[test]
    fn the_clip_pools_are_sources_and_an_unregistered_clip_is_not() {
        let root = stamp_fixture("pools");
        std::fs::create_dir_all(root.join("assets/music")).unwrap();
        std::fs::write(root.join("assets/music/soft-1.mp3"), vec![1u8; 32]).unwrap();
        std::fs::write(
            root.join("assets/music-pool.json"),
            r#"{"soft-1":{"tags":["soft"],"files":["music/soft-1.mp3"]}}"#,
        )
        .unwrap();
        let base = stamp(&root);

        // Re-running with nothing touched must not resync — otherwise every
        // provision would push the clips for no reason.
        assert!(base.sources_in_sync(&stamp(&root)));

        // The registry is a manifest: editing it changes what a scene means,
        // so the worker has to receive it.
        std::fs::write(
            root.join("assets/music-pool.json"),
            r#"{"soft-1":{"tags":["calm"],"files":["music/soft-1.mp3"]}}"#,
        )
        .unwrap();
        let edited = stamp(&root);
        assert!(
            !base.sources_in_sync(&edited),
            "a pool edit must resync sources"
        );

        // Replacing the bytes of a clip a registry names resyncs too: the
        // manifest hashes content, where the old directory signature only
        // caught a change the mtime moved for.
        std::fs::write(root.join("assets/music/soft-1.mp3"), vec![2u8; 32]).unwrap();
        let swapped = stamp(&root);
        assert!(
            !edited.sources_in_sync(&swapped),
            "a replaced clip must resync sources"
        );

        // …and a clip nothing registers neither travels nor drifts a box.
        std::fs::write(root.join("assets/music/leftover.mp3"), vec![3u8; 32]).unwrap();
        assert!(swapped.sources_in_sync(&stamp(&root)));
    }

    /// The crawlers a crawl box is given are sources too: an edit there must
    /// drift the stamp, or `:prov` reports "in sync" and every box keeps the
    /// old crawler while the inductor probes through the new one.
    ///
    /// The set is the *workspace's* `crawl/` plus the profile's templates, read
    /// through the same `Layout` the push uses — which is why switching books
    /// drifts the stamp: the crawlers travel with the book.
    #[test]
    fn a_workspace_crawler_edit_drifts_the_stamp() {
        let root = stamp_fixture("wscrawl");
        let crawl = [bm_proto::Stage::Crawl];
        let at = |ws: &str| {
            compute_provision_stamp(
                &workspace_layout(&root, ws),
                &crawl,
                "0.2.0",
                &agent_bin(&root),
            )
            .unwrap()
        };

        std::fs::create_dir_all(root.join("workspaces/book/crawl")).unwrap();
        std::fs::write(root.join("workspaces/book/crawl/site.lua"), "v1").unwrap();
        let a = at("book");

        // Editing the workspace crawler is a source change.
        std::fs::write(root.join("workspaces/book/crawl/site.lua"), "v2").unwrap();
        let b = at("book");
        assert_ne!(
            a.sources_hash, b.sources_hash,
            "a workspace crawler edit must resync sources"
        );

        // A different book is a different set: the crawler that is pushed is
        // the workspace's own, so switching workspaces drifts the stamp too.
        std::fs::create_dir_all(root.join("workspaces/other/crawl")).unwrap();
        std::fs::write(root.join("workspaces/other/crawl/site.lua"), "other").unwrap();
        let c = at("other");
        assert_ne!(
            b.sources_hash, c.sources_hash,
            "switching workspaces must resync sources"
        );
        // …and a crawl box holds the profile's templates either way: they are
        // what a crawler named by a registry resolves against.
        assert_eq!(c.sources_stages, vec!["crawl".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A rebuild under the same version redeploys the agent and nothing else.
    ///
    /// This is the gap the field exists for: the version string cannot see a
    /// rebuild, so `sources_in_sync` stays true while the bytes moved.
    #[test]
    fn a_rebuild_under_the_same_version_redeploys_the_agent_only() {
        let root = stamp_fixture("agent-drift");
        let bin = agent_bin(&root);
        let base = stamp_bin(&root, &bin);
        assert!(base.agent_in_sync(&stamp_bin(&root, &bin)));

        // Same version string, different bytes: drift.
        std::fs::write(&bin, b"agent-bytes-v2").unwrap();
        let rebuilt = stamp_bin(&root, &bin);
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
        let nobin = stamp_bin(&root, &root.join("no-such-binary"));
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
        assert_eq!(
            s.tts_bin_hash, "",
            "ditto for the sidecar: one redeploy, then the hash is recorded"
        );
    }

    /// The Rust sidecar's artifacts are their own hash: a box on the Python path
    /// must not be re-provisioned because the models were re-baked.
    #[test]
    fn the_tts_artifacts_are_tracked_separately_from_the_sources() {
        let root = stamp_fixture("tts");
        let without = stamp(&root);
        assert_eq!(without.tts_hash.len(), 64);
        assert!(
            without.sources_in_sync(&without),
            "a stamp is always in sync with itself"
        );

        // Baking the models moves only the TTS hash.
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(root.join("models/manifest.json"), r#"{"files":{}}"#).unwrap();
        let baked = stamp(&root);
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
        let swapped = stamp(&root);
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
            tts_bin_hash: "e".repeat(64),
            sources_stages: vec!["digest".into(), "merge".into()],
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
