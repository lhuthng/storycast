//! The FilmCraft side: `filmcraft-cli` in, JSON out.
//!
//! The CLI is FilmCraft's documented headless interface, and it already speaks
//! JSON in both directions, so the plugin drives a subprocess rather than
//! reimplementing the control channel or MCP. Every call reopens the project
//! and writes it back, which is what makes a run resumable between steps.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// FilmCraft's time base: it divides evenly by every common frame rate.
pub const TICKS_PER_SECOND: f64 = 254_016_000_000.0;

pub struct Driver {
    bin: PathBuf,
    /// The project the calls open, once there is one.
    project: Option<PathBuf>,
}

impl Driver {
    pub fn new(bin: PathBuf) -> Driver {
        Driver { bin, project: None }
    }

    /// `file.newProject`, saved to `path` so the next call can open it.
    pub fn new_project(&mut self, path: &Path, name: &str) -> Result<()> {
        let mut cmd = self.base();
        cmd.arg("--save-as")
            .arg(path)
            .arg("exec")
            .arg("file.newProject")
            .arg(json!({"name": name}).to_string());
        self.run(cmd, "file.newProject")?;
        self.project = Some(path.to_path_buf());
        Ok(())
    }

    /// `file.newSequence`, which also makes it the active sequence.
    ///
    /// `tracks` is how many video tracks the look composes on. A track index
    /// past the existing tracks makes FilmCraft add one and put the clip there,
    /// so the look's tracks have to exist before it is built.
    pub fn new_sequence(
        &self,
        name: &str,
        canvas: (u32, u32),
        fps: f64,
        tracks: u32,
    ) -> Result<Value> {
        let (width, height) = canvas;
        self.exec(
            "file.newSequence",
            json!({
                "name": name,
                "width": width,
                "height": height,
                "fps": fps,
                "video": tracks
            }),
        )
    }

    /// The length FilmCraft decoded for every project item, by id.
    ///
    /// Its clock is the one clips land on, so it decides where the next chapter
    /// starts. A chapter's mp3 runs past its last cue — the tail the mixer
    /// wrote — so laying the next one down on the cue sheet's figure would cut
    /// that tail off.
    pub fn item_durations(&self) -> Result<HashMap<i64, f64>> {
        let reply = self.exec("project.inspect", json!({}))?;
        let mut out: HashMap<i64, f64> = HashMap::new();
        if let Some(root) = reply.get("root") {
            collect_durations(root, &mut out);
        }
        Ok(out)
    }

    /// `file.import`, and the item ids it made — one per path, in order.
    pub fn import(&self, paths: &[PathBuf]) -> Result<Vec<i64>> {
        let value = self.exec("file.import", json!({"paths": paths}))?;
        let errors: Vec<Value> = value
            .get("errors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !errors.is_empty() {
            bail!("FilmCraft refused {} of {} files: {errors:?}", errors.len(), paths.len());
        }
        let items: Vec<i64> = value
            .get("items")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        Ok(items)
    }

    /// Every font family the app can render in, for the template's font check.
    pub fn fonts(&self) -> Result<Vec<String>> {
        let reply = self.exec("fonts.list", json!({"system": true}))?;
        Ok(reply
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|f| f.get("family").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// One command. A failing command is an error, not a silent no-op.
    pub fn exec(&self, id: &str, params: Value) -> Result<Value> {
        let mut cmd = self.base();
        if let Some(p) = &self.project {
            cmd.arg("--project").arg(p).arg("--save");
        }
        cmd.arg("exec").arg(id).arg(params.to_string());
        let out = self.run(cmd, id)?;
        serde_json::from_str(&out).with_context(|| format!("parsing the reply to {id}: {out}"))
    }

    /// `run`, the whole build in one session: one command per line, stopping at
    /// the first failure. One reply per line, in order.
    pub fn run_script(&self, script: &Path) -> Result<Vec<Value>> {
        let mut cmd = self.base();
        if let Some(p) = &self.project {
            cmd.arg("--project").arg(p).arg("--save");
        }
        cmd.arg("run").arg(script);
        let out = self.run(cmd, "run")?;
        let mut replies = Vec::new();
        for line in out.lines() {
            let value: Value = serde_json::from_str(line)
                .with_context(|| format!("parsing a script line: {line}"))?;
            if value.get("ok") != Some(&Value::Bool(true)) {
                bail!("FilmCraft rejected a step: {line}");
            }
            replies.push(value);
        }
        Ok(replies)
    }

    /// `file.exportMedia`, through FilmCraft's own encoder, waiting for it.
    pub fn export(&self, video: &Path, preview: Option<f64>, burn: bool) -> Result<Value> {
        let mut params = json!({
            "path": video,
            "format": "h264",
            "wait": true,
            "burnCaptions": burn,
        });
        if burn {
            params["captionSidecar"] = json!("srt");
        }
        match preview {
            Some(secs) => {
                params["range"] = json!("custom");
                params["startSeconds"] = json!(0.0);
                params["endSeconds"] = json!(secs);
            }
            None => params["range"] = json!("entire"),
        }
        self.exec("file.exportMedia", params)
    }

    fn base(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--compact");
        cmd
    }

    fn run(&self, mut cmd: Command, what: &str) -> Result<String> {
        let out = cmd
            .output()
            .with_context(|| format!("running {} ({what}); is it on PATH?", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            bail!("{what} failed ({}): {detail}", out.status);
        }
        Ok(stdout)
    }
}

/// Every item in the project tree, bins and all.
fn collect_durations(node: &Value, out: &mut HashMap<i64, f64>) {
    if let (Some(item), Some(ticks)) = (
        node.get("item").and_then(Value::as_i64),
        node.get("duration").and_then(Value::as_i64),
    ) {
        out.insert(item, ticks as f64 / TICKS_PER_SECOND);
    }
    for child in node.get("children").and_then(Value::as_array).into_iter().flatten() {
        collect_durations(child, out);
    }
}
