use super::super::stamp::parse_stamp;
use super::super::{REMOTE_DIR, TTS_PORT};
use super::*;
use anyhow::Context;
use anyhow::Result;

impl Ssh {
    /// Ask a machine what it already has.
    pub fn probe(&self, engine: &str) -> Probe {
        let script = format!(
            r#"echo "hostname=$(hostname 2>/dev/null || echo unknown)"
echo "os=$(uname -s 2>/dev/null || echo unknown)"
echo "arch=$(uname -m 2>/dev/null || echo unknown)"
echo "nproc=$(nproc 2>/dev/null || echo 0)"
echo "mem_mb=$(awk '/MemTotal/{{printf "%d", $2/1024}}' /proc/meminfo 2>/dev/null || echo 0)"
echo "disk_mb=$(df -Pm "$HOME" 2>/dev/null | awk 'NR==2{{print $4}}' || echo 0)"
if [ -x "$HOME/{dir}/bm-agent" ]; then
  echo "agent=$("$HOME/{dir}/bm-agent" --version 2>/dev/null | awk '{{print $NF}}' || echo unknown)"
else
  echo "agent=absent"
fi
if [ -x "$HOME/{dir}/python/.venv/bin/python" ]; then
  echo "python=present"
else
  echo "python=absent"
fi
if [ -x "$HOME/{dir}/{rel}/bm-tts" ]; then
  echo "tts_bin=present"
else
  echo "tts_bin=absent"
fi
if [ -f "$HOME/{dir}/{rel}/libonnxruntime.so.1" ]; then
  echo "tts_lib=present"
else
  echo "tts_lib=absent"
fi
if [ -f "$HOME/{dir}/{rel}/models/manifest.json" ]; then
  echo "models=present"
else
  echo "models=absent"
fi
if command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg=present"
else
  echo "ffmpeg=absent"
fi
if command -v sox >/dev/null 2>&1; then
  echo "sox=present"
else
  echo "sox=absent"
fi
# The roster the sidecar is *serving*, asked of the process that owns the
# answer rather than re-parsed out of the file it loaded. Two things fall out
# of that: the list describes what a render will actually find, not what a file
# says it should; and the probe's last `python3` is gone, which is what used to
# let a stock box be described as needing Python. `-f` makes a 503 (still
# loading) an empty answer instead of the error body read as a voice name.
#
# The octet 037 separator is `chr(31)`, which the Rust side splits on.
echo "voices=$(curl -s -f --max-time 3 http://127.0.0.1:{port}/voices 2>/dev/null | tr -d '[]"' | tr ',' '\037')"
if [ -f "$HOME/{dir}/.provision_stamp.json" ]; then
  echo "stamp=$(tr '\n' ' ' < "$HOME/{dir}/.provision_stamp.json" 2>/dev/null)"
fi
if command -v curl >/dev/null 2>&1 && curl -s --max-time 3 http://127.0.0.1:{port}/health >/dev/null 2>&1; then
  echo "tts=up"
else
  echo "tts=down"
fi
echo "probe=done"
"#,
            dir = REMOTE_DIR,
            rel = engine_rel(engine),
            port = TTS_PORT
        );

        let mut probe = Probe::default();
        match self.run(&script, 30) {
            Ok((code, stdout, stderr)) => {
                if code != 0 {
                    probe.note = format!(
                        "ssh exit {code}: {}",
                        crate::util::head_chars(stderr.trim(), 160)
                    );
                    return probe;
                }
                probe.reachable = true;
                for line in stdout.lines() {
                    let Some((k, v)) = line.split_once('=') else {
                        continue;
                    };
                    let v = v.trim();
                    match k.trim() {
                        "hostname" => probe.hostname = v.to_string(),
                        "os" => probe.os = normalize_os(v),
                        "arch" => probe.arch = normalize_arch(v),
                        "nproc" => probe.nproc = v.parse().unwrap_or(0),
                        "mem_mb" => probe.mem_mb = v.parse().unwrap_or(0),
                        "disk_mb" => probe.disk_free_mb = v.parse().unwrap_or(0),
                        "agent" if v != "absent" => probe.agent_version = Some(v.to_string()),
                        "python" => probe.python_present = v == "present",
                        "tts_bin" => probe.tts_bin_present = v == "present",
                        "tts_lib" => probe.tts_lib_present = v == "present",
                        "models" => probe.models_present = v == "present",
                        "ffmpeg" => probe.ffmpeg_present = v == "present",
                        "sox" => probe.sox_present = v == "present",
                        "voices" => {
                            // Names contain spaces ("Minh Triết"), the probe
                            // joins them with \x1f, never whitespace.
                            probe.voices = v
                                .split('\u{1f}')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect()
                        }
                        "stamp" => {
                            probe.stamp = parse_stamp(v);
                        }
                        "tts" => probe.tts_up = v == "up",
                        _ => {}
                    }
                }
                probe.note = "ok".into();
            }
            Err(e) => probe.note = format!("{e}"),
        }
        probe
    }
    /// Create the worker root skeleton, including the bound engine's own tree.
    pub fn ensure_root(&self, engine: &str) -> Result<()> {
        let script = format!(
            "mkdir -p $HOME/{d}/{rel} $HOME/{d}/prompts $HOME/{d}/assets/effects \
             $HOME/{d}/assets/music $HOME/{d}/assets/injects $HOME/{d}/data/chapters \
             $HOME/{d}/data/audio $HOME/{d}/crawl $HOME/{d}/output && echo READY",
            d = REMOTE_DIR,
            rel = engine_rel(engine)
        );
        let (code, stdout, stderr) = self.run(&script, 30)?;
        if code != 0 || !stdout.contains("READY") {
            anyhow::bail!("ensure_root failed (exit {code}): {}", stderr.trim());
        }
        Ok(())
    }
    /// Install or upgrade the agent binary, then verify it runs.
    pub fn install_agent(
        &self,
        agent_binary: &Path,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        self.rsync_push(agent_binary, "bm-agent", false, progress(live, "agent"))?;
        let script = format!(
            "chmod +x $HOME/{d}/bm-agent && $HOME/{d}/bm-agent --version",
            d = REMOTE_DIR
        );
        let (code, stdout, _stderr) = self.run(&script, 30)?;
        if code != 0 {
            anyhow::bail!("installed agent would not run (exit {code})");
        }
        Ok(stdout.trim().to_string())
    }
    /// Push the files this box's policy needs, as one bundle.
    ///
    /// Replaces three whole-tree rsyncs (`prompts/`, `assets/`, `refs/`) — 202 MB
    /// per box on this repo, 144 MB of it `refs/`, which no worker reads. What
    /// travels instead is the set `super::sources` selects: the registries a
    /// stage opens, the clips they register, the prompts a digest reads, the
    /// crawlers a crawl resolves, and the cast files the legacy paths fall back
    /// to. See that module for the per-stage table and why `refs/` is in none of
    /// it.
    ///
    /// The bundle is **named by its own manifest digest**, which is also the
    /// stamp's `sources_hash`. That is what makes the cache safe to keep: a file
    /// at that name is by definition the artifact this plan would produce, so
    /// there is no mtime to compare and nothing to rebuild.
    ///
    /// Delivery is a *replacement*, not a merge: the box prunes the trees the
    /// archive owns and extracts over them, which is the `--delete` semantics
    /// the old rsyncs had, plus the one-time removal of the `refs/` tree earlier
    /// versions left on every configured box.
    pub fn install_sources(
        &self,
        layout: &crate::Layout,
        stages: &[bm_proto::Stage],
        pack: Option<&crate::artifact::PackRelease>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<Vec<String>> {
        let plan = super::super::sources::Sources::plan_for(layout, stages, pack)?;
        let manifest = plan.manifest()?;
        let hash = super::super::sources::Sources::hash(&manifest);
        let dir = layout.root.join(".bm").join("sources");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let bundle = dir.join(format!("{hash}.tar.zst"));
        if !bundle.is_file() {
            plan.pack(&manifest, &bundle)?;
        }

        let mut lines = vec![format!(
            "sources: {} -> {}",
            plan.summary(),
            super::super::sources::BUNDLE_NAME
        )];
        // A registry naming a clip that is not on disk here is worth a line: the
        // merge degrades that one sound to silence with its own warning, and an
        // operator reading a provision log is the last person who can still fix
        // it cheaply. Bounded, because a pool with a moved directory would
        // otherwise fill the pane.
        //
        // With a fetched pack the warning is about **this** tree, not the box's,
        // and the box may well have the clip — so it says so rather than
        // sending the operator to re-push a bundle that no longer carries it.
        for missing in plan.missing.iter().take(5) {
            lines.push(match pack {
                Some(_) => format!(
                    "registry names a clip this checkout has lost (the release pack may still have it): {missing}"
                ),
                None => format!(
                    "registry names a clip that is not here (the merge goes silent for it): {missing}"
                ),
            });
        }
        if plan.missing.len() > 5 {
            lines.push(format!(
                "…and {} more missing clip(s)",
                plan.missing.len() - 5
            ));
        }

        self.rsync_push_plain(
            &bundle,
            super::super::sources::BUNDLE_NAME,
            progress(live, "sources"),
        )?;
        let extract = if pack.is_some() {
            // The bundle carries no `assets/` members when a pack is
            // configured, so the pruning extract would delete a tree it
            // cannot restore. Spare it; the pack step owns that tree.
            super::super::sources::extract_script_keep_assets()
        } else {
            super::super::sources::extract_script()
        };
        let (code, stdout, stderr) = self.run(&extract, 600)?;
        if code != 0 {
            let hint = if stderr.contains("zstd") || stdout.contains("zstd-missing") {
                " — install zstd on the box (apt install -y zstd)"
            } else {
                ""
            };
            anyhow::bail!(
                "unpacking the sources bundle failed (exit {code}){hint}: {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        if !stdout.contains("SOURCES-OK") {
            anyhow::bail!(
                "the box did not confirm the bundle: {}",
                crate::util::head_chars(stdout.trim(), 200)
            );
        }
        lines.push(format!(
            "sources in sync ({} @ {})",
            &hash[..12.min(hash.len())],
            if plan.stages.is_empty() {
                "no stage".to_string()
            } else {
                plan.stages
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("+")
            }
        ));

        // **After** the extract, so the log reads in delivery order. Either
        // order is safe now — the pack-configured extract spares `$D/assets`
        // — but the pack is the bigger story and belongs last in the report.
        if let Some(r) = pack {
            lines.push(self.install_pack(layout, r, live)?);
        }

        // One bundle per policy, kept as a cache; anything a day old goes. Age
        // rather than "everything but mine": provisions run one per machine and
        // several can be in flight, so a sweep by name could delete the artifact
        // another push is about to send.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let now = std::time::SystemTime::now();
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                if p == bundle || p.extension().and_then(|x| x.to_str()) != Some("zst") {
                    continue;
                }
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .map(|age| age.as_secs() > 24 * 3600)
                    .unwrap_or(false);
                if old {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(lines)
    }
}
