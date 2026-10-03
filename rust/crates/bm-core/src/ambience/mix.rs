use super::plan::FxReport;
use super::timeline::headline_end;
use super::track::build_voice_track;
use super::track::ffmpeg;
use super::track::s;
use super::track::voice_reserve;
use super::*;
/// One thing to place on the layer's own track.
#[derive(Debug, Clone)]
struct Slice {
    path: PathBuf,
    start: f64,
    /// How long the slice file actually is, for the fade-out position.
    dur: f64,
    fade_in: f64,
    fade_out: f64,
}

/// Place slices at exact offsets (`adelay` + `amix`, so no drift) with edge
/// fades against clicks, and pad the whole track out to `total`.
///
/// Slices may overlap: that is how the music layer crossfades between tracks.
/// Two complementary linear fades summing to unity is not a true equal-power
/// crossfade, but for uncorrelated beds the error is a fraction of a dB in the
/// middle of a two-second overlap, cheaper than a filter chain that would have
/// to know which slice comes next.
fn place(slices: &[Slice], out: &Path, total: f64) -> Result<()> {
    if slices.is_empty() {
        return ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=mono:d={total:.3}"),
            s(out.display()),
        ]);
    }
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    let mut filters: Vec<String> = Vec::new();
    for (n, sl) in slices.iter().enumerate() {
        args.push("-i".into());
        args.push(s(sl.path.display()));
        // Fades may not be longer than half the slice, or they would run past
        // each other and invert.
        let fi = sl.fade_in.clamp(0.0, sl.dur / 2.0);
        let fo = sl.fade_out.clamp(0.0, sl.dur / 2.0);
        let mut chain: Vec<String> = Vec::new();
        if fi > 0.0 {
            chain.push(format!("afade=t=in:st=0:d={fi:.3}"));
        }
        if fo > 0.0 {
            chain.push(format!(
                "afade=t=out:st={:.3}:d={fo:.3}",
                (sl.dur - fo).max(0.0)
            ));
        }
        let ms = (sl.start * 1000.0) as i64;
        chain.push(format!("adelay={ms}|{ms}"));
        filters.push(format!("[{n}:a]{}[s{n}]", chain.join(",")));
    }
    let labels: String = (0..slices.len()).map(|n| format!("[s{n}]")).collect();
    filters.push(format!(
        "{labels}amix=inputs={}:normalize=0,apad=whole_dur={total:.3},\
         aformat=sample_rates=48000:channel_layouts=mono[out]",
        slices.len()
    ));
    args.push("-filter_complex".into());
    args.push(filters.join(";"));
    args.push("-map".into());
    args.push("[out]".into());
    args.push("-t".into());
    args.push(format!("{total:.3}"));
    args.push(s(out.display()));
    ffmpeg(&args)
}

/// The music layer's gain over time, as an ffmpeg expression.
///
/// A flat level, plus one lift per pause built from two complementary `clip`
/// ramps. The alternative, slicing the track at each level change and
/// concatenating, restarts the loop at every boundary, which is audible; and
/// `t` is monotonic across `-stream_loop` boundaries, so an expression keyed on
/// it is safe on a looped source.
pub(crate) fn level_expr(
    base: f64,
    lift: f64,
    ramp: f64,
    run_start: f64,
    pauses: &[(f64, f64)],
) -> String {
    let mut e = format!("{base:.6}");
    let d = lift - base;
    if d.abs() < 1e-9 || ramp <= 0.0 {
        return e;
    }
    for (a, b) in pauses {
        let ra = (a - run_start).max(0.0);
        let rb = (b - run_start).max(0.0);
        e.push_str(&format!(
            " + {d:.6}*clip((t-{ra:.3})/{ramp:.3},0,1) - {d:.6}*clip((t-{rb:.3})/{ramp:.3},0,1)"
        ));
    }
    e
}

/// Resolve a clip path. Pool files are `assets/`-relative, which is the same
/// directory the scene map came from, one root, so a pool and its clips cannot
/// be read from different places.
pub(crate) fn clip_path(assets: &Path, file: &str) -> PathBuf {
    assets.join(file)
}

/// Effective start offset per music run, so adjacent tracks crossfade.
///
/// The planner covers only speech (`run.end` is the slot's end), while the
/// renderer extends each run by `xfade_s`, leaving the two fades misaligned
/// by the inter-slot gap and dipping the mix mid-transition. Starting the
/// next track where the previous one's audible coverage ended aligns them.
/// Gaps wider than the crossfade plus a beat are intentional silence (`none`,
/// unpooled moods) and keep their hole.
///
/// The first run is passed through: [`plan_music`] already moved it to the
/// head of the timeline, so the chapter opens on it rather than on the first
/// line that names a mood.
pub(crate) fn music_starts(runs: &[MusicRun], xfade: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(runs.len());
    for (n, run) in runs.iter().enumerate() {
        let start = if n == 0 {
            run.start
        } else {
            let gap = run.start - runs[n - 1].end;
            if gap >= 0.0 && gap <= xfade.max(0.0) + 1.0 {
                runs[n - 1].end
            } else {
                run.start
            }
        };
        out.push(start);
    }
    out
}

/// The two edge fades of one music run: `(fade_in, fade_out)`.
///
/// The chapter's first slice rises and its last falls over `fade_s` (3 s). An
/// opening and a closing are *heard*, and a third of a second of either reads
/// as a cut, the one thing the layer's first and last moments must not read
/// as. Everywhere the track changes mid-chapter the edge is the short
/// `xfade_s` instead, because that seam is covered by the next track arriving.
pub(crate) fn music_fades(n: usize, last: bool, cfg: &MusicLayer) -> (f64, f64) {
    (
        if n == 0 { cfg.fade_s } else { cfg.xfade_s },
        if last { cfg.fade_s } else { cfg.xfade_s },
    )
}

// ---------------------------------------------------------------------------
// the pass
// ---------------------------------------------------------------------------

/// Mix the two sound-design layers under the voice track.
///
/// `slots` is the timeline the concat already wrote, so the layer offsets and
/// the voice's own gaps agree by construction. `chapter` seeds the pool picks,
/// which is what makes a re-merge reproduce the same audio.
///
/// `work` is a directory the caller owns, used for the per-span slices this
/// pass needs. It is created on demand and never cleaned up here, so pass a
/// throwaway path, the merge passes its per-chapter scratch directory.
/// The voice treatment for one slot, and whether the slot is the Narrator —
/// who takes the same room at a tenth of its depth, never as a full wet.
pub(crate) fn slot_effect<'a>(
    slot: &Slot,
    spans: &[Span],
    presets: &'a BTreeMap<String, VoiceFx>,
) -> Option<(&'a VoiceFx, bool)> {
    let span = spans
        .iter()
        .find(|s| slot.start >= s.start && slot.start < s.end)?;
    let fx = span.reverb.as_ref().and_then(|r| presets.get(r))?;
    Some((fx, slot.speaker == "Narrator"))
}

#[allow(clippy::too_many_arguments)]
pub fn apply_layers(
    voice_wav: &Path,
    slots: &[Slot],
    chapter: u32,
    on: LayerSwitch,
    out: &Path,
    work: &Path,
    assets: &Path,
    inj_durs: &BTreeMap<String, f64>,
) -> Result<PathBuf> {
    let mut cfg = load_map(&assets.join("scene-map.json"))?;
    cfg.layers.effect.trim *= on.effect_volume.max(0.0);
    cfg.layers.music.level *= on.music_volume.max(0.0);
    cfg.layers.music.pause_level *= on.music_volume.max(0.0);
    cfg.layers.inject.level *= on.inject_volume.max(0.0);
    let effect_pool = audio_pool::load_pool(&assets.join("effect-pool.json"));
    let music_pool = audio_pool::load_pool(&assets.join("music-pool.json"));
    let spans = build_spans(slots, &cfg);
    let pauses = pause_intervals(slots);

    // 1. the voice track: one treatment per slot, a short edge fade on every
    //    line, and a reserved decay so a reverb rings out. Not a layer: it is
    //    applied to the voice itself, before anything is mixed under it. It
    //    rides the effect switch because it is that layer's scene treatment —
    //    effects off is a plain read, not a plain read in a cave.
    let work = work.join("layers");
    std::fs::create_dir_all(&work)?;
    // Header probe: the mix WAV is the biggest file in the merge, and the
    // layers need only its length.
    let voice_total = wav_seconds(voice_wav)?;
    // Room for the longest decay any slot asks for, so a tail at the end of the
    // chapter rings out instead of being cut by the mix edge. Only when the
    // treatment runs: effects off is a plain read, not a read plus a silence.
    let reserve = if on.effects {
        voice_reserve(&spans, &cfg.reverb_presets)
    } else {
        0.0
    };
    let total = voice_total + reserve;
    let voice_fx = if on.effects {
        build_voice_track(voice_wav, slots, &spans, &cfg.reverb_presets, total, &work)?
    } else {
        voice_wav.to_path_buf()
    };

    // 2. the effect layer: gated windows, sparse on purpose.
    let windows = if on.effects {
        plan_windows(&spans, &cfg.layers.effect, total)
    } else {
        Vec::new()
    };
    let mut missing: Vec<String> = Vec::new();
    let mut fx_slices: Vec<Slice> = Vec::new();
    let mut fx_log: Vec<FxReport> = Vec::new();
    for (n, w) in windows.iter().enumerate() {
        let seed = audio_pool::seed(chapter, n, &w.tags);
        let Some(clip) = audio_pool::pick(&effect_pool, &w.tags, seed) else {
            continue;
        };
        let src = clip_path(assets, &clip.file);
        if !src.is_file() {
            if !missing.contains(&clip.file) {
                missing.push(clip.file.clone());
            }
            continue;
        }
        let dur = w.end - w.start;
        // Three rungs, multiplied: the rule's balance against other scenes, the
        // layer's own trim, and this sound's trim. A sound with no `level` is
        // 1.0, so a registry that predates the field mixes byte for byte as it
        // did, which `the_shipped_registries_are_all_at_unity_today` keeps
        // honest.
        let vol = w.level * clip.level;
        let p = work.join(format!("fx{n}.wav"));
        let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
        if clip.looped {
            args.push("-stream_loop".into());
            args.push("-1".into());
        }
        args.push("-i".into());
        args.push(s(src.display()));
        args.push("-t".into());
        args.push(format!("{dur:.3}"));
        // A one-shot is faded at *its own* end, whatever that is: `areverse`
        // twice puts the fade on the tail of an unprobed clip. A loop just
        // takes the level; `place` fades its window edges.
        let af = if clip.looped {
            format!(
                "volume={:.4},aformat=sample_rates=48000:channel_layouts=mono",
                vol
            )
        } else {
            format!(
                "volume={:.4},areverse,afade=t=in:st=0:d=0.4,areverse,\
                 aformat=sample_rates=48000:channel_layouts=mono",
                vol
            )
        };
        args.push("-af".into());
        args.push(af);
        args.push(s(p.display()));
        ffmpeg(&args)?;
        // A window that reaches the end of the chapter is the last thing this
        // layer says, so a looped bed closes over `end_fade_s` instead of being
        // cut on the mix's edge. Everywhere else the edge is the short
        // `fade_s`: there is speech after it, and the next scene's bed may
        // follow.
        let closing = w.end >= total - 0.05;
        fx_slices.push(Slice {
            path: p,
            start: w.start,
            dur,
            fade_in: cfg.layers.effect.fade_s,
            fade_out: match (clip.looped, closing) {
                (true, true) => cfg.layers.effect.end_fade_s,
                (true, false) => cfg.layers.effect.fade_s,
                (false, _) => 0.0,
            },
        });
        fx_log.push(FxReport {
            span: w.span,
            start: w.start,
            end: w.end,
            // The sound's name, not the take's: `day`, never `day-2`. The log
            // is read by a human asking which sound answered.
            name: clip.sound.clone(),
            // What was actually applied, not the rule's share of it: the report
            // is read against the audio that came out.
            level: vol,
            one_shot: !clip.looped,
        });
    }
    for f in &missing {
        eprintln!("effect: pooled clip missing ({f}) -> no effect for its windows");
    }
    let effect_mix = if fx_slices.is_empty() {
        None
    } else {
        let p = work.join("effect.wav");
        place(&fx_slices, &p, total)?;
        Some(p)
    };

    // 3. the music layer: continuous, quiet, lifting inside a pause.
    let runs = if on.music {
        plan_music(slots, &pauses, chapter, &music_pool, &cfg.music_palette)
    } else {
        Vec::new()
    };
    let mut mu_slices: Vec<Slice> = Vec::new();
    let starts = music_starts(&runs, cfg.layers.music.xfade_s);
    // How long each take is, so a run longer than its track can be rendered as
    // a crossfaded loop rather than a butt-jointed repeat. Probed once for the
    // chapter, not once per run: the same track usually runs twice, and a
    // second ffprobe is a second answer to a question already asked.
    let mu_durs = probe_durs(
        &runs.iter().map(|r| r.file.clone()).collect::<Vec<String>>(),
        assets,
        "music",
    );
    for (n, run) in runs.iter().enumerate() {
        // The take was chosen once, in `plan_music`, and travels on the run:
        // re-picking here would be a second answer to a question already
        // answered, and the two could disagree.
        let src = clip_path(assets, &run.file);
        if !src.is_file() {
            eprintln!(
                "music: pooled clip missing ({}) -> that run is silent",
                run.file
            );
            continue;
        }
        let last = n + 1 == runs.len();
        // Every run but the last carries a tail long enough to overlap the next
        // one's fade-in, so a track change is a crossfade and not a hole.
        // `starts` bridges small inter-slot gaps so the two fades align: the
        // next track begins where the previous one's audible coverage ended.
        let start = starts[n];
        let tail = if last { 0.0 } else { cfg.layers.music.xfade_s };
        let dur = (run.end - run.start) + tail + (run.start - start);
        let p = work.join(format!("mu{n}.wav"));
        let expr = level_expr(
            // Both levels carry the run's own trim, so a track that sits quiet
            // still lifts inside a pause by the same ratio as every other.
            cfg.layers.music.level * run.level,
            cfg.layers.music.pause_level * run.level,
            cfg.layers.music.ramp_s,
            start,
            &run.pauses,
        );
        // A run longer than its track is the common case — a 2-minute bed under
        // a 20-minute chapter — and the seam is the whole point of the choice.
        // `-stream_loop -1` butt-joins the tail onto the head, and every clip
        // entering a pool is trimmed with a short fade at each end (see
        // `tools/normalize-audio.sh`), so that seam is a small hole once every
        // couple of minutes for the length of the chapter. `loop_filter` is the
        // same crossfade the inject layer already uses for its looped beds, and
        // the gain expression rides on it rather than replacing it, so a track
        // that loops still lifts inside a pause.
        //
        // Below one clip length this is `-stream_loop` exactly as before, so a
        // run that fits never moves.
        let copies = loop_copies(dur, mu_durs.get(&run.file).copied().unwrap_or(0.0), 2.0);
        match copies {
            Some(k) => {
                let graph =
                    loop_filter_with_tail(k, 2.0, &format!("volume=volume='{expr}':eval=frame"));
                ffmpeg(&[
                    "-y".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-i".into(),
                    s(src.display()),
                    "-filter_complex".into(),
                    graph,
                    "-map".into(),
                    "[out]".into(),
                    "-t".into(),
                    format!("{dur:.3}"),
                    s(p.display()),
                ])?;
            }
            None => {
                ffmpeg(&[
                    "-y".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-stream_loop".into(),
                    "-1".into(),
                    "-i".into(),
                    s(src.display()),
                    "-t".into(),
                    format!("{dur:.3}"),
                    "-af".into(),
                    format!(
                        "volume=volume='{expr}':eval=frame,\
                         aformat=sample_rates=48000:channel_layouts=mono"
                    ),
                    s(p.display()),
                ])?;
            }
        }
        let (fade_in, fade_out) = music_fades(n, last, &cfg.layers.music);
        mu_slices.push(Slice {
            path: p,
            start,
            dur,
            fade_in,
            fade_out,
        });
    }
    let music_mix = if mu_slices.is_empty() {
        None
    } else {
        let p = work.join("music.wav");
        place(&mu_slices, &p, total)?;
        Some(p)
    };

    // 4. the inject layer: script-placed spot effects on their own track.
    //    Planned against the same delivered clock the other layers read, so a
    //    hit lands in the silence `plan_inject_holds` wrote for it and a tail
    //    runs under the speech that follows. Takes are re-picked here rather
    //    than threaded through: the pick is a pure function of the chapter,
    //    the slot and the sound, so this is the same answer, not a second one.
    let events: Vec<InjectEvent> = if on.effects {
        let pool = audio_pool::load_pool(&assets.join("inject-pool.json"));
        let takes = plan_inject_takes(slots, &pool, chapter);
        plan_injects(slots, &takes, inj_durs, &cfg.layers.inject)
    } else {
        Vec::new()
    };
    let inject_mix: Option<PathBuf> = match events.is_empty() {
        true => None,
        false => {
            let mut ij_slices: Vec<Slice> = Vec::new();
            for (n, e) in events.iter().enumerate() {
                let dur = e.end - e.start;
                if dur <= 0.01 {
                    continue;
                }
                let src = clip_path(assets, &e.file);
                if !src.is_file() {
                    eprintln!("inject: pooled clip missing ({}) -> skipped", e.file);
                    continue;
                }
                let p = work.join(format!("ij{n}.wav"));
                let vol = e.level * cfg.layers.inject.level;
                let clip_s = inj_durs.get(&e.file).copied().unwrap_or(0.0);
                // A bed the pool marks `looped` fills its window by repeating,
                // with a short crossfade at each seam. The window is only longer
                // than the clip when a `stop` ended it, with no stop there is
                // nothing to fill and `loop_copies` returns None, so the sound
                // plays once exactly as it did before.
                let copies = if e.looped {
                    loop_copies(dur, clip_s, cfg.layers.inject.loop_xfade_s)
                } else {
                    None
                };
                match copies {
                    Some(k) => {
                        let graph = loop_filter(k, cfg.layers.inject.loop_xfade_s, vol);
                        ffmpeg(&[
                            "-y".into(),
                            "-loglevel".into(),
                            "error".into(),
                            "-i".into(),
                            s(src.display()),
                            "-filter_complex".into(),
                            graph,
                            "-map".into(),
                            "[out]".into(),
                            "-t".into(),
                            format!("{dur:.3}"),
                            s(p.display()),
                        ])?;
                    }
                    None => {
                        ffmpeg(&[
                            "-y".into(),
                            "-loglevel".into(),
                            "error".into(),
                            "-i".into(),
                            s(src.display()),
                            "-t".into(),
                            format!("{dur:.3}"),
                            "-af".into(),
                            format!(
                                "volume={vol:.4},aformat=sample_rates=48000:channel_layouts=mono"
                            ),
                            s(p.display()),
                        ])?;
                    }
                }
                ij_slices.push(Slice {
                    path: p,
                    start: e.start,
                    dur,
                    fade_in: e.fade_in,
                    fade_out: e.fade_out,
                });
            }
            if ij_slices.is_empty() {
                None
            } else {
                let p = work.join("inject.wav");
                // The track is at least as long as what it carries, never
                // shorter: an event past the voice's end would otherwise be
                // delayed off the end of its own track and vanish, with the
                // plan log still naming it. The concat writes the silence this
                // normally lands in; this is the net under that.
                let end = ij_slices
                    .iter()
                    .map(|s| s.start + s.dur)
                    .fold(total, f64::max);
                place(&ij_slices, &p, end)?;
                Some(p)
            }
        }
    };

    // 5. one duck for the beds, keyed on the voice, and the inject layer
    //    mixed in *after* it.
    //
    //    The inject registry's own contract is foreground: `-20 LUFS / -3 dBTP`,
    //    "voice territory, not the -26 bed contract". It was riding the beds'
    //    ducked bus anyway, and the duck keys on the voice while a spot effect
    //    fires at the instant the voice stops, so the compressor was at full
    //    reduction with a 400 ms release exactly when the sound began. Measured
    //    on ch9: a `cooking` bed the script asked for played at **-34.8 dB**,
    //    15 dB under the speech, and was inaudible. A bed has to get out of the
    //    way of the voice; a spot effect is the thing the voice is getting out
    //    of the way *for*, and `inject_volume` is the knob for balancing it.
    let beds: Vec<&PathBuf> = [effect_mix.as_ref(), music_mix.as_ref()]
        .into_iter()
        .flatten()
        .collect();
    if beds.is_empty() && inject_mix.is_none() {
        eprintln!("sound design: no layer produced anything, skipped");
        log_plan(&spans, &pauses, &fx_log, &runs, &[], &cfg);
        // The voice track IS the mix when nothing plays under it. Promote it to
        // the caller's path before the scratch dir that holds it is removed:
        // returning the path inside `work` handed back a file this line deletes.
        if voice_fx.as_path() != out {
            std::fs::copy(&voice_fx, out)?;
        }
        cleanup(&work);
        return Ok(out.to_path_buf());
    }
    let duck = &cfg.duck;
    let sc = format!(
        "sidechaincompress=threshold={}:ratio={}:attack={}:release={}",
        duck.threshold, duck.ratio, duck.attack, duck.release
    );
    // Input 0 is the voice key; 1..=beds are the beds in order; the inject
    // track, when there is one, is the last input and never enters `[under]`.
    // The headline keeps its own level: the music is under it by design, and
    // the duck is what was hiding it there.
    let headline = headline_end(slots).map(|end| (end, duck.head_key));
    let graph = layer_graph(beds.len(), inject_mix.is_some(), &sc, headline);
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    args.push("-i".into());
    args.push(s(voice_fx.display()));
    for l in beds.iter().copied().chain(inject_mix.as_ref()) {
        args.push("-i".into());
        args.push(s(l.display()));
    }
    args.push("-filter_complex".into());
    args.push(graph);
    args.push("-map".into());
    args.push("[a]".into());
    args.push(s(out.display()));
    ffmpeg(&args)?;

    log_plan(&spans, &pauses, &fx_log, &runs, &events, &cfg);
    cleanup(&work);
    Ok(out.to_path_buf())
}

/// The mix graph: the beds ducked under the voice, the inject layer on top, and
/// a true-peak limiter on the sum.
///
/// Split out and pure because it is the one part of the signal path that is
/// invisible in every artifact: a wrong bus assignment does not fail, it just
/// makes a layer quiet, so it is asserted on instead of eyeballed. `beds` is
/// how many bed tracks are inputs 1..=beds; the inject track, if present, is
/// the input right after them.
///
/// `headline` is `(end, key gain)`: the seconds at the head of the chapter
/// where the sidechain key is held down ([`Duck::head_key`]) so the beds
/// arrive with the title. The key listens to a *copy* of the voice, never the
/// voice that reaches the mix: a key that also changed what the listener hears
/// would be a level edit wearing a compressor's name.
///
/// The limiter is not decoration. Clips are normalized to a -3 dBTP ceiling
/// and the layer gains multiply on top, but nothing was enforcing the ceiling
/// on the *sum*: ch9 measured -0.11 dBFS with the inject layer off entirely.
/// `alimiter` is a lookahead limiter, so it caps the peak without a clipper's
/// distortion, and `level=0` keeps it from re-normalizing behind the
/// operator's back.
pub(crate) fn layer_graph(
    beds: usize,
    inject: bool,
    sc: &str,
    headline: Option<(f64, f64)>,
) -> String {
    let inject_in = beds + 1;
    let lim = "alimiter=limit=0.589:attack=5:release=100:level=0";
    let tail = |n: usize| format!("amix=inputs={n}:normalize=0[mixed];[mixed]{lim}[a]");
    // Nothing to duck: just sum whatever is there.
    if beds == 0 {
        return format!("[0:a][{inject_in}:a]{}", tail(2));
    }
    // The key, and the voice the mix keeps: the same stream twice, unless the
    // headline holds the key down, then the voice is split and only the copy
    // the compressor listens to is attenuated. A gain of 1.0 (or no headline)
    // is the key taken as-is, which is exactly the graph this used to emit.
    let (prologue, key, vox) = match headline {
        Some((end, gain)) if end > 0.0 && gain < 1.0 => (
            format!(
                "[0:a]asplit=2[vox][sc];\
                 [sc]volume=volume='if(lt(t,{end:.3}),{gain:.4},1)':eval=frame[key];"
            ),
            "[key]",
            "[vox]",
        ),
        _ => (String::new(), "[0:a]", "[0:a]"),
    };
    let ins: String = (1..=beds).map(|i| format!("[{i}:a]")).collect();
    let bed_bus = if beds == 1 {
        // A single bed needs no summing before the compressor.
        "[1:a]anull[under]".to_string()
    } else {
        format!("{ins}amix=inputs={beds}:normalize=0[under]")
    };
    if inject {
        format!(
            "{prologue}{bed_bus};[under]{key}{sc}[duck];\
             {vox}[duck][{inject_in}:a]{}",
            tail(3)
        )
    } else {
        format!(
            "{prologue}{bed_bus};[under]{key}{sc}[duck];{vox}[duck]{}",
            tail(2)
        )
    }
}

/// Build the plan report. The mix is otherwise invisible in the logs, and a
/// chapter that came out silent should say *why* it came out silent.
///
/// Three lists, because the three things have three different clocks. A *span*
/// is a place and carries the reverb; a *window* is an effect and may open
/// later than its span (the cooldown can push it); a *run* is a mood, and
/// `none` emits no run at all, so silence shows up as a gap between two runs'
/// ranges rather than as a line. Folding these into one span line is what hid
/// exactly the behaviours the per-window and per-slot designs exist to
/// express.
pub(crate) fn plan_lines(
    spans: &[Span],
    pauses: &[(f64, f64)],
    fx: &[FxReport],
    runs: &[MusicRun],
    inj: &[InjectEvent],
    cfg: &SceneMap,
) -> Vec<String> {
    let mut out = Vec::new();
    for (a, b) in pauses {
        out.push(format!(
            "pause [{a:.1}-{b:.1}s] {:.2}s — music lifts to {:.3}",
            b - a,
            cfg.layers.music.pause_level
        ));
    }
    for span in spans {
        out.push(format!(
            "span  [{:.0}-{:.0}s] {}{}",
            span.start,
            span.end,
            if span.scene.is_empty() {
                "?"
            } else {
                &span.scene
            },
            span.reverb
                .as_ref()
                .map(|r| format!(" | reverb: {r}"))
                .unwrap_or_default()
        ));
    }
    for f in fx {
        // Attribute by index, not by offset: the window may have been pushed
        // past its span's start, and it is still that place's sound.
        let place = spans.get(f.span).map(|s| s.scene.as_str()).unwrap_or("?");
        out.push(format!(
            "effect [{:.0}-{:.0}s] {}@{:.2}{} <- {}",
            f.start,
            f.end,
            f.name,
            f.level,
            if f.one_shot { " (one-shot)" } else { "" },
            if place.is_empty() { "?" } else { place }
        ));
    }
    if runs.is_empty() {
        out.push("music none — no cue resolved for this chapter".into());
    }
    for r in runs {
        // The level that was actually applied, the layer's, times this track's
        // own trim, so the log reads the same way the effect line above it
        // does, and a per-sound trim is visible in the one place a human looks
        // to ask what the mix did.
        out.push(format!(
            "music [{:.0}-{:.0}s] {} <- {}@{:.3}",
            r.start,
            r.end,
            r.sound,
            r.mood,
            cfg.layers.music.level * r.level
        ));
    }
    for e in inj {
        let mode = match e.mode {
            InjectMode::Hit => "hit",
            InjectMode::Overlap => "overlap",
            InjectMode::Trail => "trail",
        };
        // The take's basename, not the pool path: `blood-spatter-2`, never
        // `injects/blood-spatter-2.mp3`. The log answers "what played", and
        // the directory is not part of that answer.
        let take = e.file.rsplit('/').next().unwrap_or(&e.file);
        out.push(format!(
            "inject [{:.1}-{:.1}s] {} ({mode}, {:.1}s, take {take})",
            e.start,
            e.end,
            e.sound,
            e.end - e.start,
        ));
    }
    out
}

fn log_plan(
    spans: &[Span],
    pauses: &[(f64, f64)],
    fx: &[FxReport],
    runs: &[MusicRun],
    inj: &[InjectEvent],
    cfg: &SceneMap,
) {
    for line in plan_lines(spans, pauses, fx, runs, inj, cfg) {
        eprintln!("{line}");
    }
}

fn cleanup(work: &Path) {
    if let Ok(entries) = std::fs::read_dir(work) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
