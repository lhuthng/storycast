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

/// Group consecutive same-speaker segments: one TTS call per run.
pub fn runs(segments: &[Value]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        let speaker = seg
            .get("speaker")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        match out.last_mut() {
            Some(last) if last.speaker == speaker => last.idx.push(i),
            _ => out.push(Run {
                speaker,
                idx: vec![i],
            }),
        }
    }
    out
}

pub(crate) fn seg_text(seg: &Value) -> &str {
    seg.get("text").and_then(|t| t.as_str()).unwrap_or("")
}

/// True when a segment is an embedded chapter headline ("Chương 12: ...").
/// ASCII-prefix scan only — safe on UTF-8 text.
pub fn is_headline(text: &str) -> bool {
    let rest = match text.trim_start().strip_prefix("Chương") {
        Some(r) => r,
        None => return false,
    };
    rest.trim_start().chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
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
    segments: &[Value],
    cast: &Cast,
    seg_dir: &Path,
    local: bool,
    title: Option<&TitleSpeech>,
) -> Result<Vec<PathBuf>> {
    let segments = drop_headline(segments);
    let mut out = Vec::new();
    if let Some(t) = title {
        out.push(seg_dir.join(format!("title_{}.wav", t.voice)));
    }
    if local {
        for run in runs(segments) {
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

/// The spoken chapter headline ("Chương 46, <title>", Narrator). Digests
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
    if first_text.contains(title.as_str()) || is_headline(first_text) {
        return None;
    }
    let voice = cast.get("Narrator")?.clone();
    Some(TitleSpeech { voice, text: format!("Chương {n}, {title}") })
}

/// Same, when only the script path is known (merge path): the chapter number
/// comes from `script-NN.json`, the title from the sibling chapter text.
pub fn title_speech_for_script(script_path: &Path, cast: &Cast, segments: &[Value]) -> Option<TitleSpeech> {
    let stem = script_path.file_stem()?.to_str()?;
    let n: u32 = stem.strip_prefix("script-")?.parse().ok()?;
    let data_dir = script_path.parent()?;
    let layout = crate::Layout::new(data_dir.parent()?);
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
    segments: &[Value],
    cast: &Cast,
    seg_dir: &Path,
    local: bool,
    title: Option<&TitleSpeech>,
) -> Result<Vec<RenderUnit>> {
    let segments = drop_headline(segments);
    let mut units = Vec::new();
    if let Some(t) = title {
        units.push(title_unit(seg_dir, t));
    }
    if local {
        for run in runs(segments) {
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
            let speaker = s.get("speaker").and_then(|v| v.as_str()).unwrap_or("").to_string();
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
    // Same policy the renderer assigned with: a completeness check that
    // admits excluded voices would call a correctly-rendered chapter ready,
    // or a correctly-planned one missing.
    let Ok(policy) = crate::cast::policy_for_bible(engine, bible_path) else {
        return false;
    };
    let Ok(cast) = crate::cast::load_cast(script_path, cast_path, bible_path, &policy, false) else {
        return false;
    };
    let local = engine == "vieneu";
    let title = title_speech_for_script(script_path, &cast, segments);
    let Ok(wavs) = expected_wavs(segments, &cast, seg_dir, local, title.as_ref()) else {
        return false;
    };
    wavs.iter()
        .all(|w| w.metadata().map(|m| m.len() > 1000).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::wav::silent_wav;
    use serde_json::json;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bm-assemble-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn runs_group_consecutive_speakers() {
        let segs = vec![
            json!({"speaker": "A", "text": "1"}),
            json!({"speaker": "A", "text": "2"}),
            json!({"speaker": "B", "text": "3"}),
            json!({"speaker": "A", "text": "4"}),
        ];
        let r = runs(&segs);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].idx, vec![0, 1]);
        assert_eq!(r[1].idx, vec![2]);
        assert_eq!(r[2].idx, vec![3]);
    }

    #[test]
    fn expected_wavs_names_match_the_run_shape() {
        let segs = vec![
            json!({"speaker": "A", "text": "1"}),
            json!({"speaker": "A", "text": "2"}),
            json!({"speaker": "B", "text": "3"}),
        ];
        let mut cast = Cast::new();
        cast.insert("A".into(), "Đức Trí".into());
        cast.insert("B".into(), "Adam".into());
        let local = expected_wavs(&segs, &cast, Path::new("segs"), true, None).unwrap();
        assert!(local[0].ends_with("0000-0001_Đức Trí.wav"), "{:?}", local[0]);
        assert!(local[1].ends_with("0002_Adam.wav"), "{:?}", local[1]);
        let cloud = expected_wavs(&segs, &cast, Path::new("segs"), false, None).unwrap();
        assert!(cloud[0].ends_with("0000_Đức Trí.wav"));
        assert_eq!(cloud.len(), 3);
    }

    #[test]
    fn expected_wavs_errors_on_an_uncast_speaker() {
        let segs = vec![json!({"speaker": "Nobody", "text": "1"})];
        let err = expected_wavs(&segs, &Cast::new(), Path::new("s"), true, None).unwrap_err();
        assert!(err.to_string().contains("no voice for"), "{err}");
    }

    #[test]
    fn plan_render_is_per_run_locally_and_per_line_in_the_cloud() {
        let segs = vec![
            json!({"speaker": "A", "text": "one", "mood": "calm"}),
            json!({"speaker": "A", "text": "two", "mood": "calm"}),
        ];
        let mut cast = Cast::new();
        cast.insert("A".into(), "Đức Trí".into());
        let local = plan_render(&segs, &cast, Path::new("s"), true, None).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].tag, "0000-0001");
        assert_eq!(local[0].text, "one two");
        assert_eq!(local[0].temperature, take_for_mood("calm").0);

        let cloud = plan_render(&segs, &cast, Path::new("s"), false, None).unwrap();
        assert_eq!(cloud.len(), 2);
    }

    fn titled_layout(tag: &str, headline: &str) -> (PathBuf, crate::Layout) {
        let d = tmpdir(&format!("title-{tag}"));
        let l = crate::Layout::new(&d);
        std::fs::create_dir_all(l.chapters()).unwrap();
        std::fs::write(l.chapter_txt(7), format!("{headline}\n\nbody\n")).unwrap();
        (d, l)
    }

    #[test]
    fn headline_gets_its_own_leading_run_and_cache_file() {
        let (_d, l) = titled_layout("t", "Chương 7: Kiếm khí xung thiên. . .");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        cast.insert("A".into(), "Adam".into());
        let segs = vec![json!({"speaker": "A", "text": "mở đầu"})];
        let first = "mở đầu";
        let title = title_speech(&l, 7, &cast, first).unwrap();
        assert_eq!(title.text, "Chương 7, Kiếm khí xung thiên");
        assert_eq!(title.voice, "Đức Trí");
        let units = plan_render(&segs, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].tag, "title");
        assert!(units[0].dest.ends_with("title_Đức Trí.wav"), "{:?}", units[0].dest);
        assert_eq!(units[1].tag, "0000");
        let wavs = expected_wavs(&segs, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(wavs.len(), 2);
        assert!(wavs[0].ends_with("title_Đức Trí.wav"));
    }

    #[test]
    fn headline_skipped_when_the_digest_kept_its_own() {
        // Its own tag: `tmpdir` does `remove_dir_all` first, and tests run in
        // parallel threads, so sharing a tag with the test above let one delete
        // the other's chapter file mid-read — which surfaced as an unrelated
        // `title_speech(..).unwrap()` on None.
        let (_d, l) = titled_layout("t4", "Chương 7: Kiếm khí xung thiên");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        assert!(title_speech(&l, 7, &cast, "Kiếm khí xung thiên vang lên").is_none());
        // A quoted chapter number is dialogue, not a headline: title still spoken.
        assert!(title_speech(&l, 7, &cast, "\"Chương 7\" ai đó nói").is_some());
    }

    #[test]
    fn headline_skipped_without_chapter_text() {
        let d = tmpdir("title-missing");
        let l = crate::Layout::new(&d);
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        assert!(title_speech(&l, 7, &cast, "mở đầu").is_none());
    }

    #[test]
    fn drop_headline_only_cuts_a_leading_chapter_heading() {
        let hl = || json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"});
        let body = || json!({"speaker": "A", "text": "mở đầu"});
        assert_eq!(drop_headline(&[hl(), body()]).len(), 1);
        assert_eq!(drop_headline(&[body(), hl()]).len(), 2); // headline later: kept
        assert_eq!(drop_headline(&[]).len(), 0);
        assert!(is_headline("  Chương 12: x"));
        assert!(!is_headline("Chương pháp này rất hay")); // no digits: content
        assert!(!is_headline("mở đầu"));
    }

    #[test]
    fn kept_headline_never_speaks_twice() {
        let (_d, l) = titled_layout("t2", "Chương 7: Kiếm khí xung thiên");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        cast.insert("A".into(), "Adam".into());
        // Digest kept "Chương 7: ..." as its first segment.
        let segs = vec![
            json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"}),
            json!({"speaker": "A", "text": "mở đầu"}),
        ];
        let planned = drop_headline(&segs);
        assert_eq!(planned.len(), 1);
        let first = planned[0].get("text").and_then(|t| t.as_str()).unwrap();
        let title = title_speech(&l, 7, &cast, first).unwrap();
        let units = plan_render(planned, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].tag, "title");
        assert_eq!(units[0].text, "Chương 7, Kiếm khí xung thiên");
        // The embedded raw headline appears in no unit.
        assert!(!units.iter().any(|u| u.text.contains("Chương 7:")));
        let wavs = expected_wavs(&segs, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(wavs.len(), 2);
        assert!(wavs[0].ends_with("title_Đức Trí.wav"));
    }

    #[test]
    fn stripped_and_kept_scripts_plan_the_same_runs() {
        let (_d, l) = titled_layout("t3", "Chương 7: Kiếm khí xung thiên");
        let mut cast = Cast::new();
        cast.insert("Narrator".into(), "Đức Trí".into());
        cast.insert("A".into(), "Adam".into());
        let body = vec![json!({"speaker": "A", "text": "mở đầu"})];
        let first = "mở đầu";
        let title = title_speech(&l, 7, &cast, first).unwrap();
        let a = plan_render(&body, &cast, Path::new("s"), true, Some(&title)).unwrap();
        let mut kept = vec![json!({"speaker": "Narrator", "text": "Chương 7: x"})];
        kept.extend(body.clone());
        let b = plan_render(&kept, &cast, Path::new("s"), true, Some(&title)).unwrap();
        assert_eq!(
            a.iter().map(|u| u.tag.clone()).collect::<Vec<_>>(),
            b.iter().map(|u| u.tag.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn segments_complete_is_false_until_every_wav_exists() {
        let d = tmpdir("complete");
        let script = d.join("script-01.json");
        std::fs::write(
            &script,
            r#"{"segments":[{"speaker":"A","text":"one"},{"speaker":"A","text":"two"}]}"#,
        )
        .unwrap();
        let cast = d.join("cast-vieneu.json");
        std::fs::write(&cast, r#"{"A":"Đức Trí"}"#).unwrap();
        let segs = d.join("segs");
        std::fs::create_dir_all(&segs).unwrap();
        assert!(!segments_complete(
            &script,
            &cast,
            &d.join("bible.json"),
            &segs,
            "vieneu"
        ));
        silent_wav(&segs.join("0000-0001_Đức Trí.wav"), 0.05, 48_000).unwrap();
        assert!(segments_complete(
            &script,
            &cast,
            &d.join("bible.json"),
            &segs,
            "vieneu"
        ));
    }
}

// ---------------------------------------------------------------------------
// rendered-segment discovery (audition without synthesis)
// ---------------------------------------------------------------------------

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

impl RenderedSegment {
    /// Read the wav, with a cap against accidents (segments are KBs).
    pub fn read_bytes(&self) -> Result<Vec<u8>, String> {
        match std::fs::read(&self.path) {
            Ok(b) if b.len() > 8 << 20 => Err(format!(
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
    crate::util::fold(s).chars().filter(|c| !matches!(c, '-' | '_' | ' ')).collect()
}

/// Whether `character` speaks any line in the local scripts. Answers the miss
/// question "rendered on another box, or never rendered at all": lines here
/// with no local wavs means the former.
pub fn character_has_lines(layout: &Layout, character: &str) -> bool {
    if character.trim().is_empty() {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(layout.data()) else {
        return false;
    };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let num = match n.strip_prefix("script-").and_then(|s| s.strip_suffix(".json")) {
            Some(num) => num,
            None => continue,
        };
        if num.parse::<u32>().is_err() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if doc
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|segs| {
                segs.iter().any(|s| s.get("speaker").and_then(|v| v.as_str()) == Some(character))
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
        return format!("that line isn't rendered in {voice} yet — Shift+Tab renders it");
    }
    if character_has_lines(layout, character) {
        format!("“{character}” has lines here but nothing rendered in {voice} — those chapters rendered on another box or not yet")
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
/// both match), and filename indices count past the headline via the same
/// `drop_headline` call. A wav whose script drifted out from under it still
/// lists; only its text is dropped, never the audio.
pub fn rendered_segments(layout: &Layout, engine: &str, voice: &str) -> Vec<RenderedSegment> {
    let voice = voice.trim();
    if voice.is_empty() {
        return Vec::new();
    }
    let name = crate::voices::resolve_voice_name(engine, voice);
    let key = crate::voices::key_for_name(engine, &name).unwrap_or_else(|| voice.to_string());
    // Normalized once: catalogue keys, display names and wav tags meet after
    // folding, whatever form each side was written in.
    let want = [voice, &name, &key].iter().map(|s| norm_voice(s)).collect::<Vec<_>>();

    // Chapters present as scripts, in order. A missing script or seg dir is
    // skipped, not an error — the range is aspirational, the files are truth.
    let mut chapters: Vec<u32> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(layout.data()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(num) = n.strip_prefix("script-").and_then(|s| s.strip_suffix(".json")) {
                if let Ok(c) = num.parse::<u32>() {
                    chapters.push(c);
                }
            }
        }
    }
    chapters.sort();

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
        let eff = drop_headline(&all);
        let rd = match std::fs::read_dir(layout.seg_dir(engine, n)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let stem = match fname.strip_suffix(".wav") {
                Some(s) => s,
                None => continue,
            };
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
            out.push(RenderedSegment { speaker, text, path: e.path(), chapter: n });
        }
    }
    out
}

/// One segment to play: the requested character's own lines first — hearing
/// *them* is the point — else anything in that voice. Random every call, so
/// Tab triages instead of repeating itself.
pub fn pick_rendered<'a>(cands: &'a [RenderedSegment], character: &str) -> Option<&'a RenderedSegment> {
    if cands.is_empty() {
        return None;
    }
    let own: Vec<&RenderedSegment> =
        cands.iter().filter(|c| !character.is_empty() && c.speaker == character).collect();
    let pool: Vec<&RenderedSegment> =
        if own.is_empty() { cands.iter().collect() } else { own };
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| {
            d.as_secs().wrapping_mul(1_000_000_000).wrapping_add(d.subsec_nanos() as u64)
        })
        .unwrap_or(0);
    let seed = nanos
        .wrapping_add(CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    Some(pool[(seed % pool.len() as u64) as usize])
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    #[test]
    fn pick_prefers_own_lines_and_picks_something() {
        let seg = |speaker: &str| RenderedSegment {
            speaker: speaker.into(),
            text: "x".into(),
            path: PathBuf::from("x.wav"),
            chapter: 1,
        };
        assert!(pick_rendered(&[], "A").is_none(), "empty pool, no pick");
        let one = vec![seg("A")];
        assert_eq!(pick_rendered(&one, "Z").unwrap().speaker, "A");
        // Deterministic whenever the preferred set has exactly one member.
        let two = vec![seg("A"), seg("B")];
        assert_eq!(pick_rendered(&two, "B").unwrap().speaker, "B");
    }

    #[test]
    fn norm_voice_meets_keys_names_and_wav_tags() {
        // The three surface forms of one clone voice.
        assert_eq!(norm_voice("pham-tuyen"), norm_voice("Phạm Tuyên"));
        assert_eq!(norm_voice("adam"), norm_voice("Adam"));
        assert_eq!(norm_voice("young-female-1"), norm_voice("young-female-1"));
        assert_ne!(norm_voice("minh-duc"), norm_voice("minh-triet"));
    }
}
