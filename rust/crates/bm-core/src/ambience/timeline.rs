use super::*;
/// A planned turn: the wav the renderer produced, and what the layers need to
#[derive(Debug, Clone)]
pub struct Turn {
    pub wav: PathBuf,
    pub scene: String,
    /// A palette value, or `""` for none. Resolved by the caller from the
    pub music: String,
    pub speaker: String,
    /// Script-placed spot effects, anchored at this turn's end.
    pub injects: Vec<Inject>,
}

/// One position in the mix: what plays, when it starts and ends, and how much
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
    pub pause_ms: u32,
    /// How much of `gap_ms` is the injects' own solo time, in pre-tempo
    pub inject_ms: u32,
}

/// Lay the turns out on the mix clock, inserting the planned pauses.
pub fn timeline(turns: &[Turn], gap_ms: u32, pauses: &BTreeMap<usize, u32>) -> Result<Vec<Slot>> {
    let mut out: Vec<Slot> = Vec::with_capacity(turns.len());
    let mut params: Option<(u16, u32, u16)> = None;
    let mut t = 0.0f64;
    for (i, turn) in turns.iter().enumerate() {
        // Header probe, not a full read: only the params and the duration are
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
pub(crate) fn headline_end(slots: &[Slot]) -> Option<f64> {
    let first = slots.first()?;
    (first.end > 0.0 && first.scene.is_empty() && first.music.is_empty()).then_some(first.end)
}

/// Where a beat fits in this chapter, as `turn index -> pre-tempo milliseconds`.
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
