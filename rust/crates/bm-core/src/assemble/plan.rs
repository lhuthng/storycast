use super::mood::{mood_cluster, mood_take, run_text, take_for_mood};
use crate::cast::Cast;
use crate::paths::Layout;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------

/// One TTS call: the script segments it speaks.
#[derive(Debug, Clone)]
pub struct Run {
    pub speaker: String,
    pub idx: Vec<usize>,
}

/// The script the pipeline plans against.
/// the script, not computed here: the injection *is* "half a sentence, the
/// sound, the other half", and writing it that way is the only shape in which
#[derive(Debug, Clone, Default)]
pub struct Planned {
    /// The lines, in order: one element per TTS call, what the timeline lays
    pub speech: Vec<Value>,
    /// Parallel to `speech`: the sound items that fire at that line's end, in
    pub fires: Vec<Vec<Value>>,
    /// Parallel to `speech`: which post-headline `segments` item the line came
    pub origin: Vec<usize>,
}

impl Planned {
    /// Drop the headline, lift sounds out, and fold detached punctuation into
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

    /// One TTS call per script segment.
    ///
    /// A speaker's consecutive lines used to be folded into a single wav, which
    /// saved TTS calls and cost something nobody could see: inside one wav the
    /// segments share the span by character count, so a character who talks at
    /// length drags the captions a second or more behind the voice, and the
    /// error is worst exactly where the script is busiest. One wav per segment
    /// puts every caption boundary on a real wav edge.
    pub fn runs(&self) -> Vec<Run> {
        self.speech
            .iter()
            .enumerate()
            .map(|(i, seg)| Run {
                speaker: seg
                    .get("speaker")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                idx: vec![i],
            })
            .collect()
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
pub fn drop_headline(segments: &[Value]) -> &[Value] {
    match segments.first() {
        Some(s) if is_headline(seg_text(s)) => &segments[1..],
        _ => segments,
    }
}

/// Filenames the renderer is expected to produce. Shared by the renderer, the
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
#[derive(Debug, Clone)]
pub struct RenderedSegment {
    pub speaker: String,
    pub text: String,
    pub path: PathBuf,
    pub chapter: u32,
}

/// Largest single unit the pipeline accepts: ~11 minutes of 48 kHz 16-bit
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
fn norm_voice(s: &str) -> String {
    crate::util::fold(s)
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect()
}

/// Whether `character` speaks any line in the local scripts. Answers the miss
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
pub fn rendered_segments(layout: &Layout, engine: &str, voice: &str) -> Vec<RenderedSegment> {
    let voice = voice.trim();
    if voice.is_empty() {
        return Vec::new();
    }
    let name = crate::voices::resolve_voice_name(engine, voice);
    let key = crate::voices::key_for_name(engine, &name).unwrap_or_else(|| voice.to_string());
    // Normalized once: catalogue keys, display names and wav tags meet after
    let want = [voice, &name, &key]
        .iter()
        .map(|s| norm_voice(s))
        .collect::<Vec<_>>();

    // Chapters present as scripts, in order. A missing script or seg dir is
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
        let planned = Planned::plan(&all);
        let eff = &planned.speech;
        let rd = match std::fs::read_dir(layout.seg_dir(engine, n)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // The recorded takes for this chapter, if the cluster renderer wrote
        let takes: Vec<Value> = std::fs::read_to_string(layout.plan(n))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|d| d.get("takes").and_then(|t| t.as_array()).cloned())
            .unwrap_or_default();
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            // Whatever the storage tier stores: the take name's extension is
            let stem = match super::renderplan::TAKE_EXTENSIONS
                .iter()
                .find_map(|ext| fname.strip_suffix(&format!(".{ext}")))
            {
                Some(s) => s,
                None => continue,
            };
            // What the renderer writes now: content-addressed takes. The take
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
