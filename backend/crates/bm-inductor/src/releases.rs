//! Pulling a composition's dependencies out of their releases.

use anyhow::{bail, Context, Result};
use bm_core::compose;
use bm_core::pack_update::{Available, Fetched, Releases};
use bm_core::{config::Settings, Layout};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The release plane, asked one question at a time.
pub struct GitHub {
    repo: String,
    /// Which piece's tags to look at: `<name>-<piece>-v<version>`.
    piece: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
    /// Where `_extends/` is. Kept rather than the staging path, because the
    extends: PathBuf,
    stage: Option<PathBuf>,
    page: Option<Vec<Value>>,
}

impl GitHub {
    pub fn new(repo: &str, layout: &Layout) -> Result<Self> {
        Self::with_piece(repo, layout, "pack")
    }

    pub fn with_piece(repo: &str, layout: &Layout, piece: &str) -> Result<Self> {
        Ok(GitHub {
            repo: repo.to_string(),
            piece: piece.to_string(),
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
        let prefix = format!("{name}-{}-v", self.piece);
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
            manifest_hash: None,
        }))
    }
}

impl Releases for GitHub {
    fn latest(&mut self, name: &str) -> Result<Option<Available>> {
        self.newest(name)
    }

    /// Download, verify against the bundle's own manifest, and hand back its
    fn fetch(&mut self, release: &Available) -> Result<Fetched> {
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
        let migrations = match &landing.migrations {
            Some(path) => Some(bm_core::pack_migration::read_file(path)?),
            None => None,
        };
        Ok(Fetched {
            tree: dest,
            migrations,
        })
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

/// The preset bucket, as `profile update` sees it: one `latest.json` per pack,
/// pinning a version and the manifest hash the unpacked tree must have.
pub struct R2 {
    base: String,
    extends: PathBuf,
    stage: Option<PathBuf>,
}

impl R2 {
    pub fn new(base: &str, layout: &Layout) -> Result<Self> {
        Ok(R2 {
            base: base.trim_end_matches('/').to_string(),
            extends: compose::extends_dir(&layout.assets()),
            stage: None,
        })
    }

    fn stage(&mut self) -> Result<PathBuf> {
        if self.stage.is_none() {
            let dir = self
                .extends
                .join(format!(".update.{}", std::process::id()));
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            self.stage = Some(dir);
        }
        Ok(self.stage.as_ref().expect("just made it").clone())
    }
}

impl Releases for R2 {
    fn latest(&mut self, name: &str) -> Result<Option<Available>> {
        // A `--dep` release is `<name>.tar.zst`, the name `profile.sh pack`
        // gives it; the bucket keeps the plain basename.
        let file = format!("{name}.tar.zst");
        let Some(pin) = bm_core::artifact::preset_latest_opt(&self.base, "sounds", name, &file)?
        else {
            return Ok(None);
        };
        Ok(Some(Available {
            name: name.to_string(),
            version: pin.version.clone(),
            tag: format!("{name}-pack-v{}", pin.version),
            url: pin.url,
            manifest_hash: Some(pin.manifest_hash),
        }))
    }

    fn fetch(&mut self, release: &Available) -> Result<Fetched> {
        let dest = self.stage()?.join(&release.name);
        let _ = std::fs::remove_dir_all(&dest);
        let landing = match &release.manifest_hash {
            Some(hash) => {
                bm_core::artifact::fetch_pack(&release.url, &dest, hash, &release.tag, |_, _| {})
            }
            None => bm_core::artifact::fetch_pack_unpinned(
                &release.url,
                &dest,
                &release.tag,
                |_, _| {},
            )
            .map(|(landing, _)| landing),
        };
        let landing = landing.map_err(|e| {
            anyhow::anyhow!(
                "{}: {e} — nothing was replaced; the tree is as it was",
                release.tag
            )
        })?;
        println!(
            "  pulled {} ({} files, {} MiB)",
            release.tag,
            landing.files,
            landing.bytes / (1024 * 1024)
        );
        let migrations = match &landing.migrations {
            Some(path) => Some(bm_core::pack_migration::read_file(path)?),
            None => None,
        };
        Ok(Fetched {
            tree: dest,
            migrations,
        })
    }
}

impl Drop for R2 {
    fn drop(&mut self) {
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

/// The bucket's word for a piece: a `pack` is published under `sounds` and an
/// `adapter` under `profiles` (docs/ARCHITECTURE.md §Packs), because a bucket holds
/// every kind and the piece names only two of them.
fn preset_kind(piece: &str) -> Result<&'static str> {
    match piece {
        "pack" => Ok("sounds"),
        "adapter" => Ok("profiles"),
        other => bail!("unknown piece `{other}` (pack|adapter)"),
    }
}

/// `bm-inductor profile fetch`: put one piece's bundle where `verify` and
/// `unpack` look for it — `profiles/<piece>/<name>.tar.zst` — from the newest
/// release or from a version named outright.
///
/// The bucket is the plane this reads when the setting is a URL, and GitHub
/// when it is `owner/name`: both are still in the field, and which one a
/// checkout talks to is the setting's shape, not a flag.
pub fn cmd_fetch(
    layout: &Layout,
    settings: &Settings,
    name: &str,
    piece: &str,
    version: Option<&str>,
    repo: Option<&str>,
) -> Result<()> {
    let repo = repo
        .map(str::to_string)
        .unwrap_or_else(|| settings.packs_release.clone());
    let repo = repo.trim().to_string();
    if repo.is_empty() {
        bail!(
            "a fetch needs to know where the releases live — set `packs_release` in \
             settings.json (or the TUI's `:packrelease`), or pass --repo owner/name"
        );
    }
    let dir = layout.root.join("profiles").join(piece);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(format!("{name}.tar.zst"));
    // Beside the destination and dot-named: a cut transfer leaves this behind
    // and never a half-written bundle under the name `verify` reads.
    let part = dir.join(format!(".{name}.tar.zst.part"));
    let _ = std::fs::remove_file(&part);

    // `expect` is the archive hash the plane pins, when it pins one. GitHub
    // publishes none, and a bucket publishes it only in latest.json.
    let (url, expect, label) = if bm_core::artifact::is_preset_url(&repo) {
        let kind = preset_kind(piece)?;
        let base = repo.trim_end_matches('/');
        let file = format!("{name}.tar.zst");
        match version {
            Some(want) => {
                let v = want.trim_start_matches('@').trim_start_matches('v');
                // latest.json describes exactly one version, so it pins this
                // pin only when the pin *is* it. Otherwise the bundle's own
                // manifest is the only check there is — worth saying out
                // loud, because that is a weaker promise than the floating
                // path's, and versions are immutable for a reason.
                let pin = bm_core::artifact::preset_latest_opt(&repo, kind, name, &file)?;
                let expect = pin.filter(|p| p.version == v).map(|p| p.hash);
                if expect.is_none() {
                    println!(
                        "  {name}: {v} is not the version latest.json describes — the \
                         bundle's own manifest is the only check on it"
                    );
                }
                (
                    format!("{base}/preset/{kind}/{name}/{v}/{file}"),
                    expect,
                    format!("{name}-{piece}-v{v}"),
                )
            }
            None => {
                let pin = bm_core::artifact::preset_latest(&repo, kind, name, &file)?;
                println!("  {name}: newest is {}", pin.version);
                let label = format!("{name}-{piece}-v{}", pin.version);
                (pin.url, Some(pin.hash), label)
            }
        }
    } else {
        let (owner, repo_name) = bm_core::artifact::parse_repo(&repo)?;
        match version {
            // A named tag is a URL, not a question: asking the API for the
            // newest release and then filtering would find an older pin only
            // when it happened to be the newest.
            Some(want) => {
                let v = want.trim_start_matches('@').trim_start_matches('v');
                let tag = format!("{name}-{piece}-v{v}");
                let url = format!(
                    "https://github.com/{owner}/{repo_name}/releases/download/{tag}/{name}.tar.zst"
                );
                (url, None, tag)
            }
            None => {
                let mut gh = GitHub::with_piece(&format!("{owner}/{repo_name}"), layout, piece)?;
                let rel = gh.newest(name)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "{owner}/{repo_name} holds no {name}-{piece}-v* release — a `--dep` \
                         pack release is `<name>.tar.zst` (`tools/profile.sh pack {name} --dep`)"
                    )
                })?;
                println!("  {name}: newest is {}", rel.version);
                (rel.url, None, rel.tag)
            }
        }
    };

    let bytes =
        bm_core::artifact::download(&url, &part, &mut |_, _| {}).map_err(|e| anyhow::anyhow!("{e}"))?;
    let got = bm_core::artifact::sha256_of(&part)?;
    if let Some(expect) = &expect {
        if &got != expect {
            let _ = std::fs::remove_file(&part);
            bail!(
                "{url}: sha256 {got}, but latest.json pins {expect} — the download is not \
                 the file that was published; nothing was replaced"
            );
        }
    }
    std::fs::rename(&part, &dest).with_context(|| format!("moving into {}", dest.display()))?;
    // An adapter is tens of KiB, so a MiB-only number would print `0`.
    let size = if bytes >= 1024 * 1024 {
        format!("{} MiB", bytes / (1024 * 1024))
    } else {
        format!("{} KiB", bytes / 1024)
    };
    println!(
        "  {label} ({size}, {}{})",
        &got[..12.min(got.len())],
        if expect.is_some() {
            ", hash checked"
        } else {
            ""
        }
    );
    Ok(())
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
    // reject fails here rather than at the first dependency.
    let mut source: Box<dyn Releases> = if bm_core::artifact::is_preset_url(&repo) {
        Box::new(R2::new(&repo, layout)?)
    } else {
        let (owner, name) = bm_core::artifact::parse_repo(&repo)?;
        Box::new(GitHub::new(&format!("{owner}/{name}"), layout)?)
    };
    let update =
        bm_core::pack_update::update(&layout.assets(), source.as_mut(), dry_run, force)?;

    println!("{repo} — {}", update.summary());
    for step in &update.steps {
        println!("  {}", step.line());
    }
    if let Some(fold) = &update.fold {
        println!("assets/ — {}", fold.summary());
    }
    // The update moved the tree, so it owes the checkout a pointer that matches
    // it. Said out loud because it is the difference between a checkout that
    // runs when this command returns and one whose first run has to adopt what
    // the update did and call it drift.
    if let Some(pointer) = &update.stamped {
        println!(
            "  binding: {} re-stamped {}",
            pointer.name,
            &pointer.hash[..12.min(pointer.hash.len())]
        );
    }
    if dry_run {
        println!("  (nothing was written — `profile update` to pull)");
        return Ok(());
    }
    if update.replaced.is_empty() {
        return Ok(());
    }
    println!("  replaced: {}", update.replaced.join(", "));
    if !update.migrations.is_empty() {
        let report = bm_core::pack_migration::apply(layout, &update.migrations)?;
        println!("  scripts: {}", report.summary());
        for line in report.lines() {
            println!("    {line}");
        }
    }
    if let Ok(pointer) = bm_core::profile::in_force(layout).map(|b| b.pack) {
        println!(
            "  the composition changed — `tools/profile.sh pack {} --version V` to publish it \
             under a new version",
            pointer.name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::preset_kind;

    #[test]
    fn a_piece_maps_to_the_kind_it_is_published_under() {
        assert_eq!(preset_kind("pack").unwrap(), "sounds");
        assert_eq!(preset_kind("adapter").unwrap(), "profiles");
        // The piece names two of the bucket's four kinds. Anything else is a
        // typo that would build a URL under a prefix nobody writes, and the
        // 404 would read as "this pack was never published".
        assert!(preset_kind("voice").is_err());
        assert!(preset_kind("").is_err());
    }
}
