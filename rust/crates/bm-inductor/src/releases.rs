//! Pulling a composition's dependencies out of their releases.
//!
//! [`bm_core::pack_update`] is the decision — what the closure is, what moved,
//! and whether anything may be replaced — and it knows nothing about hosts.
//! This is the half that answers its two questions, and the answer has three
//! parts: a GitHub release list, the tag convention `<name>-pack-v<version>`,
//! and `bm_core::artifact`'s verified unpack.
//!
//! **The tag is the version**, which is why the lookup is a name match inside
//! the release list rather than a rolling `latest` tag. `profile.sh pack <name>
//! --dep --version V` cuts `<name>-pack-vV`, and a box resolves the same string
//! out of the load pointer — so an update and a provision are two routes to one
//! release rather than two names to keep in step. A `-latest` tag would answer
//! faster and be worth less: a version that *changed* is the only thing an
//! update is permitted to act on, and `gh release create` refusing an existing
//! tag is what makes that comparison mean "the bytes did not change".
//!
//! **It stages inside `_extends/`, not beside it.** The tree the fold will read
//! has to be on the same filesystem as the directory it replaces, because the
//! update renames rather than copies, and `assets/_extends/` is the one tree the
//! profile hash deliberately skips — so a staging directory there cannot drift
//! the pack. A run that dies leaves `.update.<pid>/` behind, which the next run
//! wipes and no fold can see.

use anyhow::{bail, Context, Result};
use bm_core::compose;
use bm_core::pack_update::{Available, Releases};
use bm_core::{config::Settings, Layout};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The release plane, asked one question at a time.
///
/// The listing is fetched once for the whole run: `latest` is called for every
/// pack in the closure and the page is the same page, so a three-dependency
/// update costs one API call rather than three — and an unauthenticated caller
/// has 60 an hour to spend.
pub struct GitHub {
    repo: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
    /// Where `_extends/` is. Kept rather than the staging path, because the
    /// staging path is not made until something is actually fetched.
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
                // follows: a 60 MB bundle off a slow link is minutes, and a total
                // timeout that fires mid-transfer is a worse answer than waiting.
                .connect_timeout(std::time::Duration::from_secs(20))
                .build()
                .context("building the HTTP client")?,
            extends: compose::extends_dir(&layout.assets()),
            stage: None,
            page: None,
        })
    }

    /// The staging directory, made on the first fetch and never before.
    ///
    /// Lazily, because `--dry-run` promises to write nothing and a directory
    /// inside the live tree *is* a write — a dry run that left one behind (or
    /// created `_extends/` on a checkout that had none) would be reporting on a
    /// tree it had already changed.
    fn stage(&mut self) -> Result<&Path> {
        if self.stage.is_none() {
            let dir = self.extends.join(format!(".update.{}", std::process::id()));
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            self.stage = Some(dir);
        }
        Ok(self.stage.as_ref().expect("just made it"))
    }

    /// `api.github.com/…/releases`, unauthenticated unless `GH_TOKEN` is set.
    ///
    /// A failure here is fatal and says so, unlike a box's provision: a box that
    /// cannot reach a release falls back to the push, but an update *is* the
    /// fetch, and "the list would not load so nothing moved" is a sentence the
    /// operator needs rather than a silent no-op that reads as "up to date".
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
    ///
    /// Newest by `created_at`, which is what the release list can actually
    /// order: a pack version is an operator's string (`0.10.0` and `0.9.0` sort
    /// the wrong way lexically), so the only honest ordering is when it was cut.
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
    /// unpacked tree for the update to rename into place.
    ///
    /// The bytes and the file count are printed here because this is the only
    /// place they exist: verify-then-swap throws the staging tree away, so a
    /// caller that wanted to report the transfer would have to do it twice.
    fn fetch(&mut self, release: &Available) -> Result<PathBuf> {
        let dest = self.stage()?.join(&release.name);
        let _ = std::fs::remove_dir_all(&dest);
        let fetched =
            bm_core::artifact::fetch_pack_unpinned(&release.url, &dest, &release.tag, |_, _| {});
        let (landing, hash) = match fetched {
            Ok(pair) => pair,
            // *Corrupt* is the one failure with a known remedy, so it gets it:
            // the four published `-pack-v0.1.0` releases are all full of `._name`
            // sidecars (audited 2026-09-29: common 80, xianxia 129, weapons 35,
            // magic 18), and "a member its manifest never listed" reads as a
            // mystery until the command is named. *Unreachable* is a different
            // conversation (no such release, no route) and gets the plain
            // message.
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
        // invisible to the fold, but it is still 60 MB of somebody's disk.
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
///
/// The report is the point as much as the install is. Doing this by hand is what
/// left a checkout running a parent nobody remembered to re-fetch, and the two
/// facts that catch it — which releases are behind, and which trees were edited
/// here — are printed whether or not anything moves.
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
    // refuse is refused here too rather than turned into an API path.
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
