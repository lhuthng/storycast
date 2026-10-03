use super::*;
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub effect: Vec<String>,
    pub level: f64,
    pub reverb: Option<String>,
    pub scene: String,
    pub start: f64,
    pub end: f64,
}

/// Tile the timeline into spans of identical *place* treatment. Adjacent turns
/// that resolve to the same rule merge, so a bed is not restarted every line.
///
/// Deliberately not keyed on music: the effect layer's windows and the music
/// layer's cues are independent timelines, and folding a mood change into this
/// merge would let a change of track cut an effect window short. Music is read
/// per slot by [`plan_music`].
pub fn build_spans(slots: &[Slot], cfg: &SceneMap) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for slot in slots {
        let rule = match_scene(&slot.scene, cfg);
        match spans.last_mut() {
            Some(last)
                if last.effect == rule.effect
                    && last.level == rule.level
                    && last.reverb == rule.reverb =>
            {
                last.end = slot.end;
            }
            _ => spans.push(Span {
                effect: rule.effect,
                level: rule.level,
                reverb: rule.reverb,
                scene: slot.scene.clone(),
                start: slot.start,
                end: slot.end,
            }),
        }
    }
    spans
}

// ---------------------------------------------------------------------------
// the effect layer's windows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start: f64,
    pub end: f64,
    pub level: f64,
    pub tags: Vec<String>,
    /// Index into the span list, for the log line and the seeded pick.
    pub span: usize,
}

/// One effect window as the *report* needs it: the place it came from, when it
/// plays, and the clip that answered it.
///
/// The span index is the whole point. `plan_windows` opens a window at
/// `span.start.max(free_at)`, so a window can start later than the span it came
/// from, the previous window's cooldown pushes it. The report used to match
/// windows to spans by start offset, through a formatted string
/// (`l.starts_with("[110-")`), so any window the cooldown had pushed read as
/// "no effect" on its own span. Chapter 13 measured 75 s of night under
/// `courtyard-evening` that the log claimed was silent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FxReport {
    pub(crate) span: usize,
    pub(crate) start: f64,
    pub(crate) end: f64,
    pub(crate) name: String,
    pub(crate) level: f64,
    pub(crate) one_shot: bool,
}

/// Which stretches of the chapter carry an effect, and for how long.
///
/// Four gates, applied in this order: the span must name effect tags, must be
/// at least `min_span_s` long, may not start until `cooldown_s` after the
/// previous window closed, and the chapter's total may not exceed
/// `max_coverage`. The window is then clipped to `max_window_s`.
///
/// The budget is a *stop*, not a trim: once the chapter has spent its share,
/// later eligible scenes get nothing. Taking a sliver of the budget for a scene
/// that would only get a few seconds of it is worse than silence.
pub fn plan_windows(spans: &[Span], cfg: &EffectLayer, total: f64) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    let budget = total * cfg.max_coverage.max(0.0);
    let mut used = 0.0f64;
    let mut free_at = 0.0f64;
    for (i, span) in spans.iter().enumerate() {
        if span.effect.is_empty() || span.level <= 0.0 {
            continue;
        }
        if span.end - span.start < cfg.min_span_s {
            continue;
        }
        let start = span.start.max(free_at);
        if start >= span.end {
            continue;
        }
        let room = budget - used;
        if room < cfg.min_span_s {
            break;
        }
        let len = (span.end - start).min(cfg.max_window_s).min(room);
        if len < cfg.min_span_s {
            continue;
        }
        out.push(Window {
            start,
            end: start + len,
            // The rule's relative balance, scaled by the layer's one master
            // gain. Read here rather than in `build_spans` so the span merge
            // still compares raw rule levels, the trim is a property of the
            // layer, not of a scene, and folding it in earlier would make two
            // rules that differ only by trim merge as one.
            level: span.level * cfg.trim,
            tags: span.effect.clone(),
            span: i,
        });
        used += len;
        free_at = start + len + cfg.cooldown_s;
    }
    out
}

// ---------------------------------------------------------------------------
// the music layer's runs
// ---------------------------------------------------------------------------

/// A stretch of the clock with one track under it, and the pauses inside it
/// where the music lifts.
#[derive(Debug, Clone, PartialEq)]
pub struct MusicRun {
    /// The palette value that chose this track, what the log reports and what
    /// a re-merge is compared against.
    pub mood: String,
    /// The *sound* the palette resolved to (`soft-relax`). Consecutive slots
    /// that land on the same sound are one run, because a repeated mood must be
    /// continuous music rather than a crossfade into the same tune.
    pub sound: String,
    /// The take that answers it. Which of `soft-relax-bg-1/2` plays is an
    /// implementation detail of the pick, so the mix reads it from here rather
    /// than looking the sound up a second time, one lookup, one answer.
    pub file: String,
    /// The sound's own trim (`Sound::level`), carried the same way and for the
    /// same reason as `file`: the mix multiplies it into the layer level, and
    /// re-looking it up would be a second answer to a question already asked.
    pub level: f64,
    /// Audible coverage ends here; the slice may extend past it into a
    /// crossfade with the next run. The chapter's first run is the exception at
    /// the other end: it starts at the head of the timeline, under the headline
    /// (see [`plan_music`]), and the slice is extended back to meet it.
    pub start: f64,
    pub end: f64,
    pub pauses: Vec<(f64, f64)>,
}

/// Which track plays when, and where it lifts.
///
/// Read per *slot*, not per span: the mood is a property of the line being
/// spoken, and a cue breaks exactly where the mood changes. The pick is seeded
/// from the palette *value* rather than its tags, so a change of value is a
/// change of track by construction; the chapter goes into the seed too, so two
/// chapters in the same mood still differ.
///
/// A slot with no value, with `none`, or whose palette entry names tags nothing
/// in the pool answers contributes nothing. `none` is a choice; a pool that
/// has lost its last clip for a mood is a degraded mix, and both are reported
/// once each.
///
/// The chapter's *first* run is pulled back to the head of the timeline (the
/// slot the headline is spoken in) so the music comes up under the title; every
/// later cue keeps the offset its own slot gave it. The layer's own knobs
/// (`level`, `xfade_s`, `ramp_s`) are not read here: they shape how a run is
/// *rendered*, which is the caller's job.
pub fn plan_music(
    slots: &[Slot],
    pauses: &[(f64, f64)],
    chapter: u32,
    pool: &ClipPool,
    palette: &MusicPalette,
) -> Vec<MusicRun> {
    let mut out: Vec<MusicRun> = Vec::new();
    let mut unpooled: Vec<String> = Vec::new();
    for slot in slots {
        let mood = slot.music.trim();
        if mood.is_empty() || mood == "none" {
            continue;
        }
        // A value outside the palette is a script that never went through the
        // digest validator, say so rather than silently going quiet.
        let Some(entry) = palette.get(mood) else {
            let msg = format!("{mood} (not a palette value)");
            if !unpooled.contains(&msg) {
                unpooled.push(msg);
            }
            continue;
        };
        if entry.tags.is_empty() {
            continue;
        }
        let key = [mood.to_string()];
        let Some(picked) = audio_pool::pick(pool, &entry.tags, audio_pool::seed(chapter, 0, &key))
        else {
            let msg = format!("{mood} (no pooled clip for [{}])", entry.tags.join(", "));
            if !unpooled.contains(&msg) {
                unpooled.push(msg);
            }
            continue;
        };
        let inside: Vec<(f64, f64)> = pauses
            .iter()
            .copied()
            .filter(|(a, b)| *b > slot.start && *a < slot.end)
            .collect();
        match out.last_mut() {
            // Merge on the *sound*, not the take: the seed is derived from the
            // mood, so a repeated mood resolves to the same sound and the same
            // take anyway, merging on the sound is what keeps a scene change
            // inside one mood from cutting the music.
            Some(last) if last.sound == picked.sound => {
                last.end = slot.end;
                last.pauses.extend(inside);
            }
            _ => out.push(MusicRun {
                mood: mood.to_string(),
                sound: picked.sound,
                file: picked.file,
                level: picked.level,
                start: slot.start,
                end: slot.end,
                pauses: inside,
            }),
        }
    }
    for m in &unpooled {
        eprintln!("music: {m} -> no music there");
    }
    // Pulled back, never pushed forward: `min` against the head keeps a cue
    // that somehow starts before the first slot where it is.
    if let (Some(first), Some(head)) = (out.first_mut(), slots.first()) {
        first.start = first.start.min(head.start);
    }
    out
}

// ---------------------------------------------------------------------------
// injects: script-placed spot effects
// ---------------------------------------------------------------------------
