//! Pulling a composition's dependencies out of their releases.

use anyhow::{bail, Context, Result};
use bm_core::compose;
use bm_core::pack_update::{Available, Releases};
use bm_core::{config::Settings, Layout};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The release plane, asked one question at a time.
pub struct GitHub {
    repo: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
    /// Where `_extends/` is. Kept rather than the staging path, because the
    extends: PathBuf,
    stage: Option<PathBuf>,
    page: Option<Vec<Value>>,
}

impl GitHub {
    pub fn new(repo: &str, layout: &Layout) -> Result<Self> {
        Ok(GitHub {
            repo: repo.to_string(),
            token: std::env::var("GH_TOKEN")
                .ok()
                .filter(|t| !t.trim().is_empty()),
            client: reqwest::blocking::Client::builder()
                // Only the connect is bounded, the same rule `artifact::download`
                .connect_timeout(std::time::Duration::from_secs(20))
                .build()
                .context("building the HTTP client")?,
            extends: compose::extends_dir(&layout.assets()),
            stage: None,
            page: None,
        })
    }

    /// The staging directory, made on the first fetch and never before.
    fn stage(&mut self) -> Result<&Path> {
        if self.stage.is_none() {
            let dir = self.extends.join(format!(".update.{}", std::process::id()));
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            self.stage = Some(dir);
        }
        Ok(self.stage.as_ref().expect("just made it"))
    }

    /// `api.github.com/…/releases`, unauthenticated unless `GH_TOKEN` is set.
    fn fetch_page(&self) -> Result<Vec<Value>> {
        let url = format!(
            "https://api.github.com/repos/{}/releases?per_page=100",
            self.repo
        );
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "bm-inductor-profile-update");
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.send().with_context(|| format!("asking {url}"))?;
        if !resp.status().is_success() {
            bail!(
                "{} answered {} — a private repo needs GH_TOKEN set, and a full \
                 release list is what this reads",
                url,
                resp.status()
            );
        }
        resp.json::<Vec<Value>>()
            .with_context(|| format!("parsing {url}"))
    }

    /// The newest release of `name` this repo holds.
    fn newest(&mut self, name: &str) -> Result<Option<Available>> {
        if self.page.is_none() {
            let page = self.fetch_page()?;
            self.page = Some(page);
        }
        let prefix = format!("{name}-pack-v");
        let page = self.page.as_ref().expect("just filled in");
        let mut best: Option<&Value> = None;
        for rel in page {
            let matches = rel
                .get("tag_name")
                .and_then(Value::as_str)
                .and_then(|tag| tag.strip_prefix(&prefix))
                .is_some_and(|version| !version.is_empty());
            if !matches {
                continue;
            }
            let when = rel
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let newer = match best {
                None => true,
                Some(have) => {
                    have.get("created_at")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        <= when
                }
            };
            if newer {
                best = Some(rel);
            }
        }
        let Some(rel) = best else {
            return Ok(None);
        };
        let tag = rel
            .get("tag_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let version = tag.strip_prefix(&prefix).unwrap_or_default().to_string();
        let url = asset_url(rel).ok_or_else(|| {
            anyhow::anyhow!(
                "release {tag} carries no .tar.zst asset — a `--dep` pack release is \
                 `<name>.tar.zst` (`tools/profile.sh pack {name} --dep --version {version}`)"
            )
        })?;
        Ok(Some(Available {
            name: name.to_string(),
            version,
            tag,
            url,
        }))
    }
}

impl Releases for GitHub {
    fn latest(&mut self, name: &str) -> Result<Option<Available>> {
        self.newest(name)
    }

    /// Download, verify against the bundle's own manifest, and hand back its
    fn fetch(&mut self, release: &Available) -> Result<PathBuf> {
        let dest = self.stage()?.join(&release.name);
        let _ = std::fs::remove_dir_all(&dest);
        let fetched =
            bm_core::artifact::fetch_pack_unpinned(&release.url, &dest, &release.tag, |_, _| {});
        let (landing, hash) = match fetched {
            Ok(pair) => pair,
            // *Corrupt* is the one failure with a known remedy, so it gets it:
            Err(bm_core::artifact::FetchError::Corrupt(e)) => bail!(
                "{tag} does not verify and was not unpacked: {e}\n  \
                 nothing was replaced — the tree is as it was. Re-publish it under the \
                 same tag (`tools/profile.sh pack {name} --dep --version {version}` then \
                 `gh release upload {tag} profiles/pack/{name}.tar.zst --clobber`) — a \
                 bundle packed by macOS `tar` before COPYFILE_DISABLE=1 carries `._name` \
                 members its own manifest never lists",
                tag = release.tag,
                name = release.name,
                version = release.version,
            ),
            Err(e) => bail!(
                "{}: {e} — nothing was replaced; the tree is as it was",
                release.tag
            ),
        };
        println!(
            "  pulled {} ({} files, {} MiB, {})",
            release.tag,
            landing.files,
            landing.bytes / (1024 * 1024),
            &hash[..12.min(hash.len())]
        );
        Ok(dest)
    }
}

impl Drop for GitHub {
    fn drop(&mut self) {
        // Whatever is still in the stage was not installed. A dot-directory is
        if let Some(stage) = &self.stage {
            let _ = std::fs::remove_dir_all(stage);
        }
    }
}

/// The `.tar.zst` asset's download URL, or `None` when the release has none.
fn asset_url(release: &Value) -> Option<String> {
    release.get("assets")?.as_array()?.iter().find_map(|a| {
        let name = a.get("name")?.as_str()?;
        if !name.ends_with(".tar.zst") {
            return None;
        }
        Some(a.get("browser_download_url")?.as_str()?.to_string())
    })
}

/// `bm-inductor profile update`: pull the newest release of every dependency.
pub fn cmd_update(
    layout: &Layout,
    settings: &Settings,
    repo: Option<&str>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let repo = repo
        .map(str::to_string)
        .unwrap_or_else(|| settings.packs_release.clone());
    let repo = repo.trim().to_string();
    if repo.is_empty() {
        bail!(
            "an update needs to know where the dependencies live — set `packs_release` in \
             settings.json (or the TUI's `:packrelease`), or pass --repo owner/name"
        );
    }
    // Validated by the release side's own rule, so a setting that a box would
    let (owner, name) = bm_core::artifact::parse_repo(&repo)?;
    let repo = format!("{owner}/{name}");

    let mut source = GitHub::new(&repo, layout)?;
    let update = bm_core::pack_update::update(&layout.assets(), &mut source, dry_run, force)?;

    println!("{repo} — {}", update.summary());
    for step in &update.steps {
        println!("  {}", step.line());
    }
    if let Some(fold) = &update.fold {
        println!("assets/ — {}", fold.summary());
    }
    if dry_run {
        println!("  (nothing was written — `profile update` to pull)");
        return Ok(());
    }
    if update.replaced.is_empty() {
        return Ok(());
    }
    println!("  replaced: {}", update.replaced.join(", "));
    if let Ok(pointer) = bm_core::profile::in_force(layout).map(|b| b.pack) {
        println!(
            "  the composition changed — `tools/profile.sh pack {} --version V` to publish it \
             under a new version",
            pointer.name
        );
    }
    Ok(())
}
