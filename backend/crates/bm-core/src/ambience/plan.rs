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

/// A stretch of the clock with one track under it, and the pauses inside it
#[derive(Debug, Clone, PartialEq)]
pub struct MusicRun {
    /// The palette value that chose this track, what the log reports and what
    pub mood: String,
    /// The *sound* the palette resolved to (`soft-relax`). Consecutive slots
    pub sound: String,
    /// The take that answers it. Which of `soft-relax-bg-1/2` plays is an
    pub file: String,
    /// The sound's own trim (`Sound::level`), carried the same way and for the
    pub level: f64,
    /// Audible coverage ends here; the slice may extend past it into a
    pub start: f64,
    pub end: f64,
    pub pauses: Vec<(f64, f64)>,
}

/// Which track plays when, and where it lifts.
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
    if let (Some(first), Some(head)) = (out.first_mut(), slots.first()) {
        first.start = first.start.min(head.start);
    }
    out
}

// ---------------------------------------------------------------------------
