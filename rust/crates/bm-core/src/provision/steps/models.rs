use anyhow::Result;
use super::super::{REMOTE_DIR, TTS_PORT};
use super::*;
use anyhow::Context;

impl Ssh {
    /// Push the TTS sidecar binary and the shared ONNX Runtime it links.
    ///
    /// Replaces `ensure_python`, which built a 647 MB virtualenv on every
    /// worker. Two files and a symlink do the same job now.
    ///
    /// The library is pushed under **every** name it is known by, the linker
    /// wants the plain `libonnxruntime.so`, the loader wants the SONAME
    /// `libonnxruntime.so.1`, and the versioned file is what those two point at.
    /// Shipping only one of them produces a failure that names none of this.
    ///
    /// `runtime_dir` is `None` where the sidecar is self-contained (macOS
    /// links its runtime statically, the native binary runs with no `.so`
    /// beside it), so only the binary travels.
    pub fn install_tts_runtime(
        &self,
        engine: &str,
        tts_binary: &Path,
        runtime_dir: Option<&Path>,
        live: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    ) -> Result<String> {
        // Beside the engine's weights, not at the root: `bm-tts` is VieNeu's
        // binary, and a second engine ships its own under its own tree.
        let rel = engine_rel(engine);
        self.rsync_push(
            tts_binary,
            &format!("{rel}/bm-tts"),
            false,
            progress(live, "bm-tts"),
        )?;
        if let Some(runtime_dir) = runtime_dir {
            // Whatever the make target staged, rather than a version hardcoded here:
            // the pin lives in the Makefile, and two copies of it would drift.
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
        // directory, which is what `LD_LIBRARY_PATH` names.
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
    /// contract `bm-agent fetch-artifact` documents: `0` landed, `2` wrong
    /// bytes, `3` not there.
    ///
    /// The agent is what fetches, not a `curl | zstd | tar` line in a heredoc,
    /// for the reason the artifact exists at all: a shell pipeline cannot verify
    /// seventeen files and then swap a directory in two renames, and a
    /// half-extracted `models/` is the failure this was built to remove. The
    /// agent also carries the decompressor, so the box needs no `zstd` package
    /// and there is no second cross-built binary to version-gate.
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
        // hour, and the timeout is here to catch a wedged ssh rather than to
        // second-guess a slow box.
        let (code, stdout, stderr) = self
            .run(&fetch_script(release, engine), 3600)
            .map_err(|e| FetchOutcome::Unreachable(e.to_string()))?;
        classify_fetch(code, &stdout, &stderr, &release.tag, "models")
    }
    /// Put the baked `models/` directory on the box: from the release when one
    /// is configured and reachable, over the push otherwise.
    ///
    /// 668 MB, content-addressed by the stamp's `tts_hash`, so a re-provision
    /// with nothing changed costs nothing either way.
    ///
    /// **A release, when there is one.** The weights are the one payload that
    /// is identical on every machine and changes only when the operator
    /// re-bakes them, so they are the one payload worth naming: the box
    /// downloads `models.tar.zst` from a GitHub release tagged by the manifest
    /// hash and verifies it itself, and 363 MB comes off a CDN instead of this
    /// house's uplink, once per box instead of once per operator. The split of
    /// failures is what makes the fallback safe — *unreachable* (no such
    /// release, no route, a 5xx) falls back to the push below, and says so;
    /// *wrong bytes* stops, because pushing the same wrong bytes again would
    /// only hide the disagreement.
    ///
    /// **What arrives is verified, not assumed.** rsync exiting 0 says the
    /// *transfer* worked, which is weaker than "the weights are intact": a box
    /// that dies mid-push, a corrupt source file, or a `--delete` racing a
    /// writer all leave a directory rsync is happy with and the sidecar is
    /// not. So the bake's own `sha256` entries are written out as a
    /// `sha256sum -c` list and checked on the box, with `models/voices.json`
    /// excluded: the manifest's entry for it is stale by design, because
    /// enrollment rewrites the file after the bake.
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
                    // install, and the fetch has already verified every file
                    // against the same manifest, so it is written either way:
                    // a later push of a *different* bake then has a list to
                    // check against.
                    let sums = model_checksums(src);
                    if !sums.is_empty() {
                        self.write_model_checksums(engine, &sums)?;
                    }
                    return Ok(line);
                }
                Err(FetchOutcome::Corrupt(e)) => {
                    // Not a fallback trigger. The bytes disagreed with the
                    // manifest the operator's own bake declares, and pushing
                    // them would be the same bytes with a worse provenance.
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
        // that is being pushed the directory has no use for a second copy of
        // the same 668 MB. Excluding it also keeps `--delete` from putting one
        // back on a box that fetched a bundle.
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
        // workers run, but "present" is assumed rather than proved, and a box
        // without it is reported rather than silently reported as verified.
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
    ///
    /// Generated here, from the bake's own declaration, rather than derived on
    /// the box: the box has no JSON parser left (the probe's `python3` is gone),
    /// and a shell re-parse of a single-line 492 KB document is exactly the kind
    /// of thing that works until a voice is named with a brace in it.
    ///
    /// It lives at the worker root, not inside `models/`, because that push
    /// carries `--delete` and would eat it.
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
    /// (browser login); without it remote digests fail loudly, never silently.
    pub fn ensure_opencode(&self, allow_install: bool) -> Result<String> {
        // `allow_install` is false on a box that already passed a full
        // provision. The check stays, it is one `command -v`, but the install
        // does not: `npm i -g` is bounded at ten minutes, and on a box whose
        // answer will not change it was ten minutes of a `B` press that looked
        // like a hang. A deliberate re-provision still installs.
        let (code, stdout, stderr) = self.run(&opencode_script(allow_install), 600)?;
        if code != 0 {
            anyhow::bail!("opencode check failed: {}", stderr.trim());
        }
        Ok(stdout.trim().to_string())
    }
    /// Best-effort ffmpeg install for the merge lane.
    ///
    /// The merge stage shells out to `ffmpeg`, so a box without it takes every
    /// merge it is offered and fails each one. This installs it from the
    /// platform's package manager when it is missing, never a hard failure:
    /// a refused install (no sudo, an offline mirror) only warns, and the
    /// worker's `merge` capability gate keeps merge off this box until ffmpeg
    /// appears. Returns a one-line verdict for the provision log.
    ///
    /// `allow_install` carries the same meaning as on [`Self::ensure_opencode`]:
    /// a present `ffmpeg` short-circuits either way, so the flag only decides
    /// whether a *missing* one is chased with a package manager this time.
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

    pub fn ensure_ffmpeg(&self, allow_install: bool) -> Result<String> {
        let (code, stdout, stderr) = self.run(&ffmpeg_script(allow_install), 300)?;
        if code != 0 {
            // A failure to even run the check is itself a warning, never fatal:
            // the merge lane stays available to other boxes.
            return Ok(format!(
                "FFMPEG-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }
    /// The merge stage's second engine, installed beside ffmpeg. A refusal is
    /// a warning, and the worker reports no `merge` capability until sox is
    /// present (so the scheduler simply never offers it one).
    pub fn ensure_sox(&self, allow_install: bool) -> Result<String> {
        let (code, stdout, stderr) = self.run(&sox_script(allow_install), 300)?;
        if code != 0 {
            return Ok(format!(
                "SOX-SKIP (check failed: {})",
                crate::util::head_chars(stderr.trim(), 120)
            ));
        }
        Ok(stdout.trim().to_string())
    }
    /// Start the TTS sidecar detached, unless it is already answering, and
    /// **wait for it to be ready** before returning.
    ///
    /// Waiting is the point, not politeness. `bm-tts` binds its port before it
    /// loads ~2.85 GB of weights, so between the launch and the first ready
    /// `/health` there is a window where the box has a sidecar that cannot
    /// answer yet. A short sleep there made the worker's first render spawn a
    /// **second** model on an 8 GiB box: the OOM this cluster kept taking.
    ///
    /// A ready server answers 200; a loading one answers 503, which `curl`
    /// reports as success unless told otherwise, so the check is on the
    /// *code*. The budget is deliberately under the ssh call's own timeout
    /// (240 s of polling, 300 s allowed).
    pub fn start_tts(&self, engine: &str) -> Result<String> {
        // The engine's tree holds the binary, its runtime and its weights; the
        // log and the pid stay at the worker root, where `stop_tts` reads the
        // pid and where an operator goes looking for the log.
        //
        // `--dict` is emitted only when the engine declares a lexicon, so a
        // second engine is never handed VieNeu's and asked to mispronounce
        // through it.
        let dict = match crate::voices::dictionary(engine) {
            Some(name) => format!("--dict \"$E/models/{name}\" "),
            None => String::new(),
        };
        // The per-box ONNX thread override rides the launch too, so a
        // provision-started sidecar opens with the same count the worker's own
        // `ensure` would have used — otherwise the next render would adopt a
        // half-core sidecar and the setting would look ignored. Empty means
        // the sidecar's own default (half the cores, capped at 8).
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
            // contained in the ssh wrapper's own argv, so `-f` kills the wrapper
            // running this script. `-x` matches the process name only.
            r#"if [ -f "$HOME/{d}/tts.pid" ]; then kill "$(cat "$HOME/{d}/tts.pid")" 2>/dev/null || true; rm -f "$HOME/{d}/tts.pid"; fi
pkill -x bm-tts 2>/dev/null || true
echo stopped"#,
            d = REMOTE_DIR
        );
        let _ = self.run(&script, 30)?;
        Ok(())
    }
}



