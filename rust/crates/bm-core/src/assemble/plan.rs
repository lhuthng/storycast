use super::mood::{mood_cluster, mood_take, run_text, take_for_mood};
use crate::cast::Cast;
use crate::paths::Layout;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// segment planning
// ---------------------------------------------------------------------------

/// A consecutive run of lines by one speaker — one TTS call.
#[derive(Debug, Clone)]
pub struct Run {
    pub speaker: String,
    pub idx: Vec<usize>,
}

/// The script the pipeline plans against.
///
/// Two things happen here, and they are the reason every stage reads this
/// rather than the raw `segments`:
///
/// - the embedded chapter headline is dropped (the synthetic title run speaks
///   it instead), and
/// - the script's **sound items are lifted out of the speech**, leaving
///   `speech` as lines and only lines, and
/// - detached punctuation-only lines are folded into the previous speech line,
///   so punctuation stays in the text without getting a TTS take of its own.
///
/// A `segments` array is a sequence of *items*, and an item is one of two
/// kinds. A line carries a `speaker` and a `text`. A **sound** carries neither
/// — it is its own object, `{"sound": "page-turn", "mode": "overlap"}`, sitting
/// between the two halves of a line the script split for it. The split is in
/// the script, not computed here: the injection *is* "half a sentence, the
/// sound, the other half", and writing it that way is the only shape in which
/// the TTS can never be handed the syntax — a sound item has no text to feed,
/// so no renderer can read it as one, by construction rather than by a rule
/// each renderer has to remember.
#[derive(Debug, Clone, Default)]
pub struct Planned {
    /// The lines, in order: one element per TTS call, what the timeline lays
    /// out, and the index space every wav tag is named in. Sound items are not
    /// here.
    pub speech: Vec<Value>,
    /// Parallel to `speech`: the sound items that fire at that line's end, in
    /// the order the script lists them. A sound between two lines lands in the
    /// earlier one's slot, which is the seam the two halves meet at.
    pub fires: Vec<Vec<Value>>,
    /// Parallel to `speech`: which post-headline `segments` item the line came
    /// from. The only way back to the item a caller is thinking in, now that
    /// `speech` and `segments` are no longer the same array.
    pub origin: Vec<usize>,
}

impl Planned {
    /// Drop the headline, lift sounds out, and fold detached punctuation into
    /// the previous speech line.
    ///
    /// Lenient by construction, like the rest of the merge: a sound with no
    /// line before it has no seam to fire at and is warned and dropped rather
    /// than invented into one. A punctuation-only line has no audio of its own;
    /// its mark is appended to the previous line, and a leading mark with no
    /// line to attach to is dropped rather than sent to TTS alone.
    pub fn plan(segments: &[Value]) -> Planned {
        let mut out = Planned::default();
        for (i, seg) in drop_headline(segments).iter().enumerate() {
            if crate::util::is_sound_item(seg) {
                match out.fires.last_mut() {
                    Some(f) => f.push(seg.clone()),
                    None => eprintln!(
                        "inject: a sound leads the chapter with no line to fire after -> skipped"
                    ),
                }
                continue;
            }
            let text = seg_text(seg).trim();
            if !text.is_empty() && !crate::util::has_speakable_content(text) {
                if let Some(previous) = out.speech.last_mut() {
                    let previous_text = seg_text(previous).trim_end();
                    if crate::util::has_speakable_content(previous_text)
                        && !previous_text.chars().next_back().is_some_and(|c| {
                            matches!(
                                c,
                                ',' | '.'
                                    | '!'
                                    | '?'
                                    | ';'
                                    | ':'
                                    | '…'
                                    | '。'
                                    | '、'
                                    | '，'
                                    | '！'
                                    | '？'
                                    | '；'
                                    | '：'
                            )
                        })
                    {
                        previous["text"] = Value::String(format!("{previous_text}{text}"));
                    }
                }
                continue;
            }
            out.speech.push(seg.clone());
            out.fires.push(Vec::new());
            out.origin.push(i);
        }
        out
    }

    /// Group consecutive same-speaker pieces: one TTS call per run.
    ///
    /// A piece carrying an inject ends its run. The sound fires at that piece's
    /// end, and inside a multi-piece run that offset is unknowable — one wav,
    /// many pieces. Splitting is also the better read: the effect lands in a
    /// real pause rather than glued mid-breath. Everything downstream (render
    /// units, expected wavs, merge turns) reads these runs, so the split
    /// propagates to all of them by construction.
    pub fn runs(&self) -> Vec<Run> {
        let mut out: Vec<Run> = Vec::new();
        let mut prev_fires = false;
        for (i, seg) in self.speech.iter().enumerate() {
            let speaker = seg
                .get("speaker")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            match out.last_mut() {
                Some(last) if last.speaker == speaker && !prev_fires => last.idx.push(i),
                _ => out.push(Run {
                    speaker,
                    idx: vec![i],
                }),
            }
            prev_fires = !self.fires[i].is_empty();
        }
        out
    }

    /// Whether any effect fires at piece `i`'s end.
    pub fn fires_at(&self, i: usize) -> bool {
        self.fires.get(i).map(|f| !f.is_empty()).unwrap_or(false)
    }
}

pub(crate) fn seg_text(seg: &Value) -> &str {
    seg.get("text").and_then(|t| t.as_str()).unwrap_or("")
}

/// True when a segment is an embedded chapter headline (`Chương 12: …` or
/// `Chapter 12: …`). ASCII-prefix scan only — safe on UTF-8 text.
///
/// Both spellings are recognized because the word follows the *content
/// language* (see [`heading_word`]) while a script may carry either: a
/// translation can keep the source's `Chapter`, and a digest written before a
/// language was declared can carry the other. Requiring a following digit keeps
/// prose that merely opens with the word (`Chương pháp này…`) out.
pub fn is_headline(text: &str) -> bool {
    let t = text.trim_start();
    let rest = match t
        .strip_prefix("Chương")
        .or_else(|| t.strip_prefix("Chapter"))
    {
        Some(r) => r,
        None => return false,
    };
    rest.trim_start()
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
}

/// Drop an embedded headline so it never plans twice: the synthetic title run
/// (see [`TitleSpeech`]) replaces it everywhere. Idempotent — every planning
/// entry point applies it to raw input, so all of them always agree.
pub fn drop_headline(segments: &[Value]) -> &[Value] {
    match segments.first() {
        Some(s) if is_headline(seg_text(s)) => &segments[1..],
        _ => segments,
    }
}

/// Filenames the renderer is expected to produce. Shared by the renderer, the
/// completeness check and the merger so they can never disagree.
pub fn expected_wavs(
    planned: &Planned,
    cast: &Cast,
    seg_dir: &Path,
    local: bool,
    title: Option<&TitleSpeech>,
) -> Result<Vec<PathBuf>> {
    let segments = &planned.speech;
    let mut out = Vec::new();
    if let Some(t) = title {
        out.push(seg_dir.join(format!("title_{}.wav", t.voice)));
    }
    if local {
        for run in planned.runs() {
            let a = run.idx[0];
            let b = run.idx[run.idx.len() - 1];
            let tag = if a == b {
                format!("{a:04}")
            } else {
                format!("{a:04}-{b:04}")
            };
            let voice = cast
                .get(&run.speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {:?}", run.speaker))?;
            out.push(seg_dir.join(format!("{tag}_{voice}.wav")));
        }
    } else {
        for (i, s) in segments.iter().enumerate() {
            let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("");
            let voice = cast
                .get(speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {speaker:?}"))?;
            out.push(seg_dir.join(format!("{i:04}_{voice}.wav")));
        }
    }
    Ok(out)
}

/// One unit of render work, ready to hand to the TTS sidecar.
#[derive(Debug, Clone)]
pub struct RenderUnit {
    pub tag: String,
    pub dest: PathBuf,
    pub speaker: String,
    pub voice: String,
    pub text: String,
    pub temperature: f64,
    pub silence_p: f64,
    pub indices: Vec<usize>,
}

/// The spoken chapter headline ("Chương 46, <title>" / "Chapter 46, <title>",
/// Narrator). Digests
/// routinely drop the headline and concatenation would glue it to the first
/// line with no pause — so the headline is its own leading run with its own
/// cache file (`title_<voice>.wav`, never colliding with numeric tags) and
/// the normal inter-turn gap after it.
pub struct TitleSpeech {
    pub voice: String,
    pub text: String,
}

pub fn title_speech(
    layout: &crate::Layout,
    n: u32,
    cast: &Cast,
    first_text: &str,
) -> Option<TitleSpeech> {
    let title = layout.chapter_title(n);
    if title.is_empty() || title == format!("Chapter {n}") {
        return None; // no chapter text on disk — nothing truthful to say
    }
    // The planned first line already carries the headline (a second embedded
    // headline, or narration quoting the title): don't speak it twice.
    // Callers pass post-drop text; is_headline matches drop_headline exactly.
    //
    // The `contains` half is only right under `title_mode: auto`. There the
    // title is the digest's own name — a phrase, not a word — and a line
    // mentioning it really is a repeat. Under `default` the title *is* the
    // crawled headline, which the planner drops itself, so nothing has said it;
    // the guard would instead fire on any prose mentioning a one-word title
    // (a chapter headed `Chapter 1: Maomao` is about Maomao) and silence the
    // heading altogether.
    let auto = crate::config::Settings::load(&layout.settings()).auto_title();
    if is_headline(first_text) || (auto && first_text.contains(title.as_str())) {
        return None;
    }
    let voice = cast.get("Narrator")?.clone();
    Some(TitleSpeech {
        voice,
        text: format!("{} {n}, {title}", heading_word(layout)),
    })
}

/// The word a chapter's spoken headline is announced with, in the content
/// language: `Chapter 3, <title>` for English, `Chương 3, <title>` for
/// Vietnamese.
///
/// The language is the adapter's, as its manifest declares it. An adapter that
/// claims none keeps the checkout's long-standing Vietnamese wording rather
/// than guessing — and [`is_headline`] matches both spellings regardless, so a
/// script that carries the other word is still recognized as a headline.
fn heading_word(layout: &Layout) -> &'static str {
    let language = crate::adapter::in_force(layout)
        .ok()
        .flatten()
        .map(|m| m.language)
        .unwrap_or_default();
    if language.trim().to_ascii_lowercase().starts_with("en") {
        "Chapter"
    } else {
        "Chương"
    }
}

/// Same, when only the script path is known (merge path): the chapter number
/// comes from `script-NN.json`, the title from the sibling chapter text.
pub fn title_speech_for_script(
    script_path: &Path,
    cast: &Cast,
    segments: &[Value],
) -> Option<TitleSpeech> {
    let (layout, n) = crate::Layout::of_script(script_path)?;
    let planned = drop_headline(segments);
    let first = planned.first().map(seg_text).unwrap_or("");
    title_speech(&layout, n, cast, first)
}

fn title_unit(seg_dir: &Path, title: &TitleSpeech) -> RenderUnit {
    RenderUnit {
        tag: "title".to_string(),
        dest: seg_dir.join(format!("title_{}.wav", title.voice)),
        speaker: "Narrator".to_string(),
        voice: title.voice.clone(),
        text: title.text.clone(),
        temperature: 0.80,
        silence_p: 0.15,
        indices: Vec::new(),
    }
}

/// Decide what to render, without rendering it. The agent turns each unit into
/// one call to the TTS sidecar.
pub fn plan_render(
    planned: &Planned,
    cast: &Cast,
    seg_dir: &Path,
    local: bool,
    title: Option<&TitleSpeech>,
) -> Result<Vec<RenderUnit>> {
    let segments = &planned.speech;
    for (i, segment) in segments.iter().enumerate() {
        if !crate::util::has_speakable_content(seg_text(segment)) {
            anyhow::bail!(
                "segment {i}: cannot plan TTS render for punctuation-only text (no speakable content)"
            );
        }
    }
    let mut units = Vec::new();
    if let Some(t) = title {
        units.push(title_unit(seg_dir, t));
    }
    if local {
        for run in planned.runs() {
            let voice = cast
                .get(&run.speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {:?}", run.speaker))?
                .clone();
            let a = run.idx[0];
            let b = run.idx[run.idx.len() - 1];
            let tag = if a == b {
                format!("{a:04}")
            } else {
                format!("{a:04}-{b:04}")
            };
            let (temperature, silence_p) = mood_take(segments, &run.idx);
            units.push(RenderUnit {
                dest: seg_dir.join(format!("{tag}_{voice}.wav")),
                tag,
                speaker: run.speaker.clone(),
                voice,
                text: run_text(segments, &run.idx),
                temperature,
                silence_p,
                indices: run.idx.clone(),
            });
        }
    } else {
        for (i, s) in segments.iter().enumerate() {
            let speaker = s
                .get("speaker")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let voice = cast
                .get(&speaker)
                .ok_or_else(|| anyhow::anyhow!("cast has no voice for {speaker:?}"))?
                .clone();
            let mood = s.get("mood").and_then(|m| m.as_str()).unwrap_or("neutral");
            let (temperature, silence_p) = take_for_mood(&mood_cluster(mood));
            units.push(RenderUnit {
                dest: seg_dir.join(format!("{i:04}_{voice}.wav")),
                tag: format!("{i:04}"),
                speaker,
                voice,
                text: seg_text(s).to_string(),
                temperature,
                silence_p,
                indices: vec![i],
            });
        }
    }
    Ok(units)
}

/// True if every expected segment wav exists — the renderer's skip check and
/// the merger's ready check. Read-only.
pub fn segments_complete(
    layout: &Layout,
    script_path: &Path,
    cast_path: &Path,
    bible_path: &Path,
    seg_dir: &Path,
    engine: &str,
) -> bool {
    let Ok(text) = std::fs::read_to_string(script_path) else {
        return false;
    };
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let Some(segments) = data.get("segments").and_then(|s| s.as_array()) else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }
    // Same policy the renderer assigned with: the completeness check and the
    // renderer read one set of names.
    let policy = crate::cast::policy_for_bible(engine, layout);
    let installed = crate::pool::installed_voices(layout);
    let Ok(cast) = crate::cast::load_cast(
        script_path,
        cast_path,
        bible_path,
        &policy,
        installed.as_ref(),
        false,
    ) else {
        return false;
    };
    let local = engine == "vieneu";
    let title = title_speech_for_script(script_path, &cast, segments);
    let Ok(wavs) = expected_wavs(
        &Planned::plan(segments),
        &cast,
        seg_dir,
        local,
        title.as_ref(),
    ) else {
        return false;
    };
    wavs.iter()
        .all(|w| w.metadata().map(|m| m.len() > 1000).unwrap_or(false))
}

/// One segment wav already on disk: who speaks it, in which chapter, where.
///
/// The mirror image of `expected_wavs` — that function names files the
/// renderer must produce, this one reads back files it did produce.
#[derive(Debug, Clone)]
pub struct RenderedSegment {
    pub speaker: String,
    pub text: String,
    pub path: PathBuf,
    pub chapter: u32,
}

/// Largest single unit the pipeline accepts: ~11 minutes of 48 kHz 16-bit
/// mono. Runs are speaker-continuous, and narration runs for thousands of
/// characters (ch63 opens with a 7-minute monologue) — an 8 MB cap shelved
/// those chapters as "suspicious" when they are merely long.
pub const MAX_SEGMENT_BYTES: usize = 64 << 20;

impl RenderedSegment {
    /// Read the wav, with a cap against accidents (segments are KBs).
    pub fn read_bytes(&self) -> Result<Vec<u8>, String> {
        match std::fs::read(&self.path) {
            Ok(b) if b.len() > MAX_SEGMENT_BYTES => Err(format!(
                "{} MB — refusing a suspicious segment",
                b.len() >> 20
            )),
            Ok(b) => Ok(b),
            Err(e) => Err(format!("segment unreadable: {e}")),
        }
    }
}

/// Voice identity for file matching: folded ASCII lowercase with separators
/// dropped, so a clone key (`pham-tuyen`), its display name (`Phạm Tuyên`)
/// and a wav tagged with either all meet. Two voices that differ only by
/// case, diacritics or separators are the same voice for this purpose — the
/// bible treats them the same way when it folds aliases.
fn norm_voice(s: &str) -> String {
    crate::util::fold(s)
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect()
}

/// Whether `character` speaks any line in the local scripts. Answers the miss
/// question "rendered on another box, or never rendered at all": lines here
/// with no local wavs means the former.
pub fn character_has_lines(layout: &Layout, character: &str) -> bool {
    if character.trim().is_empty() {
        return false;
    }
    for path in layout.scripts() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if doc
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|segs| {
                segs.iter()
                    .any(|s| s.get("speaker").and_then(|v| v.as_str()) == Some(character))
            })
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// The candidate that speaks exactly `text` (folded comparison — the held
/// sentence and the script agree up to case, diacritics and whitespace).
/// Deterministic: replaying a held line replays the same audio, which is what
/// makes Tab a test instead of another random pick. The character preference
/// still applies first, so a shared sentence stays with its speaker.
pub fn pick_exact<'a>(
    cands: &'a [RenderedSegment],
    character: &str,
    text: &str,
) -> Option<&'a RenderedSegment> {
    let want = crate::util::fold(text.trim());
    if want.is_empty() {
        return None;
    }
    let hits: Vec<&RenderedSegment> = cands
        .iter()
        .filter(|c| crate::util::fold(c.text.trim()) == want)
        .collect();
    if hits.is_empty() {
        return None;
    }
    if !character.is_empty() {
        if let Some(own) = hits.iter().find(|c| c.speaker == character) {
            return Some(own);
        }
    }
    Some(hits[0])
}

/// Miss explanation for a voice with no local renders: lines here with no
/// local wavs means those chapters rendered on another box (or not yet);
/// no lines either means the chapters themselves live elsewhere. An exact
/// request that misses names the render key instead — the line exists, just
/// not in that voice.
pub fn segment_miss(layout: &Layout, character: &str, voice: &str, exact: bool) -> String {
    if exact {
        return format!("that line isn't rendered in {voice} yet — T renders it");
    }
    if character_has_lines(layout, character) {
        format!(
            "“{character}” has lines here but nothing rendered in {voice} — those chapters rendered on another box or not yet"
        )
    } else if character.trim().is_empty() {
        format!("no rendered segments for {voice} here — render first")
    } else {
        format!("no lines for “{character}” here — their chapters rendered elsewhere or not yet")
    }
}

/// Every rendered segment wav for `voice` in the local cache.
///
/// The pool is every line *in that voice*, whoever speaks it; the caller
/// (`pick_rendered`) prefers the requested character's own lines. Matching
/// mirrors the renderer: stems are `{idx}_{voice}` or `{a}-{b}_{voice}` where
/// the voice part is whatever the cast held at render time (a name or a key —
/// both match), and filename indices count past the headline and past every
/// sound item, via the same [`Planned::plan`] call. A wav whose script drifted
/// out from under it still lists; only its text is dropped, never the audio.
pub fn rendered_segments(layout: &Layout, engine: &str, voice: &str) -> Vec<RenderedSegment> {
    let voice = voice.trim();
    if voice.is_empty() {
        return Vec::new();
    }
    let name = crate::voices::resolve_voice_name(engine, voice);
    let key = crate::voices::key_for_name(engine, &name).unwrap_or_else(|| voice.to_string());
    // Normalized once: catalogue keys, display names and wav tags meet after
    // folding, whatever form each side was written in.
    let want = [voice, &name, &key]
        .iter()
        .map(|s| norm_voice(s))
        .collect::<Vec<_>>();

    // Chapters present as scripts, in order. A missing script or seg dir is
    // skipped, not an error — the range is aspirational, the files are truth.
    let chapters = layout.script_chapters();

    let mut out: Vec<RenderedSegment> = Vec::new();
    for n in chapters {
        let text = match std::fs::read_to_string(layout.script(n)) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let doc: Value = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let all: Vec<Value> = doc
            .get("segments")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        // The same plan the render and the merge used, cuts and all: a tag has
        // to name the piece of text the wav was actually spoken from.
        let planned = Planned::plan(&all);
        let eff = &planned.speech;
        let rd = match std::fs::read_dir(layout.seg_dir(engine, n)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // The recorded takes for this chapter, if the cluster renderer wrote
        // any: take-key files carry no speaker or voice in their names, so
        // they resolve through this instead of through filename parsing.
        let takes: Vec<Value> = std::fs::read_to_string(layout.plan(n))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|d| d.get("takes").and_then(|t| t.as_array()).cloned())
            .unwrap_or_default();
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            // Whatever the storage tier stores: the take name's extension is
            // the tier's, so only these suffixes are takes at all.
            let stem = match super::renderplan::TAKE_EXTENSIONS
                .iter()
                .find_map(|ext| fname.strip_suffix(&format!(".{ext}")))
            {
                Some(s) => s,
                None => continue,
            };
            // What the renderer writes now: content-addressed takes. The take
            // carries its own speaker, voice and text.
            if let Some(key) = stem.strip_prefix("t-") {
                if let Some(take) = takes
                    .iter()
                    .find(|t| t.get("take_key").and_then(|k| k.as_str()) == Some(key))
                {
                    let v = take.get("voice").and_then(|v| v.as_str()).unwrap_or("");
                    let vk = take.get("voice_key").and_then(|v| v.as_str()).unwrap_or("");
                    if !want.contains(&norm_voice(v)) && !want.contains(&norm_voice(vk)) {
                        continue;
                    }
                    out.push(RenderedSegment {
                        speaker: take
                            .get("speaker")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string(),
                        text: take
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        path: e.path(),
                        chapter: n,
                    });
                }
                continue;
            }
            // Anything else (titles, strays) is not a book line.
            let (tag, v) = match stem.split_once('_') {
                Some((t, v)) if t.chars().next().is_some_and(|c| c.is_ascii_digit()) => (t, v),
                _ => continue,
            };
            if !want.contains(&norm_voice(v)) {
                continue;
            }
            let mut idxs: Vec<usize> = Vec::new();
            let mut ok = true;
            for part in tag.split('-') {
                match part.parse::<usize>() {
                    Ok(i) => idxs.push(i),
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || idxs.is_empty() {
                continue;
            }
            // `.get` everywhere: a script edited since the render must cost
            // the text, never a panic, and never the audio.
            let first = match eff.get(*idxs.first().unwrap()) {
                Some(s) => s,
                None => continue,
            };
            let speaker = first
                .get("speaker")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let mut text = String::new();
            for i in idxs {
                let Some(s) = eff.get(i) else {
                    ok = false;
                    break;
                };
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(seg_text(s));
            }
            if !ok {
                continue;
            }
            out.push(RenderedSegment {
                speaker,
                text,
                path: e.path(),
                chapter: n,
            });
        }
    }
    out
}

/// One segment to play: the requested character's own lines first — hearing
/// *them* is the point — else anything in that voice. Random every call, so
/// Tab triages instead of repeating itself.
pub fn pick_rendered<'a>(
    cands: &'a [RenderedSegment],
    character: &str,
) -> Option<&'a RenderedSegment> {
    if cands.is_empty() {
        return None;
    }
    let own: Vec<&RenderedSegment> = cands
        .iter()
        .filter(|c| !character.is_empty() && c.speaker == character)
        .collect();
    let pool: Vec<&RenderedSegment> = if own.is_empty() {
        cands.iter().collect()
    } else {
        own
    };
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| {
            d.as_secs()
                .wrapping_mul(1_000_000_000)
                .wrapping_add(d.subsec_nanos() as u64)
        })
        .unwrap_or(0);
    let seed = nanos.wrapping_add(
        CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15),
    );
    Some(pool[(seed % pool.len() as u64) as usize])
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod segment_tests;
