//! The files a worker needs — selected by what its policy lets it run, and

use anyhow::{Context, Result};
use bm_proto::{Stage, TaskPref};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The artifact's name, at the worker root.
pub const BUNDLE_NAME: &str = "sources.tar.zst";

/// The manifest inside the artifact, at its top level (so it lands at the
pub const MANIFEST_NAME: &str = "sources-manifest.json";

/// The trees the bundle owns. Extraction prunes exactly these, which is what
pub const OWNED: [&str; 4] = ["prompts", "assets", "crawl", "adapters"];

/// One filesystem read of the tree the bundle owns on a *worker*: everything
pub fn is_owned(rel: &str) -> bool {
    OWNED
        .iter()
        .any(|o| rel == *o || rel.starts_with(&format!("{o}/")))
}

/// The registry files each stage reads, relative to `assets/`.
fn registries(stage: Stage) -> &'static [&'static str] {
    match stage {
        // The prompt is rendered from the scene map, the effect pool and the
        Stage::Digest => &[
            "scene-map.json",
            "effect-pool.json",
            "inject-pool.json",
            "tag-aliases.json",
        ],
        // `assemble` reads the scene map and the inject pool; `ambience` reads
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
pub fn stages_of(policy: &[TaskPref]) -> Vec<Stage> {
    Stage::ALL
        .into_iter()
        .filter(|s| policy.iter().any(|p| p.stage == *s && p.enabled))
        .collect()
}

/// One **slot**: a stage, and the adapter whose files make it runnable.
pub fn slot(stage: Stage, adapter: &str) -> String {
    format!("{}@{}", stage.as_str(), adapter)
}

/// Whether `slots` covers `stage` for `adapter`.
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
    pub to: String,
    /// The directory `to` is relative to locally, and the `-C` the packer uses.
    pub base: PathBuf,
    pub from: PathBuf,
    pub bytes: u64,
}

/// The whole set: what is being sent, and what a registry named that is not on
#[derive(Debug, Clone, Default)]
pub struct Sources {
    /// The stages this set was built for — part of the manifest, so a policy
    pub stages: Vec<Stage>,
    /// The adapters (languages) whose trees this set carries.
    pub adapters: Vec<String>,
    /// Sorted by `to`, so the archive's member order is a property of the set.
    pub members: Vec<Member>,
    /// Clips a registry names that are not on disk here. Reported, not shipped:
    pub missing: Vec<String>,
}

impl Sources {
    /// Select the set for `stages`.
    pub fn plan(layout: &crate::Layout, stages: &[Stage]) -> Result<Self> {
        Self::plan_for(layout, stages, None)
    }

    /// The same, for a box whose profile arrives from a release.
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
        let assets = layout.assets();
        let assets_base = assets.parent().unwrap_or(root).to_path_buf();

        // The clone manifest: 5 KB, and the inductor's warnings are computed
        out.push_file(root, "voices.json");

        // The prompts, on the same reasoning and with more force: a digest
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
                    out.push_tree(&layout.root, "crawlers");
                    // …and the workspace's own crawlers, which
                    out.push_tree(&layout.work, "crawl");
                }
                // The registries the digest prompt renders from are still its
                Stage::Digest => {}
                Stage::Render | Stage::Merge => {
                    // The cast files decide the *filenames* a render writes and a
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
        if out.stages.contains(&Stage::Merge) {
            out.push_clips(layout);
        }

        // Attribution for the media, whenever any of it travels.
        if out.stages.iter().any(|s| *s != Stage::Render) {
            out.push_file(&assets_base, "assets/LICENSES.json");
        }

        // Sorted *and* deduped: the stages overlap on purpose (scene-map and
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
        let base = assets.parent().unwrap_or(&layout.root).to_path_buf();
        for kind in crate::audio_pool::PoolKind::ALL {
            let pool = crate::audio_pool::load_pool(&layout.pool(kind));
            for (sound, entry) in pool {
                for file in entry.files {
                    // The registry's paths are relative to `assets/`, the same
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
                    self.push_member(&base, format!("assets/{file}"), from);
                }
            }
        }
        self.missing.sort();
        self.missing.dedup();
    }

    /// One file, addressed as `rel` under `base` — and landed at `rel` too.
    fn push_file(&mut self, base: &Path, rel: &str) -> bool {
        let from = base.join(rel);
        if !from.is_file() {
            return false;
        }
        self.push_member(base, rel.to_string(), from);
        true
    }

    /// A whole directory, `.DS_Store` skipped.
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
    pub fn pack(&self, manifest: &SourcesManifest, out: &Path) -> Result<()> {
        let staging = out
            .parent()
            .context("bundle path has no parent directory")?
            .to_path_buf();
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating {}", staging.display()))?;

        // The manifest is a member of the archive and lands beside what it
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourcesManifest {
    /// The `(stage, adapter)` slots the set covers — see [`slot`].
    #[serde(default)]
    pub slots: Vec<String>,
    /// Worker-relative path -> sha256 of the bytes.
    pub files: BTreeMap<String, String>,
}

/// The shell the worker runs to take delivery.
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
