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
//! | crawl | the crawler: workspace `crawl/` + the language's own `crawl/` |
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
//! crawlers and the clips are still selected, and `slots` in the manifest says
//! which `(stage, adapter)` pairs a box has.
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
//! manifest, including the slot list it was built for, is the stamp's
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
pub const OWNED: [&str; 4] = ["prompts", "assets", "crawl", "adapters"];

/// One filesystem read of the tree the bundle owns on a *worker*: everything
/// under these names is the bundle's to replace, and anything else on a box is
/// the box's own state.
pub fn is_owned(rel: &str) -> bool {
    OWNED
        .iter()
        .any(|o| rel == *o || rel.starts_with(&format!("{o}/")))
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

/// One **slot**: a stage, and the adapter whose files make it runnable.
///
/// **The unit of the gate.** A box may hold several adapters — every adapter on
/// the inductor ships to every box — so "digest" is not a fact about a box;
/// "digest for vi-VN" is. The policy says which stages the operator wants, the
/// manifest says which (stage, adapter) pairs the box was actually handed the
/// files for, and a chapter is offered only where the two meet.
///
/// The wire spelling is `stage@adapter` (`digest@vi-VN`). A bare stage is the
/// pre-slot spelling and reads as *that stage, whatever adapter* — which is
/// exactly what it meant when a box held one language and could not say which.
pub fn slot(stage: Stage, adapter: &str) -> String {
    format!("{}@{}", stage.as_str(), adapter)
}

/// Whether `slots` covers `stage` for `adapter`.
///
/// A bare stage name — a manifest or a beat from before the second dimension
/// existed — covers it for every adapter, so an old box is offered work exactly
/// as before rather than starved by a fact its reporter never had.
pub fn holds(slots: &[String], stage: Stage, adapter: &str) -> bool {
    slots.iter().any(|s| match s.split_once('@') {
        Some((named_stage, named_adapter)) => {
            named_stage == stage.as_str() && named_adapter == adapter
        }
        None => s == stage.as_str(),
    })
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
    /// The adapters (languages) whose trees this set carries.
    ///
    /// **Every adapter home on the inductor**, which is the decision rather
    /// than an accident: one bundle is handed to every box, so no box needs an
    /// adapter set of its own, and no field or screen has to describe the
    /// subset. One entry even when the checkout has no `adapters/` tree at all
    /// — the pre-split layout, whose language is the flat `prompts/` and whose
    /// name is the one in force.
    pub adapters: Vec<String>,
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
        Self::plan_for(layout, stages, None)
    }

    /// The same, for a box whose profile arrives from a release.
    ///
    /// `pack` is not a hint that some `assets/` files are already elsewhere —
    /// it *replaces the whole subtree*, and that is the point. A pack release is
    /// `assets/` minus `assets/_extends/`, which is a **superset** of every
    /// `assets/`-rooted member this plan would otherwise select: the registries
    /// a stage opens, the clips they register, the attribution, the language's
    /// bundled crawlers. So when the box is fetching it, keeping those members
    /// would push the same bytes twice over the operator's uplink and then
    /// write them again on top — the one outcome the two halves are supposed to
    /// make impossible.
    ///
    /// The reduction is not optional-by-flag, it is a property of the plan, and
    /// that is what keeps the stamp honest: `compute_provision_stamp` and the
    /// push both call *this* function with the same `pack`, so the digest names
    /// the artifact that was actually sent rather than one that could have been.
    pub fn plan_for(
        layout: &crate::Layout,
        stages: &[Stage],
        pack: Option<&crate::artifact::PackRelease>,
    ) -> Result<Self> {
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
            adapters: Vec::new(),
            members: Vec::new(),
            missing: Vec::new(),
        };
        let root = &layout.root;
        // The `assets/` tree **in force** — the workspace's own composition when
        // it has one, the checkout's when it does not — and the directory its
        // paths are relative to. Both matter: a member must come from the tree
        // the run reads *and* land at `assets/…` on the worker, and addressing
        // the workspace's files relative to the checkout root breaks that
        // pairing (`a member's source must be its landing path under its base`).
        let assets = layout.assets();
        let assets_base = assets.parent().unwrap_or(root).to_path_buf();

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
        //
        // The adapter trees: **every one this checkout carries**, not only the
        // one in force. A whole **home** each — its prompts *and* its crawlers —
        // in one member at the relative path its own resolver reads
        // (`adapters/<name>/`, which is what a box's binding names).
        //
        // All of them for two reasons. The narrow one is the one the single
        // tree in force was already chosen for: shipping the *root* tree
        // unconditionally would hand a box one language's prompts while the
        // inductor driving it reads another's, agreeing on every file name and
        // disagreeing on every word. The wider one is that a box need not be
        // re-provisioned to run a language it did not — 21 KB of text against a
        // 59 MB artifact, and it retires the per-machine adapter set (and the
        // field, and the screen) that a subset would need.
        //
        // Shipping a flat `prompts/` *beside* the homes would be worse than
        // redundant: the resolver prefers the home, so the flat copy would be a
        // stale tree the box quietly ignored and a prompt edit would stop
        // reaching it. Hence one or the other, never both.
        let homes = layout.adapter_homes();
        if homes.is_empty() {
            out.push_tree(&layout.prompts_base(), "prompts");
            out.adapters.push(layout.adapter.clone());
        } else {
            for (name, scope) in &homes {
                out.push_tree(scope, &format!("{}/{name}", crate::paths::ADAPTERS_DIR));
                out.adapters.push(name.clone());
            }
        }

        for stage in out.stages.clone() {
            for reg in registries(stage) {
                out.push_file(&assets_base, &format!("assets/{reg}"));
            }
            match stage {
                Stage::Crawl => {
                    // The **global** crawler tree: `crawlers/known/…` (the
                    // registry's sites) and `crawlers/examples/…`. A named path
                    // (`crawlers/known/storya.lua`) resolves out of it on any
                    // box, so it travels even though the offer also carries the
                    // script's source — a box that reads `crawl.script` against
                    // its own filesystem must find the same file.
                    out.push_tree(&layout.root, "crawlers");
                    // …and the workspace's own crawlers, which
                    // `resolve_script` searches first: a book whose site needs
                    // its own script keeps it out of the shared tree.
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
            out.push_file(&assets_base, "assets/LICENSES.json");
        }

        // Sorted *and* deduped: the stages overlap on purpose (scene-map and
        // the effect pool are read by both digest and merge), and a member named
        // twice would be staged twice — the packer's symlink step refuses the
        // second one, which is how this surfaced.
        out.members.sort_by(|a, b| a.to.cmp(&b.to));
        out.members.dedup_by(|a, b| a.to == b.to);
        if pack.is_some() {
            out.members
                .retain(|m| !m.to.starts_with(&format!("{}/", crate::artifact::PACK_DIR)));
        }
        Ok(out)
    }

    /// Every file the three clip registries name, resolved under `assets/`.
    fn push_clips(&mut self, layout: &crate::Layout) {
        let assets = layout.assets();
        // The clip's base is the `assets/` parent, so a workspace's own tree is
        // addressed relative to the workspace and not to the checkout root.
        let base = assets.parent().unwrap_or(&layout.root).to_path_buf();
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
                    self.push_member(&base, format!("assets/{file}"), from);
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

    /// The slots this set covers: every stage it was built for, for every
    /// adapter it carries. Canonical on both axes, so the list is a property of
    /// the set rather than of the order the policy or the filesystem answered
    /// in.
    pub fn slots(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.stages.len() * self.adapters.len());
        for stage in &self.stages {
            for adapter in &self.adapters {
                out.push(slot(*stage, adapter));
            }
        }
        out
    }

    /// `path -> sha256`, plus the slots this set was built for.
    ///
    /// The manifest *is* the path system: every member is keyed by where it
    /// lands, so the artifact carries its own map from the archive to the
    /// worker's tree.
    pub fn manifest(&self) -> Result<SourcesManifest> {
        let mut files = BTreeMap::new();
        for m in &self.members {
            let bytes =
                std::fs::read(&m.from).with_context(|| format!("reading {}", m.from.display()))?;
            files.insert(m.to.clone(), hex(Sha256::digest(&bytes)));
        }
        Ok(SourcesManifest {
            slots: self.slots(),
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
        for slot in &manifest.slots {
            h.update(slot.as_bytes());
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
            .filter(|m| {
                m.to.starts_with("assets/effects/")
                    || m.to.starts_with("assets/music/")
                    || m.to.starts_with("assets/injects/")
            })
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
                    std::fs::create_dir_all(parent).with_context(|| format!("staging {}", m.to))?;
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

/// The manifest inside the artifact: the slots it was built for, and one
/// sha256 per path.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourcesManifest {
    /// The `(stage, adapter)` slots the set covers — see [`slot`].
    ///
    /// In the manifest *on purpose*: widening a box's policy has to be visible
    /// to the drift check, or the box quietly keeps running a stage whose files
    /// it was never given. The same argument added the second dimension, because
    /// the box's bundle now carries several languages and a stage alone no
    /// longer says which of them the box can run.
    ///
    /// `#[serde(default)]` so a manifest extracted before slots existed (an
    /// `install_sources` from an older inductor) still parses: an empty list is
    /// "no opinion", which is the safe read, and the next provisioning replaces
    /// the file.
    #[serde(default)]
    pub slots: Vec<String>,
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
/// `keep_assets` spares `$D/assets`: with a pack release configured the bundle
/// carries no `assets/` members, so pruning it deletes a tree the archive
/// cannot restore — and the pack step that follows owns that tree, by fetch,
/// by push, or by delta. Pruning only what the archive owns is what keeps the
/// two deliveries from deleting each other's work.
///
/// `unzstd` is not assumed present by name: `zstd -dc` is the same binary the
/// box needs anyway, and a box without it is told so in one line rather than
/// failing with a shell error naming nothing.
pub fn extract_script() -> String {
    extract_script_with(false)
}

/// [`extract_script`], sparing `$D/assets` for the pack step that follows.
pub fn extract_script_keep_assets() -> String {
    extract_script_with(true)
}

fn extract_script_with(keep_assets: bool) -> String {
    let owned: Vec<String> = OWNED
        .iter()
        .filter(|o| !keep_assets || **o != crate::artifact::PACK_DIR)
        .map(|o| format!("\"$D/{o}\""))
        .collect();
    format!(
        r#"set -e
D="$HOME/{d}"
command -v zstd >/dev/null || {{ echo "zstd-missing on this box"; exit 6; }}
rm -rf {owned} "$D/refs" "$D/data"/cast*.json
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
mod tests;
