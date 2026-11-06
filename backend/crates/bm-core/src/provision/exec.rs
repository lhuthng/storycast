//! InstaBox's transport: `exec` and the files API instead of ssh and rsync.
//!
//! A machine there has no address of its own, so a provision run reaches it by
//! id over the provider's HTTP API: commands by `POST /exec`, bytes by
//! `PUT|GET /files`.

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::REMOTE_DIR;
use crate::cloud::instabox::{self, HOME, MAX_EXEC_SECS};

/// One machine's exec and file access, as the API exposes it.
#[derive(Debug, Clone)]
pub struct Exec {
    api: String,
    key: String,
    machine: String,
    home: String,
}

impl Exec {
    pub fn new(api: &str, key: &str, machine: &str) -> Self {
        Exec {
            api: api.trim_end_matches('/').to_string(),
            key: key.trim().to_string(),
            machine: machine.trim().to_string(),
            home: HOME.to_string(),
        }
    }

    /// The worker's root on that machine: `$HOME/bm-worker`, the same layout
    /// every other transport uses.
    fn root(&self) -> String {
        format!("{}/{REMOTE_DIR}", self.home)
    }

    /// An absolute path under the worker root, refusing to climb out of it.
    fn abs(&self, remote_rel: &str) -> Result<String> {
        if remote_rel.starts_with('/') || remote_rel.split('/').any(|p| p == "..") {
            anyhow::bail!("refusing {remote_rel:?}: worker paths stay under the root");
        }
        Ok(if remote_rel.is_empty() {
            self.root()
        } else {
            format!("{}/{remote_rel}", self.root())
        })
    }

    fn client(&self, secs: u64) -> Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(secs.max(1)))
            .build()
            .context("building the InstaBox HTTP client")
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
        secs: u64,
    ) -> Result<(u16, Vec<u8>)> {
        let url = format!("{}{path}", self.api);
        let mut req = self
            .client(secs)?
            .request(method, &url)
            .header("X-API-Key", &self.key)
            .header("accept", "application/json");
        if let Some(body) = body {
            req = req.header("content-type", "application/json").body(body);
        }
        let sent = req
            .send()
            .with_context(|| format!("{} answered nothing", self.api))?;
        let status = sent.status().as_u16();
        let bytes = sent.bytes().context("reading the answer")?.to_vec();
        Ok((status, bytes))
    }

    fn json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let body = body.map(|b| serde_json::to_vec(&b)).transpose()?;
        let (status, bytes) = self.request(method, path, body, 60)?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        if !(200..300).contains(&status) {
            let s = instabox::shortfall(status, &text, None, None, 1);
            anyhow::bail!("{}", s.line());
        }
        serde_json::from_str(&text)
            .with_context(|| format!("unparsable answer: {}", crate::util::head_chars(&text, 200)))
    }

    /// Run a shell script as the machine's user: `(exit_code, stdout, stderr)`.
    ///
    /// The API kills a call after [`MAX_EXEC_SECS`], so anything longer is
    /// started detached and its files polled — the same shape a worker's own
    /// long step needs.
    pub fn exec(&self, script: &str, timeout_secs: u64) -> Result<(i32, String, String)> {
        let script = format!("export PATH=$HOME/.local/bin:$HOME/.cargo/bin:$PATH\n{script}");
        if timeout_secs <= MAX_EXEC_SECS {
            return self.exec_here(&script, timeout_secs);
        }
        self.exec_detached(&script, timeout_secs)
    }

    fn exec_here(&self, script: &str, timeout_secs: u64) -> Result<(i32, String, String)> {
        let body = serde_json::json!({
            "cmd": script,
            "timeoutSeconds": timeout_secs.clamp(1, MAX_EXEC_SECS),
        });
        let answer = self.json(
            reqwest::Method::POST,
            &format!("/v1/machines/{}/exec", self.machine),
            Some(body),
        )?;
        let text = |k: &str| {
            answer
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let code = answer
            .get("exitCode")
            .and_then(|c| c.as_i64())
            .unwrap_or(255) as i32;
        let mut stderr = text("stderr");
        if answer.get("timedOut").and_then(|t| t.as_bool()) == Some(true) {
            stderr.push_str(&format!(
                "\nthe command was killed after {timeout_secs}s on the box"
            ));
        }
        if answer.get("truncated").and_then(|t| t.as_bool()) == Some(true) {
            stderr.push_str("\nits output was cut at the API's 1 MiB");
        }
        Ok((code, text("stdout"), stderr))
    }

    /// Start it in the background, then poll its own exit status: the API
    /// returns as soon as the launcher exits, whatever the job does next.
    fn exec_detached(&self, script: &str, timeout_secs: u64) -> Result<(i32, String, String)> {
        let id = unique();
        let dir = format!("{}/.bm-exec", self.home);
        let sh = format!("{dir}/{id}.sh");
        let runner = format!("{dir}/{id}.runner");
        let out = format!("{dir}/{id}.out");
        let err = format!("{dir}/{id}.err");
        let status = format!("{dir}/{id}.status");
        self.exec_here(&format!("mkdir -p {dir}"), 30)?;
        self.put(&sh, Some("0700"), script.as_bytes())?;
        let body = format!(
            "bash {} >{} 2>{}; echo $? >{}\n",
            shq(&sh),
            shq(&out),
            shq(&err),
            shq(&status)
        );
        self.put(&runner, Some("0700"), body.as_bytes())?;
        self.exec_here(
            &format!(
                "nohup bash {} </dev/null >/dev/null 2>&1 & echo started",
                shq(&runner)
            ),
            30,
        )?;
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let code = loop {
            if let Some(code) = self
                .get_opt(&status)?
                .and_then(|bytes| String::from_utf8_lossy(&bytes).trim().parse::<i32>().ok())
            {
                break Some(code);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        let stdout = self.get_opt(&out)?.unwrap_or_default();
        let mut stderr =
            String::from_utf8_lossy(&self.get_opt(&err)?.unwrap_or_default()).to_string();
        let code = match code {
            Some(code) => code,
            None => {
                stderr.push_str(&format!(
                    "\nstill running on the box after {timeout_secs}s — not waited for"
                ));
                255
            }
        };
        // The scratch files are the box's, not a worker's: clear them.
        let _ = self.exec_here(
            &format!(
                "rm -f {} {} {} {} {}",
                shq(&sh),
                shq(&runner),
                shq(&out),
                shq(&err),
                shq(&status)
            ),
            30,
        );
        Ok((code, String::from_utf8_lossy(&stdout).to_string(), stderr))
    }

    /// Upload one file, creating its parents.
    pub fn put(&self, abs: &str, mode: Option<&str>, bytes: &[u8]) -> Result<()> {
        let mut path = format!(
            "/v1/machines/{}/files?path={}&parents=true",
            self.machine,
            pct(abs)
        );
        if let Some(mode) = mode {
            path.push_str(&format!("&mode={mode}"));
        }
        let (status, body) =
            self.request(reqwest::Method::PUT, &path, Some(bytes.to_vec()), 1800)?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        let s = instabox::shortfall(status, &String::from_utf8_lossy(&body), None, None, 1);
        anyhow::bail!("uploading {abs}: {}", s.line())
    }

    /// Download one file's bytes.
    pub fn get(&self, abs: &str) -> Result<Vec<u8>> {
        self.get_opt(abs)?
            .ok_or_else(|| anyhow::anyhow!("{abs} is not on the box"))
    }

    fn get_opt(&self, abs: &str) -> Result<Option<Vec<u8>>> {
        let path = format!(
            "/v1/machines/{}/files/content?path={}",
            self.machine,
            pct(abs)
        );
        let (status, body) = self.request(reqwest::Method::GET, &path, None, 1800)?;
        if (200..300).contains(&status) {
            return Ok(Some(body));
        }
        if status == 404 {
            return Ok(None);
        }
        let s = instabox::shortfall(status, &String::from_utf8_lossy(&body), None, None, 1);
        anyhow::bail!("reading {abs}: {}", s.line())
    }

    /// Push a local directory over the worker root, one tarball at a time.
    pub(crate) fn push_dir(
        &self,
        src: &Path,
        remote_rel: &str,
        delete: bool,
        excludes: &[&str],
    ) -> Result<()> {
        let dst = self.abs(remote_rel)?;
        // A single file pushed as itself (`bm-agent`, the sources bundle): no
        // tar, and no `-C` on a path that is not a directory.
        if src.is_file() {
            let bytes = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
            return self.put(&dst, None, &bytes);
        }
        let tar = tar_local(src, excludes)?;
        let bytes = std::fs::read(&tar).with_context(|| format!("reading {}", tar.display()))?;
        let _ = std::fs::remove_file(&tar);
        let remote = format!("{}/.bm-exec/{}.tar", self.home, unique());
        self.exec_here(&format!("mkdir -p {}/.bm-exec", self.home), 30)?;
        self.put(&remote, Some("0600"), &bytes)?;
        let mut script = String::new();
        if delete {
            script.push_str(&format!("rm -rf -- {}\n", shq(&dst)));
        }
        script.push_str(&format!(
            "mkdir -p -- {}\ntar -xf {} -C {}\nrm -f -- {}\n",
            shq(&dst),
            shq(&remote),
            shq(&dst),
            shq(&remote)
        ));
        let (code, _, stderr) = self.exec(&script, 1800)?;
        if code != 0 {
            anyhow::bail!(
                "push to {remote_rel} failed: {}",
                crate::util::head_chars(&stderr, 300)
            );
        }
        Ok(())
    }

    /// Push exactly the named root-relative paths.
    pub(crate) fn push_files(&self, root: &Path, files: &[String]) -> Result<()> {
        for f in files {
            if f.starts_with('/') || f.split('/').any(|p| p == "..") {
                anyhow::bail!("refusing to push {f:?}: manifest paths stay under the root");
            }
        }
        let tar = tar_named(root, files)?;
        let bytes = std::fs::read(&tar).with_context(|| format!("reading {}", tar.display()))?;
        let _ = std::fs::remove_file(&tar);
        let remote = format!("{}/.bm-exec/{}.tar", self.home, unique());
        self.exec_here(&format!("mkdir -p {}/.bm-exec", self.home), 30)?;
        self.put(&remote, Some("0600"), &bytes)?;
        let (code, _, stderr) = self.exec(
            &format!(
                "mkdir -p -- {}\ntar -xf {} -C {}\nrm -f -- {}\n",
                shq(&self.root()),
                shq(&remote),
                shq(&self.root()),
                shq(&remote)
            ),
            1800,
        )?;
        if code != 0 {
            anyhow::bail!(
                "delta push failed: {}",
                crate::util::head_chars(&stderr, 300)
            );
        }
        Ok(())
    }

    /// Pull a worker-root-relative path into a local destination. Files and
    /// directories land the same way: what arrived at `dst` is the node.
    pub(crate) fn pull(&self, remote_rel: &str, dst: &Path) -> Result<()> {
        let src = self.abs(remote_rel)?;
        let (parent, name) = match src.rsplit_once('/') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (src.clone(), String::new()),
        };
        let tar = format!("{}/.bm-exec/{}.tar", self.home, unique());
        self.exec_here(&format!("mkdir -p {}/.bm-exec", self.home), 30)?;
        let packed = format!("tar -cf {} -C {} {}\n", shq(&tar), shq(&parent), shq(&name));
        let (code, _, stderr) = self.exec(&packed, 1800)?;
        if code != 0 {
            anyhow::bail!(
                "packing {remote_rel} failed: {}",
                crate::util::head_chars(&stderr, 300)
            );
        }
        let bytes = self.get(&tar)?;
        let _ = self.exec_here(&format!("rm -f -- {}", shq(&tar)), 30);

        let id = unique();
        let file = std::env::temp_dir().join(format!("bm-exec-{id}.tar"));
        let work = std::env::temp_dir().join(format!("bm-exec-{id}"));
        std::fs::File::create(&file)
            .and_then(|mut f| f.write_all(&bytes))
            .with_context(|| format!("writing {}", file.display()))?;
        std::fs::create_dir_all(&work)?;
        let out = std::process::Command::new("tar")
            .args([
                "-xf",
                &file.to_string_lossy(),
                "-C",
                &work.to_string_lossy(),
            ])
            .output()
            .context("running tar")?;
        let _ = std::fs::remove_file(&file);
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&work);
            anyhow::bail!(
                "unpacking {remote_rel} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let arrived = work.join(&name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let placed = match std::fs::rename(&arrived, dst) {
            Ok(()) => Ok(()),
            // Another filesystem (a tmpdir on a different mount): copy, then
            // drop the copy.
            Err(_) => move_any(&arrived, dst),
        };
        let _ = std::fs::remove_dir_all(&work);
        placed
    }
}

/// Move a file or a whole directory, across filesystems if need be.
fn move_any(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            move_any(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst)
            .with_context(|| format!("copying {} -> {}", src.display(), dst.display()))?;
        Ok(())
    }
}

/// A tar of one directory's contents, on this machine.
fn tar_local(src: &Path, excludes: &[&str]) -> Result<PathBuf> {
    let out = std::env::temp_dir().join(format!("bm-exec-push-{}.tar", unique()));
    let mut cmd = std::process::Command::new("tar");
    cmd.arg("-cf").arg(&out).arg("-C").arg(src);
    for e in excludes {
        cmd.arg("--exclude").arg(e);
    }
    cmd.arg(".");
    let done = cmd.output().context("running tar")?;
    if !done.status.success() {
        anyhow::bail!(
            "packing {} failed: {}",
            src.display(),
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    Ok(out)
}

/// A tar of exactly the named root-relative paths, rooted at `root`.
fn tar_named(root: &Path, files: &[String]) -> Result<PathBuf> {
    let out = std::env::temp_dir().join(format!("bm-exec-named-{}.tar", unique()));
    let done = std::process::Command::new("tar")
        .arg("-cf")
        .arg(&out)
        .arg("-C")
        .arg(root)
        .args(files)
        .output()
        .context("running tar")?;
    if !done.status.success() {
        anyhow::bail!(
            "packing {} failed: {}",
            root.display(),
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    Ok(out)
}

fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// One shell word, quoted so nothing in it is taken as syntax.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A query value, percent-encoded far enough for a path.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests;
