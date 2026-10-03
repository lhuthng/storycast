use super::mix::clip_path;
use super::*;
/// How one injected sound sits on the timeline. Per *directive*, not per
/// sound: the same boil can underscore one scene (`overlap`) and punctuate
/// another (`trail`), and the filename never says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMode {
    /// Inline: the narration waits out the whole clip.
    Hit,
    /// Parallel: zero timeline time, runs under the following speech until the
    /// clip ends or a `stop` fades it.
    Overlap,
    /// Both: `hold_s` of solo time, then the tail runs under the speech.
    Trail,
}

impl InjectMode {
    /// How much of its own level this mode plays at.
    ///
    /// A `hit` owns the silence it was written into, so it plays at the level
    /// the pool gives it. An `overlap` and a `trail` own nothing: they run
    /// *under* the speech, and at full level they stop being a bed and start
    /// competing with the voice. The library had already voted on this, every
    /// overlap or trail clip that sat right had been hand-trimmed to 0.05–0.2 in
    /// its own pool entry, which is a per-clip workaround for a property of the
    /// mode. So the mode carries the trim, and the pool's `level` goes back to
    /// being the balance *between* clips of one mode.
    pub const fn gain(self) -> f64 {
        match self {
            InjectMode::Hit => 1.0,
            InjectMode::Overlap | InjectMode::Trail => 0.1,
        }
    }
}

/// The mode a pool entry's string names, or `None` for a string that names
/// none, which the caller reads as *skip this directive*, never as a default.
///
/// The one place the string is parsed. `injects_of` needs the mode to place the
/// clip; the `:sound` editor needs it to say how loud the clip will be, and a
/// second `match` there would be a second answer to the same question, the kind
/// that goes stale silently when a fourth mode is added.
pub fn inject_mode(name: &str) -> Option<InjectMode> {
    match name {
        "hit" => Some(InjectMode::Hit),
        "overlap" => Some(InjectMode::Overlap),
        "trail" => Some(InjectMode::Trail),
        _ => None,
    }
}

/// One inject directive: start a sound at the place it was cut into, or fade
/// out a running one from there.
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
///
/// A directive is the script's own **sound item**, `{"sound": "page-turn"}`
/// a sibling of the lines rather than a field on one. All it carries is the
/// *name*; **how the sound behaves comes from the pool**, because that is a
/// property of the clip and not of the chapter (a per-use mode is not
/// something a reader of prose can know, and the analyzer guessed at it).
///
/// Lenient on purpose: the digest validator is the strict gate (it can ask
/// the analyzer for a repair), while a merge must survive a hand edit the way
/// it survives a missing clip, malformed entries are skipped, and an unknown
/// sound resolves to no take at [`plan_inject_takes`] with one warning rather
/// than a dead chapter.
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
        // entry, so it has no mode either, and `plan_inject_takes` warns once
        // and plays nothing. Defaulting it to `hit` here would give a sound
        // nobody registered a length of silence nobody asked for.
        let Some(entry) = pool.get(sound) else {
            continue;
        };
        let Some(mode) = inject_mode(entry.mode.as_deref().unwrap_or("hit")) else {
            continue;
        };
        let hold_s = entry.hold.filter(|h| *h > 0.0).unwrap_or(default_hold);
        // The pool's trim, then the mode's. A `hit` is an event and keeps its
        // level; an `overlap`/`trail` is a bed and renders at a tenth of it.
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
/// `sound (mode; tags; Ns)`.
///
/// Everything the analyzer needs to choose a sound and nothing it has to
/// decide: the *name* it writes, the mode so it knows whether the narration
/// will pause for it (`hit`) or carry on over it (`overlap`/`trail`), the tags
/// that say what it sounds like, and the length. The mode is rendered rather
/// than left to the analyzer because it is a property of the clip, the script
/// says *which* sound and *where*, never how it behaves.
pub fn inject_prompt(pool: &ClipPool) -> String {
    pool.iter()
        .map(|(name, s)| {
            let mut inner = s.mode.clone().unwrap_or_else(|| "hit".into());
            if let Some(h) = s.hold.filter(|h| *h > 0.0) {
                inner.push_str(&format!(" {}s", trim_num(h)));
            }
            // `loop` is the one piece of behaviour the analyzer has to *act* on
            // beyond naming the sound: a looped bed runs until it is stopped, so
            // starting one without a `stop` means it plays once.
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
///
/// A direct registry lookup, not a tag pick: the analyzer names the *sound*,
/// and sounds are disjoint by construction, so scoring tags could only answer
/// a question nobody asked. The take rolls on the chapter, the slot and the
/// sound, a re-merge reproduces it, and neighbouring chapters vary. `None`
/// is an unknown sound (a hand edit past the validator), warned once here so
/// the chapter degrades to skipping it rather than dying on it.
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
/// and shared by the hold planner and the event planner, so the silence the
/// concat writes and the sounds the layers place agree on which take plays.
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
/// ffprobe per file; a file gone missing since the registry was written is
/// absent from the map, and both planners read absence as zero with a warning
///, a renamed clip degrades to a skipped inject, not a dead merge.
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
/// relative path the registry writes. One ffprobe per file, and a file that
/// cannot be read is simply absent — every caller reads absence as zero and
/// degrades, which is what a renamed clip should do rather than kill a merge.
///
/// `what` names the layer in the warning, because the two layers do not fail
/// the same way: an unprobeable inject is a skipped spot effect, an
/// unprobeable track is a run that cannot be told from one that fits.
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
/// slot, the same place a planned pause goes, in the same pre-tempo
/// milliseconds, so [`retime`] keeps them honest for free. Hits cost their
/// whole clip, trails their hold (never more than the clip), overlaps nothing.
/// Multiple directives queue in listed order; the event planner replays the
/// same queue, so the silence and the sounds agree.
///
/// The gaps are not the whole story. Writing a hold makes the concat longer,
/// so every slot after it starts later than the clock [`timeline`] laid out,
/// and every layer is placed by reading `Slot::start` and `Slot::end`. A hold
/// early in a chapter therefore slid everything after it late while the layers
/// stayed on the old clock. So the timeline is re-laid here, in the same call
/// that moved it: a caller cannot have the gaps without the clock that
/// matches them.
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
            // slot, and this is the part of it that has to survive that.
            slot.inject_ms = ms;
        }
    }
    relayout(slots);
}

/// Re-accumulate `start`/`end` from each slot's own duration and gap.
///
/// A slot's duration is `end - start`, the only copy of it the struct holds,
/// and the one thing a gap can never change. [`timeline`] lays the clock out
/// once; this is what re-lays it after something moves the gaps, so the two
/// cannot disagree about where a slot begins.
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
/// directive's own, the pool's trim with [`InjectMode::gain`] already folded
/// in, and the layer and operator gains are applied at render.
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
    /// by repeating it rather than by playing once and leaving silence. False
    /// for every one-shot, and false for a bed with no `stop`, see
    /// [`loop_copies`].
    pub looped: bool,
}

/// A running overlap/trail tail (and, transiently, a hit): what is sounding,
/// and when the mix stops hearing it.
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
///
/// `n` copies joined by `n-1` crossfades of `xfade` seconds run
/// `n*clip - (n-1)*xfade`, so solving that for `>= window` gives
/// `n >= (window - xfade) / (clip - xfade)`. `None` when one play already
/// covers the window: a one-shot is not a loop, and a clip that needs no repeat
/// must not be handed a seam it never had.
///
/// A window only exists when something ends the sound, the `stop` in the
/// script. A bed with no `stop` gets the clip's own length and is therefore
/// never looped, which is why the digest has to place one.
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
///
/// The seam is the whole point: a hard repeat of a sizzle clicks at every join,
/// and a bed that clicks four times a minute is worse than no bed. `acrossfade`
/// overlaps each pair into a short equal-power blend, `c1=tri:c2=tri` is
/// ffmpeg's default curve, which is what a short seam wants. The caller
/// truncates with an output `-t`, so the loop is allowed to run past the window
/// and no `atrim` is needed here.
pub fn loop_filter(copies: usize, xfade: f64, volume: f64) -> String {
    loop_filter_with_tail(copies, xfade, &format!("volume={volume:.4}"))
}

/// [`loop_filter`] with the post-crossfade chain supplied, which is what lets the
/// two callers share one graph builder: the inject layer wants a scalar gain
/// and the music layer wants a time-varying `volume` expression for its pause
/// lift, and both want the same `aformat` and the same `[out]` label. A
/// hand-spliced string is how a graph ends up with the gain filter *before* the
/// fade, which ducks the crossfade instead of the loop.
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
/// than cut. A stop for a sound with nothing running, or one already over, is
/// a no-op: the script outliving its sounds is ordinary, not an error.
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
///
/// Every directive anchors at its slot's end, it plays when the segment's
/// speech ends, in listed order, hits queuing inside the silence
/// [`plan_inject_holds`] already wrote. Overlap and trail tails keep sounding
/// under the following speech until the clip ends or a `stop` names them: a
/// stop fades from its anchor over `stop_fade_s`, and starting a sound
/// retriggers it (the old instance fades in `fade_s`, so two boils never
/// stack +6 dB). A tail nobody stops ends with the clip, eased by
/// `tail_fade_s` against a click.
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
                    // cursor, and hits and trails advance it by their solo
                    // time. That is the only anchor that keeps the sounds
                    // where the silence is, `plan_inject_holds` wrote the
                    // *sum* of those solos into the gap, so a trail that
                    // started back at `slot.end` would play its hold under a
                    // queued hit and leave its own tail in dead air.
                    let anchor = cursor;
                    // Retrigger: the old instance gets out of the way before
                    // the new one starts, so the same sound never stacks.
                    stop_actives(&mut actives, sound, anchor, cfg.fade_s);
                    // A looped bed has no natural end: the clip is a length of
                    // texture, not a statement, and the script's `stop` is what
                    // says when the scene moved on. So it opens *unbounded*
                    // `stop_actives` can only shorten, and a stop that arrives
                    // past the clip's own end would otherwise be ignored, which
                    // is exactly how a 7 s bed ended in a 142 s kitchen.
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
    // behaviour a bed had before looping existed, so a chapter that forgets the
    // stop loses the loop, not the sound.
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
// ffmpeg
// ---------------------------------------------------------------------------
