use super::*;
/// A planned turn: the wav the renderer produced, and what the layers need to
/// know about it.
///
/// `scene` is the place (effect + reverb) and `music` is the mood (which track
/// plays). Two fields, two jobs, see this module's docs.
#[derive(Debug, Clone)]
pub struct Turn {
    pub wav: PathBuf,
    pub scene: String,
    /// A palette value, or `""` for none. Resolved by the caller from the
    /// script's own `music` field, or from the legacy shim.
    pub music: String,
    pub speaker: String,
    /// Script-placed spot effects, anchored at this turn's end.
    pub injects: Vec<Inject>,
}

/// One position in the mix: what plays, when it starts and ends, and how much
/// air follows it.
///
/// The timeline is built once and read twice, [`crate::assemble::concat_slots`]
/// writes these gaps and the layers read these offsets, which is the whole
/// reason it exists. The gap arithmetic used to live in two places (the concat
/// and the span builder) and stayed correct only because the gap was a
/// constant; a variable-length pause would have let them drift apart silently,
/// and the layers would have slid onto the wrong turns.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub wav: PathBuf,
    pub scene: String,
    pub music: String,
    pub speaker: String,
    /// Script-placed spot effects, anchored at this slot's end.
    pub injects: Vec<Inject>,
    pub start: f64,
    pub end: f64,
    /// Silence written after this turn: the uniform gap plus any beat.
    pub gap_ms: u32,
    /// How much of `gap_ms` is a planned beat, held between this turn and the
    /// next. The layers need the beat on its own, it is where the music lifts.
    pub pause_ms: u32,
    /// How much of `gap_ms` is the injects' own solo time, in pre-tempo
    /// milliseconds, the room a hit or a trail's hold plays in.
    ///
    /// Tracked separately because it is the one part of a gap that outlives the
    /// last slot: an inject anchored at the final line's end has nowhere else
    /// to sound, so the concat writes this much silence after it even though it
    /// writes no bare gap there. Without that the hit is placed past the end of
    /// the mix and dropped in silence, with the plan log still claiming it ran.
    pub inject_ms: u32,
}

/// Lay the turns out on the mix clock, inserting the planned pauses.
///
/// `pauses` maps a turn index to the beat held *before* that turn, in pre-tempo
/// milliseconds, the scene map authors it as `pause_before_s` on the scene
/// being entered. The mix has no place to hold a beat before the first thing in
/// the chapter, so a beat is written into the gap that *follows* turn `i-1`:
/// the same silence, named from the other side. [`Slot::pause_ms`] therefore
/// reads as "the beat after this turn", and [`pause_intervals`] can hand the
/// music layer an interval without knowing which side it was authored from.
pub fn timeline(turns: &[Turn], gap_ms: u32, pauses: &BTreeMap<usize, u32>) -> Result<Vec<Slot>> {
    let mut out: Vec<Slot> = Vec::with_capacity(turns.len());
    let mut params: Option<(u16, u32, u16)> = None;
    let mut t = 0.0f64;
    for (i, turn) in turns.iter().enumerate() {
        // Header probe, not a full read: only the params and the duration are
        // needed here, and the samples are read again by the concat anyway.
        let info = wav_info(&turn.wav)?;
        let p = (info.channels, info.sample_rate, info.bits);
        match params {
            None => params = Some(p),
            Some(prev) if prev != p => anyhow::bail!(
                "{}: {p:?} != {prev:?} (mixed engines/rates — use per-engine seg dirs)",
                turn.wav.display()
            ),
            _ => {}
        }
        let dur = info.seconds();
        let pause_ms = pauses.get(&(i + 1)).copied().unwrap_or(0);
        out.push(Slot {
            wav: turn.wav.clone(),
            scene: turn.scene.clone(),
            music: turn.music.clone(),
            speaker: turn.speaker.clone(),
            injects: turn.injects.clone(),
            start: t,
            end: t + dur,
            gap_ms: gap_ms + pause_ms,
            pause_ms,
            inject_ms: 0,
        });
        t += dur + (gap_ms + pause_ms) as f64 / 1000.0;
    }
    Ok(out)
}

/// Rescale a timeline from pre-tempo seconds to delivered seconds.
///
/// [`timeline`] lays the slots out from the raw voice wavs, so every offset is
/// on the *pre-tempo* clock. Once the speech has been through `atempo` the
/// delivered clock is `pre / speed`, and a layer placed against the pre-tempo
/// clock slides further behind the voice with every line, by the end of a
/// 7-minute chapter the music is a minute and a half out of place.
///
/// Scaling the whole timeline by one factor is exact rather than approximate:
/// `t` accumulates `duration + gap`, and `atempo` divides both by `speed`.
/// The gaps are scaled too, even though the concat has already written them,
/// because a `Slot` whose `start` is delivered seconds and whose `gap_ms` is
/// pre-tempo milliseconds is a struct lying about itself.
pub fn retime(slots: &mut [Slot], speed: f64) {
    if speed <= 0.0 || (speed - 1.0).abs() <= f64::EPSILON {
        return;
    }
    for s in slots.iter_mut() {
        s.start /= speed;
        s.end /= speed;
        s.gap_ms = (s.gap_ms as f64 / speed).round() as u32;
        s.pause_ms = (s.pause_ms as f64 / speed).round() as u32;
        s.inject_ms = (s.inject_ms as f64 / speed).round() as u32;
    }
}

/// The pause intervals of a timeline, absolute seconds: where the music lifts.
pub fn pause_intervals(slots: &[Slot]) -> Vec<(f64, f64)> {
    slots
        .iter()
        .filter(|s| s.pause_ms > 0)
        .map(|s| (s.end, s.end + s.pause_ms as f64 / 1000.0))
        .collect()
}

/// Where the chapter's headline ends, in delivered seconds, the stretch where
/// the duck lets go (see [`Duck::head_key`]).
///
/// The headline is the opening turn, and `plan_turns` builds it with neither a
/// place nor a mood: that pairing is its signature, which makes this a property
/// of the chapter rather than a number of seconds somebody has to guess at and
/// keep in step with the writing. A chapter that opens on a scene, or on a
/// line the analyzer gave a label but no mood, has no headline and no
/// exemption.
pub(crate) fn headline_end(slots: &[Slot]) -> Option<f64> {
    let first = slots.first()?;
    (first.end > 0.0 && first.scene.is_empty() && first.music.is_empty()).then_some(first.end)
}

/// Where a beat fits in this chapter, as `turn index -> pre-tempo milliseconds`.
///
/// A beat belongs where a *scene changes and the change is narrated*: the
/// incoming or the outgoing turn must be the Narrator, so the pause lands on
/// narration handing over rather than in the middle of an exchange. Narration
/// *resuming* is the stronger signal, a new scene establishing itself, so it
/// outranks narration handing off; the longest `pause_before_s` the scene map
/// declares breaks the remaining ties, and the earliest boundary breaks those.
///
/// `speed` is applied here because the pause is authored in *delivered*
/// seconds: at `atempo=1.25` a 1.5 s beat written as 1.5 s of silence would
/// arrive as 1.2 s, and every pause in the book would be quietly short by the
/// same factor.
pub fn plan_pauses(turns: &[Turn], map: &SceneMap, speed: f64) -> BTreeMap<usize, u32> {
    let mut out = BTreeMap::new();
    let cfg = &map.pause;
    if cfg.max_per_chapter == 0 || turns.len() < 2 {
        return out;
    }
    let mut cands: Vec<(u8, f64, usize)> = Vec::new();
    for i in 1..turns.len() {
        let (prev, cur) = (&turns[i - 1], &turns[i]);
        // A beat marks a *change of scene*, so both sides have to name one. An
        // untagged turn is not a scene: the chapter headline leads with none,
        // and a mid-chapter line the analyzer left blank must not manufacture a
        // boundary, that would put a beat in the middle of a continuous scene.
        if prev.scene.is_empty() || cur.scene.is_empty() || cur.scene == prev.scene {
            continue;
        }
        if cfg.require_narration && cur.speaker != "Narrator" && prev.speaker != "Narrator" {
            continue;
        }
        let rule = match_scene(&cur.scene, map);
        let secs = if rule.pause_before_s > 0.0 {
            rule.pause_before_s
        } else {
            cfg.pause_s
        };
        let rank = if cur.speaker == "Narrator" { 2u8 } else { 1 };
        cands.push((rank, secs, i));
    }
    // Best first: strongest narration signal, then longest declared beat, then
    // earliest, so the choice is a decision, not whichever rule happened to
    // come first in the file.
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.total_cmp(&a.1)).then(a.2.cmp(&b.2)));
    for (_, secs, i) in cands.into_iter().take(cfg.max_per_chapter) {
        let ms = (secs * speed * 1000.0).round();
        if ms > 0.0 {
            out.insert(i, ms as u32);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// spans: one rule, one stretch of the clock
// ---------------------------------------------------------------------------
