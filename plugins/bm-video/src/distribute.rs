//! Distributed rendering: one plan owner, many boxes.
//!
//! The plan is the source of truth, and it is *reproducible*: a chunk's frame
//! range plus the same inputs always yields the same content-addressed key. So a
//! box only needs to be told **which slice of the plan** to render — it
//! recomputes the plan itself from the staged copy of the inputs, and its chunks
//! land in the same cache by name. There is no central queue, no lock and
//! nothing to reconcile: the dispatcher only fills the cache, then the ordinary
//! local render assembles.
//!
//! A box is given the plugin's **source** and builds it for itself, the way the
//! pipeline builds its own crates on the box. Nothing is cross-compiled here.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model;
use crate::render::{self, Options};
use crate::source;
use crate::template;

/// One box the plan is handed to.
pub struct Remote {
    pub host: String,
    pub user: Option<String>,
    pub port: u16,
    pub key: Option<PathBuf>,
    /// Absolute working root on the box (`~` is expanded once, up front).
    pub root: String,
    /// Worker processes to run there at once.
    pub slots: usize,
    /// Threads each of those workers may use.
    pub worker_jobs: usize,
    /// Run here as if it were a box, over `sh` instead of ssh — the pipeline's
    /// own convention for a local node, and the way this path is tested.
    pub hold: Option<String>,
    pub release: Option<String>,
}

impl Remote {
    pub fn is_local(&self) -> bool {
        matches!(self.host.as_str(), "local" | "127.0.0.1" | "localhost" | "::1")
    }

    fn target(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    fn ssh_opts(&self) -> Vec<String> {
        let mut v: Vec<String> = [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            // A freshly imaged box presents a host key nobody has seen; refusing
            // it would make the first run the hardest. Mirrors bm-core's Ssh.
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if self.port != 22 {
            v.push("-p".into());
            v.push(self.port.to_string());
        }
        if let Some(k) = &self.key {
            v.push("-i".into());
            v.push(k.display().to_string());
        }
        v
    }

    fn rsync_e(&self) -> String {
        let opts: Vec<String> = self.ssh_opts().into_iter().filter(|o| o != "-p").collect();
        format!("ssh {}", opts.join(" "))
    }

    fn ssh(&self, cmd: &str) -> Result<()> {
        let out = Command::new("ssh")
            .args(self.ssh_opts())
            .arg(self.target())
            .arg(cmd)
            .output()
            .with_context(|| format!("ssh to {}", self.host))?;
        if !out.status.success() {
            bail!(
                "ssh {}: {}\n{}",
                self.host,
                cmd,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// `~` cannot be expanded by anything but the remote shell, so ask once and
    /// use a plain absolute path everywhere after this.
    pub fn resolve_root(&mut self) -> Result<()> {
        if !self.root.starts_with('~') {
            return Ok(());
        }
        if self.is_local() {
            let home = std::env::var("HOME").context("no HOME in this environment")?;
            self.root = self.root.replacen('~', &home, 1);
            return Ok(());
        }
        let out = Command::new("ssh")
            .args(self.ssh_opts())
            .arg(self.target())
            .arg("echo $HOME")
            .output()
            .with_context(|| format!("asking {} for $HOME", self.host))?;
        if !out.status.success() {
            bail!("ssh {} failed: {}", self.host, String::from_utf8_lossy(&out.stderr).trim());
        }
        let home = String::from_utf8_lossy(&out.stdout).trim().to_string();
        self.root = self.root.replacen('~', &home, 1);
        Ok(())
    }

    /// A non-interactive ssh shell does not read `~/.profile`, so a rustup
    /// toolchain is not on its PATH even when it is installed. The pipeline's
    /// own Makefile carries the same note; the fix is the same here.
    const PATH_FIX: &'static str = "export PATH=\"$HOME/.cargo/bin:$PATH\"";

    fn preflight(&self) -> Result<()> {
        self.ssh(&format!(
            "{path}; command -v cargo >/dev/null || {{ echo 'no cargo on this box: install rustup (curl https://sh.rustup.rs -sSf | sh -s -- -y)' >&2; exit 1; }}",
            path = Self::PATH_FIX
        ))?;
        self.ssh(&format!("mkdir -p \"{}\"", self.root))
    }

    fn push(&self, src: &Path, dst: &str, exclude_target: bool, mirror: bool) -> Result<()> {
        let mut c = Command::new("rsync");
        c.args(["-az", "--no-perms", "-e", &self.rsync_e()]);
        if exclude_target {
            c.args(["--exclude", "target"]);
        }
        // The plugin directory is a mirror of this source, not an increment on
        // top of it: a file deleted here would otherwise linger on the box and
        // make its digest differ from this one for no real reason.
        if mirror {
            c.arg("--delete");
        }
        c.arg(format!("{}/", src.display()))
            .arg(format!("{}:{dst}", self.target()));
        let out = c.output().with_context(|| format!("rsync to {}", self.host))?;
        if !out.status.success() {
            bail!(
                "rsync to {} failed: {}\n(a box without rsync: apt install -y rsync)",
                self.host,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn pull(&self, src: &str, dst: &Path) -> Result<()> {
        std::fs::create_dir_all(dst)?;
        let status = Command::new("rsync")
            .args(["-az", "--no-perms", "-e", &self.rsync_e()])
            .arg(format!("{}:{src}", self.target()))
            .arg(format!("{}/", dst.display()))
            .status()
            .with_context(|| format!("rsync pull from {}", self.host))?;
        if !status.success() {
            bail!("rsync pull from {} failed", self.host);
        }
        Ok(())
    }

    /// The command this box would be given, for `--plan-only`.
    fn describe(&self, o: &Options, stage: &Path, from: usize, count: usize) -> Result<String> {
        let outdir = if self.is_local() {
            o.outdir.display().to_string()
        } else {
            format!("{}/out", self.root)
        };
        let plan = self.plan_arg(stage);
        let args = worker_args(o, &outdir, &plan, from, count, self.worker_jobs)?;
        if self.is_local() {
            return Ok(format!("{} {}", std::env::current_exe()?.display(), args.join(" ")));
        }
        let exe = format!("{}/plugin/target/release/bm-video", self.root);
        let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
        Ok(format!("cd \"{}/stage\" && \"{exe}\" {}", self.root, quoted.join(" ")))
    }

    /// Prove the box is on this source, by asking the binary it just built to
    /// hash its own tree. rsync can leave a box a file behind, and a box that
    /// renders different pixels under the same chunk name is silently wrong.
    fn verify_source(&self, want: &str) -> Result<()> {
        let out = if self.is_local() {
            Command::new(std::env::current_exe()?).arg("digest").output()
        } else {
            Command::new("ssh")
                .args(self.ssh_opts())
                .arg(self.target())
                .arg(format!("{}/plugin/target/release/bm-video digest", self.root))
                .output()
        }
        .with_context(|| format!("asking {} for its source digest", self.host))?;
        let got = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if got == want {
            println!("[{}] source {} ✓", self.host, source::short(&got));
            return Ok(());
        }
        bail!(
            "{} reports source {} but this run shipped {want}: the source did not land or did not \
             build there — check rsync on the box, then re-run",
            self.host,
            if got.is_empty() { "(nothing)".to_string() } else { source::short(&got).to_string() }
        )
    }

    /// The plan file where this box will find it: the stage is the worker's
    /// working directory on a remote box, and an absolute path on this one.
    fn plan_arg(&self, stage: &Path) -> String {
        if self.is_local() {
            stage.join("plan.json").display().to_string()
        } else {
            "plan.json".to_string()
        }
    }

    /// Render one slice of the plan into this box's chunk cache.
    fn work(&self, o: &Options, stage: &Path, from: usize, count: usize) -> Result<()> {
        let outdir = if self.is_local() {
            o.outdir.display().to_string()
        } else {
            format!("{}/out", self.root)
        };
        let plan = self.plan_arg(stage);
        let args = worker_args(o, &outdir, &plan, from, count, self.worker_jobs)?;
        let exe = if self.is_local() {
            std::env::current_exe().context("resolving this binary")?
        } else {
            PathBuf::from(format!("{}/plugin/target/release/bm-video", self.root))
        };
        let status = if self.is_local() {
            Command::new(&exe)
                .current_dir(&o.root)
                .args(&args)
                .status()
                .with_context(|| "running the local worker")?
        } else {
            // `cd` so the stage's relative paths (template, tmp assets) resolve,
            // exactly as they do in the plan owner's own directory.
            let quoted: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
            let cmd = format!(
                "cd \"{}/stage\" && \"{}\" {}",
                self.root,
                exe.display(),
                quoted.join(" ")
            );
            Command::new("ssh")
                .args(self.ssh_opts())
                .arg(self.target())
                .arg(cmd)
                .status()
                .with_context(|| format!("ssh worker on {}", self.host))?
        };
        if !status.success() {
            bail!("worker {}..{} on {} exited {status}", from, from + count, self.host);
        }
        Ok(())
    }
}

/// A path must sit under the plugin root: a stage mirrors that root, so an
/// absolute path in the template could not be mirrored anywhere.
fn relative(o: &Options, p: &Path) -> Result<PathBuf> {
    p.strip_prefix(&o.root).map(|r| r.to_path_buf()).map_err(|_| {
        anyhow::anyhow!(
            "{} is outside {} — a distributed stage mirrors the plugin root, so every \
             template path must be relative to it",
            p.display(),
            o.root.display()
        )
    })
}

/// The worker's argv. Relative paths only, so the same strings work on the box.
fn worker_args(
    o: &Options,
    outdir: &str,
    plan: &str,
    from: usize,
    count: usize,
    jobs: usize,
) -> Result<Vec<String>> {
    let mut a: Vec<String> = vec![
        "--template".into(),
        relative(o, &o.template_path)?.display().to_string(),
        "--acts".into(),
        relative(o, &o.acts_path)?.display().to_string(),
        "--workspace".into(),
        relative(o, &o.workspace)?.display().to_string(),
        "--outdir".into(),
        outdir.to_string(),
        "--name".into(),
        o.name.clone(),
        "--chunk-secs".into(),
        o.chunk_secs.to_string(),
        "--chapter-gap".into(),
        o.chapter_gap.to_string(),
        "--preset".into(),
        o.preset.clone(),
        "--crf".into(),
        o.crf.to_string(),
        "--jobs".into(),
        jobs.to_string(),
        "--from".into(),
        from.to_string(),
        "--count".into(),
        count.to_string(),
        "--plan".into(),
        plan.to_string(),
    ];
    if let Some(p) = o.preview {
        a.push("--preview".into());
        a.push(p.to_string());
    }
    if o.no_subs {
        a.push("--no-subs".into());
    }
    Ok(a)
}

/// Build the mini plugin-root a box renders from: the template, the manifest,
/// the assets they name, and the published chapters the manifest lists.
pub fn pack(o: &Options, stage: &Path) -> Result<()> {
    let files = stage_files(o)?;
    if stage.exists() {
        std::fs::remove_dir_all(stage).ok();
    }
    for rel in &files {
        let dst = stage.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(o.root.join(rel), &dst)
            .with_context(|| format!("staging {}", rel.display()))?;
    }
    println!("  stage: {} files under {}", files.len(), stage.display());
    Ok(())
}

/// Everything a worker reads to render a chunk: the template, the manifest, the
/// assets they name, and the published chapters the manifest lists. Relative to
/// the root, so a box can mirror it anywhere.
pub fn stage_files(o: &Options) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = vec![relative(o, &o.template_path)?, relative(o, &o.acts_path)?];
    for rel in o.template.font_files.values() {
        files.push(relative(o, &template::resolve(&o.root, rel))?);
    }
    if let Some(th) = &o.template.timeline.thumb {
        if let Some(rel) = th.single.as_ref().or(th.sheet.as_ref()) {
            files.push(relative(o, &template::resolve(&o.root, rel))?);
        }
    }
    if let Some(cfg) = &o.template.speaker_sticker {
        for rel in cfg.speakers.values() {
            files.push(relative(o, &template::resolve(&o.root, rel))?);
        }
        if let Some(rel) = &cfg.fallback {
            files.push(relative(o, &template::resolve(&o.root, rel))?);
        }
    }
    for rel in o.template.sprite_assets() {
        files.push(relative(o, &template::resolve(&o.root, rel))?);
    }
    for act in &o.manifest.acts {
        for n in &act.chapters {
            let (mp3, cues) = model::find_chapter(&o.workspace, *n)?;
            files.push(relative(o, &mp3)?);
            files.push(relative(o, &cues)?);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Fill every box's chunk cache, then assemble here.
pub fn dispatch(
    o: &Options,
    hosts: &mut [Remote],
    stage: &Path,
    keep_stage: bool,
    plan_only: bool,
) -> Result<()> {
    let tl = render::timeline(o)?;
    let n = tl.n_chunks;
    let remote_boxes = hosts.iter().filter(|h| !h.is_local()).count();
    let total_slots: usize = hosts.iter().map(|h| h.slots.max(1)).sum();
    let digest = source::digest()?;
    println!(
        "distributed: {n} chunks · {} box(es) ({} local) · {total_slots} worker(s) · source {}",
        hosts.len(),
        hosts.len() - remote_boxes,
        source::short(&digest)
    );

    // Contiguous ranges, one per worker slot. Contiguous on purpose: a chunk is
    // 4 s of frames and neighbouring chunks share a caption run, so a worker's
    // tiles stay warm.
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    for (hi, h) in hosts.iter().enumerate() {
        for _ in 0..h.slots.max(1) {
            let left_slots = total_slots - ranges.len();
            if left_slots == 0 {
                break;
            }
            let remaining = n - cursor;
            let take = remaining.div_ceil(left_slots);
            if take == 0 {
                continue;
            }
            ranges.push((hi, cursor, take));
            cursor += take;
        }
    }
    if cursor < n {
        bail!("{n} chunks but only {cursor} were assigned");
    }

    if plan_only {
        println!("plan-only: nothing shipped, nothing rendered");
        println!("  stage would be {}", stage.display());
        for (hi, from, count) in &ranges {
            let h = &hosts[*hi];
            println!(
                "  {} ← chunks {from}..{} ({} chunks)",
                if h.is_local() { "local".to_string() } else { h.target() },
                from + count,
                count
            );
            if let Some(c) = &h.hold {
                println!("      hold:    {c}");
            }
            println!("      {}", h.describe(o, stage, *from, *count)?);
        }
        return Ok(());
    }

    // 1. source + inputs on every remote box, built there.
    if remote_boxes > 0 {
        pack(o, stage)?;
    }
    // The clock travels with the work: each box renders these numbers instead of
    // probing the container, whose reported duration varies by ffmpeg build.
    // Written after `pack`, which rebuilds the stage from scratch.
    std::fs::create_dir_all(stage)?;
    std::fs::write(
        stage.join("plan.json"),
        serde_json::to_vec_pretty(&tl.plan()).context("encoding the plan")?,
    )
    .with_context(|| format!("writing the plan into {}", stage.display()))?;
    for h in hosts.iter_mut() {
        if h.is_local() {
            continue;
        }
        h.resolve_root()?;
        h.preflight()?;
        println!("[{}] shipping source → {}/plugin (built on the box)", h.host, h.root);
        h.push(&source::root(), &format!("{}/plugin/", h.root), true, true)?;
        println!("[{}] shipping inputs → {}/stage", h.host, h.root);
        h.push(stage, &format!("{}/stage/", h.root), false, false)?;
        println!("[{}] cargo build --release (on the box, for the box)", h.host);
        h.ssh(&format!(
            "cd \"{}/plugin\" && {} && cargo build --release",
            h.root,
            Remote::PATH_FIX
        ))?;
        h.verify_source(&digest)?;
    }

    if hosts.iter().any(|h| h.hold.is_some()) {
        for h in hosts.iter() {
            if let Some(cmd) = &h.hold {
                h.ssh(cmd)?;
                println!("[{}] held the box: {cmd}", h.host);
            }
        }
    }

    // 2. work, in parallel across every slot.
    println!("dispatching…");
    let mut failures: Vec<String> = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (hi, from, count) in &ranges {
            let h = &hosts[*hi];
            handles.push(scope.spawn(move || {
                h.work(o, stage, *from, *count)
                    .map_err(|e| format!("{}: {e:#}", h.host))
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failures.push(e),
                Err(_) => failures.push("a worker thread panicked".to_string()),
            }
        }
    });
    if !failures.is_empty() {
        for f in &failures {
            eprintln!("  worker failed: {f}");
        }
        bail!("{} of {} worker(s) failed", failures.len(), ranges.len());
    }

    // 3. the boxes' chunks come home, then the ordinary local render assembles
    //    — it finds every chunk cached and renders none.
    for h in hosts.iter_mut() {
        if h.is_local() {
            continue;
        }
        println!("[{}] pulling chunks home", h.host);
        h.pull(
            &format!("{}/out/{}.parts/chunks/", h.root, o.name),
            &render::chunks_dir(o),
        )?;
    }
    for h in hosts.iter() {
        if let Some(cmd) = &h.release {
            h.ssh(cmd)?;
            println!("[{}] released the box: {cmd}", h.host);
        }
    }

    println!("assembling locally (all chunks cached)");
    render::run(o)?;
    if !keep_stage && stage.exists() {
        let _ = std::fs::remove_dir_all(stage);
    }
    Ok(())
}
