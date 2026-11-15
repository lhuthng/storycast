//! Push finished chapters to an author's book API.
//!
//! The server owns the truth: its manifest answers a sha256 per chapter and a
//! PUT carries the same hash, so "up to date" is a hash compare and re-running
//! an unchanged book writes nothing.

use crate::config::UploadSettings;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// The server's answer to the manifest read: the chapters it already holds.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub chapters: Vec<RemoteChapter>,
}

/// One chapter the server holds.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteChapter {
    pub number: u32,
    #[serde(default)]
    pub sha256: String,
}

/// A finished chapter on this machine: `output/Ch.N - Title.mp3`.
#[derive(Debug, Clone)]
pub struct Local {
    pub number: u32,
    pub path: PathBuf,
    pub title: String,
    /// The cue sidecar's own duration, when one was written beside it.
    pub duration_s: Option<u64>,
    pub sha256: String,
    pub size: u64,
}

/// What the push will do with one chapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The server has no chapter with this number.
    Create,
    /// The server holds different bytes.
    Replace,
    /// Same sha256: nothing is written.
    Unchanged,
}

impl Action {
    /// The verb a dry run prints for this chapter.
    pub fn label(self) -> &'static str {
        match self {
            Action::Create => "new",
            Action::Replace => "replace",
            Action::Unchanged => "unchanged",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Planned {
    pub local: Local,
    pub action: Action,
}

impl Planned {
    /// One chapter, one word: `ch17 replace`.
    pub fn describe(&self) -> String {
        format!("ch{} {}", self.local.number, self.action.label())
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    pub created: usize,
    pub replaced: usize,
    pub unchanged: usize,
    /// Bytes actually sent, which is what the push cost.
    pub bytes: u64,
}

impl Report {
    /// The counts a plan implies, without sending it.
    pub fn of(planned: &[Planned]) -> Report {
        let mut report = Report::default();
        for planned in planned {
            match planned.action {
                Action::Create => report.created += 1,
                Action::Replace => report.replaced += 1,
                Action::Unchanged => report.unchanged += 1,
            }
        }
        report
    }

    pub fn summary(&self) -> String {
        if self.created + self.replaced == 0 {
            return format!(
                "upload: nothing to send — all {} chapter(s) already match",
                self.unchanged
            );
        }
        let mb = self.bytes as f64 / (1024.0 * 1024.0);
        format!(
            "upload: {} created, {} replaced, {} unchanged ({mb:.1} MB sent)",
            self.created, self.replaced, self.unchanged
        )
    }
}

/// The sha256 both sides name a chapter by, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `Ch.17 - Title.mp3` -> `(17, "Title")`. Every other file is not a book.
pub fn parse_output_name(name: &str) -> Option<(u32, &str)> {
    let rest = name.strip_prefix("Ch.")?;
    let (number, rest) = rest.split_once(" - ")?;
    let title = rest.strip_suffix(".mp3")?;
    Some((number.parse().ok()?, title))
}

/// Every finished chapter in the output folder, hashed, in chapter order.
/// A chapter left under an older title keeps its newest file.
pub fn chapters(layout: &crate::Layout) -> Result<Vec<Local>> {
    let Ok(entries) = std::fs::read_dir(layout.output()) else {
        return Ok(Vec::new());
    };
    let mut newest: BTreeMap<u32, (u64, PathBuf, String)> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some((number, title)) = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(parse_output_name)
            .map(|(n, t)| (n, t.to_string()))
        else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if newest.get(&number).is_none_or(|(seen, ..)| mtime >= *seen) {
            newest.insert(number, (mtime, path, title));
        }
    }
    let mut out = Vec::with_capacity(newest.len());
    for (number, (_, path, title)) in newest {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        out.push(Local {
            number,
            duration_s: duration_s(&crate::assemble::cues_path(&path)),
            sha256: sha256_hex(&bytes),
            size: bytes.len() as u64,
            title,
            path,
        });
    }
    Ok(out)
}

/// The duration the mix was written on, which is the cue sidecar's clock.
fn duration_s(cues: &Path) -> Option<u64> {
    #[derive(Deserialize)]
    struct Sidecar {
        duration_s: f64,
    }
    let text = std::fs::read_to_string(cues).ok()?;
    let side: Sidecar = serde_json::from_str(&text).ok()?;
    (side.duration_s > 0.0).then(|| side.duration_s.round() as u64)
}

/// What each local chapter needs, from the server's own hashes.
pub fn plan(local: &[Local], manifest: &Manifest) -> Vec<Planned> {
    let remote: BTreeMap<u32, &RemoteChapter> =
        manifest.chapters.iter().map(|c| (c.number, c)).collect();
    local
        .iter()
        .map(|l| {
            let action = match remote.get(&l.number) {
                None => Action::Create,
                Some(r) if r.sha256.trim().eq_ignore_ascii_case(&l.sha256) => Action::Unchanged,
                Some(_) => Action::Replace,
            };
            Planned {
                local: l.clone(),
                action,
            }
        })
        .collect()
}

/// The manifest URL with no trailing slash; empty is not a target.
fn base_url(settings: &UploadSettings) -> Result<&str> {
    let base = settings.url.trim().trim_end_matches('/');
    if base.is_empty() {
        bail!("no upload target — set it with `:uploadurl <manifest url>`");
    }
    Ok(base)
}

fn token(settings: &UploadSettings) -> Result<String> {
    settings.token().with_context(|| {
        format!(
            "no upload token — set it with `:uploadkey`, or export {}",
            settings.token_env_name()
        )
    })
}

fn local_chapters(layout: &crate::Layout) -> Result<Vec<Local>> {
    let local = chapters(layout)?;
    if local.is_empty() {
        bail!(
            "no finished chapters in {} — nothing to upload",
            layout.output().display()
        );
    }
    Ok(local)
}

/// The diff a push would write, without sending anything: the manifest read
/// and the hash compare, and not one PUT.
pub async fn dry(
    settings: &UploadSettings,
    http: &reqwest::Client,
    layout: &crate::Layout,
) -> Result<Vec<Planned>> {
    let base = base_url(settings)?;
    let token = token(settings)?;
    let local = local_chapters(layout)?;
    let manifest = fetch_manifest(http, base, &token).await?;
    Ok(plan(&local, &manifest))
}

/// Read the manifest, diff by hash, and PUT only what differs.
pub async fn run(
    settings: &UploadSettings,
    http: &reqwest::Client,
    layout: &crate::Layout,
) -> Result<Report> {
    let base = base_url(settings)?;
    let token = token(settings)?;
    let local = local_chapters(layout)?;
    let manifest = fetch_manifest(http, base, &token).await?;
    let mut report = Report::default();
    for planned in plan(&local, &manifest) {
        if planned.action == Action::Unchanged {
            report.unchanged += 1;
            continue;
        }
        let bytes = std::fs::read(&planned.local.path)
            .with_context(|| format!("reading {}", planned.local.path.display()))?;
        report.bytes += bytes.len() as u64;
        put(http, base, &token, &planned.local, bytes).await?;
        match planned.action {
            Action::Create => report.created += 1,
            _ => report.replaced += 1,
        }
    }
    Ok(report)
}

async fn fetch_manifest(http: &reqwest::Client, base: &str, token: &str) -> Result<Manifest> {
    let resp = http
        .get(base)
        .bearer_auth(token)
        .send()
        .await
        .context("reading the manifest")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("manifest answered {status}: {body}");
    }
    resp.json::<Manifest>()
        .await
        .context("parsing the manifest — expected {\"chapters\":[{\"number\",\"sha256\"}]}")
}

/// One idempotent write. 201 created, 200 the slot now holds these bytes (we
/// only send a differing hash, so this is a replace), 422 the hash disagreed.
async fn put(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    chapter: &Local,
    bytes: Vec<u8>,
) -> Result<()> {
    let mut req = http
        .put(format!("{base}/chapters/{}", chapter.number))
        .bearer_auth(token)
        .header("Content-Type", "audio/mpeg")
        .header("X-Content-SHA256", &chapter.sha256)
        .query(&[("title", chapter.title.as_str())]);
    if let Some(d) = chapter.duration_s {
        req = req.query(&[("duration_seconds", d.to_string())]);
    }
    let resp = req
        .body(bytes)
        .send()
        .await
        .with_context(|| format!("sending chapter {}", chapter.number))?;
    let status = resp.status();
    match status.as_u16() {
        200 | 201 => Ok(()),
        422 => {
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "chapter {} refused (422): the server's hash check disagreed — {body}",
                chapter.number
            )
        }
        other => {
            let body = resp.text().await.unwrap_or_default();
            bail!("chapter {} answered {other}: {body}", chapter.number)
        }
    }
}
