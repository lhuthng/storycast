//! The files a worker needs — selected by what its policy lets it run, and
//! carried as one bundle.
//!
//! **Why this exists.** Provisioning used to rsync three whole trees
//! (`prompts/`, `assets/`, `refs/`) with `--delete`. Measured on this repo that
//! is 202 MB per box per push, of which `refs/` was 144 MB — and 107 MB of that
//! was the operator's own download staging (`refs/temp/`), which no worker has
//! ever read. Every one of those bytes also had to be walked again every time
//! the stamp asked *is what is on the box still what we publish?*. The trees
//! grow with the book, so the cost grew with it while the need did not.
//!
//! **What a worker is actually handed is narrower than the tree.** The offers
//! carry the chapter, the script, the cast and the bible, so a box never needs
//! a chapter to work; and the fingerprinted merge design is computed on the
//! inductor (`design::MergeDesign::load` has no caller in the agent), so a
//! merge box does not need to recompute it either. What each stage reads from
//! these trees is:
//!
//! | stage | files |
//! |---|---|
//! | crawl | the crawler: workspace `crawl/` + `assets/crawl/templates/` |
//! | digest | the four registries the prompt and the validators read |
//! | render | nothing here — the voice store travels in `models/` |
//! | merge | the scene map, the three clip pools, and the clips they register |
//!
//! `prompts/` is in **every** bundle rather than in the digest's share of it.
//! It is 21 KB against a 59 MB artifact, and it is the one file a digest cannot
//! begin without: a box whose policy gains `digest` after its last push was
//! offered the work as soon as the operator enabled it, ran the stage, and died
//! on `reading prompt template … No such file or directory` until somebody
//! re-provisioned. The rest of the narrowing still stands — the registries, the
//! crawlers and the clips are still selected, and `stages` in the manifest still
//! says which of them a box has.
//! `refs/` is in none of them: enrollment happens on the inductor, which is the
//! machine with the encoder, and what crosses to a worker is the *encoded* store
//! (`models/voices.json` holds codes and speaker embeddings — not one of its
//! entries names a clip). The clips `voices.json` and `voice-pool.json` name are
//! read here, by `:A`/`:N`, the bake and audition.
//!
//! **Two properties make the set safe to narrow.** It is *closed*: a file is in
//! it because a manifest names it or a stage declares it, never because it
//! happened to be in a directory — so a clip copied into `assets/music/` but
//! left unregistered stops travelling. And it is *hashed as a set*: the
//! manifest, including the stage list it was built for, is the stamp's
//! `sources_hash`. Widening a box's policy is therefore drift by construction,
//! which is what stops the narrowing from being silent.
//!
//! Packing is `tar` + `zstd`, one artifact, named by that hash. Compression buys
//! little here — the bundle is 57 MB of already-compressed mp3 and 100 KB of
//! text — so the reason to pack is not size: it is that a push becomes one file
//! with one name, the extraction prunes the trees it owns exactly, and the stamp
//! is a hash of a manifest rather than a walk of 202 MB of mtimes.

use anyhow::{Context, Result};
use bm_proto::{Stage, TaskPref};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The artifact's name, at the worker root.
pub const BUNDLE_NAME: &str = "sources.tar.zst";

/// The manifest inside the artifact, at its top level (so it lands at the
/// worker root beside everything it describes).
pub const MANIFEST_NAME: &str = "sources-manifest.json";

/// The trees the bundle owns. Extraction prunes exactly these, which is what
/// gives a push rsync's `--delete` semantics: the archive is the whole truth
/// for them, so a file that fell out of the selection goes.
pub const OWNED: [&str; 3] = ["prompts", "assets", "crawl"];

/// One filesystem read of the tree the bundle owns on a *worker*: everything
/// under these names is the bundle's to replace, and anything else on a box is
/// the box's own state.
pub fn is_owned(rel: &str) -> bool {
    OWNED.iter().any(|o| rel == *o || rel.starts_with(&format!("{o}/")))
}

/// The registry files each stage reads, relative to `assets/`.
///
/// Kept per stage rather than as one list so a digest-only box is not handed
/// `music-pool.json` (the digest never opens it: its prompt renders the palette
/// from the scene map) and a merge box is not handed `tag-aliases.json` (aliases
/// are applied in the digest, before anything is written).
fn registries(stage: Stage) -> &'static [&'static str] {
    match stage {
        // The prompt is rendered from the scene map, the effect pool and the
        // inject pool; the aliases are read by the validator.
        Stage::Digest => &[
            "scene-map.json",
            "effect-pool.json",
            "inject-pool.json",
            "tag-aliases.json",
        ],
        // `assemble` reads the scene map and the inject pool; `ambience` reads
        // the scene map and all three pools. No aliases: nothing on this path
        // opens them.
        Stage::Merge => &[
            "scene-map.json",
            "effect-pool.json",
            "music-pool.json",
            "inject-pool.json",
        ],
        Stage::Crawl | Stage::Render => &[],
    }
}

/// Which stages a machine's policy lets it run, in [`Stage::ALL`] order.
///
/// Canonical order, deliberately: the policy list's order is the *scheduler's*
/// preference, and re-ordering it must not look like a change to what is on the
/// disk. A box whose stages were merely reordered gets an identical bundle.
pub fn stages_of(policy: &[TaskPref]) -> Vec<Stage> {
    Stage::ALL
        .into_iter()
        .filter(|s| policy.iter().any(|p| p.stage == *s && p.enabled))
        .collect()
}

/// One file: where it lands on the worker, and the base its path is relative to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The path as it must land on the worker, relative to the worker root —
    /// `assets/music/sad-bg-1.mp3`, `data/cast.json`. This is the "correct path
    /// system": it is what `tar -C <base> -T <list>` writes and what the
    /// manifest is keyed by, so the archive, the manifest and the resolved tree
    /// cannot disagree.
    pub to: String,
    /// The directory `to` is relative to locally, and the `-C` the packer uses.
    pub base: PathBuf,
    pub from: PathBuf,
    pub bytes: u64,
}

/// The whole set: what is being sent, and what a registry named that is not on
/// disk.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// The stages this set was built for — part of the manifest, so a policy
    /// change is drift.
    pub stages: Vec<Stage>,
    /// Sorted by `to`, so the archive's member order is a property of the set.
    pub members: Vec<Member>,
    /// Clips a registry names that are not on disk here. Reported, not shipped:
    /// the merge degrades them to one warning each, and refusing to provision a
    /// box over one lost clip would be worse than the silence it already
    /// handles.
    pub missing: Vec<String>,
}

impl Sources {
    /// Select the set for `stages`.
    pub fn plan(layout: &crate::Layout, stages: &[Stage]) -> Result<Self> {
        let stages: Vec<Stage> = Stage::ALL
            .into_iter()
            .filter(|s| stages.contains(s))
            .collect();
        // **A bundle for no stage is not a bundle.** It would carry the clone
        // manifest and nothing else, and delivery *prunes* every tree the
        // archive owns before it extracts — so pushing it would delete a box's
        // prompts, assets and crawlers to put nothing in their place. A policy
        // that enables nothing is a box that is deliberately not working, and
        // the operator has to say what it should run before it is sent
        // anything.
        if stages.is_empty() {
            anyhow::bail!(
                "this box's policy covers no stage — enable one (P) and provision again; \
                 an empty bundle would only prune its prompts and assets"
            );
        }
        let mut out = Sources {
            stages,
            members: Vec::new(),
            missing: Vec::new(),
        };
        let root = &layout.root;

        // The clone manifest: 5 KB, and the inductor's warnings are computed
        // against it, so a box holding a different declaration is a silent
        // desync even though no stage opens it. Cheap enough to always send.
        out.push_file(root, "voices.json");

        // The prompts, on the same reasoning and with more force: a digest
        // reads `prompts/analyze.txt` before it does anything else, a box can
        // gain `digest` at any time by a single keypress in the policy screen,
        // and a stage whose *whole* input is a file is a stage that cannot
        // degrade — it fails, on every retry, until the operator notices. 21 KB
        // of text is not worth a class of failure.
        out.push_tree(root, "prompts");

        for stage in out.stages.clone() {
            for reg in registries(stage) {
                out.push_file(root, &format!("assets/{reg}"));
            }
            match stage {
                Stage::Crawl => {
                    // The bundled templates: a crawler named by a registry
                    // resolves out of `assets/crawl/templates/`.
                    out.push_tree(root, "assets/crawl");
                    // …and the workspace's own crawlers, which
                    // `resolve_script` searches first: a book whose site needs
                    // its own script keeps it out of the shared profile tree.
                    out.push_tree(&layout.work, "crawl");
                }
                // The registries the digest prompt renders from are still its
                // own: they are JSON the merge path shares, so a merge box
                // already has them and a digest-only box gets them here.
                Stage::Digest => {}
                Stage::Render | Stage::Merge => {
                    // The cast files decide the *filenames* a render writes and a
                    // merge looks for. The offer wins when it carries one (it
                    // always does on the current path), so this is the fallback
                    // for an older inductor — which is exactly when a box that is
                    // missing them names every segment differently and finds
                    // none.
                    for engine in ["vieneu", "gemini"] {
                        let src = layout.cast(engine);
                        if let Ok(rel) = src.strip_prefix(&layout.work) {
                            out.push_file(&layout.work, &rel.display().to_string());
                        }
                    }
                }
            }
        }

        // The clips. Only for a merge box, and only the ones a registry
        // registers: the registry is the pool, so a file nothing names can
        // never play.
        if out.stages.contains(&Stage::Merge) {
            out.push_clips(layout);
        }

        // Attribution for the media, whenever any of it travels.
        if out.stages.iter().any(|s| *s != Stage::Render) {
            out.push_file(root, "assets/LICENSES.json");
        }

        // Sorted *and* deduped: the stages overlap on purpose (scene-map and
        // the effect pool are read by both digest and merge), and a member named
        // twice would be staged twice — the packer's symlink step refuses the
        // second one, which is how this surfaced.
        out.members.sort_by(|a, b| a.to.cmp(&b.to));
        out.members.dedup_by(|a, b| a.to == b.to);
        Ok(out)
    }

    /// Every file the three clip registries name, resolved under `assets/`.
    fn push_clips(&mut self, layout: &crate::Layout) {
        let assets = layout.assets();
        for kind in crate::audio_pool::PoolKind::ALL {
            let pool = crate::audio_pool::load_pool(&layout.pool(kind));
            for (sound, entry) in pool {
                for file in entry.files {
                    // The registry's paths are relative to `assets/`, the same
                    // convention the merge resolves them by.
                    if file.starts_with('/') || file.contains("..") {
                        self.missing
                            .push(format!("{sound}: {file} (not under assets/)"));
                        continue;
                    }
                    let from = assets.join(&file);
                    if !from.is_file() {
                        self.missing.push(format!("{sound}: {file}"));
                        continue;
                    }
                    // The member name is the registry path under `assets/`,
                    // because that is where it has to *land*: the merge resolves
                    // it from the worker's own `assets/`.
                    self.push_member(&layout.root, format!("assets/{file}"), from);
                }
            }
        }
        self.missing.sort();
        self.missing.dedup();
    }

    /// One file, addressed as `rel` under `base` — and landed at `rel` too.
    ///
    /// A missing file is skipped rather than fatal, so a profile that ships no
    /// `LICENSES.json` is not an error.
    fn push_file(&mut self, base: &Path, rel: &str) -> bool {
        let from = base.join(rel);
        if !from.is_file() {
            return false;
        }
        self.push_member(base, rel.to_string(), from);
        true
    }

    /// A whole directory, `.DS_Store` skipped.
    ///
    /// OS noise is not content, and it is the one thing that would make the
    /// manifest depend on whether anybody opened the folder in Finder.
    fn push_tree(&mut self, base: &Path, rel: &str) {
        let mut stack = vec![base.join(rel)];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            let mut paths: Vec<PathBuf> =
                entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.file_name().and_then(|n| n.to_str()) == Some(".DS_Store") {
                    continue;
                }
                if p.is_dir() {
                    stack.push(p);
                } else if p.is_file() {
                    let to = p.strip_prefix(base).unwrap().display().to_string();
                    self.push_member(base, to, p);
                }
            }
        }
    }

    /// Record one member: the base it is relative to, the name it lands under,
    /// and the file it comes from.
    ///
    /// The invariant is `from == base.join(to)`, and it is *enforced* because
    /// the packer depends on it twice over: `tar -C base -T <names>` names the
    /// member by that relative path, and the manifest is keyed by `to` as the
    /// worker will see it. A member whose two halves disagree extracts to a
    /// path nothing resolves — the first version of the clip selection shipped
    /// every sound into the worker root instead of `assets/`, and this assertion
    /// is what caught it.
    fn push_member(&mut self, base: &Path, to: String, from: PathBuf) {
        debug_assert_eq!(
            base.join(&to),
            from,
            "a member's source must be its landing path under its base"
        );
        let bytes = std::fs::metadata(&from).map(|m| m.len()).unwrap_or(0);
        self.members.push(Member {
            to,
            base: base.to_path_buf(),
            from,
            bytes,
        });
    }

    /// Total bytes, uncompressed.
    pub fn bytes(&self) -> u64 {
        self.members.iter().map(|m| m.bytes).sum()
    }

    /// `path -> sha256`, plus the stages this set was built for.
    ///
    /// The manifest *is* the path system: every member is keyed by where it
    /// lands, so the artifact carries its own map from the archive to the
    /// worker's tree.
    pub fn manifest(&self) -> Result<SourcesManifest> {
        let mut files = BTreeMap::new();
        for m in &self.members {
            let bytes = std::fs::read(&m.from)
                .with_context(|| format!("reading {}", m.from.display()))?;
            files.insert(m.to.clone(), hex(Sha256::digest(&bytes)));
        }
        Ok(SourcesManifest {
            stages: self.stages.iter().map(|s| s.as_str().to_string()).collect(),
            files,
        })
    }

    /// The stamp's `sources_hash`: the manifest, folded in sorted order.
    ///
    /// Takes the manifest rather than recomputing it, so the digest the stamp
    /// compares is the digest of the artifact that was actually pushed.
    pub fn hash(manifest: &SourcesManifest) -> String {
        let mut h = Sha256::new();
        h.update(b"bm-sources-v1");
        h.update([0]);
        for stage in &manifest.stages {
            h.update(stage.as_bytes());
            h.update([0]);
        }
        h.update([0]);
        for (path, digest) in &manifest.files {
            h.update(path.as_bytes());
            h.update([0]);
            h.update(digest.as_bytes());
            h.update([0]);
        }
        hex(h.finalize())
    }

    /// A line per layer for the provision log: how many files, how many bytes,
    /// and what answered.
    pub fn summary(&self) -> String {
        let stages: Vec<&str> = self.stages.iter().map(|s| s.as_str()).collect();
        let clips = self
            .members
            .iter()
            .filter(|m| m.to.starts_with("assets/effects/") || m.to.starts_with("assets/music/") || m.to.starts_with("assets/injects/"))
            .count();
        format!(
            "{} file(s), {:.1} MB, for {} ({} clip(s))",
            self.members.len(),
            self.bytes() as f64 / 1e6,
            if stages.is_empty() {
                "no stage".to_string()
            } else {
                stages.join("+")
            },
            clips
        )
    }

    /// Write the bundle: `tar` over the members, `zstd` over the tar, one file.
    ///
    /// The members are **staged as symlinks** under the paths they land on, and
    /// one `tar -C <stage> -T <list> -h` carries the lot. Two reasons, and the
    /// first is not an optimisation:
    ///
    /// * macOS's `bsdtar` resolves every `-T` list against the **last** `-C`, so
    ///   a per-group `-C`/`-T` invocation — the obvious way to put members from
    ///   two directories into one archive — silently looks for them in the wrong
    ///   place. GNU tar is positional and finds them, which is exactly the kind
    ///   of host-dependent difference a Linux-only test would not see.
    /// * A staging *copy* would move 58 MB per push for no reason; a symlink
    ///   moves nothing and `-h` stores the content it points at.
    ///
    /// The member list is a file rather than one argument per member: it runs to
    /// a hundred names, and an argument vector that long is a portability
    /// question nobody needs to answer.
    pub fn pack(&self, manifest: &SourcesManifest, out: &Path) -> Result<()> {
        let staging = out
            .parent()
            .context("bundle path has no parent directory")?
            .to_path_buf();
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating {}", staging.display()))?;

        // The manifest is a member of the archive and lands beside what it
        // describes, so the box can always say what it was handed. Staged in a
        // scratch directory of its own rather than beside the artifact, because
        // two bundles in one directory must not fight over one name.
        let scratch = staging.join(format!(".pack.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)
            .with_context(|| format!("creating {}", scratch.display()))?;

        let tmp = out.with_extension(format!("zst.tmp{}", std::process::id()));
        let packed = (|| -> Result<()> {
            crate::atomic_write(
                &scratch.join(MANIFEST_NAME),
                &String::from_utf8_lossy(&serde_json::to_vec_pretty(manifest)?),
            )?;
            let mut names = vec![MANIFEST_NAME.to_string()];
            for m in &self.members {
                let at = scratch.join(&m.to);
                if let Some(parent) = at.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("staging {}", m.to))?;
                }
                // Canonicalised, because a symlink is resolved from where it
                // sits: a relative source path would point into the stage.
                let target = std::fs::canonicalize(&m.from)
                    .with_context(|| format!("resolving {}", m.from.display()))?;
                std::os::unix::fs::symlink(&target, &at)
                    .with_context(|| format!("staging {}", m.to))?;
                names.push(m.to.clone());
            }
            names.sort();
            let list = scratch.join("members.list");
            crate::atomic_write(&list, &format!("{}\n", names.join("\n")))?;

            let _ = std::fs::remove_file(&tmp);
            let mut child = std::process::Command::new("tar")
                .arg("-cf")
                .arg("-")
                .arg("-h")
                .arg("--no-recursion")
                .arg("-C")
                .arg(&scratch)
                .arg("-T")
                .arg(&list)
                // No AppleDouble. macOS `tar` (libarchive) writes a `._name`
                // member for every file carrying an extended attribute — and
                // `com.apple.provenance` is on a great many of them, including
                // ones this process just wrote — which a Linux box unpacks as a
                // real file beside the content: 115 of them, one per member, on
                // the first worker this shipped to. This machine's own `tar -t`
                // *hides* those members (it reads them as the metadata of the
                // file beside them), so the junk is visible only from the far
                // side, which is where it lands. `tar` on Linux ignores the
                // variable, unlike `--no-mac-metadata`, which it would reject.
                .env("COPYFILE_DISABLE", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("spawning tar (is tar on PATH?)")?;
            let stdout = child.stdout.take().context("tar stdout")?;
            let zstd = std::process::Command::new("zstd")
                .arg("-q")
                .arg("-3")
                .arg("-o")
                .arg(&tmp)
                .stdin(std::process::Stdio::from(stdout))
                .status()
                .context("spawning zstd (is zstd on PATH? `brew install zstd`)")?;
            let tar_status = child.wait().context("waiting for tar")?;
            if !tar_status.success() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                anyhow::bail!("tar failed: {}", crate::util::head_chars(err.trim(), 300));
            }
            if !zstd.success() {
                anyhow::bail!("zstd failed (exit {:?})", zstd.code());
            }
            std::fs::rename(&tmp, out)
                .with_context(|| format!("moving {} into place", out.display()))?;
            Ok(())
        })();

        let _ = std::fs::remove_dir_all(&scratch);
        packed
    }
}

/// The manifest inside the artifact: the stages it was built for, and one
/// sha256 per path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourcesManifest {
    /// The stages the set covers. In the manifest *on purpose*: widening a
    /// box's policy has to be visible to the drift check, or the box quietly
    /// keeps running a stage whose files it was never given.
    pub stages: Vec<String>,
    /// Worker-relative path -> sha256 of the bytes.
    pub files: BTreeMap<String, String>,
}

/// The shell the worker runs to take delivery.
///
/// Prune, then extract, in that order: a push is a *replacement*, and the trees
/// named in [`OWNED`] are the archive's to own. Without the prune a clip that
/// left a registry would live on the box for ever — the exact failure rsync's
/// `--delete` was there to prevent — and `refs/` (144 MB of the inductor's own
/// material on every box that predates this) would never leave.
///
/// `unzstd` is not assumed present by name: `zstd -dc` is the same binary the
/// box needs anyway, and a box without it is told so in one line rather than
/// failing with a shell error naming nothing.
pub fn extract_script() -> String {
    let owned: Vec<String> = OWNED
        .iter()
        .map(|o| format!("\"$D/{o}\""))
        .collect();
    format!(
        r#"set -e
D="$HOME/{d}"
command -v zstd >/dev/null || {{ echo "zstd-missing on this box"; exit 6; }}
rm -rf {owned} "$D/refs" "$D/data/cast.json" "$D/data/cast-vieneu.json"
zstd -dc "$D/{bundle}" | tar -xf - -C "$D"
rm -f "$D/{bundle}"
test -f "$D/{manifest}" || {{ echo "bundle carried no {manifest}"; exit 8; }}
echo "SOURCES-OK"
"#,
        d = crate::provision::REMOTE_DIR,
        owned = owned.join(" "),
        bundle = BUNDLE_NAME,
        manifest = MANIFEST_NAME,
    )
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let mut out = String::with_capacity(bytes.as_ref().len() * 2);
    for b in bytes.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bm_proto::TaskPref;

    /// A checkout with the tracked fixture profile, a cast file, a crawler, and
    /// one registered *and* one unregistered clip.
    ///
    /// A named directory under the system temp root rather than a `TempDir`:
    /// bm-core has no `tempfile`, and every other test in this crate builds its
    /// fixture the same way. Named per test, because the suite runs in parallel
    /// inside one process.
    fn fixture(name: &str) -> crate::Layout {
        let dir = std::env::temp_dir().join(format!("bm-sources-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        crate::profile::install_fixture(&dir).expect("fixture profile");
        let l = crate::Layout::new(&dir);
        for rel in [
            "assets/effects/night-1.mp3",
            "assets/music/market-bg-1.mp3",
            "assets/injects/coin-1.mp3",
            // In the directory, named by no registry: the file the old
            // wholesale push sent and the merge could never pick.
            "assets/music/leftover-bg-9.mp3",
        ] {
            let p = l.assets().join(rel.trim_start_matches("assets/"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        std::fs::create_dir_all(l.work.join("data")).unwrap();
        std::fs::write(l.work.join("data/cast.json"), "{}").unwrap();
        std::fs::write(l.root.join("voices.json"), r#"{"Narrator":"refs/narrator.mp3"}"#).unwrap();
        std::fs::create_dir_all(l.work.join("crawl")).unwrap();
        std::fs::write(l.work.join("crawl/site.lua"), "-- crawl").unwrap();
        // Finder noise must never enter the manifest.
        std::fs::write(l.assets().join(".DS_Store"), b"junk").unwrap();
        l
    }

    fn policy(stages: &[Stage]) -> Vec<TaskPref> {
        Stage::ALL
            .iter()
            .map(|s| TaskPref {
                stage: *s,
                enabled: stages.contains(s),
            })
            .collect()
    }

    fn paths(l: &crate::Layout, stages: &[Stage]) -> Vec<String> {
        let mut s = Sources::plan(l, stages).unwrap();
        s.members.sort_by(|a, b| a.to.cmp(&b.to));
        s.members.into_iter().map(|m| m.to).collect()
    }

    #[test]
    fn a_digest_box_gets_the_prompts_and_registries_and_no_clip() {
        let l = fixture("digest");
        let got = paths(&l, &[Stage::Digest]);
        assert!(got.contains(&"prompts/analyze.txt".to_string()), "{got:?}");
        for reg in [
            "assets/scene-map.json",
            "assets/effect-pool.json",
            "assets/inject-pool.json",
            "assets/tag-aliases.json",
        ] {
            assert!(got.contains(&reg.to_string()), "{reg} missing: {got:?}");
        }
        // The digest never opens the music pool, and no stage reads a clip.
        assert!(!got.contains(&"assets/music-pool.json".to_string()), "{got:?}");
        assert!(
            !got.iter().any(|p| p.ends_with(".mp3")),
            "a digest box was sent audio: {got:?}"
        );
        assert!(!got.iter().any(|p| p.starts_with("crawl/")), "{got:?}");
    }

    #[test]
    fn a_merge_box_gets_the_pools_and_only_registered_clips() {
        let l = fixture("merge");
        let got = paths(&l, &[Stage::Merge]);
        for reg in [
            "assets/scene-map.json",
            "assets/effect-pool.json",
            "assets/music-pool.json",
            "assets/inject-pool.json",
        ] {
            assert!(got.contains(&reg.to_string()), "{reg} missing: {got:?}");
        }
        assert!(got.contains(&"assets/music/market-bg-1.mp3".to_string()), "{got:?}");
        // A file nothing registers never travels: the registry is the pool.
        assert!(
            !got.iter().any(|p| p.contains("leftover")),
            "an unregistered clip was shipped: {got:?}"
        );
        // Nothing on the merge path opens the aliases.
        assert!(!got.contains(&"assets/tag-aliases.json".to_string()), "{got:?}");
    }

    #[test]
    fn a_crawl_box_gets_the_crawlers_and_nothing_else() {
        let l = fixture("crawl");
        let got = paths(&l, &[Stage::Crawl]);
        assert!(got.contains(&"crawl/site.lua".to_string()), "{got:?}");
        assert!(
            got.iter().any(|p| p.starts_with("assets/crawl/templates/")),
            "{got:?}"
        );
        assert!(
            !got.contains(&"assets/scene-map.json".to_string()),
            "a crawler does not mix: {got:?}"
        );
        assert!(
            got.contains(&"prompts/analyze.txt".to_string()),
            "the prompts are not a per-stage file: {got:?}"
        );
    }

    /// Render reads none of these trees — its voice store travels in `models/`
    /// and its cast, script and bible ride in the offer — so a render-only box
    /// is handed the cast, the clone manifest and the prompts.
    #[test]
    fn a_render_box_gets_its_cast_and_no_media() {
        let l = fixture("render");
        let got = paths(&l, &[Stage::Render]);
        assert_eq!(
            got,
            vec![
                "data/cast.json".to_string(),
                "prompts/analyze.txt".to_string(),
                "prompts/script.txt".to_string(),
                "voices.json".to_string(),
            ]
        );
        assert!(
            !got.iter().any(|p| p.starts_with("assets/")),
            "no media, no registries: {got:?}"
        );
    }

    /// **The prompt is not a stage's file.** A box can gain `digest` with one
    /// keypress, and a stage that dies on a missing template fails on every
    /// retry until somebody re-provisions — so the 21 KB rides every bundle,
    /// whatever the policy says. This is the assertion that keeps a future
    /// narrowing from taking it back out.
    #[test]
    fn every_bundle_carries_the_prompts_whatever_the_policy() {
        let l = fixture("prompts-always");
        for stages in [
            vec![Stage::Crawl],
            vec![Stage::Digest],
            vec![Stage::Render],
            vec![Stage::Merge],
            vec![Stage::Render, Stage::Merge],
            Stage::ALL.to_vec(),
        ] {
            let got = paths(&l, &stages);
            for prompt in ["prompts/analyze.txt", "prompts/script.txt"] {
                assert!(
                    got.contains(&prompt.to_string()),
                    "{prompt} missing for {stages:?}: {got:?}"
                );
            }
        }
        // …but a policy that covers no stage is refused outright, because the
        // delivery this plan describes is a prune first: an empty set would
        // delete a box's trees and hand it nothing back.
        let err = Sources::plan(&l, &[]).unwrap_err().to_string();
        assert!(err.contains("covers no stage"), "{err}");
    }

    /// The whole point of hashing a set rather than a tree: a policy change has
    /// to be drift, or a box keeps a stage it has no files for.
    #[test]
    fn widening_the_policy_changes_the_digest_and_reordering_does_not() {
        let l = fixture("policy");
        let digest = Sources::plan(&l, &[Stage::Digest]).unwrap();
        let both = Sources::plan(&l, &[Stage::Digest, Stage::Merge]).unwrap();
        let dh = Sources::hash(&digest.manifest().unwrap());
        let bh = Sources::hash(&both.manifest().unwrap());
        assert_ne!(dh, bh, "adding merge must drift the set");

        // The policy list's order is the scheduler's preference, not content.
        let reordered = stages_of(&policy(&[Stage::Merge, Stage::Digest]));
        assert_eq!(
            reordered,
            vec![Stage::Digest, Stage::Merge],
            "canonical order, whatever the policy's own order was"
        );
        let plain = stages_of(&policy(&[Stage::Digest, Stage::Merge]));
        assert_eq!(reordered, plain);
    }

    #[test]
    fn the_manifest_is_stable_and_moves_with_content() {
        let l = fixture("manifest");
        let a = Sources::plan(&l, &[Stage::Merge]).unwrap().manifest().unwrap();
        let b = Sources::plan(&l, &[Stage::Merge]).unwrap().manifest().unwrap();
        assert_eq!(Sources::hash(&a), Sources::hash(&b), "same set, same digest");
        assert!(
            !a.files.keys().any(|k| k.contains(".DS_Store")),
            "OS noise entered the manifest: {:?}",
            a.files.keys()
        );

        std::fs::write(l.assets().join("music/market-bg-1.mp3"), b"different").unwrap();
        let c = Sources::plan(&l, &[Stage::Merge]).unwrap().manifest().unwrap();
        assert_ne!(a.files["assets/music/market-bg-1.mp3"], c.files["assets/music/market-bg-1.mp3"]);
        assert_ne!(Sources::hash(&a), Sources::hash(&c));
    }

    /// A registry naming a clip that is not here is reported, not shipped and
    /// not fatal: the merge already degrades that one sound to silence.
    #[test]
    fn a_registry_naming_an_absent_clip_is_reported_rather_than_shipped() {
        let l = fixture("missing-clip");
        // Self-contained registries: the fixture's own name clips this checkout
        // does not ship, and the point here is the one line that matters.
        std::fs::write(l.assets().join("effect-pool.json"), "{}").unwrap();
        std::fs::write(l.assets().join("inject-pool.json"), "{}").unwrap();
        std::fs::write(
            l.assets().join("music-pool.json"),
            r#"{"market":{"tags":["market"],"files":["music/market-bg-1.mp3","music/ghost-bg-1.mp3"]}}"#,
        )
        .unwrap();
        let s = Sources::plan(&l, &[Stage::Merge]).unwrap();
        assert_eq!(s.missing, vec!["market: music/ghost-bg-1.mp3".to_string()]);
        assert!(!s.members.iter().any(|m| m.to.contains("ghost")));
        // …and the take that *is* here goes, under the path the merge resolves.
        assert!(s.members.iter().any(|m| m.to == "assets/music/market-bg-1.mp3"));
    }

    /// A base outside `assets/` is a registry line that would have escaped the
    /// tree the merge resolves against, so it is refused rather than followed.
    #[test]
    fn a_registry_clip_outside_assets_is_refused() {
        let l = fixture("escape");
        std::fs::write(l.assets().join("effect-pool.json"), "{}").unwrap();
        std::fs::write(l.assets().join("inject-pool.json"), "{}").unwrap();
        std::fs::write(
            l.assets().join("music-pool.json"),
            r#"{"market":{"tags":["market"],"files":["../../secrets.mp3"]}}"#,
        )
        .unwrap();
        let s = Sources::plan(&l, &[Stage::Merge]).unwrap();
        assert_eq!(s.missing.len(), 1);
        assert!(s.missing[0].contains("not under assets"), "{:?}", s.missing);
    }

    #[test]
    fn the_extract_script_prunes_the_trees_it_owns_and_removes_refs() {
        let s = extract_script();
        assert!(s.contains(r#"rm -rf "$D/prompts" "$D/assets" "$D/crawl""#), "{s}");
        assert!(s.contains(r#""$D/refs""#), "the inductor's own material must leave: {s}");
        assert!(s.contains("zstd -dc"), "{s}");
        assert!(s.contains("SOURCES-OK"), "{s}");
    }

    #[test]
    fn a_bundle_round_trips_through_tar_and_zstd() {
        // Needs the real tools; the whole push path does, and a silent skip
        // would let a broken packer reach a box.
        let l = fixture("roundtrip");
        let s = Sources::plan(&l, &[Stage::Merge, Stage::Digest]).unwrap();
        let manifest = s.manifest().unwrap();
        let dir = std::env::temp_dir().join("bm-sources-roundtrip-out");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(BUNDLE_NAME);
        s.pack(&manifest, &out).unwrap();
        assert!(out.is_file(), "no bundle was written");
        assert!(std::fs::metadata(&out).unwrap().len() > 0);

        // Unpack it the way a worker does and check the paths landed.
        let dst = dir.join("worker");
        std::fs::create_dir_all(&dst).unwrap();
        let sh = format!(
            "set -e; zstd -dc {bundle} | tar -xf - -C {dst}",
            bundle = shq(&out.display().to_string()),
            dst = shq(&dst.display().to_string())
        );
        let status = std::process::Command::new("sh").arg("-c").arg(&sh).status().unwrap();
        assert!(status.success(), "extract failed: {sh}");
        for rel in [
            "assets/music/market-bg-1.mp3",
            "assets/effect-pool.json",
            "prompts/analyze.txt",
            "voices.json",
            "data/cast.json",
            MANIFEST_NAME,
        ] {
            assert!(dst.join(rel).is_file(), "{rel} did not land");
        }
        // The manifest was carried too, and describes what landed.
        let back: SourcesManifest =
            serde_json::from_str(&std::fs::read_to_string(dst.join(MANIFEST_NAME)).unwrap())
                .unwrap();
        assert_eq!(back.files, manifest.files);
        assert_eq!(back.stages, vec!["digest", "merge"]);
        assert!(!dst.join("assets/music/leftover-bg-9.mp3").exists());
    }

    /// The archive holds the plan and nothing beside it.
    ///
    /// "Nothing beside it" is the load-bearing half, and it is not what a `tar
    /// -t` on this machine reports: macOS `tar` *hides* AppleDouble `._name`
    /// members (it reads them as the metadata of the file they name), so the
    /// first push of this bundle looked clean here and unpacked as 115 extra
    /// files on the Linux box, one per member. Hence a header walk rather than
    /// a listing: what a Linux box sees is the question.
    #[test]
    fn the_bundle_holds_exactly_the_plan_and_no_appledouble_junk() {
        let l = fixture("exact");
        // The attribute is what makes libarchive write the sidecar, so the
        // regression only reproduces with one present. macOS-only, like the
        // mechanism: a Linux `tar` has no AppleDouble to write.
        #[cfg(target_os = "macos")]
        {
            let marked = l.assets().join("music/market-bg-1.mp3");
            let ok = std::process::Command::new("xattr")
                .args(["-w", "com.apple.provenance", "x"])
                .arg(&marked)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "could not set the attribute this test needs");
        }

        let s = Sources::plan(&l, &[Stage::Merge, Stage::Digest]).unwrap();
        let manifest = s.manifest().unwrap();
        let dir = std::env::temp_dir().join("bm-sources-exact-out");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join(BUNDLE_NAME);
        s.pack(&manifest, &out).unwrap();

        let raw = std::process::Command::new("zstd")
            .arg("-dc")
            .arg(&out)
            .output()
            .expect("zstd -dc");
        assert!(raw.status.success(), "zstd refused the bundle");
        let members = tar_members(&raw.stdout);
        for m in &members {
            assert!(
                m == MANIFEST_NAME || manifest.files.contains_key(m),
                "{m} is in the archive and not in the plan"
            );
            assert!(!m.starts_with("._"), "an AppleDouble sidecar traveled: {m}");
        }
        assert_eq!(
            members.len(),
            manifest.files.len() + 1,
            "members: {members:?}"
        );
    }

    /// The member names of a `tar` stream, exactly as a Linux box would see
    /// them: 512-byte headers, the name at offset 0, the size in octal at 124,
    /// the body padded to a whole block, two empty blocks to end it. PAX header
    /// entries are metadata, not members.
    fn tar_members(bytes: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 512 <= bytes.len() {
            let block = &bytes[at..at + 512];
            if block.iter().all(|b| *b == 0) {
                break;
            }
            let name = block[..100].split(|b| *b == 0).next().unwrap_or_default();
            let size = std::str::from_utf8(&block[124..136])
                .ok()
                .map(|s| s.trim_matches(['\0', ' ']))
                .and_then(|s| usize::from_str_radix(s, 8).ok())
                .unwrap_or(0);
            if block[156] != b'x' && block[156] != b'g' && !name.is_empty() {
                out.push(String::from_utf8_lossy(name).to_string());
            }
            at += 512 + size.div_ceil(512) * 512;
        }
        out
    }

    fn shq(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
