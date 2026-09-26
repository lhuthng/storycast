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
    /// gate reported "in sync". Both of those are pushed by `install_sources`,
    /// so they now live in `sources_hash`, where the push they gate actually
    /// reads them. This field covers the one file that push does not carry: the
    /// store the sidecar loads at startup.
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
/// * `sources_hash` — `prompts/` by signature, plus the *content* of the small
///   manifests the worker must match exactly (`requirements.txt`, the cast
///   files, the clone manifest `voices.json`, the scene map and the three
///   clip-pool registries), plus the effect, music, inject **and `refs/`** clip
///   directories by signature, plus the crawl scripts by content, plus the
///   agent version so a release bump redeploys. (A rebuild under the *same*
///   version is `agent_hash`'s job.) **Everything `install_sources` pushes is
///   in here**, and `refs/` was the one that was not.
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

    // …and the reference clips, because `install_sources` pushes `refs/`
    // whether or not any digest here mentions it.
    //
    // This is a fix, not a nicety. `refs/` used to be folded into `voices_hash`
    // — a digest provisioning computed, carried, and never read — so adding or
    // editing a clip drifted no gate at all. A worker kept the stale clip while
    // every box and every log said "in sync", and a render naming the new voice
    // failed only on the boxes that never received it. A signature rather than
    // content, for the reason `refs/` is 125 MB of audio and mtime+size is the
    // test rsync itself uses.
    sources.update(b"refs");
    sources.update([0]);
    sources.update(signature_of_dir(&repo_root.join("refs")).as_bytes());
    sources.update([0]);

    // The crawl scripts, by **content**: they are small, and they decide which
    // bytes become a chapter. A worker left holding a stale crawler would fetch
    // something different from what the inductor's own probe read, and the
    // directory signature would only catch that if the mtime moved — which
    // `cp -p`, a checkout and rsync all decline to guarantee. Both sources are
    // hashed: the profile's shared `assets/crawl/` and the active workspace's
    // own `crawl/` (which `resolve_script` searches first), so an edit to
    // either drifts the stamp and reaches every box with the next `:prov`.
    {
        // The workspace's dir, by `Layout::resolve_or_root`'s semantics: a
        // missing or stale pointer means the root *is* the workspace, whose
        // crawlers live at `<root>/crawl`. The same dir `install_sources`
        // pushes, so the stamp and the sync can never disagree about what
        // "the workspace's crawler" is.
        let ws = std::fs::read_to_string(repo_root.join(".bm/active-workspace"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let ws_dir = repo_root.join("workspaces").join(&ws);
        let ws_crawl = if !ws.is_empty() && ws_dir.is_dir() {
            ws_dir.join("crawl")
        } else {
            repo_root.join("crawl")
        };
        let mut files = Vec::new();
        let mut stack = vec![repo_root.join("assets/crawl"), ws_crawl];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    files.push(p);
                }
            }
        }
        files.sort();
        for p in files {
            let (Ok(rel), Ok(bytes)) = (p.strip_prefix(repo_root), std::fs::read(&p)) else {
                continue;
            };
            sources.update(rel.display().to_string().as_bytes());
            sources.update([0]);
            sources.update(&bytes);
            sources.update([0]);
        }
    }

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

    ProvisionStamp {
        agent_version: agent_version.to_string(),
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
    }
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
/// each file's length and mtime, recursively. Contents are never read — this
/// runs over `refs/`, where a single clip is megabytes and mtime+size is
/// exactly the test `copy_dir` and rsync already use to decide "unchanged".
fn signature_of_dir(dir: &Path) -> String {
    signature_of_dir_skipping(dir, &[])
}

/// [`signature_of_dir`], ignoring any entry whose file name is in `extra`.
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

    /// The clone manifest and `refs/` are **sources**, because that is the push
    /// that carries them.
    ///
    /// This test replaces one that asserted the opposite for `refs/` ("a new
    /// clip must re-enroll" / "refs/ is not part of the sources hash"). Both
    /// halves were describing a bug rather than a design: `voices_hash` was
    /// never read, so a new reference clip drifted no gate that any push
    /// consulted, and the clip shipped nowhere. The gate is now the one the
    /// push reads.
    #[test]
    fn the_clone_manifest_and_the_reference_clips_are_sources() {
        let root = stamp_fixture("voices");
        let base = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));

        // A rename in voices.json has to reach the worker's copy, so it is a
        // source change — and because `install_sources` is what ships the
        // manifest, it must NOT be a reason to re-push the model store.
        std::fs::write(root.join("voices.json"), r#"{"Storyteller":"refs/n.wav"}"#).unwrap();
        let renamed = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(
            !base.sources_in_sync(&renamed),
            "a rename must resync the manifest"
        );
        assert!(
            base.voices_in_sync(&renamed),
            "…and must not look like a voice-store change"
        );
        assert!(base.tts_in_sync(&renamed), "…nor re-send 668 MB of weights");

        // A new clip changes the refs signature without touching the manifest —
        // and `install_sources` pushes `refs/`, so this must drift sources.
        std::fs::write(root.join("refs/m.wav"), vec![2u8; 64]).unwrap();
        let added = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(
            !renamed.sources_in_sync(&added),
            "a new clip must resync the directory that is pushed for it"
        );
        assert!(
            renamed.tts_in_sync(&added),
            "…without re-sending the weights"
        );
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
        let base = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));

        // Enrolling a voice rewrites the store and nothing else.
        std::fs::write(
            root.join("models/voices.json"),
            r#"{"presets":{"A":{},"B":{}}}"#,
        )
        .unwrap();
        let enrolled = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
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
        let swapped = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
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
        let base = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert!(!base.tts_bin_hash.is_empty());
        assert!(base.tts_bin_in_sync(&compute_provision_stamp(&root, "0.2.0", &agent_bin(&root))));

        std::fs::write(&bin, b"sidecar-bytes-v2").unwrap();
        let rebuilt = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
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
        let none = compute_provision_stamp(&root.join("elsewhere"), "0.2.0", &root.join("a"));
        assert!(base.tts_bin_in_sync(&none));
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

    /// The active workspace's own crawlers are sources too: an edit there must
    /// drift the stamp, or `:prov` reports "in sync" and every box keeps the
    /// old crawler while the inductor probes through the new one.
    #[test]
    fn a_workspace_crawler_edit_drifts_the_stamp() {
        let root = stamp_fixture("wscrawl");
        std::fs::create_dir_all(root.join(".bm")).unwrap();
        std::fs::write(root.join(".bm/active-workspace"), "book\n").unwrap();
        std::fs::create_dir_all(root.join("workspaces/book/crawl")).unwrap();
        std::fs::write(root.join("workspaces/book/crawl/site.lua"), "v1").unwrap();
        let a = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));

        // Editing the workspace crawler is a source change.
        std::fs::write(root.join("workspaces/book/crawl/site.lua"), "v2").unwrap();
        let b = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_ne!(
            a.sources_hash, b.sources_hash,
            "a workspace crawler edit must resync sources"
        );

        // A different workspace's dir is a different input: switching the
        // pointer to a book with another crawler drifts the stamp as well.
        std::fs::create_dir_all(root.join("workspaces/other/crawl")).unwrap();
        std::fs::write(root.join("workspaces/other/crawl/site.lua"), "other").unwrap();
        std::fs::write(root.join(".bm/active-workspace"), "other\n").unwrap();
        let c = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_ne!(
            b.sources_hash, c.sources_hash,
            "switching workspaces must resync sources"
        );

        // A stale pointer falls back to the root's own `crawl/` — the same
        // directory `install_sources` pushes in that case, never the missing
        // workspace's.
        std::fs::write(root.join(".bm/active-workspace"), "gone\n").unwrap();
        std::fs::create_dir_all(root.join("crawl")).unwrap();
        std::fs::write(root.join("crawl/site.lua"), "legacy").unwrap();
        let d = compute_provision_stamp(&root, "0.2.0", &agent_bin(&root));
        assert_ne!(c.sources_hash, d.sources_hash);
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
            tts_bin_hash: "e".repeat(64),
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
