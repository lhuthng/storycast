//! Stage 2 — digest a chapter into `script-NN.json`.
//!
//! Ported from `analyze.py`. Two behavioural changes are forced by running
//! across a cluster:
//!
//! 1. The worker never writes the authoritative bible. It receives a snapshot
//!    in its task offer, uses it to build the prompt, and returns a *delta*
//!    (new characters, new aliases, who spoke) which the inductor merges as the
//!    single writer. Concurrent digests therefore cannot clobber each other.
//! 2. The snapshot is mirrored to the worker's local `data/bible.json` so the
//!    cast assigner can still read voice hints.

use crate::config::Settings;
use crate::paths::Layout;
use crate::util::{atomic_write, head_chars, squeeze_ws};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

mod canon;
mod llm;
mod reconcile;
mod tags;

pub use canon::{
    apply_merges, canon_key, canonicalize_script, merge_bible, resolve_speaker, BibleMerge,
};
pub use llm::{generate, parse_retry_delay, GenError};
pub use reconcile::{cast_only_folds, parse_reconcile_merges, reconcile_plan, ReconcilePlan};
pub use tags::{
    retag_text, tags_of, validate_context, validate_effect_tags, validate_injects, validate_script,
    validate_title, warn_vietnamese,
};

/// What a digest produces: the per-chapter script plus the bible delta.
#[derive(Debug, Clone)]
pub struct DigestOutcome {
    pub script: Value,
    /// `{new_characters, new_aliases, roster, speakers}` — merged by the inductor.
    pub delta: Value,
    pub segments: usize,
    pub log: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// bible
// ---------------------------------------------------------------------------

pub fn load_bible(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.get("characters").is_some())
        .unwrap_or_else(|| json!({"characters": []}))
}

pub fn save_bible(bible: &Value, path: &Path) -> Result<()> {
    atomic_write(path, &serde_json::to_string_pretty(bible)?)
}

/// Lean context for the prompt: identity only, no chapter baggage.
pub fn bible_context(bible: &Value) -> String {
    let lean: Vec<Value> = bible
        .get("characters")
        .and_then(|c| c.as_array())
        .map(|chars| {
            chars
                .iter()
                .map(|c| {
                    json!({
                        "name": c.get("name"),
                        "personality": c.get("personality"),
                        "voice_hint": c.get("voice_hint"),
                        "tags": c.get("tags").cloned().unwrap_or(json!([])),
                        "proper_aliases": c.get("proper_aliases"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::to_string(&lean).unwrap_or_else(|_| "[]".to_string())
}

fn strip_fences(raw: &str) -> &str {
    let s = raw.trim();
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

// ---------------------------------------------------------------------------
// the stage entry point
// ---------------------------------------------------------------------------

/// Read the scene map, refusing a map that declares no music palette: without
/// it the prompt would offer the analyzer an empty vocabulary and every emitted
/// `music` value would be rejected. One read, feeding both the prompt and the
/// validator, so the two cannot disagree about what is allowed.
fn load_map(layout: &Layout) -> Result<crate::ambience::SceneMap> {
    let path = layout.assets().join("scene-map.json");
    let map = crate::ambience::load_map(&path)?;
    if map.music_palette.is_empty() {
        anyhow::bail!(
            "{} declares no `music_palette`, so the digest prompt has no \
             vocabulary to offer and no value it emits could be accepted",
            path.display()
        );
    }
    Ok(map)
}

/// The context pass's prompt: the bible, the chapter, and nothing else.
///
/// The three sound palettes used to be rendered here too. They belong to the
/// script pass now — see [`build_script_prompt`] — and leaving them out is the
/// point of the split: this pass is asked one question, about the cast, and a
/// prompt that also carries a hundred lines of sound rules is a prompt whose
/// last instruction gets dropped.
pub fn build_prompt(layout: &Layout, bible: &Value, chapter_text: &str) -> Result<String> {
    let path = layout.prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    Ok(template
        .replace("{bible_json}", &bible_context(bible))
        .replace("{chapter_text}", chapter_text))
}

/// What the context pass found, rendered for the script pass: the cast it must
/// attribute against and every surface form the chapter uses for them.
///
/// Only these two keys: the script pass is handed the answer to the question
/// the context pass asked, not its whole output. `new_characters` and
/// `new_aliases` are the bible's business and the script pass never reads them.
fn cast_context(context: &Value) -> String {
    let lean = json!({
        "roster": context.get("roster").cloned().unwrap_or(json!([])),
        "mentions": context.get("mentions").cloned().unwrap_or(json!({})),
    });
    serde_json::to_string_pretty(&lean).unwrap_or_else(|_| "{}".to_string())
}

/// The script pass's prompt: the bible, the cast the context pass resolved, and
/// the chapter.
///
/// The palettes are rendered from the pools rather than written into the
/// template, so adding a mood (and the clip that answers it) is one edit to one
/// file. A prompt that listed its own vocabulary would drift the moment the pool
/// changed, and the drift would be silent.
pub fn build_script_prompt(
    layout: &Layout,
    bible: &Value,
    context: &Value,
    chapter_text: &str,
) -> Result<String> {
    let path = layout.script_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let map = load_map(layout)?;
    let palette = crate::ambience::palette_prompt(&map);
    let pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let effects = crate::ambience::effect_tags(&pool).join(", ");
    let injects = crate::ambience::inject_prompt(&crate::audio_pool::load_pool(
        &layout.assets().join("inject-pool.json"),
    ));
    Ok(template
        .replace("{bible_json}", &bible_context(bible))
        .replace("{cast_json}", &cast_context(context))
        .replace("{music_palette}", &palette)
        .replace("{effect_tags}", &effects)
        .replace("{inject_sounds}", &injects)
        .replace("{chapter_text}", chapter_text))
}

/// Ask the analyzer for one chapter, and return what it said.
///
/// **Nothing is written and nothing is queued.** No script lands on disk, no
/// bible is merged, no render is invalidated, no stage is scheduled — the
/// return value *is* the answer. That is the whole point of the split from
/// [`digest_chapter`]: getting the LLM's read of a chapter is a question worth
/// being able to ask on its own, and it was previously impossible to ask it
/// without also committing the answer and waking every stage downstream of it.
///
/// `digest_chapter` is this plus a write, so the worker path and a one-off
/// question go through the same prompts, the same model, the same validators and
/// the same one-repair-per-round policy. A second implementation would drift
/// from the first, and the drift would show up as a chapter that digests one way
/// in a run and another way by hand.
///
/// **Two rounds, because one round was dropping the tail.** The analyzer is a
/// flash-lite model and the single prompt had grown to 204 lines with 13 rules,
/// of which the three sound layers were 116 — at the end, right before the title
/// rule. What came back was the signature of a dropped tail instruction: a
/// `{"stop": "cooking"}` with no `{"sound": "cooking"}` anywhere, i.e. half of a
/// two-item obligation. So the work is split by *question* rather than by
/// chapter: round 1 reads the chapter and answers only "who is in it and what is
/// it about", round 2 is handed that cast and answers only "split it and tag
/// it". Each round is a task a small model can hold, and each gets its own
/// repair attempt — a bad cast list is not a reason to throw away a good script.
///
/// Round 2 failing is fatal on purpose. A chapter with no sound design is
/// exactly the silent loss the split exists to fix, and shipping it quietly is
/// how the last one went unnoticed.
pub async fn analyze_chapter(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<DigestOutcome> {
    let chapter_path = layout.chapter_txt(n);
    let text = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    // The same map the script prompt renders, read once more for the validator:
    // one file, so the vocabulary offered and the vocabulary accepted are the
    // same vocabulary by construction.
    let palette = crate::ambience::palette_names(&load_map(layout)?);
    let effect_pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let effects = crate::ambience::effect_tags(&effect_pool);
    let inject_pool = crate::audio_pool::load_pool(&layout.assets().join("inject-pool.json"));

    // ---- round 1: the cast and the story -----------------------------------
    progress(0.10, format!("digest ch{n} via {analyzer}: cast"));
    let prompt = build_prompt(layout, bible, &text)?;
    let raw = generate_retrying(&prompt, analyzer, settings, progress, 0.10, 0.30).await?;
    dump_raw(layout, "cast", &raw);
    let context = match parse_context(&raw, bible) {
        Ok(d) => d,
        Err(e) => {
            progress(0.32, "invalid cast list, asking for one repair".to_string());
            let again = repair_once(&prompt, &e, analyzer, settings).await?;
            match parse_context(&again, bible) {
                Ok(d) => d,
                Err(e2) => {
                    let dump = layout.data().join(".last-analyze-context.json");
                    let _ = atomic_write(&dump, &again);
                    anyhow::bail!("cast pass invalid ({e2}); raw saved to {}", dump.display());
                }
            }
        }
    };

    // ---- round 2: the script, cast already resolved -------------------------
    progress(0.45, format!("digest ch{n} via {analyzer}: script"));
    let script_prompt = build_script_prompt(layout, bible, &context, &text)?;
    let raw2 = generate_retrying(&script_prompt, analyzer, settings, progress, 0.45, 0.60).await?;
    dump_raw(layout, "script", &raw2);
    let mut script = match parse_script(&raw2, bible, &context, &palette, &effects, &inject_pool) {
        Ok(d) => d,
        Err(e) => {
            progress(0.62, "invalid script, asking for one repair".to_string());
            let again = repair_once(&script_prompt, &e, analyzer, settings).await?;
            match parse_script(&again, bible, &context, &palette, &effects, &inject_pool) {
                Ok(d) => d,
                Err(e2) => {
                    let dump = layout.data().join(".last-analyze-raw.json");
                    let _ = atomic_write(&dump, &again);
                    anyhow::bail!("script pass invalid ({e2}); raw saved to {}", dump.display());
                }
            }
        }
    };

    // The script pass sometimes returns a clean script with `none` on every
    // line: it engaged with the per-line fields and skipped the placement
    // decision. A chapter that stages a sound and places none is not a judgment
    // call, so it is asked once more — and if the second answer is the same, the
    // chapter fails rather than merging with no sound design and saying nothing.
    if let Some(gap) = sound_design_gap(&script, &text, &inject_pool) {
        progress(0.66, format!("sound design incomplete, asking again: {gap}"));
        let again = repair_once(&script_prompt, &anyhow::anyhow!(gap), analyzer, settings).await?;
        dump_raw(layout, "script-retry", &again);
        script = parse_script(&again, bible, &context, &palette, &effects, &inject_pool)?;
        if let Some(gap) = sound_design_gap(&script, &text, &inject_pool) {
            anyhow::bail!("script pass left the sound design incomplete: {gap}");
        }
    }

    let data = merge_rounds(&context, &script);

    let mut log = Vec::new();
    let warnings = warn_vietnamese(&data, bible);

    // Grammar fixes must reference text that is actually in the chapter.
    let fixes = data
        .get("fixes")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    for fx in &fixes {
        let before = fx.get("before").and_then(|b| b.as_str()).unwrap_or("");
        let after = fx.get("after").and_then(|a| a.as_str()).unwrap_or("");
        if before.is_empty() || after.is_empty() {
            anyhow::bail!("fix needs before+after: {fx}");
        }
        if !text.contains(before) {
            log.push(format!(
                "   WARN: fix source not found in chapter: {:?}",
                head_chars(before, 60)
            ));
        }
    }
    if !fixes.is_empty() {
        log.push(format!("   grammar fixes: {}", fixes.len()));
    }

    let segments = data
        .get("segments")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let script = json!({
        // The chapter's own name, rewritten out of the machine-translated
        // headline the crawl left on line 1. `Layout::chapter_title` prefers
        // this, so it is the mp3's filename *and* the spoken headline — one
        // value, two consumers, no chance of them disagreeing.
        "title": data.get("title").cloned().unwrap_or(json!("")),
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "mentions": data.get("mentions").cloned().unwrap_or(json!({})),
        // Spot effects ride *inside* this array, as their own items between
        // the halves of the lines they belong to. There is deliberately no
        // sibling array: a directive parked outside the speech would have to
        // name its position, and every scheme for naming one (a phrase to cut
        // at, an ordinal) is a second copy of a fact the script already knows.
        "segments": segments,
        "fixes": fixes,
    });

    let delta = json!({
        "new_characters": data.get("new_characters").cloned().unwrap_or(json!([])),
        "new_aliases": data.get("new_aliases").cloned().unwrap_or(json!({})),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "segments": script.get("segments").cloned().unwrap_or(json!([])),
    });

    progress(1.0, format!("digest ch{n} done"));
    log.push(format!(
        "segments={} sounds={} roster={}",
        script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.iter().filter(|i| crate::util::is_sound_item(i)).count())
            .unwrap_or(0),
        squeeze_ws(
            &script
                .get("roster")
                .map(|r| r.to_string())
                .unwrap_or_else(|| "[]".into())
        ),
    ));

    Ok(DigestOutcome {
        segments: script
            .get("segments")
            .and_then(|s| s.as_array())
            .map(|s| s.len())
            .unwrap_or(0),
        script,
        delta,
        log,
        warnings,
    })
}

/// Digest one chapter and persist it. `bible` is the inductor's snapshot; the
/// returned delta is merged by the inductor, never here.
///
/// [`analyze_chapter`] plus the one write it deliberately does not do. Keeping
/// the write here and only here is what lets the same analysis be run without
/// committing it.
pub async fn digest_chapter(
    layout: &Layout,
    n: u32,
    bible: &Value,
    settings: &Settings,
    analyzer: &str,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<DigestOutcome> {
    let mut out = analyze_chapter(layout, n, bible, settings, analyzer, progress).await?;
    let script_path = layout.script(n);
    atomic_write(&script_path, &serde_json::to_string_pretty(&out.script)?)?;
    out.log.push(format!("-> {}", script_path.display()));
    Ok(out)
}

/// One generation, retried through rate limits.
///
/// Split out because the digest makes two calls now and the retry policy must
/// not differ between them — a round that gave up sooner than the other would
/// fail chapters for a reason that has nothing to do with the round.
async fn generate_retrying(
    prompt: &str,
    analyzer: &str,
    settings: &Settings,
    progress: &mut (dyn FnMut(f32, String) + Send),
    from: f32,
    to: f32,
) -> Result<String> {
    let mut last_rl = String::new();
    for attempt in 0..6 {
        match generate(prompt, analyzer, settings).await {
            Ok(t) => return Ok(t),
            Err(GenError::RateLimited(msg)) => {
                let wait = parse_retry_delay(&msg)
                    .unwrap_or_else(|| (30.0 * 2f64.powi(attempt)).min(300.0));
                progress(
                    (from + 0.05 * attempt as f32).min(to),
                    format!("rate-limited, sleeping {wait:.0}s"),
                );
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                last_rl = msg;
            }
            Err(GenError::Fatal(e)) => return Err(e),
        }
    }
    anyhow::bail!("analyzer {analyzer} still rate-limited after retries: {last_rl}")
}

/// Ask the same round again with the validator's complaint appended.
///
/// One attempt, per round: a second failure is a chapter to look at, not a
/// prompt to keep re-sending. The caller decides whether the round is fatal.
async fn repair_once(
    prompt: &str,
    complaint: &anyhow::Error,
    analyzer: &str,
    settings: &Settings,
) -> Result<String> {
    let repair = format!(
        "{prompt}\n\nYour last output was invalid: {complaint}. Return ONLY the corrected JSON object."
    );
    match generate(&repair, analyzer, settings).await {
        Ok(t) => Ok(t),
        Err(GenError::RateLimited(m)) => anyhow::bail!("repair attempt rate-limited: {m}"),
        Err(GenError::Fatal(e)) => Err(e),
    }
}

/// Dump a round's raw answer when `BM_DIGEST_RAW` is set.
///
/// The digest throws the model's text away once it parses, which is right for a
/// run and useless for a post-mortem: "the analyzer placed no sounds" is a
/// symptom, and the raw is the only place the cause is visible — whether it
/// reasoned about the layer and dropped it, or never considered it at all.
fn dump_raw(layout: &Layout, round: &str, raw: &str) {
    if std::env::var("BM_DIGEST_RAW").is_err() {
        return;
    }
    let path = layout.data().join(format!(".last-{round}-raw.json"));
    let _ = atomic_write(&path, raw);
    eprintln!("{round} raw -> {}", path.display());
}

/// Lift the sound fields off the lines and into sibling items at their seams.
///
/// The script pass is asked for a sound as a *field on the line it follows*
/// rather than as an item of its own, and that is a deliberate concession to the
/// model, not a design: given an array of objects to write it fills every field
/// of every object and will not introduce an object it was not handed. Asked for
/// sound items directly it returns none at all — measured, not assumed (see the
/// module note on the two rounds). Asked for a field it fills the field.
///
/// So the pipeline does the moving. What lands on disk is still a sibling
/// `{"sound": ...}` item at the seam, so no renderer is ever handed a line with
/// a sound on it and `text` is never touched — the shape the script has is
/// unchanged, only the shape the *prompt* asks for.
///
/// Total on purpose: the fields are removed from every line whether or not the
/// name is any good. A bad one then fails validation with the message that
/// explains it, instead of sitting on a line being read by nobody — which is the
/// silent-loss bug this whole area keeps producing.
fn expand_sound_fields(segments: &[Value]) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(segments.len());
    for (i, s) in segments.iter().enumerate() {
        let mut line = s.clone();
        let take = |key: &str, line: &mut Value| -> Option<String> {
            line.as_object_mut()
                .and_then(|o| o.remove(key))
                .map(|v| v.as_str().map(str::trim).unwrap_or("").to_string())
        };
        let fields = [
            ("sound_after", take("sound_after", &mut line)),
            ("stop_after", take("stop_after", &mut line)),
        ];
        for (key, v) in &fields {
            // An empty string is what a line looks like when nobody decided.
            // `none` is the token the prompt asks for; a blank is refused, so
            // the choice is made per line instead of defaulted away.
            if v.as_deref() == Some("") {
                anyhow::bail!(
                    "segment {i}: `{key}` is empty — write \"none\" when nothing fires at this \
                     seam. A blank value is a line nobody decided about"
                );
            }
        }
        out.push(line);
        // A sound and then its stop, in that order: the pair brackets the line
        // the model marked, and a stop can never precede its own start.
        for (key, v) in &fields {
            let Some(v) = v.as_deref().filter(|v| !v.eq_ignore_ascii_case("none")) else {
                continue;
            };
            out.push(if *key == "sound_after" {
                json!({ "sound": v })
            } else {
                json!({ "stop": v })
            });
        }
    }
    Ok(out)
}

/// Phrases from rule 10's own sweep that are literal on the page in this genre.
///
/// Narrow on purpose: a hit here can fail a chapter, so a word that is usually a
/// metaphor does not belong on the list. `dao` alone is out for that reason —
/// `dao phay` is in.
const SOUND_CUES: [&str; 22] = [
    "phun ra",
    "máu tươi",
    "máu văng",
    "thổ huyết",
    "dao phay",
    "thái thịt",
    "phòng bếp",
    "vào bếp",
    "nấu đồ ăn",
    "nấu ăn",
    "rửa sạch",
    "vòi nước",
    "lật xem",
    "lật sách",
    "giở sách",
    "lật đến",
    "niệm chú",
    "bắn tên",
    "rút kiếm",
    "vung kiếm",
    "chém",
    "đâm",
];

/// Two sound-design answers that cannot be right, checked in that order.
///
/// Both are things the prompt says in as many words and the model does anyway,
/// and both are silent failures: the chapter merges, sounds fine at a glance,
/// and has no sound design where the prose staged one. Neither is a judgment
/// call, which is why they can be gated at all — a chapter that places three
/// sounds and misses a fourth is the model's business, and no word list can
/// second-guess it.
///
/// 1. A `loop`ed bed started and never stopped. The prompt calls this "the one
///    way to get a bed wrong": the clip plays once and stops dead. Measured on
///    ch9 — a 25 s bed opened into a 130 s kitchen, then digital silence.
/// 2. A chapter that stages a sound and places none at all — the cue list from
///    rule 10's own last check, matched against the chapter text.
fn sound_design_gap(
    script: &Value,
    chapter_text: &str,
    pool: &crate::audio_pool::ClipPool,
) -> Option<String> {
    let segments = script.get("segments").and_then(|s| s.as_array())?;

    let mut open: Vec<&str> = Vec::new();
    for item in segments {
        if let Some(sound) = item.get("sound").and_then(|v| v.as_str()) {
            if pool.get(sound).map(|e| e.looped).unwrap_or(false) {
                open.push(sound);
            }
        }
        if let Some(stop) = item.get("stop").and_then(|v| v.as_str()) {
            if let Some(i) = open.iter().position(|s| *s == stop) {
                open.remove(i);
            }
        }
    }
    if !open.is_empty() {
        return Some(format!(
            "{} is a looping bed started with no `stop_after` — it plays once and stops dead. \
             Close it on the line where the scene moves on",
            open.join(", ")
        ));
    }

    let placed = segments
        .iter()
        .filter(|i| crate::util::is_sound_item(i))
        .count();
    if placed > 0 {
        return None;
    }
    let hit: Vec<&str> = SOUND_CUES
        .iter()
        .copied()
        .filter(|c| chapter_text.contains(c))
        .collect();
    if hit.is_empty() {
        return None;
    }
    Some(format!(
        "this chapter stages sounds ({}) and this script places none — the last check in rule 10 \
         was skipped. Place a sound for each moment the text stages",
        hit.join(", ")
    ))
}

/// Put the two rounds back into the one object everything downstream reads.
///
/// Ownership is by key, not by "whoever ran last": the cast pass owns identity
/// (`title`, `atmosphere`, `roster`, `mentions`, `new_characters`,
/// `new_aliases`) and the script pass owns the speech (`segments`, `fixes`).
/// Neither can overwrite the other's keys, so a round that helpfully invents a
/// `title` of its own is ignored rather than silently believed.
fn merge_rounds(context: &Value, script: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for key in [
        "title",
        "atmosphere",
        "roster",
        "mentions",
        "new_characters",
        "new_aliases",
    ] {
        if let Some(v) = context.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    for key in ["segments", "fixes"] {
        if let Some(v) = script.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Round 1's answer, checked. There are no segments here to check.
fn parse_context(raw: &str, bible: &Value) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let data: Value = serde_json::from_str(cleaned).context("not valid JSON")?;
    validate_context(&data, bible)?;
    validate_title(&data)?;
    Ok(data)
}

/// Round 2's answer, checked against round 1's cast.
///
/// The sound fields come off the lines *before* anything is validated, so the
/// validators only ever see the script as it will exist on disk.
fn parse_script(
    raw: &str,
    bible: &Value,
    context: &Value,
    palette: &[String],
    effect_tags: &[String],
    inject_pool: &crate::audio_pool::ClipPool,
) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let mut data: Value = serde_json::from_str(cleaned).context("not valid JSON")?;
    if let Some(segs) = data.get("segments").and_then(|s| s.as_array()).cloned() {
        data["segments"] = json!(expand_sound_fields(&segs)?);
    }
    validate_script(&data, bible, context, palette)?;
    validate_effect_tags(&data, effect_tags)?;
    validate_injects(&data, inject_pool)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn bible_context_is_identity_only() {
        let bible = json!({"characters": [{
            "name": "A", "personality": "p", "voice_hint": "adult male",
            "proper_aliases": ["B"], "first_seen": "01", "chapters_seen": ["01"]
        }]});
        let ctx = bible_context(&bible);
        assert!(ctx.contains("\"name\":\"A\""));
        assert!(
            !ctx.contains("chapters_seen"),
            "context leaked chapter baggage: {ctx}"
        );
    }

    #[test]
    fn load_bible_defaults_when_missing_or_corrupt() {
        let missing = load_bible(Path::new("/nonexistent/bible.json"));
        assert_eq!(missing, json!({"characters": []}));
    }

    /// The two prompts and the two renderers must agree.
    ///
    /// A placeholder the code never fills reaches the analyzer literally, and a
    /// vocabulary the prompt never names might as well not exist. The split also
    /// has to hold: the cast prompt must carry no sound vocabulary at all, or
    /// the tail it was split away from creeps back in.
    #[test]
    fn the_two_prompts_render_their_own_placeholders() {
        let dir = std::env::temp_dir().join("bm-prompt-tags");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join("prompts/analyze.txt"),
            "{bible_json}|{chapter_text}",
        )
        .unwrap();
        std::fs::write(
            dir.join("prompts/script.txt"),
            "{music_palette}|{effect_tags}|{inject_sounds}|{cast_json}|{bible_json}|{chapter_text}",
        )
        .unwrap();
        std::fs::write(
            dir.join("assets/scene-map.json"),
            r#"{"music_palette": {"quiet": {"tags": ["soft"], "note": "low"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("assets/effect-pool.json"),
            r#"{"night": {"tags": ["night"], "files": ["effects/night-1.mp3"]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("assets/inject-pool.json"),
            // `looped` stated, like every shipped entry: the struct's default
            // is `true` (bed-shaped, which is what the effect pool wants), so an
            // inject entry that omits it silently becomes a looping bed.
            r#"{"coin": {"tags": ["coin", "metal"], "files": ["injects/coin-1.mp3"], "looped": false, "dur_s": 0.6}}"#,
        )
        .unwrap();
        let layout = Layout::new(&dir);
        let bible = json!({"characters": []});
        let context = json!({"roster": ["Narrator", "Dịch Phong"], "mentions": {"hắn": "Dịch Phong"}});

        // The cast prompt: bible and chapter, and nothing else.
        let cast = build_prompt(&layout, &bible, "text").unwrap();
        for ph in ["{bible_json}", "{chapter_text}"] {
            assert!(!cast.contains(ph), "placeholder leaked: {ph}");
        }
        assert!(
            !cast.contains("{music_palette}") && !cast.contains("{inject_sounds}"),
            "the cast prompt must not carry sound vocabulary: {cast}"
        );

        // The script prompt: the three vocabularies and the resolved cast.
        let p = build_script_prompt(&layout, &bible, &context, "text").unwrap();
        assert!(p.contains("quiet (soft; low)"), "{p}");
        assert!(p.contains("night"), "{p}");
        // the inject vocabulary renders the clip's own mode first
        assert!(p.contains("coin (hit; coin, metal; 0.6s)"), "{p}");
        assert!(p.contains("\"roster\""), "{p}");
        assert!(p.contains("Dịch Phong"), "{p}");
        assert!(p.contains("hắn"), "{p}");
        for ph in [
            "{effect_tags}",
            "{music_palette}",
            "{inject_sounds}",
            "{cast_json}",
            "{bible_json}",
            "{chapter_text}",
        ] {
            assert!(!p.contains(ph), "placeholder leaked: {ph}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sound fields come off the lines and become items at their seams.
    ///
    /// This is the concession the whole shape rests on, so it is pinned: the
    /// field must never survive onto a line, `text` must never be touched, a
    /// *bad* name must still be lifted so the validator can refuse it, and a
    /// blank must be refused here — a blank is a line nobody decided about, and
    /// it was exactly what the model wrote on all 68 lines of a chapter that
    /// stages six kitchen events.
    #[test]
    fn expand_sound_fields_lifts_the_seam_out_of_the_line() {
        let line = |text: &str| json!({"speaker": "Narrator", "text": text});
        let mut a = line("Nàng lau mồ hôi trên trán, siết chặt cuốn võ thư trong tay");
        a["sound_after"] = json!("page-turn");
        a["stop_after"] = json!("none");
        let b = line(", như nhặt được báu vật.");
        let got = expand_sound_fields(&[a, b.clone()]).unwrap();
        assert_eq!(got.len(), 3, "{got:?}");
        // The half keeps its text, word for word, and loses both fields.
        assert_eq!(
            got[0]["text"],
            json!("Nàng lau mồ hôi trên trán, siết chặt cuốn võ thư trong tay")
        );
        assert!(
            got[0].get("sound_after").is_none() && got[0].get("stop_after").is_none(),
            "{:?}",
            got[0]
        );
        // The sound lands between the halves — not after the whole line.
        assert_eq!(got[1], json!({"sound": "page-turn"}));
        assert_eq!(got[2], b);

        // A bed and its stop, both marked, in that order: start then stop, so a
        // stop can never end up before its own start.
        let mut open = line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp.");
        open["sound_after"] = json!("food-prep");
        let mut close = line("Thanh Sơn lão tổ gật đầu lia lịa như gà mổ thóc.");
        close["stop_after"] = json!("food-prep");
        let got = expand_sound_fields(&[open, close]).unwrap();
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[1], json!({"sound": "food-prep"}));
        assert_eq!(got[3], json!({"stop": "food-prep"}));

        // `none` is a decision and adds nothing; the field still comes off.
        let mut quiet = line("x");
        quiet["sound_after"] = json!("none");
        quiet["stop_after"] = json!("NONE");
        let got = expand_sound_fields(&[quiet, line("y")]).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].get("sound_after").is_none() && got[0].get("stop_after").is_none());

        // A blank is refused, by name, and the message says what to write.
        for key in ["sound_after", "stop_after"] {
            let mut blank = line("z");
            blank[key] = json!("  ");
            let err = expand_sound_fields(&[blank]).unwrap_err();
            assert!(err.to_string().contains("none"), "{key}: {err}");
        }

        // A name outside the vocabulary is lifted anyway, so the validator gets
        // to refuse it by name rather than the field vanishing silently.
        let mut bogus = line("z");
        bogus["sound_after"] = json!("thunder");
        let got = expand_sound_fields(&[bogus]).unwrap();
        assert_eq!(got[1], json!({"sound": "thunder"}));
    }

    /// Both halves of the sound-design gate, on the two answers ch9 actually
    /// produced: no placements at all, and a bed with no stop.
    #[test]
    fn sound_design_gap_catches_an_empty_and_an_unclosed_design() {
        use crate::audio_pool::{ClipPool, Sound};
        let mk = |looped: bool| Sound {
            tags: vec![],
            files: vec!["injects/x.mp3".into()],
            looped,
            dur_s: Some(25.2),
            mode: Some("overlap".into()),
            hold: None,
            level: None,
        };
        let pool: ClipPool = [("food-prep", mk(true)), ("coin", mk(false))]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let line = |t: &str| json!({"speaker": "Narrator", "text": t});
        let chapter = "Sau một hồi cảm khái, hai người liền đi đến phòng bếp. Thanh Sơn lão tổ tìm thấy chiếc dao phay.";

        // 1. Nothing placed at all, in a chapter that stages plenty.
        let silent = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
        ]});
        let gap = sound_design_gap(&silent, chapter, &pool).expect("a staged chapter with none");
        assert!(gap.contains("dao phay") && gap.contains("phòng bếp"), "{gap}");

        // 2. The bed opened and never closed — ch9's exact answer.
        let unclosed = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "food-prep"},
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
        ]});
        let gap = sound_design_gap(&unclosed, chapter, &pool).expect("an unclosed bed");
        assert!(gap.contains("food-prep") && gap.contains("stops dead"), "{gap}");

        // 3. Closed: both halves satisfied.
        let closed = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "food-prep"},
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
            {"stop": "food-prep"},
        ]});
        assert!(sound_design_gap(&closed, chapter, &pool).is_none(), "closed must pass");

        // 4. A one-shot needs no stop — only a `looped` sound does.
        let oneshot = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "coin"},
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
        ]});
        assert!(sound_design_gap(&oneshot, chapter, &pool).is_none());

        // 5. A chapter that stages nothing is allowed to place nothing.
        assert!(sound_design_gap(&silent, "Trời hôm nay đẹp.", &pool).is_none());
    }

    /// The two rounds are stitched by key, and each key has one owner.
    ///
    /// Worth a test because the failure mode is quiet: a script round that
    /// helpfully returns its own `title`, or a cast round that echoes back the
    /// `segments` it was shown, would overwrite the other half and nothing
    /// downstream could tell which of the two answers it was reading.
    #[test]
    fn merge_rounds_gives_each_key_to_its_owner() {
        let context = json!({
            "title": "Bí Ẩn Dao Phay",
            "atmosphere": "A kitchen at dusk.",
            "roster": ["Narrator"],
            "mentions": {"hắn": "Dịch Phong"},
            "new_characters": [],
            "new_aliases": {},
            // Not the cast pass's business, and it does not win.
            "segments": [{"speaker": "Ghost", "text": "from the cast pass"}],
        });
        let script = json!({
            // Not the script pass's business, and it does not win.
            "title": "Tê! Thật là khủng khiếp dao phay",
            "roster": ["Nobody"],
            "segments": [
                {"speaker": "Narrator", "text": "Trời sáng."},
                {"sound": "food-prep"},
                {"speaker": "Narrator", "text": "Rồi nấu."},
                {"stop": "food-prep"},
            ],
            "fixes": [],
        });
        let merged = merge_rounds(&context, &script);
        assert_eq!(merged["title"], json!("Bí Ẩn Dao Phay"));
        assert_eq!(merged["roster"], json!(["Narrator"]));
        assert_eq!(merged["mentions"], json!({"hắn": "Dịch Phong"}));
        assert_eq!(merged["segments"].as_array().unwrap().len(), 4);
        assert_eq!(merged["segments"][0]["text"], json!("Trời sáng."));
        assert_eq!(merged["fixes"], json!([]));
        // Every key the pipeline reads is present, from whichever round owns it.
        for key in [
            "title",
            "atmosphere",
            "roster",
            "mentions",
            "new_characters",
            "new_aliases",
            "segments",
            "fixes",
        ] {
            assert!(merged.get(key).is_some(), "missing {key} after the merge");
        }
    }

    /// The shipped pair, against the fixture pools.
    #[test]
    fn shipped_prompts_render_every_placeholder() {
        let dir = std::env::temp_dir().join("bm-prompt-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        crate::profile::install_fixture(&dir).expect("fixture profile");
        let layout = Layout::new(&dir);
        let bible: Value = json!({"characters": []});
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(
            layout.chapter_txt(51),
            "Chương 51: Fixture\n\nBody text here.\n",
        )
        .unwrap();
        let text = std::fs::read_to_string(layout.chapter_txt(51)).unwrap();

        let cast = build_prompt(&layout, &bible, &text).unwrap();
        for ph in ["{bible_json}", "{chapter_text}"] {
            assert!(!cast.contains(ph), "placeholder leaked: {ph}");
        }

        let context = json!({"roster": ["Narrator"], "mentions": {}});
        let p = build_script_prompt(&layout, &bible, &context, &text).unwrap();
        for ph in [
            "{music_palette}",
            "{effect_tags}",
            "{inject_sounds}",
            "{cast_json}",
            "{bible_json}",
            "{chapter_text}",
        ] {
            assert!(!p.contains(ph), "placeholder leaked: {ph}");
        }
        assert!(p.contains("quiet (soft, calm;"), "{p}");
        assert!(p.contains("battle, birds, calm"), "{p}");
        assert!(p.contains("blood-spatter (hit; blood"), "{p}");
    }

    /// Manual gate, not CI: runs a real digest of ch51 through the analyzer
    /// and prints the resulting script, so a prompt change can be eyeballed
    /// before anything downstream reads the new fields. Writes
    /// `data/script-51.json`, exactly like a worker completion would.
    /// Run with `BM_LIVE_DIGEST=1` and the keys from `.env` in the environment.
    #[tokio::test]
    #[ignore]
    async fn live_digest_ch51_prints_script() {
        if std::env::var("BM_LIVE_DIGEST").is_err() {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let layout = Layout::new(&root);
        let bible = load_bible(&layout.bible());
        let settings = crate::config::Settings::load(&layout.settings());
        let mut progress = |_f: f32, _s: String| {};
        let out = digest_chapter(&layout, 51, &bible, &settings, "gemini", &mut progress)
            .await
            .unwrap();
        println!("{}", serde_json::to_string_pretty(&out.script).unwrap());
    }
}
