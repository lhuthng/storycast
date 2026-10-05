//! The LAN pool: boxes ask for a slice of the plan and hand chunks back.
//!
//! The controller owns the plan and the cache, and nothing is pushed at a box:
//! no ssh, no rsync, no address the controller has to know. A box dials in,
//! fetches the inputs it lacks, asks for a batch, renders it, and submits the
//! chunks; when it dies the batch's lease runs out and goes back in the pool.
//!
//! One request per connection and one line of JSON each way, so neither end
//! owes the other any state between requests.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::render::{self, Options, Plan};
use crate::source;

/// An unfinished batch goes back in the pool after this. A batch is a few
/// seconds of video, so even a slow box is well inside it.
const LEASE: Duration = Duration::from_secs(180);
const IO_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the server waits on a silent pool before it stops and assembles.
const POLL: Duration = Duration::from_millis(50);

// --------------------------------------------------------------------------- #
// the wire
// --------------------------------------------------------------------------- #

#[derive(Serialize, Deserialize, Debug)]
struct Ask {
    op: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    len: usize,
    #[serde(default)]
    want: usize,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct Ack {
    ok: bool,
    #[serde(default)]
    len: usize,
    #[serde(default)]
    from: usize,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    job: Option<Job>,
}

/// Everything a box needs to render the plan owner's chunks on its own.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Job {
    pub name: String,
    pub template: String,
    pub acts: String,
    pub workspace: String,
    pub chunk_secs: f64,
    pub chapter_gap: f64,
    pub plan: Plan,
    /// Inputs, relative to the mirror root (`stage/` on a box) that is the
    /// worker's working directory.
    pub files: Vec<String>,
    /// The plugin's own source, relative to the crate root, for `--fetch-source`.
    pub source: Vec<String>,
    /// What that source hashes to: the pool's code fingerprint, for a joiner to
    /// check itself against.
    pub source_digest: String,
}

/// A framed connection: a JSON line per message, then a raw body if it has one.
struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    at: usize,
}

impl Conn {
    fn new(s: TcpStream) -> Result<Conn> {
        // A socket accepted from a non-blocking listener inherits the flag on
        // BSD/macOS (not on Linux), which would make every read return
        // EWOULDBLOCK. The accept loop polls; the connections do not.
        s.set_nonblocking(false)?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Conn { s, buf: Vec::new(), at: 0 })
    }

    fn fill(&mut self) -> Result<()> {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        let mut block = [0u8; 64 * 1024];
        let n = self.s.read(&mut block).context("reading from the other end")?;
        if n == 0 {
            bail!("the other end hung up");
        }
        self.buf.extend_from_slice(&block[..n]);
        Ok(())
    }

    fn line(&mut self) -> Result<serde_json::Value> {
        loop {
            if let Some(i) = self.buf[self.at..].iter().position(|b| *b == b'\n') {
                let text = String::from_utf8_lossy(&self.buf[self.at..self.at + i]).into_owned();
                self.at += i + 1;
                return serde_json::from_str(&text)
                    .with_context(|| format!("reading a request {text:?}"));
            }
            self.fill()?;
        }
    }

    /// Exactly `n` bytes, after whatever the buffer already holds.
    fn body(&mut self, n: usize) -> Result<Vec<u8>> {
        while self.buf.len() - self.at < n {
            self.fill()?;
        }
        let out = self.buf[self.at..self.at + n].to_vec();
        self.at += n;
        Ok(out)
    }

    fn send<J: Serialize>(&mut self, v: &J) -> Result<()> {
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        self.s.write_all(&line)?;
        self.s.flush()?;
        Ok(())
    }

    fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.s.write_all(bytes)?;
        self.s.flush()?;
        Ok(())
    }
}

fn connect(server: &str) -> Result<Conn> {
    let s = TcpStream::connect(server).with_context(|| format!("connecting to {server}"))?;
    Conn::new(s)
}

fn ask(server: &str, req: &Ask) -> Result<Ack> {
    let mut c = connect(server)?;
    c.send(req)?;
    let v = c.line()?;
    let ack: Ack = serde_json::from_value(v).context("reading the reply")?;
    if !ack.ok {
        bail!("{server} refused {:?}", req.op);
    }
    Ok(ack)
}

/// The sub-ranges of `from..from+count` nobody has submitted yet.
fn missing_runs(from: usize, count: usize, received: &HashSet<usize>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for i in from..from + count {
        if received.contains(&i) {
            if let Some(s) = start.take() {
                out.push((s, i - s));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, from + count - s));
    }
    out
}

/// Resolve a wire path under `root`, refusing anything that climbs out.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("refusing the path {rel:?}");
    }
    if rel.is_empty() {
        bail!("empty path");
    }
    Ok(root.join(p))
}

// --------------------------------------------------------------------------- #
// the work pool
// --------------------------------------------------------------------------- #

/// Which chunks are left. A batch is contiguous: neighbouring chunks share a
/// caption run, so a worker's tiles stay warm.
#[derive(Default)]
struct Pool {
    n: usize,
    next: usize,
    held: Vec<(usize, usize, Instant)>,
    ready: Vec<(usize, usize)>,
    done: HashSet<String>,
    /// Which chunk indices are home. A name encodes its own frame offset, so a
    /// submission is enough to know what to stop handing out.
    received: HashSet<usize>,
}

impl Pool {
    fn take(&mut self, want: usize) -> Option<(usize, usize)> {
        self.expire();
        while let Some(r) = self.ready.pop() {
            if missing_runs(r.0, r.1, &self.received).is_empty() {
                continue;
            }
            return Some(r);
        }
        if self.next >= self.n {
            return None;
        }
        let count = want.max(1).min(self.n - self.next);
        let r = (self.next, count);
        self.next += count;
        self.held.push((r.0, r.1, Instant::now()));
        Some(r)
    }

    /// A batch nobody finished — the box died or wandered off — goes back, but
    /// only the part of it that is still missing. Without this, a box that had
    /// already submitted would be handed its own finished work again once the
    /// lease ran out.
    fn expire(&mut self) {
        let held = std::mem::take(&mut self.held);
        for (from, count, at) in held {
            if at.elapsed() <= LEASE {
                self.held.push((from, count, at));
            } else {
                self.ready.extend(missing_runs(from, count, &self.received));
            }
        }
    }

    /// Note a chunk as home, from the frame offset its name begins with.
    fn submitted(&mut self, name: &str, chunk_frames: usize) {
        self.done.insert(name.to_string());
        if let Some(f0) = name.split('-').next().and_then(|s| s.parse::<usize>().ok()) {
            if let Some(idx) = f0.checked_div(chunk_frames) {
                self.received.insert(idx);
            }
        }
    }

    fn finished(&self) -> bool {
        self.done.len() >= self.n
    }

    /// Work this pool can still hand out right now.
    fn unassigned(&self) -> bool {
        self.next < self.n || !self.ready.is_empty()
    }
}

// --------------------------------------------------------------------------- #
// the controller
// --------------------------------------------------------------------------- #

/// Serve the plan to the LAN until every chunk is home, then assemble.
pub fn serve(o: &Options, bind: &str, want: usize, idle_secs: u64, assemble: bool) -> Result<()> {
    let tl = render::timeline(o)?;
    let dir = render::chunks_dir(o);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let job = Job {
        name: o.name.clone(),
        template: rel(o, &o.template_path)?,
        acts: rel(o, &o.acts_path)?,
        workspace: rel(o, &o.workspace)?,
        chunk_secs: o.chunk_secs,
        chapter_gap: o.chapter_gap,
        plan: tl.plan(),
        files: rels(o, crate::distribute::stage_files(o)?)?,
        source: source::files()?,
        source_digest: source::digest()?,
    };

    let listener = TcpListener::bind(bind).with_context(|| format!("binding {bind}"))?;
    listener.set_nonblocking(true)?;
    println!(
        "serving {} chunks on {} (batches of {}, lease {}s)",
        tl.n_chunks,
        bind,
        want.max(1),
        LEASE.as_secs()
    );
    println!(
        "  source {} · {} inputs · {} source files · cache {}",
        source::short(&job.source_digest),
        job.files.len(),
        job.source.len(),
        dir.display()
    );
    println!("  a box joins with: bm-video join --server <this host>:{bind}");

    let pool = Arc::new(Mutex::new(Pool { n: tl.n_chunks, ..Default::default() }));
    let stop = Arc::new(AtomicBool::new(false));
    let job = Arc::new(job);
    let root = Arc::new(o.root.clone());
    let dir = Arc::new(dir);

    let chunk_frames = job.plan.chunk_frames;
    let mut last_ask = Instant::now();
    let idle = Duration::from_secs(idle_secs.max(1));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, _)) => {
                last_ask = Instant::now();
                let (p, j, r, d) = (pool.clone(), job.clone(), root.clone(), dir.clone());
                std::thread::spawn(move || {
                    if let Err(e) = handle(s, &p, &j, &r, &d, want.max(1), chunk_frames) {
                        eprintln!("  a connection ended: {e:#}");
                    }
                });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => bail!("accepting on {bind}: {e}"),
        }
        let (done, unassigned, held) = {
            let p = pool.lock().expect("pool");
            (p.finished(), p.unassigned(), p.held.len())
        };
        // With batches out, a box is mid-render and legitimately silent, so give
        // those its lease as well before deciding the pool has been abandoned.
        let patience = if held > 0 { LEASE + idle } else { idle };
        if done {
            println!("  every chunk is home");
            stop.store(true, Ordering::Relaxed);
        } else if !unassigned && last_ask.elapsed() > patience {
            println!(
                "  nothing asked for {}s with {held} batch(es) out; stopping short of a full plan",
                patience.as_secs()
            );
            stop.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(POLL);
    }

    let got = pool.lock().expect("pool").done.len();
    println!("received {got}/{} chunks", tl.n_chunks);
    if assemble {
        println!("assembling (the local pass renders whatever is still missing)");
        render::run(o)?;
    }
    Ok(())
}

fn handle(
    s: TcpStream,
    pool: &Arc<Mutex<Pool>>,
    job: &Arc<Job>,
    root: &Arc<PathBuf>,
    dir: &Arc<PathBuf>,
    want: usize,
    chunk_frames: usize,
) -> Result<()> {
    let mut c = Conn::new(s)?;
    let req: Ask = serde_json::from_value(c.line()?).context("reading the request")?;
    match req.op.as_str() {
        "hello" => c.send(&Ack {
            ok: true,
            job: Some((**job).clone()),
            ..Default::default()
        }),
        "file" => {
            let p = safe_join(root, &req.path)?;
            let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            c.send(&Ack { ok: true, len: bytes.len(), ..Default::default() })?;
            c.send_raw(&bytes)
        }
        "source" => {
            let p = safe_join(&source::root(), &req.path)?;
            let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            c.send(&Ack { ok: true, len: bytes.len(), ..Default::default() })?;
            c.send_raw(&bytes)
        }
        "task" => {
            let taken = pool.lock().expect("pool").take(req.want.max(want));
            let (from, count) = taken.unwrap_or((0, 0));
            c.send(&Ack { ok: true, from, count, ..Default::default() })
        }
        "submit" => {
            let body = c.body(req.len)?;
            if body.len() != req.len {
                bail!("short submit: {} of {} bytes", body.len(), req.len);
            }
            let p = safe_join(dir, &req.name)?;
            std::fs::write(&p, &body).with_context(|| format!("writing {}", p.display()))?;
            pool.lock().expect("pool").submitted(&req.name, chunk_frames);
            c.send(&Ack { ok: true, len: body.len(), ..Default::default() })
        }
        other => bail!("unknown op {other:?}"),
    }
}

/// A path relative to the root, as the wire carries it.
fn rel(o: &Options, p: &Path) -> Result<String> {
    p.strip_prefix(&o.root)
        .map(|r| r.display().to_string())
        .map_err(|_| anyhow::anyhow!("{} is not under the root", p.display()))
}

fn rels(o: &Options, ps: Vec<PathBuf>) -> Result<Vec<String>> {
    let root = &o.root;
    Ok(ps
        .iter()
        .map(|p| {
            root.join(p)
                .strip_prefix(root)
                .map(|r| r.display().to_string())
                .unwrap_or_else(|_| p.display().to_string())
        })
        .collect())
}

// --------------------------------------------------------------------------- #
// a box joining
// --------------------------------------------------------------------------- #

/// Join the pool: fetch what this box lacks, then ask for work until it is done.
pub fn join(
    server: &str,
    dir: &Path,
    want: usize,
    jobs: usize,
    fetch_source: bool,
    allow_stale: bool,
) -> Result<()> {
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;

    let ack = ask(server, &Ask { op: "hello".into(), ..blank() })?;
    let job = ack.job.context("the pool sent no job")?;
    println!(
        "joining {}: {} chunks waiting, {} inputs, source {}",
        server,
        job.plan.total_frames.div_ceil(job.plan.chunk_frames.max(1)),
        job.files.len(),
        source::short(&job.source_digest)
    );

    // The binary that will render, and the code it came from. A renderer that
    // differs from the pool's writes different pixels under an identical chunk
    // name, which arrives as a wrong video rather than as an error, so a
    // mismatch stops here unless this run was told to allow it.
    let mut exe = std::env::current_exe().context("resolving this binary")?;
    let mine = if fetch_source {
        let crate_dir = dir.join("plugin");
        std::fs::create_dir_all(&crate_dir)?;
        for rel in &job.source {
            let dst = crate_dir.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            fetch(server, "source", rel, &dst, true)?;
        }
        println!("  fetched {} source files; building", job.source.len());
        let cargo = cargo_bin();
        let status = std::process::Command::new(&cargo)
            .args(["build", "--release"])
            .current_dir(&crate_dir)
            .status()
            .with_context(|| format!("running {cargo} in {}", crate_dir.display()))?;
        if !status.success() {
            bail!("cargo build failed on this box");
        }
        // Render with what was just built, not with the binary that got here:
        // fetching the source is only an update if the new build is the one used.
        exe = crate_dir
            .join("target")
            .join("release")
            .join(format!("bm-video{}", std::env::consts::EXE_SUFFIX));
        Some(digest_of_exe(&exe)?)
    } else {
        source::digest().ok()
    };
    match mine.as_deref() {
        Some(d) if d == job.source_digest => {
            println!("  source {} ✓ up to date", source::short(d))
        }
        Some(d) if allow_stale => println!(
            "  source {} ≠ the pool's {} (allowed)",
            source::short(d),
            source::short(&job.source_digest)
        ),
        Some(d) => bail!(
            "this box is on source {} but the pool is on {} — re-join with --fetch-source to \
             update it, or pass --allow-stale to render anyway",
            source::short(d),
            source::short(&job.source_digest)
        ),
        None if allow_stale => {
            println!("  cannot verify this box's code against the pool's (allowed)")
        }
        None => bail!(
            "this box cannot report its own source digest — it was built somewhere that is gone; \
             re-join with --fetch-source to update it, or pass --allow-stale to render anyway"
        ),
    }

    for rel in &job.files {
        let dst = stage.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        fetch(server, "file", rel, &dst, false)?;
    }
    std::fs::write(
        stage.join("plan.json"),
        serde_json::to_vec_pretty(&job.plan).context("encoding the plan")?,
    )?;

    let outdir = dir.join("out");
    let chunks = outdir.join(format!("{}.parts", job.name)).join("chunks");
    std::fs::create_dir_all(&chunks)
        .with_context(|| format!("creating {}", chunks.display()))?;

    let mut rendered = 0usize;
    loop {
        // The pool stops listening the moment the last chunk lands, so a box
        // asking for more work at that point just finds the door closed.
        let ack = match ask(server, &Ask { op: "task".into(), want, ..blank() }) {
            Ok(a) => a,
            Err(e) => {
                println!("  the pool is no longer taking work ({e:#}); stopping");
                break;
            }
        };
        if ack.count == 0 {
            println!("  the pool has no work left");
            break;
        }
        println!("  batch {}..{} ({} chunks)", ack.from, ack.from + ack.count, ack.count);
        let mut args: Vec<String> = vec![
            "--template".into(), job.template.clone(),
            "--acts".into(), job.acts.clone(),
            "--workspace".into(), job.workspace.clone(),
            "--outdir".into(), outdir.display().to_string(),
            "--name".into(), job.name.clone(),
            "--chunk-secs".into(), job.chunk_secs.to_string(),
            "--chapter-gap".into(), job.chapter_gap.to_string(),
            "--plan".into(), "plan.json".into(),
            "--from".into(), ack.from.to_string(),
            "--count".into(), ack.count.to_string(),
        ];
        if jobs > 0 {
            args.push("--jobs".into());
            args.push(jobs.to_string());
        }
        let status = std::process::Command::new(&exe)
            .args(&args)
            .current_dir(&stage)
            .status()
            .context("running this box's render")?;
        if !status.success() {
            bail!("the render of {}..{} failed", ack.from, ack.from + ack.count);
        }
        for k in ack.from..ack.from + ack.count {
            let f0 = k * job.plan.chunk_frames;
            match find_chunk(&chunks, f0)? {
                Some((name, path)) => {
                    let bytes = std::fs::read(&path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    submit(server, &name, &bytes)?;
                    rendered += 1;
                }
                None => bail!("chunk {k} ({f0}) is not in {}", chunks.display()),
            }
        }
    }
    println!("  submitted {rendered} chunks; this box is done");
    Ok(())
}

/// `cargo`, however this box spells it. A non-interactive ssh shell does not
/// read `~/.profile`, so a rustup toolchain is often absent from PATH even
/// though it is installed — the note the pipeline's Makefile carries too.
fn cargo_bin() -> String {
    if let Ok(c) = std::env::var("CARGO") {
        return c;
    }
    if let Ok(home) = std::env::var("HOME") {
        let rustup = PathBuf::from(home).join(".cargo/bin/cargo");
        if rustup.is_file() {
            return rustup.display().to_string();
        }
    }
    "cargo".to_string()
}

/// What a binary says its own source hashes to.
fn digest_of_exe(exe: &Path) -> Result<String> {
    let out = std::process::Command::new(exe)
        .arg("digest")
        .output()
        .with_context(|| format!("running {} digest", exe.display()))?;
    if !out.status.success() {
        bail!("{} could not report a source digest", exe.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn blank() -> Ask {
    Ask { op: String::new(), path: String::new(), name: String::new(), len: 0, want: 0 }
}

fn fetch(server: &str, op: &str, rel: &str, dst: &Path, quiet: bool) -> Result<()> {
    let mut c = connect(server)?;
    c.send(&Ask { op: op.into(), path: rel.into(), ..blank() })?;
    let ack: Ack = serde_json::from_value(c.line()?).context("reading the reply")?;
    if !ack.ok {
        bail!("{server} would not send {rel:?}");
    }
    let bytes = c.body(ack.len)?;
    std::fs::write(dst, &bytes).with_context(|| format!("writing {}", dst.display()))?;
    if !quiet {
        println!("    {rel}");
    }
    Ok(())
}

fn submit(server: &str, name: &str, bytes: &[u8]) -> Result<()> {
    let mut c = connect(server)?;
    c.send(&Ask { op: "submit".into(), name: name.into(), len: bytes.len(), ..blank() })?;
    c.send_raw(bytes)?;
    let ack: Ack = serde_json::from_value(c.line()?).context("reading the reply")?;
    if !ack.ok {
        bail!("{server} refused {name}");
    }
    Ok(())
}

/// The chunk that starts at frame `f0`, whatever its key.
fn find_chunk(dir: &Path, f0: usize) -> Result<Option<(String, PathBuf)>> {
    let prefix = format!("{f0:08}-");
    let mut found = None;
    for e in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".mp4") {
            found = Some((name, e.path()));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
