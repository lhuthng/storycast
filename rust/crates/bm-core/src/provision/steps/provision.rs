use super::super::stamp::compute_provision_stamp;
use super::super::REMOTE_DIR;
use super::*;
use bm_proto::Machine;

/// Full onboarding for one machine: probe, then push only what is missing.
///
/// Returns the log lines the TUI should show, in order.
#[allow(clippy::too_many_arguments)]
pub fn provision(
    m: &Machine,
    layout: &crate::Layout,
    agent_binary: &Path,
    tts_binary: &Path,
    tts_runtime: Option<&Path>,
    agent_version: &str,
    force: bool,
    initial_probe: Option<Probe>,
    live: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    // `owner/name` of the releases that host the model artifact. `None` reads
    // it from this workspace's `settings.json`, which is where the operator
    // sets it; a one-shot provision passes it to override.
    release_repo: Option<&str>,
) -> (Probe, Vec<String>) {
    let ssh = Ssh::for_machine(m);
    // Cloned, not moved: the install steps below borrow the original for
    // their byte-progress streams.
    let mut log = LiveLog::new(live.clone());

    let mut probe = match initial_probe {
        Some(p) => {
            log.push(format!("[{}] {}", m.id, p.summary()));
            p
        }
        None => {
            log.push(format!("[{}] probing {}", m.id, ssh.target));
            let p = ssh.probe(&layout.engine);
            log.push(format!("[{}] {}", m.id, p.summary()));
            p
        }
    };
    if !probe.reachable {
        log.push(format!("[{}] unreachable, aborting provision", m.id));
        return (probe, log.lines);
    }

    // Where the weights come from, resolved once: the release named by the
    // operator, or nothing. A workspace with no `models_release` and no
    // `--release-repo` keeps the push, which is the safe direction — a box
    // that cannot reach a release is still a box that can be provisioned.
    let repo = release_repo
        .map(str::to_string)
        .unwrap_or_else(|| crate::config::Settings::load(&layout.settings()).models_release);
    let release = crate::artifact::ModelsRelease::resolve(&layout.models_dir(), &repo);
    if repo.trim().is_empty() {
        log.push(format!(
            "[{}] no models release configured, the weights travel over the push",
            m.id
        ));
    }

    // And the profile pack, from the same shape of setting and for the same
    // reason. **Separate** on purpose: the two artifacts are published
    // independently and one is far more often absent — a checkout with a
    // released pack and an unreleased bake of the weights is the ordinary
    // case, and a setting that could only say both or neither would make the
    // operator choose a 668 MB push to save a 60 MB one.
    //
    // Resolved against the **load pointer**, not a hash of the tree, so a
    // release is only used when this checkout says which one it is running. A
    // pointer with no version resolves to nothing, which is the push: every
    // box provisioned before this existed keeps working.
    let settings = crate::config::Settings::load(&layout.settings());
    // A released pack is the **checkout's** profile: its tag, its manifest and
    // the tree it lands are all that one directory. A workspace that composes
    // its own `assets/` is not that checkout — a release pointed at it would put
    // the wrong book's score on the box while this book runs its own — so the
    // book's tree travels in the bundle instead.
    let pack = if layout.owns_assets() {
        None
    } else {
        crate::artifact::PackRelease::resolve(&layout.root, &settings.packs_release)
    };
    if !settings.packs_release.trim().is_empty() {
        if layout.owns_assets() {
            log.push(format!(
                "[{}] packs_release is set but this workspace composes its own assets/ — the book's tree travels in the bundle, not as a pack release",
                m.id
            ));
        } else if pack.is_none() {
            let binding = crate::profile::in_force(layout).ok();
            let pointer = binding
                .as_ref()
                .map(|b| format!("{} (version {:?})", b.pack.name, b.pack.version))
                .unwrap_or_else(|| "no profile binding".to_string());
            log.push(format!(
                "[{}] packs_release is set but {} names no versioned pack, so assets/ travels over the push \
                 (re-publish it: tools/profile.sh pack <name> --version <v> && gh release create {}-pack-v<v> …)",
                m.id,
                pointer,
                binding.map(|b| b.pack.name).unwrap_or_default(),
            ));
        }
    }

    // Before anything is pushed: the sources bundle is `tar` + `zstd`, so the
    // tool that opens it has to be here first. This is the one install attempted
    // on an already-configured box (see `zstd_script`); a box that cannot get it
    // still gets a line here, and the push below fails with the remedy in its
    // own message rather than a shell error naming nothing.
    match ssh.ensure_zstd() {
        Ok(v) => log.push(format!("[{}] {v}", m.id)),
        Err(e) => log.push(format!("[{}] zstd check failed: {e}", m.id)),
    }

    // Self-healing enrollment: the manifest may name clones the pushed store
    // lacks (added or swapped since the last bake). Merging them here, before
    // the stamp, means the hash drift pushes the fix to workers in this same
    // run, instead of warning forever no matter how often `:prov` runs.
    // Voices enrolled nowhere stay missing; the warning below still names
    // exactly those.
    let baked = crate::pool::bake_missing_voices(layout);
    if !baked.is_empty() {
        log.push(format!(
            "[{}] baked {} voice(s) into models/voices.json: {}",
            m.id,
            baked.len(),
            baked.join(", ")
        ));
    }

    // What this box's own policy says it may run decides what it must hold. A
    // box with no stored policy enables all four, so the ordinary case is the
    // full set and only an explicitly narrowed panel goes lean.
    let stages = super::super::sources::stages_of(&m.effective_task_policy());
    let local_stamp = match compute_provision_stamp(
        layout,
        &stages,
        agent_version,
        agent_binary,
        pack.as_ref(),
    ) {
        Ok(s) => s,
        Err(e) => {
            probe.note = format!("cannot read the sources it would push: {e:#}");
            log.push(format!("[{}] {}", m.id, probe.note));
            return (probe, log.lines);
        }
    };
    let remote_stamp = probe.stamp.as_ref();

    // **And** the pack. With a release configured the bundle carries no
    // `assets/`, so two different packs produce the *same* `sources.tar.zst` —
    // the bundle cannot see the difference, and a gate that read only the
    // bundle would call a re-pointed profile "in sync" on every box and never
    // send it. The pack has its own stamp field for exactly this.
    let sources_match = !force
        && remote_stamp
            .map(|s| s.sources_in_sync(&local_stamp) && s.pack_in_sync(&local_stamp))
            .unwrap_or(false);
    // The voice store now travels inside `models/`, so `tts_hash` covers it and
    // there is no separate voices check. Also verify the remote names: a stamp
    // written by the old already-configured path could say “in sync” after it
    // skipped the model push, which is exactly how Narrator 2 stayed missing.
    let manifest = crate::pool::load_manifest(&layout.voices_manifest());
    // An *unknown* roster is not a missing one. The probe cannot read the
    // roster when the sidecar is not answering, and reading that as "this box
    // knows none of the declared voices" would answer a down sidecar with a
    // 668 MB model push. `voices_hash` is the primary gate now, it compares
    // the content of `models/voices.json` against ours, and this check is the
    // backstop for a stamp that lies, so it only ever *adds* a push when it has
    // something to say.
    let remote_voice_store_complete =
        probe.voices.is_empty() || voice_store_covers(&probe.voices, &manifest);
    let models_match =
        !models_need_push(remote_stamp, &local_stamp, force) && remote_voice_store_complete;

    // What the box already is, asked once. This gates the install steps at the
    // bottom of the function as well as the push steps here: on a box that has
    // everything, provisioning is a *verification*, and the two package
    // installs are attempts that can each take minutes and cannot succeed on
    // the second try any more than the first. That is the difference between a
    // catch-up that costs a round trip per step and one that costs minutes on a
    // machine that is already working.
    let already = probe.configured(agent_version) && !force;
    // A voice change is a model-store change. Remember that we pushed the
    // store so the already-running sidecar is restarted below; otherwise the
    // new `models/voices.json` is on disk while the old roster stays resident.
    let mut models_pushed = false;
    // And the same flag for the binary itself, which is the one that matters
    // most: a replaced `bm-tts` on disk does nothing while the old process is
    // still running it, so a redeploy that does not recycle the sidecar looks
    // like a successful provision and behaves like no provision at all.
    let mut tts_pushed = false;
    // Read here, beside `already`, so the two cannot disagree about what this
    // run is allowed to do, see [`may_install`].
    let installs = may_install(probe.configured(agent_version), force);

    if already {
        log.push(format!(
            "[{}] already configured (agent {} + tts sidecar)",
            m.id, agent_version
        ));
        // The version string cannot see a rebuild: every dev build between
        // releases reports the same one, so a same-version binary drift would
        // otherwise sit on the box for ever. The content hash catches it, and
        // rsync makes the no-op push cheap when the bytes never moved.
        if !remote_stamp
            .map(|s| s.agent_in_sync(&local_stamp))
            .unwrap_or(false)
        {
            match ssh.install_agent(agent_binary, live.as_ref()) {
                Ok(v) => log.push(format!(
                    "[{}] agent binary drifted, redeployed (reports version {v})",
                    m.id
                )),
                Err(e) => log.push(format!("[{}] agent redeploy failed: {e}", m.id)),
            }
        }
        // The sidecar binary, on the same reasoning as the agent above.
        //
        // Without this branch a rebuilt `bm-tts` never reached a configured
        // box: `tts_hash` covers `models/`, not the binary, and the only push
        // site lived in the `else` below, which an already-configured box never
        // reaches. The box kept serving the old sidecar for ever, silently.
        if !remote_stamp
            .map(|s| s.tts_bin_in_sync(&local_stamp))
            .unwrap_or(false)
        {
            match ssh.install_tts_runtime(&layout.engine, tts_binary, tts_runtime, live.as_ref()) {
                Ok(v) => {
                    tts_pushed = true;
                    log.push(format!("[{}] sidecar drifted, redeployed, {v}", m.id));
                }
                Err(e) => log.push(format!("[{}] sidecar redeploy failed: {e}", m.id)),
            }
        }
        if sources_match {
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            // One artifact, pushed whole. The hash in the bundle's name is the
            // same digest `sources_match` just compared, so a box that reaches
            // this branch is one whose set really differs.
            match ssh.install_sources(layout, &stages, pack.as_ref(), live.as_ref()) {
                Ok(lines) => {
                    for l in lines {
                        log.push(format!("[{}] {l}", m.id));
                    }
                }
                Err(e) => log.push(format!("[{}] sources push failed: {e}", m.id)),
            }
        }

        // The voice store lives in `models/voices.json`, not in `voices.json`.
        // An already-configured worker still needs the model directory pushed
        // when a new clone was baked; the old branch only synced sources, so
        // `:prov` could report success while the worker still answered
        // `unknown voice "Narrator 2"`.
        if models_match {
            log.push(format!("[{}] models in sync (cache match)", m.id));
        } else {
            log.push(format!("[{}] pushing models/voice store", m.id));
            match ssh.install_models(
                &layout.engine,
                &layout.models_dir(),
                release.as_ref(),
                live.as_ref(),
            ) {
                Ok(v) => {
                    models_pushed = true;
                    log.push(format!("[{}] {v}", m.id));
                }
                Err(e) => {
                    log.push(format!("[{}] model sync failed: {e}", m.id));
                    return (probe, log.lines);
                }
            }
        }
    } else {
        if let Err(e) = ssh.ensure_root(&layout.engine) {
            log.push(format!("[{}] ensure_root failed: {e}", m.id));
            return (probe, log.lines);
        }
        log.push(format!("[{}] worker root ready (~/{REMOTE_DIR})", m.id));

        match ssh.install_agent(agent_binary, live.as_ref()) {
            Ok(v) => log.push(format!("[{}] agent installed, reports version {v}", m.id)),
            Err(e) => {
                log.push(format!("[{}] agent install failed: {e}", m.id));
                return (probe, log.lines);
            }
        }

        if sources_match {
            log.push(format!("[{}] sources in sync (cache match)", m.id));
        } else {
            match ssh.install_sources(layout, &stages, pack.as_ref(), live.as_ref()) {
                Ok(lines) => {
                    for l in lines {
                        log.push(format!("[{}] {l}", m.id));
                    }
                }
                Err(e) => log.push(format!("[{}] sources push failed: {e}", m.id)),
            }
        }

        if !probe.tts_bin_present || !probe.tts_runtime_ok() || force {
            log.push(format!(
                "[{}] installing the TTS sidecar binary + runtime",
                m.id
            ));
            match ssh.install_tts_runtime(&layout.engine, tts_binary, tts_runtime, live.as_ref()) {
                Ok(v) => log.push(format!("[{}] {v}", m.id)),
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log.lines);
                }
            }
        } else {
            log.push(format!(
                "[{}] TTS sidecar binary already present, skipped",
                m.id
            ));
        }

        // 668 MB, and the reason `tts_hash` exists: a re-provision with nothing
        // changed must not re-send it.
        if models_match {
            log.push(format!("[{}] models in sync (cache match)", m.id));
        } else {
            log.push(format!("[{}] pushing models (~668 MB)", m.id));
            match ssh.install_models(
                &layout.engine,
                &layout.models_dir(),
                release.as_ref(),
                live.as_ref(),
            ) {
                Ok(v) => {
                    models_pushed = true;
                    log.push(format!("[{}] {v}", m.id));
                }
                Err(e) => {
                    log.push(format!("[{}] {e}", m.id));
                    return (probe, log.lines);
                }
            }
        }
    }

    // The cluster token: what lets this worker tell its own inductor from
    // anything else that can reach its port. Shipped here rather than passed at
    // launch so the secret never appears in `argv` (and so a rotated token
    // reaches the box without a manual step). A worker started with
    // `--serve-tasks` refuses to run without it.
    match crate::token::read(&layout.root) {
        Some(token) => match ssh.write_cluster_token(&token) {
            Ok(()) => log.push(format!(
                "[{}] cluster token {} (owner-only on the box)",
                m.id,
                &token[..8.min(token.len())]
            )),
            Err(e) => log.push(format!("[{}] cluster token failed: {e}", m.id)),
        },
        None => log.push(format!(
            "[{}] no cluster token on this inductor, `serve` generates one; a worker started with --serve-tasks will refuse to run until it does",
            m.id
        )),
    }

    // The worker's agent gate checks this pointer at startup: sources above
    // carry the profile content, the pointer says what it claims to be.
    // Written every provision (one small file) so a re-pointed inductor
    // cannot leave a worker verifying yesterday's profile.
    //
    // The binding **in force**, not the checkout pointer: a workspace's own
    // pack travels in the bundle above, so a box handed that tree but stamped
    // with the checkout's `xianxia` would verify against a profile it does not
    // hold.
    match crate::profile::in_force(layout) {
        Ok(binding) => match ssh.write_profile_pointer(&binding) {
            Ok(()) => log.push(format!(
                "[{}] profile pointer: {} ({})",
                m.id,
                crate::profile::label(&binding),
                &binding.pack.hash[..12.min(binding.pack.hash.len())]
            )),
            Err(e) => log.push(format!("[{}] profile pointer failed: {e}", m.id)),
        },
        Err(_) => log.push(format!(
            "[{}] no local profile pointer, load one first (`:profile` in the dashboard, or `tools/profile.sh unpack <name>`), or this worker will refuse to start",
            m.id
        )),
    }

    // Enrollment moved off the worker, it needs the encoder, which is not on a
    // worker any more. A clone declared in `voices.json` but absent from the
    // pushed store therefore cannot render anywhere, and the old flow would
    // have quietly enrolled it on first use. Say so instead.
    {
        let manifest: std::collections::HashMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(layout.voices_manifest()).unwrap_or_default(),
        )
        .unwrap_or_default();
        if !manifest.is_empty() {
            let store: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(layout.tts_voices()).unwrap_or_default(),
            )
            .unwrap_or(serde_json::Value::Null);
            let presets = store.get("presets").and_then(|v| v.as_object());
            let missing: Vec<&str> = manifest
                .keys()
                .filter(|n| !n.starts_with('_'))
                .filter(|n| presets.is_none_or(|p| !p.contains_key(*n)))
                .map(|s| s.as_str())
                .collect();
            if !missing.is_empty() {
                log.push(format!(
                    "[{}] declared in voices.json but missing from models/voices.json: {}, enroll on this machine and re-bake, or a render naming one will fail here",
                    m.id,
                    missing.join(", ")
                ));
            }
        }
    }

    match ssh.ensure_opencode(installs) {
        Ok(v) => log.push(format!("[{}] {v}", m.id)),
        Err(e) => log.push(format!("[{}] opencode check failed: {e}", m.id)),
    }

    // The merge stage's encoder. Installed here so a fresh box can merge; a
    // refusal is a warning, and the worker reports no `merge` capability
    // (so the scheduler simply never offers it one) rather than failing
    // three merges and shelving chapters.
    match ssh.ensure_ffmpeg(installs) {
        Ok(v) if v.starts_with("FFMPEG-OK") => log.push(format!("[{}] {v}", m.id)),
        Ok(v) => log.push(format!(
            "[{}] {v}, merge stays disabled on this box until ffmpeg is present (apt/dnf install ffmpeg), then force a re-provision",
            m.id
        )),        Err(e) => log.push(format!("[{}] ffmpeg install check failed: {e}", m.id)),
    }

    // The merge stage's second engine. The voice treatment (room, character,
    // decay) runs through sox, and the worker gates `merge` on it, so it is
    // installed beside ffmpeg rather than discovered at merge time.
    match ssh.ensure_sox(installs) {
        Ok(v) if v.starts_with("SOX-OK") => log.push(format!("[{}] {v}", m.id)),
        Ok(v) => log.push(format!(
            "[{}] {v}, merge stays disabled on this box until sox is present (apt/dnf install sox), then force a re-provision",
            m.id
        )),        Err(e) => log.push(format!("[{}] sox install check failed: {e}", m.id)),
    }
    // The sidecar loads its voice roster at startup. A models push therefore
    // has to recycle an already-running sidecar; otherwise the new store is
    // present on disk but the process keeps serving the old 66-voice roster.

    if models_pushed || tts_pushed {
        match ssh.stop_tts() {
            Ok(()) if models_pushed && probe.tts_up => {
                log.push(format!("[{}] stopped TTS to reload the voice store", m.id))
            }
            Ok(()) if tts_pushed => log.push(format!(
                "[{}] stopped TTS so the new sidecar binary is the one that runs",
                m.id
            )),
            Ok(()) => log.push(format!("[{}] cleared stale TTS process", m.id)),
            Err(e) => log.push(format!(
                "[{}] could not stop TTS for voice reload: {e}",
                m.id
            )),
        }
    }

    // Waits for ready, so this line is a fact and not a hope, see `start_tts`.
    // A box still loading after the budget is *not* held back here: readiness
    // is about the binary and the weights (`configured`), and the worker's own
    // `ensure` now waits for a loading server instead of racing it. Making
    // `configured` depend on a live sidecar was considered and rejected: it
    // would deny a registered box over a sidecar restart and drag
    // `may_install` into re-running package installs on a healthy cluster.
    match ssh.start_tts(&layout.engine) {
        Ok(v) if v.starts_with("TTS-STARTING") => log.push(format!(
            "[{}] {v}, the worker will wait for it rather than start a second one; re-run the probe if renders are slow to begin",
            m.id
        )),
        Ok(v) => log.push(format!("[{}] tts: {v}", m.id)),
        Err(e) => log.push(format!("[{}] could not start tts: {e}", m.id)),
    }

    // Write provision stamp so subsequent runs can skip
    let _ = ssh.write_provision_stamp(&local_stamp);

    // Re-probe so the caller records the post-provision truth.
    let after = ssh.probe(&layout.engine);
    log.push(format!("[{}] after provision: {}", m.id, after.summary()));
    // Named on its own line, not just inside the summary: a merge offered to
    // this box fails after a full render lease, and the operator's next stop is
    // this log. The fix is one package manager away on every platform.
    if !after.ffmpeg_present {
        log.push(format!(
            "[{}] ffmpeg is not on PATH, this box can crawl/digest/render but every merge it is offered will fail; install it (apt install ffmpeg / dnf install ffmpeg) and force a re-provision",
            m.id
        ));
    }
    // Same warning for the second engine, and here the capability gate means
    // the box simply never gets a merge offered — the line exists so the
    // operator knows *why* this box sits out the merge lane.
    if !after.sox_present {
        log.push(format!(
            "[{}] sox is not on PATH, this box can crawl/digest/render but advertises no merge capability; install it (apt install sox / dnf install sox) and force a re-provision",
            m.id
        ));
    }
    // Single-source-of-truth check, against the fresh probe: a store holding
    // voices nothing declares desyncs the cluster silently (renders pass here,
    // 500 everywhere else). Warn with the fix; never delete.
    {
        let engine = crate::config::Settings::load(&layout.settings()).engine;
        let manifest: std::collections::HashMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(layout.voices_manifest()).unwrap_or_default(),
        )
        .unwrap_or_default();
        let pool = crate::pool::load_pool(&layout.voice_pool());
        let catalogue: Vec<String> = crate::voices::offline_voices(&engine)
            .iter()
            .map(|v| v.name.clone())
            .collect();
        let strays = undeclared_voices(&after.voices, &manifest, &pool, &catalogue);
        if !strays.is_empty() {
            log.push(format!(
                "[{}] voices in store but declared nowhere (not a preset, not in voices.json, not pooled): {}, add each with its refs/ clip to voices.json and provision again, or drop it from the store; remote renders 500 until then",
                m.id,
                strays.join(", ")
            ));
        }
    }
    (after, log.lines)
}



