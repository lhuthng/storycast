//! Stage 3/4 — segment planning, concatenation and the final mix.
//!
//! Ported from `synthesize.py`. The WAV handling is native (RIFF PCM 16-bit),
//! so the only external binary is `ffmpeg`, and only for ambience, tempo and
//! mp3 encoding — exactly as before.

mod mood;
mod plan;
mod renderplan;
mod wav;

pub use self::plan::{
    character_has_lines, drop_headline, expected_wavs, pick_exact, pick_rendered, plan_render,
    rendered_segments, segment_miss, segments_complete, title_speech, title_speech_for_script,
    Planned, RenderUnit, RenderedSegment, Run, MAX_SEGMENT_BYTES,
};
pub use self::renderplan::{
    reconcile, reconcile_with, take_file, take_key, PlanUpdate, RenderPlan, Take, PLAN_VERSION,
};
pub use self::wav::{read_wav, sample_rate_for, silent_wav, GEMINI_RATE, VIENEU_RATE};
use self::wav::{write_wav, Wav};
use crate::util::{atomic_write, head_chars};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// concatenation and the final mix
// ---------------------------------------------------------------------------

/// Concatenate WAVs, optionally inserting `gap_ms` of silence between turns.
pub fn concat_wavs(files: &[PathBuf], out: &Path, gap_ms: u32) -> Result<()> {
    let mut params: Option<(u16, u32, u16)> = None;
    let mut frames: Vec<u8> = Vec::new();
    let mut gap: Vec<u8> = Vec::new();

    for f in files {
        let w = read_wav(f)?;
        let p = (w.channels, w.sample_rate, w.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                f.display()
            ),
            _ => {}
        }
        if gap_ms > 0 && !frames.is_empty() {
            if gap.is_empty() {
                let n = (w.sample_rate * gap_ms / 1000) as usize;
                gap = vec![0u8; n * w.channels as usize * (w.bits as usize / 8)];
            }
            frames.extend_from_slice(&gap);
        }
        frames.extend_from_slice(&w.data);
    }

    let (channels, sample_rate, bits) =
        params.ok_or_else(|| anyhow::anyhow!("no segments to concatenate"))?;
    write_wav(
        out,
        &Wav {
            channels,
            sample_rate,
            bits,
            data: frames,
        },
    )?;
    Ok(())
}

/// Concatenate a timeline, honouring each slot's own gap.
///
/// [`crate::ambience::timeline`] decided these offsets, so this writes exactly
/// the silence the layers believe is there. That is the point of the split: the
/// gap used to be one number read in two places (here and the span builder) and
/// stayed correct only because it was a constant. A planned beat is not a
/// constant, and a layers pass that assumed `gap_ms` would slide onto the wrong
/// turns by the length of every pause before it.
///
/// The gap after the last slot is not written: nothing follows it, and the
/// layer pass sizes its beds from the file it is given, so a trailing silence
/// would only make the chapter longer than the timeline claims. The one
/// exception is `Slot::inject_ms` — silence a script-placed inject needs to
/// play in, which for a hit on the final line is the difference between the
/// sound and no sound at all.
pub fn concat_slots(slots: &[crate::ambience::Slot], out: &Path) -> Result<()> {
    let mut params: Option<(u16, u32, u16)> = None;
    let mut frames: Vec<u8> = Vec::new();

    for (i, slot) in slots.iter().enumerate() {
        let w = read_wav(&slot.wav)?;
        let p = (w.channels, w.sample_rate, w.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                slot.wav.display()
            ),
            _ => {}
        }
        frames.extend_from_slice(&w.data);
        let trailing = if i + 1 < slots.len() {
            slot.gap_ms
        } else {
            slot.inject_ms
        };
        if trailing > 0 {
            let n = (w.sample_rate * trailing / 1000) as usize;
            frames.resize(
                frames.len() + n * w.channels as usize * (w.bits as usize / 8),
                0,
            );
        }
    }

    let (channels, sample_rate, bits) =
        params.ok_or_else(|| anyhow::anyhow!("no segments to concatenate"))?;
    write_wav(
        out,
        &Wav {
            channels,
            sample_rate,
            bits,
            data: frames,
        },
    )?;
    Ok(())
}

/// Whether this host can run the merge stage's encoder. Public so the worker
/// agent can advertise the `merge` capability truthfully: a box without it
/// provisions cleanly and then fails every merge it is offered.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_ffmpeg(args: &[&str]) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .context("spawning ffmpeg")?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg failed: {}",
            head_chars(&String::from_utf8_lossy(&out.stderr), 300)
        );
    }
    Ok(())
}

/// Pair every wav the renderer produced with the scene, mood and speaker the
/// layers need.
///
/// Built from the same grouping [`expected_wavs`] used, in the same order, so
/// turn `i` is wav `i` by construction rather than by convention — a mismatch
/// here would silently slide the whole chapter's sound design onto the wrong
/// lines, so the count is checked instead of assumed.
///
/// The mood is resolved here rather than in the mixer because the grain differs
/// per engine: the local engine renders one wav per *run*, so a run's mood is
/// its majority value, while the cloud engine renders one wav per segment. Doing
/// it here keeps `Turn` a flat statement of fact for the layer pass.
fn plan_turns(
    planned: &Planned,
    wavs: &[PathBuf],
    local: bool,
    titled: bool,
    cfg: &crate::ambience::SceneMap,
    pool: &crate::audio_pool::ClipPool,
) -> Result<Vec<crate::ambience::Turn>> {
    use crate::ambience::Turn;
    let segments = &planned.speech;
    let mut out: Vec<Turn> = Vec::new();

    if titled {
        // The headline opens the chapter before any scene is established: the
        // Narrator speaks it, and it carries no scene tag — so no reverb, no
        // bed, no music, and a scene change at the first real turn.
        out.push(Turn {
            wav: wavs[0].clone(),
            scene: String::new(),
            music: String::new(),
            speaker: "Narrator".into(),
            injects: Vec::new(),
        });
    }

    if local {
        let rs = planned.runs();
        let scenes = crate::ambience::run_scenes(segments, &rs);
        let musics = crate::ambience::run_music(segments, &rs, cfg);
        for (i, run) in rs.iter().enumerate() {
            out.push(Turn {
                wav: wavs[i + titled as usize].clone(),
                scene: scenes[i].clone(),
                music: musics[i].clone(),
                speaker: run.speaker.clone(),
                // Every directive that fires at a piece of this run, in order.
                // `runs` splits at a piece carrying any, so in practice only
                // the last one has them — but the turn does not assume that,
                // it just anchors everything at the run's end.
                injects: run
                    .idx
                    .iter()
                    .flat_map(|j| {
                        crate::ambience::injects_of(
                            &planned.fires[*j],
                            pool,
                            cfg.layers.inject.default_hold_s,
                        )
                    })
                    .collect(),
            });
        }
    } else {
        for (i, s) in segments.iter().enumerate() {
            let scene = s
                .get("scene")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(Turn {
                wav: wavs[i + titled as usize].clone(),
                music: crate::ambience::resolve_music(
                    &scene,
                    s.get("music").and_then(|v| v.as_str()),
                    cfg,
                ),
                scene,
                speaker: s
                    .get("speaker")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                injects: crate::ambience::injects_of(
                    &planned.fires[i],
                    pool,
                    cfg.layers.inject.default_hold_s,
                ),
            });
        }
    }

    if out.len() != wavs.len() {
        anyhow::bail!(
            "timeline has {} turns for {} rendered segments — the render plan and \
             the timeline disagree, so the layers would land on the wrong lines",
            out.len(),
            wavs.len()
        );
    }
    Ok(out)
}

/// Assemble cached segments into the chapter deliverable.
///
/// `scratch` is a directory the caller owns and may delete afterwards. Every
/// intermediate — the concat, the layer pass, the tempo pass — is written
/// there, and only the returned file is meant to survive. Pass
/// `Layout::scratch_ch(n)` so intermediates never land in `output/`.
///
/// Returns the mp3 when ffmpeg is available, otherwise the wav.
///
/// `takes` is the **recorded render plan's** file list, in mix order, when the
/// caller has one (`TaskOffer::merge_takes`). The mixer must read the same
/// names the renderer wrote, and a take's name is content-addressed — derived
/// from the voice, the text and the parameters it was spoken with — so
/// re-deriving it here from the script and the cast is exactly the five-namers
/// problem this pipeline removed. `None` is the pre-plan caller: the names are
/// then computed with [`expected_wavs`], which is what the renderer used
/// before takes were content-addressed.
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    seg_dir: &Path,
    scratch: &Path,
    chapter: u32,
    gap_ms: u32,
    on: crate::ambience::LayerSwitch,
    speed: f64,
    engine: &str,
    assets: &Path,
    takes: Option<&[String]>,
) -> Result<PathBuf> {
    let policy = crate::cast::policy_for_bible(engine, bible_path)?;
    let cast = crate::cast::load_cast(script_path, cast_path, bible_path, &policy, false)?;
    let text = std::fs::read_to_string(script_path)
        .with_context(|| format!("reading {}", script_path.display()))?;
    let data: Value = serde_json::from_str(&text)?;
    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let local = engine == "vieneu";
    let title = title_speech_for_script(script_path, &cast, &segments);
    // The script as the pipeline plans it: headline dropped, and the sound
    // items lifted out of the lines they sit between. Built once and handed to
    // every stage that names a wav — a stage that planned its own would name
    // other files.
    let planned = Planned::plan(&segments);

    let wavs: Vec<PathBuf> = match takes.filter(|t| !t.is_empty()) {
        Some(names) => names.iter().map(|n| seg_dir.join(n)).collect(),
        None => expected_wavs(&planned, &cast, seg_dir, local, title.as_ref())?,
    };
    let missing: Vec<String> = wavs
        .iter()
        .filter(|w| !w.metadata().map(|m| m.len() > 1000).unwrap_or(false))
        .map(|w| {
            w.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "{} segments missing in {} (e.g. {}): run the render stage first",
            missing.len(),
            seg_dir.display(),
            missing[0]
        );
    }

    std::fs::create_dir_all(scratch)
        .with_context(|| format!("creating scratch {}", scratch.display()))?;

    // The scene map is read here, not inside the layer pass, because it decides
    // two things the *timeline* needs: where the beats go, and what mood each
    // turn carries. A missing map degrades to "no sound design" rather than
    // failing a dry merge; a layered merge still fails loudly in `apply_layers`.
    let map = crate::ambience::load_map(&assets.join("scene-map.json")).unwrap_or_default();
    // The inject pool is read here rather than at the mix: a sound's `mode` is a
    // property of the clip, and the turn planner is where a directive becomes a
    // placed sound. One read, so the plan and the mix cannot disagree.
    let inj_pool = crate::audio_pool::load_pool(&assets.join("inject-pool.json"));
    let turns = plan_turns(&planned, &wavs, local, title.is_some(), &map, &inj_pool)?;
    // A beat is part of the sound design, so a dry chapter does not get one:
    // both layers off is an operator asking for a plain read of the text, and
    // silence inserted between the lines would be an edit they did not ask for.
    let pauses = if on.none() {
        std::collections::BTreeMap::new()
    } else {
        crate::ambience::plan_pauses(&turns, &map, speed)
    };
    let mut slots = crate::ambience::timeline(&turns, gap_ms, &pauses)?;

    // Inject holds are silence the script asked for, like a planned pause: a
    // hit needs its whole clip after its line, a trail its hold. Written into
    // the gaps before the concat, in pre-tempo milliseconds, so `retime`
    // below keeps them honest. Gated on the effects switch with the rest of
    // the sound design — a dry read gets no injected silence either.
    let inj_takes = crate::ambience::plan_inject_takes(&slots, &inj_pool, chapter);
    let inj_durs = crate::ambience::probe_inject_durs(&inj_takes, assets);
    if on.effects {
        crate::ambience::plan_inject_holds(&mut slots, &inj_takes, &inj_durs, speed);
    }

    let mut out_path = scratch.join("mix.wav");
    concat_slots(&slots, &out_path)?;

    // Tempo the SPEECH, then place the layers on the delivered clock.
    //
    // `atempo` used to run last, over the finished mix, so it sped the beds and
    // the music up along with the voice: a rain bed at 1.25x is a different
    // rain, and a loop stretched to fill its window no longer fits it. The
    // operator asked for faster *reading*, not a faster world.
    if (speed - 1.0).abs() > f64::EPSILON {
        let sped = scratch.join("voice-sped.wav");
        run_ffmpeg(&[
            "-y",
            "-loglevel",
            "error",
            "-i",
            &out_path.to_string_lossy(),
            "-filter:a",
            &format!("atempo={speed}"),
            &sped.to_string_lossy(),
        ])?;
        out_path = sped;
        // The layer pass reads `Slot::start`/`end`, so the timeline has to
        // become the one the listener will actually hear.
        crate::ambience::retime(&mut slots, speed);
    }

    if !on.none() {
        let amb_out = scratch.join("mix-amb.wav");
        out_path = crate::ambience::apply_layers(
            &out_path, &slots, chapter, on, &amb_out, scratch, assets, &inj_durs,
        )?;
    }

    if ffmpeg_available() {
        let mp3 = out_path.with_extension("mp3");
        run_ffmpeg(&[
            "-y",
            "-loglevel",
            "error",
            "-i",
            &out_path.to_string_lossy(),
            &mp3.to_string_lossy(),
        ])?;
        Ok(mp3)
    } else {
        Ok(out_path)
    }
}

/// Rewrite a chapter's title into the output filename, matching the legacy
/// `Ch.N - Title.mp3` convention.
pub fn publish(assembled: &Path, layout: &crate::Layout, n: u32) -> Result<PathBuf> {
    let final_path = layout.final_mp3(n);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(assembled, &final_path)
        .with_context(|| format!("publishing to {}", final_path.display()))?;
    Ok(final_path)
}

/// Record a manifest line (JSONL) so every render is auditable, as before.
pub fn manifest_append(path: &Path, record: &Value) -> Result<()> {
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    let mut existing = std::fs::read_to_string(path).unwrap_or_default();
    existing.push_str(&line);
    atomic_write(path, &existing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bm-assemble-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn concat_inserts_the_requested_gap() {
        let d = tmpdir("concat");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 0.10, 24_000).unwrap();
        silent_wav(&b, 0.10, 24_000).unwrap();
        let o = d.join("o.wav");
        concat_wavs(&[a, b], &o, 100).unwrap();
        let w = read_wav(&o).unwrap();
        // 0.10s + 0.10s gap + 0.10s
        assert_eq!(w.frames(), 24_000 * 3 / 10);
    }

    #[test]
    fn concat_refuses_mixed_rates() {
        let d = tmpdir("mixed");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 0.05, 24_000).unwrap();
        silent_wav(&b, 0.05, 48_000).unwrap();
        let err = concat_wavs(&[a, b], &d.join("o.wav"), 0).unwrap_err();
        assert!(err.to_string().contains("mixed engines"), "{err}");
    }

    /// A hit on the chapter's *last* line plays in silence the concat has to
    /// write, because no line follows it to carry that gap. Before this, the
    /// event was placed past the end of the mix and dropped, while the plan log
    /// went on naming it — the sound was reported and never heard.
    #[test]
    fn a_hit_on_the_last_line_gets_its_silence_written() {
        use crate::ambience::{plan_inject_holds, plan_inject_takes, plan_injects, InjectLayer};
        let d = tmpdir("inject-tail");
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 1.0, 24_000).unwrap();
        silent_wav(&b, 0.9, 24_000).unwrap();

        let pool: crate::audio_pool::ClipPool = serde_json::from_value(serde_json::json!({
            "blood": {"tags": ["blood"], "files": ["injects/blood-1.mp3"], "looped": false, "dur_s": 0.5},
        }))
        .unwrap();
        let durs = std::collections::BTreeMap::from([("injects/blood-1.mp3".to_string(), 0.5)]);
        let mk = |wav: &PathBuf, injects| crate::ambience::Slot {
            wav: wav.clone(),
            scene: "s".into(),
            music: String::new(),
            speaker: "A".into(),
            injects,
            start: 0.0,
            end: 0.0,
            gap_ms: 300,
            pause_ms: 0,
            inject_ms: 0,
        };
        let mut slots = vec![
            mk(&a, Vec::new()),
            mk(
                &b,
                crate::ambience::injects_of(
                    &[serde_json::json!({"sound": "blood"})],
                    &pool,
                    2.0,
                ),
            ),
        ];
        // Lay the clock out by hand: slot 0 spans 0.0-1.0, slot 1 starts after
        // its gap at 1.3 and ends at 2.2.
        slots[0].end = 1.0;
        slots[1].start = 1.3;
        slots[1].end = 2.2;

        let takes = plan_inject_takes(&slots, &pool, 1);
        plan_inject_holds(&mut slots, &takes, &durs, 1.0);
        assert_eq!(
            slots[1].inject_ms, 500,
            "the hit's 0.5 s is the last slot's"
        );

        let o = d.join("o.wav");
        concat_slots(&slots, &o).unwrap();
        let total = read_wav(&o).unwrap().seconds();
        let events = plan_injects(&slots, &takes, &durs, &InjectLayer::default());
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].end <= total + 1e-6,
            "the mix is {total:.3}s and the hit ends at {:.3}s — it would be dropped",
            events[0].end
        );
    }

    /// The injection is a split sentence, and the thing that makes it one is
    /// *where the effect lands*: between the two halves, not after the whole
    /// line. Each link has its own test — the split, the run break, the seam —
    /// and the seam between them is exactly where this feature has broken
    /// before, so it gets one of its own.
    #[test]
    fn a_split_sentence_is_two_wavs_and_the_effect_lands_between_them() {
        use crate::ambience::{plan_inject_holds, plan_inject_takes, plan_injects, InjectLayer};
        use std::collections::BTreeMap;
        let d = tmpdir("inject-cut");
        let seg_dir = d.join("segs");
        std::fs::create_dir_all(&seg_dir).unwrap();

        // One sentence, written as its two halves with the sound between them.
        let segments = vec![
            serde_json::json!({"speaker": "A", "text": "Hắn vung kiếm."}),
            serde_json::json!({"sound": "sword"}),
            serde_json::json!({"speaker": "A", "text": "Rồi máu văng ra."}),
        ];
        let planned = Planned::plan(&segments);
        assert_eq!(planned.speech.len(), 2, "the sound is not a line");
        assert!(
            !planned
                .speech
                .iter()
                .any(crate::util::is_sound_item),
            "nothing the renderer could read as syntax survives into the speech"
        );

        let mut cast = crate::cast::Cast::new();
        cast.insert("A".into(), "Đức Trí".into());
        let wavs = expected_wavs(&planned, &cast, &seg_dir, true, None).unwrap();
        assert_eq!(wavs.len(), 2, "two halves, two TTS calls, two files: {wavs:?}");
        silent_wav(&wavs[0], 1.0, 48_000).unwrap();
        silent_wav(&wavs[1], 0.8, 48_000).unwrap();

        let cfg = crate::ambience::SceneMap::default();
        let pool: crate::audio_pool::ClipPool = serde_json::from_value(serde_json::json!({
            "sword": {"tags": ["sword"], "files": ["injects/sword-1.mp3"], "looped": false, "dur_s": 0.5, "mode": "hit"},
        }))
        .unwrap();
        let turns = plan_turns(&planned, &wavs, true, false, &cfg, &pool).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].injects.len(), 1, "the effect fires at the cut");
        assert!(turns[1].injects.is_empty(), "{:?}", turns[1].injects);

        let mut slots = crate::ambience::timeline(&turns, 300, &BTreeMap::new()).unwrap();
        let durs = BTreeMap::from([("injects/sword-1.mp3".to_string(), 0.5)]);
        let takes = plan_inject_takes(&slots, &pool, 7);
        plan_inject_holds(&mut slots, &takes, &durs, 1.0);

        let o = d.join("mix.wav");
        concat_slots(&slots, &o).unwrap();
        let total = read_wav(&o).unwrap().seconds();
        let events = plan_injects(&slots, &takes, &durs, &InjectLayer::default());
        assert_eq!(events.len(), 1, "{events:?}");
        // The first half runs 0.0–1.0, so the cut is at 1.0: the effect starts
        // there, and the second half begins after the 0.5 s it holds.
        assert!((events[0].start - 1.0).abs() < 1e-6, "{:?}", events[0]);
        assert!(
            events[0].end <= total + 1e-6,
            "the mix is {total:.3}s and the effect ends at {:.3}s — it would be dropped",
            events[0].end
        );
        // 1.0 (first half) + 0.5 (hit) + 0.3 (gap) + 0.8 (second half).
        assert!((total - 2.6).abs() < 0.01, "the mix is {total:.3}s");
    }
}
