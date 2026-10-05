use super::super::{REMOTE_DIR, TTS_PORT};
use super::*;
use anyhow::Context;
use anyhow::Result;

impl Ssh {
    /// Push the TTS sidecar binary and the shared ONNX Runtime it links.
    pub fn install_tts_runtime(
        &self,
        engine: &str,
        tts_binary: &Path,
        runtime_dir: Option<&Path>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        // Beside the engine's weights, not at the root: `bm-tts` is VieNeu's
        let rel = engine_rel(engine);
        self.rsync_push(
            tts_binary,
            &format!("{rel}/bm-tts"),
            false,
            progress(live, "bm-tts"),
        )?;
        if let Some(runtime_dir) = runtime_dir {
            // Whatever the make target staged, rather than a version hardcoded here:
            let mut libs: Vec<std::path::PathBuf> = std::fs::read_dir(runtime_dir)
                .with_context(|| format!("reading the runtime dir {}", runtime_dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("libonnxruntime.so"))
                })
                .collect();
            libs.sort();
            if libs.is_empty() {
                anyhow::bail!(
                    "no libonnxruntime.so* in {}, run `make runtime` first",
                    runtime_dir.display()
                );
            }
            for lib in &libs {
                let name = lib.file_name().expect("filtered on a file name");
                let name = name.to_string_lossy();
                self.rsync_push(lib, &format!("{rel}/{name}"), false, progress(live, &name))?;
            }
        }

        // The SONAME has to be reachable *beside the binary* — the engine's
        let script = format!(
            r#"set -e
D="$HOME/{d}/{rel}"
cd "$D"
chmod +x bm-tts
LD_LIBRARY_PATH="$D" ./bm-tts --version >/dev/null 2>&1 || \
  {{ echo "bm-tts would not run, missing libonnxruntime.so.1 beside it?" >&2; exit 7; }}
echo "TTS-RUNTIME-OK ($(LD_LIBRARY_PATH="$D" ./bm-tts --version))"
"#,
            d = REMOTE_DIR,
            rel = rel,
        );
        let (code, stdout, stderr) = self.run(&script, 60)?;
        if code != 0 {
            anyhow::bail!(
                "installing the TTS runtime failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        Ok(stdout.trim().to_string())
    }
    /// Ask the box to take the artifact itself, and read the exit code as the
    fn fetch_models(
        &self,
        engine: &str,
        release: &crate::artifact::ModelsRelease,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> std::result::Result<String, FetchOutcome> {
        if let Some(l) = live {
            let _ = l.send(format!(
                "[{}] models: fetching {} from the release (a CDN, not this machine's uplink)",
                self.target, release.tag
            ));
        }
        // Bounded well past the transfer: 363 MB on a 200 KB/s link is half an
        let (code, stdout, stderr) = self
            .run(&fetch_script(release, engine), 3600)
            .map_err(|e| FetchOutcome::Unreachable(e.to_string()))?;
        classify_fetch(code, &stdout, &stderr, &release.tag, "models")
    }
    /// Put the baked `models/` directory on the box: from the release when one
    pub fn install_models(
        &self,
        engine: &str,
        src: &Path,
        release: Option<&crate::artifact::ModelsRelease>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        let rel = engine_rel(engine);
        if !src.is_dir() {
            anyhow::bail!(
                "no {}, run the bake first (`python3 tools/bake-models.py`)",
                src.display()
            );
        }
        if let Some(r) = release {
            match self.fetch_models(engine, r, live) {
                Ok(line) => {
                    // The checksum list is the box's own statement about its
                    let sums = model_checksums(src);
                    if !sums.is_empty() {
                        self.write_model_checksums(engine, &sums)?;
                    }
                    return Ok(line);
                }
                Err(FetchOutcome::Corrupt(e)) => {
                    // Not a fallback trigger. The bytes disagreed with the
                    anyhow::bail!(
                        "the release artifact does not verify and was not put in place: {e} \
                         (nothing was changed on this box; re-pack with tools/models.sh pack \
                         and re-publish, or clear `models_release` to push the directory)"
                    );
                }
                Err(FetchOutcome::Unreachable(e)) => {
                    if let Some(l) = live {
                        let _ = l.send(format!(
                            "[{}] release {} unreachable ({}), pushing models/ instead",
                            self.target, r.tag, e
                        ));
                    }
                }
            }
        }
        // `models.tar.zst` stays behind: it is the transfer artifact, and a box
        self.rsync_push_excluding(
            src,
            &format!("{rel}/models"),
            true,
            progress(live, "models"),
            MODELS_PUSH_EXCLUDES,
        )?;
        let sums = model_checksums(src);
        if !sums.is_empty() {
            self.write_model_checksums(engine, &sums)?;
        }
        // `sha256sum` is coreutils, so it is present on every platform these
        let script = format!(
            r#"D="$HOME/{d}/{rel}/models"
[ -f "$D/manifest.json" ] || {{ echo "models/manifest.json missing, incomplete bake" >&2; exit 8; }}
n=$(ls "$D" | wc -l)
L="$HOME/{d}/{rel}/models.sha256"
if ! command -v sha256sum >/dev/null 2>&1; then
  echo "MODELS-OK ($n files, NOT verified, sha256sum absent)" >&2
  exit 0
fi
if [ -f "$L" ]; then
  if ! out=$(cd "$D" && sha256sum -c "$L" 2>&1); then
    echo "$out" | grep -v ': OK$' | head -n 5 >&2
    echo "MODELS-CORRUPT, the weights here do not match the bake; nothing was rendered with them" >&2
    exit 10
  fi
  echo "MODELS-OK ($(wc -l < "$L") files verified)"
else
  echo "MODELS-OK ($n files, no checksum list)" >&2
fi
"#,
            d = REMOTE_DIR,
            rel = rel,
        );
        let (code, stdout, stderr) = self.run(&script, 600)?;
        if code != 0 {
            anyhow::bail!(
                "installing models failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 300)
            );
        }
        Ok(stdout.trim().to_string())
    }
    /// Write the `sha256sum -c` list the verify step reads.
    fn write_model_checksums(&self, engine: &str, sums: &[String]) -> Result<()> {
        let body = sums.join("\n");
        let script = format!(
            "cat > $HOME/{d}/{rel}/models.sha256 << 'EOF'\n{body}\nEOF\n",
            d = REMOTE_DIR,
            rel = engine_rel(engine),
        );
        let (code, _, stderr) = self.run(&script, 10)?;
        if code != 0 {
            anyhow::bail!("failed to write the model checksum list: {}", stderr.trim());
        }
        Ok(())
    }
    /// Best-effort opencode install for the digest lane. Auth stays manual
    pub fn ensure_opencode(&self, allow_install: bool) -> Result<String> {
        // `allow_install` is false on a box that already passed a full
        let (code, stdout, stderr) = self.run(&opencode_script(allow_install), 600)?;
        if code != 0 {
            anyhow::bail!("opencode check failed: {}", stderr.trim());
        }
        Ok(stdout.trim().to_string())
    }
    /// Best-effort zstd install: the sources bundle unpacks with it.
    pub fn ensure_zstd(&self) -> Result<String> {
        let (code, stdout, stderr) = self.run(&zstd_script(), 300)?;
        if code != 0 {
            return Ok(format!(
                "ZSTD-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }

    /// Best-effort ffmpeg install for the merge lane.
    pub fn ensure_ffmpeg(&self) -> Result<String> {
        let (code, stdout, stderr) = self.run(&ffmpeg_script(), 300)?;
        if code != 0 {
            // A failure to even run the check is itself a warning, never fatal:
            return Ok(format!(
                "FFMPEG-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }
    /// The merge stage's second engine, installed beside ffmpeg.
    pub fn ensure_sox(&self) -> Result<String> {
        let (code, stdout, stderr) = self.run(&sox_script(), 300)?;
        if code != 0 {
            return Ok(format!(
                "SOX-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }
    /// Start the TTS sidecar detached, unless it is already answering, and
    pub fn start_tts(&self, engine: &str) -> Result<String> {
        // The engine's tree holds the binary, its runtime and its weights; the
        let dict = match crate::voices::dictionary(engine) {
            Some(name) => format!("--dict \"$E/models/{name}\" "),
            None => String::new(),
        };
        // The per-box ONNX thread override rides the launch too, so a
        let threads = match crate::config::tts_threads() {
            0 => String::new(),
            n => format!("--threads {n} "),
        };
        let script = format!(
            r#"D="$HOME/{d}"
E="$D/{rel}"
if [ "$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/health)" = "200" ]; then
  echo "TTS-ALREADY-UP"; exit 0
fi
cd "$E" || exit 5
LD_LIBRARY_PATH="$E" nohup "$E/bm-tts" --models "$E/models" --codec "$E/models" \
  {dict}{threads}--voices "$E/models/voices.json" \
  --port {port} --bind 0.0.0.0 > "$D/tts.log" 2>&1 &
echo $! > "$D/tts.pid"
for _ in $(seq 1 120); do
  sleep 2
  [ "$(curl -s -o /dev/null -w '%{{http_code}}' --max-time 3 http://127.0.0.1:{port}/health)" = "200" ] && {{ echo "TTS-STARTED"; exit 0; }}
done
echo "TTS-STARTING (not ready after 240s, check $D/tts.log)"
"#,
            d = REMOTE_DIR,
            rel = engine_rel(engine),
            dict = dict,
            threads = threads,
            port = TTS_PORT
        );
        let (code, stdout, stderr) = self.run(&script, 300)?;
        if code != 0 {
            anyhow::bail!(
                "starting TTS failed (exit {code}): {}",
                crate::util::head_chars(stderr.trim(), 200)
            );
        }
        Ok(stdout.trim().to_string())
    }

    pub fn stop_tts(&self) -> Result<()> {
        let script = format!(
            // `pkill -x`, never `-f`: the pattern for a command-line match is
            r#"if [ -f "$HOME/{d}/tts.pid" ]; then kill "$(cat "$HOME/{d}/tts.pid")" 2>/dev/null || true; rm -f "$HOME/{d}/tts.pid"; fi
pkill -x bm-tts 2>/dev/null || true
echo stopped"#,
            d = REMOTE_DIR
        );
        let _ = self.run(&script, 30)?;
        Ok(())
    }
}
