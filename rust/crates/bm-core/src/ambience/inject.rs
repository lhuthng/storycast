use super::mix::clip_path;
use super::*;
/// How one injected sound sits on the timeline. Per *directive*, not per
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMode {
    /// Inline: the narration waits out the whole clip.
    Hit,
    /// Parallel: zero timeline time, runs under the following speech until the
    Overlap,
    /// Both: `hold_s` of solo time, then the tail runs under the speech.
    Trail,
}

impl InjectMode {
    /// How much of its own level this mode plays at.
    pub const fn gain(self) -> f64 {
        match self {
            InjectMode::Hit => 1.0,
            InjectMode::Overlap | InjectMode::Trail => 0.1,
        }
    }
}

/// The mode a pool entry's string names, or `None` for a string that names
pub fn inject_mode(name: &str) -> Option<InjectMode> {
    match name {
        "hit" => Some(InjectMode::Hit),
        "overlap" => Some(InjectMode::Overlap),
        "trail" => Some(InjectMode::Trail),
        _ => None,
    }
}

/// One inject directive: start a sound at the place it was cut into, or fade
#[derive(Debug, Clone, PartialEq)]
pub enum Inject {
    Start {
        sound: String,
        mode: InjectMode,
        hold_s: f64,
        level: f64,
    },
    Stop {
        sound: String,
    },
}

/// The directives that fire at one place on the timeline, in listed order.
pub fn injects_of(directives: &[Value], pool: &ClipPool, default_hold: f64) -> Vec<Inject> {
    let mut out = Vec::new();
    for e in directives {
        let Some(obj) = e.as_object() else { continue };
        if let Some(stop) = obj.get("stop").and_then(|v| v.as_str()).map(str::trim) {
            if !stop.is_empty() {
                out.push(Inject::Stop {
                    sound: stop.to_string(),
                });
            }
            continue;
        }
        let Some(sound) = obj.get("sound").and_then(|v| v.as_str()).map(str::trim) else {
            continue;
        };
        if sound.is_empty() {
            continue;
        }
        // An unknown sound is not the mixer's problem to guess at: it has no
        let Some(entry) = pool.get(sound) else {
            continue;
        };
        let Some(mode) = inject_mode(entry.mode.as_deref().unwrap_or("hit")) else {
            continue;
        };
        let hold_s = entry.hold.filter(|h| *h > 0.0).unwrap_or(default_hold);
        // The pool's trim, then the mode's. A `hit` is an event and keeps its
        let level = entry.level.filter(|l| *l > 0.0).unwrap_or(1.0) * mode.gain();
        out.push(Inject::Start {
            sound: sound.to_string(),
            mode,
            hold_s,
            level,
        });
    }
    out
}

/// The inject registry rendered for the digest prompt:
pub fn inject_prompt(pool: &ClipPool) -> String {
    pool.iter()
        .map(|(name, s)| {
            let mut inner = s.mode.clone().unwrap_or_else(|| "hit".into());
            if let Some(h) = s.hold.filter(|h| *h > 0.0) {
                inner.push_str(&format!(" {}s", trim_num(h)));
            }
            // `loop` is the one piece of behaviour the analyzer has to *act* on
            if s.looped {
                inner.push_str(", loop");
            }
            inner.push_str("; ");
            inner.push_str(&s.tags.join(", "));
            if let Some(d) = s.dur_s {
                inner.push_str(&format!("; {}s", trim_num(d)));
            }
            format!("{name} ({inner})")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// `0.6` and `51.3` read better than `0.600000` and `51.300000` in a prompt.
fn trim_num(v: f64) -> String {
    if v >= 10.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Which take of an injected sound plays at one slot.
pub fn inject_take(
    pool: &ClipPool,
    chapter: u32,
    slot: usize,
    sound: &str,
) -> Option<audio_pool::Picked> {
    let entry = pool.get(sound)?;
    if entry.files.is_empty() {
        return None;
    }
    let seed = audio_pool::seed(chapter, slot, &[sound.to_string()]);
    let file = entry.files[(seed % entry.files.len() as u64) as usize].clone();
    Some(audio_pool::Picked {
        sound: sound.to_string(),
        file,
        looped: entry.looped,
        level: audio_pool::sound_level(entry),
    })
}

/// The take each slot's directives resolve to, slot for slot. Computed once
pub fn plan_inject_takes(
    slots: &[Slot],
    pool: &ClipPool,
    chapter: u32,
) -> Vec<Vec<Option<audio_pool::Picked>>> {
    let mut warned: Vec<String> = Vec::new();
    slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.injects
                .iter()
                .map(|inj| match inj {
                    Inject::Stop { .. } => None,
                    Inject::Start { sound, .. } => {
                        let take = inject_take(pool, chapter, i, sound);
                        if take.is_none() && !warned.contains(sound) {
                            warned.push(sound.clone());
                            eprintln!("inject: sound {sound:?} not in the pool -> skipped");
                        }
                        take
                    }
                })
                .collect()
        })
        .collect()
}

/// Durations of the takes one chapter's injects picked, by pool path. One
pub fn probe_inject_durs(
    takes: &[Vec<Option<audio_pool::Picked>>],
    assets: &Path,
) -> BTreeMap<String, f64> {
    let mut files: Vec<String> = Vec::new();
    for slot in takes {
        for take in slot.iter().flatten() {
            if !files.contains(&take.file) {
                files.push(take.file.clone());
            }
        }
    }
    probe_durs(&files, assets, "inject")
}

/// Duration in seconds of each named pool file, keyed by the same `assets/`-
pub fn probe_durs(files: &[String], assets: &Path, what: &str) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let mut missing: Vec<String> = Vec::new();
    for file in files {
        if out.contains_key(file) {
            continue;
        }
        let p = clip_path(assets, file);
        let dur = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                &p.to_string_lossy(),
            ])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<f64>()
                    .ok()
            })
            .filter(|d| *d > 0.0);
        match dur {
            Some(d) => {
                out.insert(file.clone(), d);
            }
            None => {
                if !missing.contains(file) {
                    missing.push(file.clone());
                    eprintln!("{what}: cannot probe {file} -> treated as a single pass");
                }
            }
        }
    }
    out
}

/// Solo time each slot's injects need, written into the gap that follows the
pub fn plan_inject_holds(
    slots: &mut [Slot],
    takes: &[Vec<Option<audio_pool::Picked>>],
    durs: &BTreeMap<String, f64>,
    speed: f64,
) {
    for (slot, slot_takes) in slots.iter_mut().zip(takes.iter()) {
        let mut solo = 0.0f64;
        for (inj, take) in slot.injects.iter().zip(slot_takes.iter()) {
            let (mode, hold_s) = match inj {
                Inject::Start { mode, hold_s, .. } => (*mode, *hold_s),
                Inject::Stop { .. } => continue,
            };
            let dur = take
                .as_ref()
                .and_then(|t| durs.get(&t.file))
                .copied()
                .unwrap_or(0.0);
            solo += match mode {
                InjectMode::Hit => dur,
                InjectMode::Trail => hold_s.min(dur),
                InjectMode::Overlap => 0.0,
            };
        }
        if solo > 0.0 {
            let ms = (solo * speed * 1000.0).round() as u32;
            slot.gap_ms += ms;
            // Kept on its own too: the concat writes no bare gap after the last
            slot.inject_ms = ms;
        }
    }
    relayout(slots);
}

/// Re-accumulate `start`/`end` from each slot's own duration and gap.
fn relayout(slots: &mut [Slot]) {
    let mut t = 0.0f64;
    for s in slots.iter_mut() {
        let dur = s.end - s.start;
        s.start = t;
        s.end = t + dur;
        t = s.end + s.gap_ms as f64 / 1000.0;
    }
}

/// One placed inject: what plays, when, and how it ends. The level is the
#[derive(Debug, Clone, PartialEq)]
pub struct InjectEvent {
    pub sound: String,
    pub file: String,
    pub start: f64,
    pub end: f64,
    pub level: f64,
    pub mode: InjectMode,
    pub fade_in: f64,
    pub fade_out: f64,
    /// The pool says this sound is a bed, so the window it was given is filled
    pub looped: bool,
}

/// A running overlap/trail tail (and, transiently, a hit): what is sounding,
struct InjectActive {
    sound: String,
    file: String,
    start: f64,
    end: f64,
    level: f64,
    mode: InjectMode,
    fade_out: f64,
    looped: bool,
}

/// How many copies of a clip a crossfaded loop needs to fill `window`.
pub fn loop_copies(window: f64, clip: f64, xfade: f64) -> Option<usize> {
    if clip <= 0.0 || window <= clip {
        return None;
    }
    // A crossfade a quarter of the clip long is already a blend, not a seam.
    let x = xfade.clamp(0.0, clip / 4.0);
    let step = (clip - x).max(1e-6);
    Some((((window - x) / step).ceil().max(1.0) as usize).max(2))
}

/// The filter graph that turns input 0 into a crossfaded loop of `copies`.
pub fn loop_filter(copies: usize, xfade: f64, volume: f64) -> String {
    loop_filter_with_tail(copies, xfade, &format!("volume={volume:.4}"))
}

/// [`loop_filter`] with the post-crossfade chain supplied, which is what lets the
pub fn loop_filter_with_tail(copies: usize, xfade: f64, tail: &str) -> String {
    let ins: String = (0..copies).map(|i| format!("[c{i}]")).collect();
    let mut f = format!("[0:a]asplit={copies}{ins}");
    let mut prev = "c0".to_string();
    for i in 1..copies {
        let out = format!("o{i}");
        f.push_str(&format!(
            ";[{prev}][c{i}]acrossfade=d={xfade:.3}:c1=tri:c2=tri[{out}]"
        ));
        prev = out;
    }
    format!("{f};[{prev}]{tail},aformat=sample_rates=48000:channel_layouts=mono[out]")
}

/// End every still-sounding instance of `sound` at `at + fade`, eased rather
fn stop_actives(actives: &mut [InjectActive], sound: &str, at: f64, fade: f64) {
    for a in actives.iter_mut().filter(|a| a.sound == sound) {
        if at >= a.end {
            continue;
        }
        let end = a.end.min(at + fade);
        a.end = end;
        a.fade_out = (end - at).max(0.0);
    }
}
/// Lay a chapter's injects on the delivered clock.
pub fn plan_injects(
    slots: &[Slot],
    takes: &[Vec<Option<audio_pool::Picked>>],
    durs: &BTreeMap<String, f64>,
    cfg: &InjectLayer,
) -> Vec<InjectEvent> {
    let mut actives: Vec<InjectActive> = Vec::new();
    for (slot, slot_takes) in slots.iter().zip(takes.iter()) {
        let mut cursor = slot.end;
        for (inj, take) in slot.injects.iter().zip(slot_takes.iter()) {
            match inj {
                Inject::Stop { sound } => {
                    stop_actives(&mut actives, sound, cursor, cfg.stop_fade_s);
                }
                Inject::Start {
                    sound,
                    mode,
                    hold_s,
                    level,
                } => {
                    let Some(take) = take else { continue };
                    let dur = durs.get(&take.file).copied().unwrap_or(0.0);
                    if dur <= 0.0 {
                        continue;
                    }
                    // The queue is serial: every directive anchors at the
                    let anchor = cursor;
                    // Retrigger: the old instance gets out of the way before
                    stop_actives(&mut actives, sound, anchor, cfg.fade_s);
                    // A looped bed has no natural end: the clip is a length of
                    let open_end = if take.looped {
                        f64::INFINITY
                    } else {
                        anchor + dur
                    };
                    match mode {
                        InjectMode::Hit => {
                            actives.push(InjectActive {
                                sound: sound.clone(),
                                file: take.file.clone(),
                                start: anchor,
                                end: open_end,
                                level: *level,
                                mode: *mode,
                                fade_out: cfg.tail_fade_s,
                                looped: take.looped,
                            });
                            cursor += dur;
                        }
                        InjectMode::Overlap => {
                            actives.push(InjectActive {
                                sound: sound.clone(),
                                file: take.file.clone(),
                                start: anchor,
                                end: open_end,
                                level: *level,
                                mode: *mode,
                                fade_out: cfg.tail_fade_s,
                                looped: take.looped,
                            });
                        }
                        InjectMode::Trail => {
                            let solo = hold_s.min(dur);
                            actives.push(InjectActive {
                                sound: sound.clone(),
                                file: take.file.clone(),
                                start: anchor,
                                end: open_end,
                                level: *level,
                                mode: *mode,
                                fade_out: cfg.tail_fade_s,
                                looped: take.looped,
                            });
                            cursor += solo;
                        }
                    }
                }
            }
        }
    }
    // Anything still open never met its `stop`. Fall back to one play, the
    for a in actives.iter_mut().filter(|a| a.end.is_infinite()) {
        a.end = a.start + durs.get(&a.file).copied().unwrap_or(0.0);
        a.looped = false;
    }
    actives
        .into_iter()
        .filter(|a| a.end - a.start > 0.01)
        .map(|a| InjectEvent {
            sound: a.sound,
            file: a.file,
            start: a.start,
            looped: a.looped,
            end: a.end,
            level: a.level,
            mode: a.mode,
            fade_in: if a.mode == InjectMode::Hit { 0.0 } else { 0.05 },
            fade_out: a.fade_out,
        })
        .collect()
}

// ---------------------------------------------------------------------------
