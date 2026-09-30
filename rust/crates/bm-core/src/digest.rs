//! Stage 2, digest a chapter into `script-NN.json`.
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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use window::{plan_windows, tokens, weight, Window};

mod canon;
mod llm;
mod reconcile;
mod tags;
mod window;

pub use canon::{
    apply_merges, canon_key, canonicalize_script, merge_bible, resolve_speaker,
    scrub_ambiguous_aliases, BibleMerge,
};
pub use llm::{fetch_models, generate, parse_retry_delay, GenError};
pub use reconcile::{cast_only_folds, parse_reconcile_merges, reconcile_plan, ReconcilePlan};
pub use tags::{
    apply_tag_aliases, discard_unknown_effect_tags, retag_text, tags_of, validate_context,
    validate_effect_tags, validate_injects, validate_script, validate_title, warn_vietnamese,
    TagAliases,
};

/// What a digest produces: the per-chapter script plus the bible delta.
#[derive(Debug, Clone)]
pub struct DigestOutcome {
    pub script: Value,
    /// `{new_characters, new_aliases, roster, speakers}`, merged by the inductor.
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
    let s = raw.trim().trim_start_matches('\u{feff}');
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Parse model-produced JSON, repairing only a small, well-understood set of
/// common mistakes before preserving the normal parse error.
///
/// Prompt answers occasionally put literal quotation marks inside a Vietnamese
/// `text` value. The first unescaped quote makes serde treat the rest of the
/// sentence as JSON source and fail. Feeding each parse error back into the
/// input is safer than guessing from punctuation: the quote immediately before
/// the error is escaped, and the next parse confirms the reading. Bounded
/// retries keep truly malformed output from being rewritten indefinitely.
///
/// **The unambiguous repairs run on every pass, not once.** Escaping control
/// characters requires knowing which quotes open a string, so one unescaped
/// `"` inside a `text` value puts the scanner outside the string and every
/// later newline is emitted raw; escaping quotes afterwards moves that
/// boundary again. Alternating the repairs until neither changes anything is
/// the only order that converges on such input.
fn parse_json_repaired(input: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str(input) {
        return Ok(value);
    }

    // **Order matters, and it is quotes first, control characters second.**
    //
    // Escaping a control character needs to know which quotes open a string, so
    // an unescaped `"` inside a value puts that scanner outside the string and
    // every later newline is emitted raw. But fixing that by re-escaping after
    // each quote repair is worse than useless: an escaped quote reads as "a
    // literal quote inside a string", so the scanner never sees the string
    // *close*, swallows the rest of the document, and turns its newlines into
    // escapes, which is how a repair pass can turn one broken answer into a
    // differently broken one. ch79 died of `control character found while
    // parsing a string` twice over for exactly this reason.
    //
    // So: settle the quotes until the complaint stops being about structure,
    // then escape control characters against that now-correct pairing, once.
    let mut candidate = input.to_string();
    for _ in 0..64 {
        let error = match serde_json::from_str(&candidate) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        // A control character is the other half's job; handing it to the quote
        // walk would only escape an unrelated quote and move the error.
        if error.to_string().contains("control character") {
            break;
        }
        // A **fresh** parse every time, because serde's line and column belong
        // to the text that produced them: reusing the previous error indexes the
        // current candidate with stale offsets, which lands mid-character in
        // Vietnamese text and panics instead of repairing.
        let Some(error_offset) = json_error_offset(&candidate, &error) else {
            return Err(json_failure(&error));
        };
        let Some(quote) = nearest_unescaped_quote(&candidate, error_offset) else {
            return Err(json_failure(&error));
        };
        candidate.insert(quote, '\\');
    }

    // The scanner treats paired literal quotes as balanced, which is also how
    // the attached Vietnamese digest presents them.
    let candidate = remove_json_trailing_commas(&escape_json_control_chars(&candidate));
    match serde_json::from_str(&candidate) {
        Ok(value) => Ok(value),
        Err(error) => Err(json_failure(&error)),
    }
}

/// The parse error a model can act on.
///
/// serde's own wording is precise and useless to the thing that has to fix it:
/// `control character (\u0000-\u001F) found while parsing a string at line 67`
/// names a byte class, not a thing to write differently. The remedy goes with
/// it, because this message is what the repair prompt quotes back.
fn json_failure(error: &serde_json::Error) -> anyhow::Error {
    let raw = error.to_string();
    let remedy = if raw.contains("control character") {
        " — a raw newline, tab or carriage return was written inside a string value; \
         write them escaped as \\n, \\t and \\r"
    } else if raw.contains("expected value") {
        " — the output is not a JSON object at all; answer with the object alone, no prose \
         and no code fence"
    } else if raw.contains("trailing comma") {
        " — a comma was left before a closing brace or bracket"
    } else if raw.contains("EOF") || raw.contains("end of file") {
        " — the answer was cut off; return the whole object"
    } else {
        ""
    };
    anyhow::anyhow!("{raw}{remedy}")
}

/// Byte offset reported by serde_json (line and column are one-based; column is
/// a byte offset within the line).
fn json_error_offset(input: &str, error: &serde_json::Error) -> Option<usize> {
    let line_start = if error.line() == 1 {
        0
    } else {
        input.match_indices('\n').nth(error.line() - 2)?.0 + 1
    };
    let offset = line_start.checked_add(error.column().checked_sub(1)?)?;
    (offset <= input.len()).then_some(offset)
}

/// The nearest quote before `end` that is worth escaping.
///
/// **A quote followed by `:` is a key's closing quote, never the literal one**
/// inside a value, and escaping it turns `"segments": [...]` into a key that is
/// no longer a string, `key must be a string`, a new error one step further
/// from the truth. Skipping those is what keeps the walk on the right quote when
/// a value holds a raw `"` *and* a raw newline: the scanner is out of sync, so
/// the first complaint can land anywhere, and the nearest quote is then often
/// the wrong one.
fn nearest_unescaped_quote(input: &str, end: usize) -> Option<usize> {
    let mut end = end.min(input.len());
    while let Some(quote) = input[..end].rfind('"') {
        let backslashes = input[..quote]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\\')
            .count();
        if backslashes % 2 == 0 {
            let closes_a_key = input[quote + 1..]
                .chars()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| c == ':');
            if !closes_a_key {
                return Some(quote);
            }
        }
        end = quote;
    }
    None
}

fn escape_json_control_chars(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in input.chars() {
        if !in_string {
            output.push(ch);
            in_string = ch == '"';
            continue;
        }
        if escaped {
            output.push(ch);
            escaped = false;
        } else if ch == '\\' {
            output.push(ch);
            escaped = true;
        } else if ch == '"' {
            output.push(ch);
            in_string = false;
        } else if ch == '\n' {
            output.push_str("\\n");
        } else if ch == '\r' {
            output.push_str("\\r");
        } else if ch == '\t' {
            output.push_str("\\t");
        } else if ch == '\u{08}' {
            output.push_str("\\b");
        } else if ch == '\u{0c}' {
            output.push_str("\\f");
        } else if (ch as u32) < 0x20 {
            use std::fmt::Write as _;
            let _ = write!(output, "\\u{:04x}", ch as u32);
        } else {
            output.push(ch);
        }
    }
    output
}

fn remove_json_trailing_commas(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            output.push(ch);
        } else if ch == ',' {
            let next = input[index + ch.len_utf8()..]
                .chars()
                .find(|next| !next.is_whitespace());
            if matches!(next, Some(']') | Some('}')) {
                continue;
            }
            output.push(ch);
        } else {
            output.push(ch);
        }
    }
    output
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

/// The manual context pass: the bible, the chapter, and nothing else.
///
/// The automatic path uses [`build_attribution_prompt`] and the immutable
/// speaker map it returns. This public builder remains the legacy/manual route
/// so existing workspaces and pasted answers keep their old contract.
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
    let mut lean = serde_json::Map::new();
    lean.insert(
        "roster".into(),
        context.get("roster").cloned().unwrap_or(json!([])),
    );
    lean.insert(
        "mentions".into(),
        context.get("mentions").cloned().unwrap_or(json!({})),
    );
    if let Some(speakers) = context.get("speakers") {
        lean.insert("fixed_speakers".into(), speakers.clone());
    }
    serde_json::to_string_pretty(&Value::Object(lean)).unwrap_or_else(|_| "{}".to_string())
}

/// The script pass's prompt: the bible, the cast the context pass resolved, and
/// the chapter.
///
/// The vocabularies are rendered from the map and the pools rather than written
/// into the template, so adding a mood (and the clip that answers it), or a
/// scene rule (and the words it matches), is one edit to one file. A prompt that
/// listed its own vocabulary would drift the moment the pool changed, and the
/// drift would be silent — and a pack cannot edit a prompt at all, so a rule
/// whose match words the analyzer never sees is a rule that never fires.
pub fn build_script_prompt(
    layout: &Layout,
    engine: &str,
    bible: &Value,
    context: &Value,
    chapter_text: &str,
) -> Result<String> {
    let path = layout.script_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let map = load_map(layout)?;
    let palette = crate::ambience::palette_prompt(&map);
    let scene_words = crate::ambience::scene_prompt(&map);
    let pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let effects = crate::ambience::effect_tags(&pool).join(", ");
    let injects = crate::ambience::inject_prompt(&crate::audio_pool::load_pool(
        &layout.assets().join("inject-pool.json"),
    ));
    let mut body = template
        .replace("{bible_json}", &bible_context(bible))
        .replace("{cast_json}", &cast_context(context))
        .replace("{music_palette}", &palette)
        .replace("{scene_words}", &scene_words)
        .replace("{effect_tags}", &effects)
        .replace("{inject_sounds}", &injects)
        .replace("{chapter_text}", chapter_text);
    render_nonverbal(&mut body, engine, &mut Vec::new());
    Ok(body)
}

/// Render the bound engine's non-verbal vocabulary into a prompt — or take the
/// rule out of it.
///
/// **The rule is VieNeu's**, so an engine that voices no tags gets *no such
/// rule* rather than one that says "none": a negated rule still teaches the
/// model that brackets are a thing it may write, and this engine reads them
/// aloud. The section is bounded by its own heading and the next rule's — the
/// same mechanism rules 1–3 use — and a template that has been renumbered is
/// reported rather than silently left alone.
fn render_nonverbal(body: &mut String, engine: &str, missed: &mut Vec<String>) {
    // The declaration API, not a name test: whichever engine is bound answers
    // for itself, and an engine nobody declared answers "none" for the same
    // reason a tagless one does.
    let tags = crate::voices::nonverbals(engine);
    if tags.is_empty() {
        if !replace_prompt_section(body, NONVERBAL_RULE, MUSIC_RULE, "") {
            missed.push("rule 7 (non-verbal)".into());
        }
        // Backstop for a template numbered differently: a placeholder that
        // survives to the model is a token it copies.
        for ph in NONVERBAL_PLACEHOLDERS {
            *body = body.replace(ph, "");
        }
        return;
    }
    *body = body.replace("{voice_tags}", &vocabulary_block(tags));
    for (key, _gloss, tag) in tags {
        *body = body.replace(&format!("{{tag_{key}}}"), tag);
    }
}

/// The non-verbal rule's opening heading, which is also how it is found when it
/// has to be removed.
const NONVERBAL_RULE: &str = "7. NON-VERBAL SOUNDS.";
/// The rule after it: the far bound of the section.
const MUSIC_RULE: &str = "8. MUSIC:";

/// Every placeholder the non-verbal rule uses, for the engine that has none.
const NONVERBAL_PLACEHOLDERS: [&str; 4] =
    ["{voice_tags}", "{tag_laugh}", "{tag_sigh}", "{tag_throat}"];

/// The concept/tag list the rule renders: the gloss, then the exact token.
fn vocabulary_block(tags: &[(&str, &str, &str)]) -> String {
    tags.iter()
        .map(|(_key, gloss, tag)| format!("  {gloss:<15}{tag}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A deterministic, source-aware view of one chapter.
///
/// The preparer never rewrites prose and never asks a model what it means. It
/// only separates quoted dialogue from surrounding narration and gives both a
/// stable id. Both automatic passes receive this JSON view; the attribution pass
/// fixes speakers and the source gate proves every id was consumed once.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedEvent {
    id: String,
    kind: String,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedChapter {
    events: Vec<PreparedEvent>,
    /// The machine-readable form placed in the attribution and staging prompts.
    /// It contains the chapter text exactly once, split into ordered events.
    prompt_json: String,
    /// A quote delimiter was still open when the text ran out.
    ///
    /// Not cosmetic. An unclosed quote makes the scanner treat *every*
    /// remaining span as one dialogue event, so a chapter that lost its final
    /// `"` upstream is read start-to-finish in a single voice — the mirror of
    /// the no-quotes case below, and just as silent.
    unbalanced: bool,
}

impl PreparedChapter {
    /// How many events are dialogue, as decided by the quote delimiters alone.
    fn dialogue_count(&self) -> usize {
        self.events.iter().filter(|e| e.kind == "dialogue").count()
    }

    /// One line saying how this chapter was split, and what to check if the
    /// split looks wrong.
    ///
    /// This exists because of a **blind spot, not a bug**. Narration and
    /// dialogue are told apart by quote marks and nothing else, so a chapter
    /// with no quote marks is one long run of narration, and from there
    /// *nothing downstream complains*: every ledger row is green while the
    /// whole book is read in a single voice. The validators can only catch a
    /// model that disagrees with *the text it was given*; they cannot catch
    /// text that never offered a speaker to disagree with.
    ///
    /// Which is why the message is worded as a thing to check and not an
    /// accusation. A genuinely single-voice chapter is a real thing, and
    /// blaming the crawler on every one of them would train the operator to
    /// ignore the line exactly when it matters.
    ///
    /// The mirror case gets the same treatment. Too few quote marks reads as
    /// one voice, and so do *too many*: an unbalanced chapter leaves a
    /// delimiter open and every span after it becomes one dialogue event, so
    /// the count looks healthy while the chapter is one long speech. Neither
    /// shape is visible to the validators, which can only catch a model
    /// disagreeing with the text it was given.
    fn split_summary(&self) -> String {
        let dialogue = self.dialogue_count();
        let narration = self.events.len() - dialogue;
        let mut s = format!(
            "   prepared {} event(s): {narration} narration, {dialogue} dialogue",
            self.events.len()
        );
        if self.events.is_empty() {
            s.push_str(" — nothing to attribute; the chapter text is empty");
        } else if dialogue == 0 {
            s.push_str(
                " — no dialogue found, so all of it will be read in one voice. That is correct \
                 if the chapter really is narration. If people are talking in it, the quote \
                 marks are not in the text: check the crawler's container selector, and \
                 whether this site marks speech with something other than \" or “",
            );
        } else if self.unbalanced {
            s.push_str(
                " — a quote is still open at the end of the chapter, so everything after the \
                 last matched pair was read as one speech. Check the crawler's container \
                 selector: the chapter is probably cut short or lost a closing quote",
            );
        } else if narration == 0 {
            // Every event landed inside quotes. Legal, and true of a chapter
            // that is nothing but a system panel — so this asks rather than
            // claims, and stays a line the operator learns to read.
            s.push_str(
                " — no narration at all, so the whole chapter will be read as speech. That is \
                 correct for a chapter that is all dialogue or a system panel. If the prose is \
                 there, the site is marking speech with something this scanner reads as a quote \
                 delimiter",
            );
        }
        s
    }
}

fn prepared_event(id: usize, kind: &str, text: &str) -> Option<PreparedEvent> {
    let text = text.trim();
    if text.is_empty() || !crate::util::has_speakable_content(text) {
        return None;
    }
    Some(PreparedEvent {
        id: format!("e{id:04}"),
        kind: kind.to_string(),
        text: text.to_string(),
    })
}

/// Split source paragraphs into dialogue and narration spans without changing
/// their speakable text. Quote delimiters and standalone punctuation separators
/// are not speech, so the model is not asked to reproduce them in a segment.
/// The split report for a chapter, without digesting it.
///
/// Exists so the shape of a chapter can be inspected before spending an LLM
/// call on it: `analyze_chapter` prints this as its first log line, and this
/// is the same line, on demand. The live retraction check uses it to see what
/// the preparer decided before asking a model to agree or disagree.
pub fn preview_split(text: &str) -> String {
    prepare_chapter(text).split_summary()
}

fn prepare_chapter(text: &str) -> PreparedChapter {
    // Older workspaces can contain raw HTML entities and Storya's promo/footer
    // metadata. Sanitize at the same boundary the crawler and local reader use,
    // so those artifacts never receive source ids or become obligations for the
    // model. A decoded `&quot;` becomes a real quote delimiter, which
    // `prepare_chapter` then splits on, exactly what a properly crawled
    // chapter would have carried.
    let text = crate::crawl::sanitize_chapter_text(text);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut events = Vec::new();
    let mut quote: Option<(char, usize)> = None;
    let mut start = 0usize;
    let mut kind = "narration";

    let push = |from: usize, to: usize, kind: &str, events: &mut Vec<PreparedEvent>| {
        if from >= to {
            return;
        }
        let raw = &text[from..to];
        if let Some(event) = prepared_event(events.len() + 1, kind, raw) {
            events.push(event);
        }
    };

    let mut i = 0usize;
    while i < chars.len() {
        let (at, ch) = chars[i];
        let is_open = ch == '"' || ch == '“' || ch == '「';
        let is_close = ch == '"' || ch == '”' || ch == '」';
        if is_open && quote.is_none() {
            push(start, at, kind, &mut events);
            quote = Some((ch, at));
            start = at + ch.len_utf8();
            kind = "dialogue";
        } else if is_close && quote.is_some() {
            push(start, at, kind, &mut events);
            quote = None;
            start = at + ch.len_utf8();
            kind = "narration";
        } else if quote.is_none() && ch == '\n' {
            push(start, at, kind, &mut events);
            start = at + ch.len_utf8();
        }
        i += 1;
    }
    if start < text.len() {
        push(start, text.len(), kind, &mut events);
    }

    // Chapter headlines are spoken by the title renderer, not by the digest.
    // A few crawled chapters repeat the headline later in the file (for
    // example ch188), so checking only the first event would make the gate
    // demand that a second heading be spoken. Remove every standalone heading
    // before assigning ids; the source contract then has no hidden exemption.
    let mut content = Vec::with_capacity(events.len());
    for event in events {
        if !crate::assemble::is_headline(&event.text) {
            content.push(event);
        }
    }
    let mut events = content;
    for (i, event) in events.iter_mut().enumerate() {
        event.id = format!("e{:04}", i + 1);
    }

    let value: Vec<Value> = events
        .iter()
        .map(|e| json!({"id": e.id, "kind": e.kind, "text": e.text}))
        .collect();
    PreparedChapter {
        prompt_json: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".into()),
        events,
        // Decided before the headline filter, because it is a fact about the
        // text and not about which events survived it. A headline dropped
        // after a dangling quote does not rebalance anything.
        unbalanced: quote.is_some(),
    }
}

/// The attribution prompt's view of the chapter: dialogue events it must
/// attribute, plus the nearest narration immediately before and after each one.
///
/// Splitting the answerable events from narration keeps the map small. Keeping
/// the adjacent narration is nevertheless essential: Vietnamese web novels
/// routinely put the speaker tag *after* the quote (`"Sư tôn..." Lạc Lan Tuyết
/// ... nói.`). The old view kept narration ids but removed their text, so the
/// model was explicitly told to use surrounding narration it could not see. On
/// ch6 it assigned Lạc Lan Tuyết's three tagged lines to Chung Thanh. Context
/// beside each quote restores that evidence while leaving only dialogue ids in
/// the answer map.
fn attribution_view(prepared: &PreparedChapter) -> String {
    let mut narration_ids = Vec::new();
    let mut dialogue_events = Vec::new();
    for (i, event) in prepared.events.iter().enumerate() {
        if event.kind != "dialogue" {
            narration_ids.push(json!(event.id));
            continue;
        }

        let context = |range: std::ops::Range<usize>| {
            prepared.events[range]
                .iter()
                .find(|candidate| candidate.kind == "narration")
                .map(|candidate| json!({"id": candidate.id, "text": candidate.text}))
                .unwrap_or(Value::Null)
        };
        dialogue_events.push(json!({
            "id": event.id,
            "text": event.text,
            "previous_context": context(i.saturating_sub(1)..i),
            "following_context": context(i + 1..prepared.events.len()),
        }));
    }
    let view = json!({
        "narration_ids": narration_ids,
        "dialogue_events": dialogue_events,
        // The rules themselves live in the prompt template, where the rest of
        // the output contract is. This says only what the JSON is, so a model
        // reading the view and a model reading the contract are never told two
        // different things about the same field.
        "note": "Return `speakers` for every `dialogue_events` id, except any you also list in `not_speech` — a quoted span that is not somebody talking, judged from the context around it. Context events are evidence for resolving an id; all context and every id in `narration_ids` are spoken by Narrator and are not yours to answer. An explicit named speech tag in `following_context` is the strongest speaker evidence.",
    });
    serde_json::to_string_pretty(&view).unwrap_or_else(|_| "[]".into())
}

/// Replace one bounded prompt section when the live template still has it.
///
/// Profiles may be older than the binary, so an absent section marker is not a
/// hard error: the appended contract remains authoritative and placeholder
/// substitution still works for fixture/custom templates. It is, however,
/// never silent — the miss is returned so the caller can name it. A template
/// reword that quietly disabled a replacement is exactly how the code-side and
/// file-side prompts drift apart, with no error anywhere to say so.
fn replace_prompt_section(
    body: &mut String,
    start_marker: &str,
    end_marker: &str,
    replacement: &str,
) -> bool {
    let Some(start) = body.find(start_marker) else {
        return false;
    };
    let Some(end) = body[start..].find(end_marker).map(|n| start + n) else {
        return false;
    };
    body.replace_range(start..end, &format!("{replacement}\n"));
    true
}

/// Replace `needle` and record it when it was absent.
///
/// The prose overrides below are authored in this file and matched against the
/// profile's template by exact text. `String::replace` is silent when the text
/// has been reworded, which is the failure this wrapper exists to expose: the
/// digest still runs on the profile's own (older) wording, but the miss lands in
/// the build warning instead of nowhere.
fn replace_or_miss(body: &mut String, needle: &str, replacement: &str, missed: &mut Vec<String>) {
    if !body.contains(needle) {
        missed.push(head_chars(needle, 48).to_string());
        return;
    }
    *body = body.replace(needle, replacement);
}

/// Warn, once, about every section a prompt build expected to rewrite but did
/// not find. Loud, not fatal: a profile predating the binary still digests on
/// its own wording, and killing it would strand old workspaces over a cosmetic
/// mismatch. Making these fatal is a one-line change if the noise is wanted.
fn warn_missing_sections(which: &str, missed: &[String]) {
    if missed.is_empty() {
        return;
    }
    eprintln!(
        "{which}: {} code-side override(s) did not match the template, so the \
         profile's own text is in use: {}",
        missed.len(),
        missed.join("; ")
    );
}

/// Which of the two rounds a continuity block is written for.
///
/// The part is the same fact told twice, because the two rounds answer different
/// questions about it — and the one that only matters for staging is that a
/// looping bed may be left open for the part after this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    Attribution,
    Staging,
}

/// The part a prompt is being built for, when a chapter is staged in windows.
///
/// **A one-window chapter passes `None` everywhere this appears, and that is a
/// guarantee rather than a convenience.** With no continuity block the two
/// prompts are byte-for-byte the ones the pre-window digest built, so a chapter
/// under the budget cannot digest differently because windows exist — not
/// "usually produces the same answer", the same prompt. Everything the feature
/// adds to a prompt is therefore here, in one block, appended after the output
/// contract: no adapter template had to change, and a workspace whose profile
/// predates this feature still gets the part note and the plot.
///
/// The block, not a placeholder, for the same reason the contracts are built in
/// code: a feature that needed a prompt template re-release would be one a pack
/// could not ship, and the templates are the adapter's, shared by every
/// workspace on that language.
struct Continuity<'a> {
    /// 0-based window index.
    index: usize,
    total: usize,
    /// Every earlier part's summary, oldest first. Empty for the first part,
    /// which is the only part with nothing behind it.
    plot: &'a [String],
}

impl Continuity<'_> {
    /// `---PART 2 OF 5---`, and what it means for the round being asked for.
    fn note(&self, pass: Pass) -> String {
        let (index, total) = (self.index + 1, self.total);
        match pass {
            Pass::Attribution => format!(
                "---PART {index} OF {total}---\n\
                 This chapter is longer than one pass can carry, so it is staged in {total} parts \
                 and you are seeing part {index}. A later pass sees the events after yours, and the \
                 finished script is assembled from every part's answer. Answer for the events in \
                 front of you and nothing else: do not summarise the chapter, do not round it off, \
                 and do not write an ending, because the prose in front of you continues past your \
                 last event.\n\
                 Return one more field beside the contract above:\n\
                 \x20 \"summary\": \"2-4 sentences on what this part establishes — who speaks, where \
                 it happens, what changes — written for the pass after yours, which has not seen \
                 these events and cannot look them up\"\n"
            ),
            Pass::Staging => format!(
                "---PART {index} OF {total}---\n\
                 This is part {index} of {total} of one chapter, and the events in front of you are \
                 all you stage. Two rules change, and only these two:\n\
                 - Do not round the prose off. The story continues past your last event, so write \
                 no ending and no closing beat.\n\
                 - A `loop`ed bed may run past the end of your part and be closed by a later one. \
                 Close it here if the scene moves on inside your part; leave it open if it does \
                 not, and the gate reads the chapter whole before it complains.\n\
                 `scene` and `music` are carried forward within this part only, so name the place \
                 and the bed again on your first event where they continue what came before.\n"
            ),
        }
    }

    /// The summaries of every part before this one.
    fn plot_so_far(&self) -> String {
        if self.plot.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "\n---PLOT SO FAR--- (parts 1..{}, for reference only — the events above are what you \
             answer for)\n",
            self.index
        );
        for (i, summary) in self.plot.iter().enumerate() {
            out.push_str(&format!("PART {}: {}\n", i + 1, squeeze_ws(summary)));
        }
        out
    }

    /// The whole block, for a prompt that has no placeholder to put it in.
    fn block(&self, pass: Pass) -> String {
        format!("\n{}{}", self.note(pass), self.plot_so_far())
    }
}

/// Append the continuity block to a finished prompt body, or nothing when the
/// chapter was not split.
fn apply_continuity(body: &mut String, continuity: Option<&Continuity>, pass: Pass) {
    // A one-window chapter adds nothing, and has nothing to remove either: a part
    // note only ever arrives with a `Continuity`.
    if let Some(c) = continuity {
        body.push_str(&c.block(pass));
    }
}

/// Build the constrained attribution pass.
///
/// Dialogue detection is not a model decision: `prepare_chapter` has already
/// marked every event, and narration is attached to `Narrator` by code. The
/// chapter is therefore shown as answerable `dialogue_events`, each beside its
/// nearest source narration, plus `narration_ids` the model must not answer. The
/// answer map stays small while the tags that actually identify speakers remain
/// visible. The remaining identity fields are the chapter's own.
///
/// `continuity` is the part this prompt is for when the chapter was split, and
/// `None` for a chapter that fits one call — see [`Continuity`].
fn build_attribution_prompt(
    layout: &Layout,
    bible: &Value,
    prepared: &PreparedChapter,
    continuity: Option<&Continuity>,
    previously: Option<&str>,
) -> Result<String> {
    let path = layout.prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let mut missed: Vec<String> = Vec::new();
    let mut body = template;
    replace_or_miss(
        &mut body,
        "INPUT 2 — one raw chapter text (Vietnamese). Mixes narration and dialogue in \"...\"\nquotes, with pronouns and descriptive aliases instead of names.",
        "INPUT 2 — the prepared chapter as two lists, in exact source order. `narration_ids` are prose events: the preparer has already spoken them as `Narrator` and they are NOT yours to answer. `dialogue_events` are the quoted lines, each with the stable `id` your answer keys on and its text without quote delimiters.",
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "This is the CONTEXT pass: you read one\nchapter and report WHO is in it and WHAT it is about — the cast and the story.\nYou do NOT write the script. A second pass does that, and it is handed your answer\nas its cast list, so be exact about names and about the surface forms the chapter\nuses: everything downstream is resolved against what you return here.",
        "This is the ATTRIBUTION pass: prepared narration and dialogue events are already separated deterministically. Resolve the chapter cast and assign one immutable speaker to every event. You do NOT stage audio, choose music, or write segments; the next pass is handed this exact speaker map.",
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "`roster` is the cast list the second pass must attribute against: canonical\n   names only, plus \"Narrator\" when the chapter has narration.",
        "`roster` is the cast list the next pass consumes: canonical names and the\n   reserved `Anonymous` speaker, plus \"Narrator\" when the chapter has narration.",
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "{bible_json}",
        &bible_context(bible),
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "{chapter_text}",
        &attribution_view(prepared),
        &mut missed,
    );

    if let Some(task) = body.find("TASK:") {
        let end = body
            .find("\nRULES:")
            .filter(|rules| *rules > task)
            .unwrap_or(body.len());
        body.replace_range(
            task..end,
            "TASK: return only the strict attribution JSON defined at the end of this prompt.\n",
        );
    } else {
        missed.push("TASK:".into());
    }
    if !replace_prompt_section(
        &mut body,
        "1. mentions is chapter-local",
        "2. new_characters",
        "1. `mentions` is chapter-local evidence, not a chapter-wide identity table.\n   Include only exact, name-bearing surface forms that identify the same owner\n   wherever they occur. Omit pronouns and context-dependent role or address terms\n   such as `Đồ nhi`, `đệ tử`, `sư tôn`, or `sư phụ`: different scenes in one chapter\n   can give the same form different owners. A mention never determines who speaks\n   a quote; use the explicit tag in the quote's nearby narration first.\n",
    ) {
        missed.push("rule 1 (mentions)".into());
    }
    if !replace_prompt_section(
        &mut body,
        "2. new_characters",
        "3. TITLE:",
        "2. `new_characters` and `new_aliases` are for established proper identities only.\n   A quote whose speaker cannot be identified is an anonymous dialogue speaker, not\n   a new character. Never create a Bible character for a pronoun, generic role, or\n   anonymous passer-by.\n",
    ) {
        missed.push("rule 2 (new_characters)".into());
    }
    warn_missing_sections("attribution prompt", &missed);

    let contract = r#"
---ATTRIBUTION OUTPUT CONTRACT---
Return ONE strict JSON object, never markdown or commentary:
{
  "title": "3-8 word Vietnamese chapter title; do not start it with `Chương`",
  "atmosphere": "1-2 English sentences",
  "excerpt": "2-4 English sentences on the state this chapter ENDS in: who is present, identity reveals (X is Y), disguises, deaths, and any stranger the prose still has not named — written for the NEXT chapter's analyzer, who has not seen this chapter and resolves its cast against it. State, not plot.",
  "roster": ["Narrator", "canonical character name", "Anonymous"],
  "mentions": {"exact name-bearing source form": "canonical character name"},
  "new_characters": [{
    "name": "canonical proper name",
    "personality": "optional English trait",
    "voice_hint": "optional free-form English description",
    "tags": ["optional lowercase single tokens"],
    "proper_aliases": []
  }],
  "new_aliases": {},
  "not_speech": ["e0012"],
  "speakers": {
    "e0002": "canonical character name",
    "e0003": "Anonymous",
    "e0012": "Narrator"
  }
}

The prepare step's split is authoritative for WHERE the quote marks are, not for
WHAT they contain: `narration_ids` are already spoken by `Narrator` and are
attached by code, so return exactly one `speakers` entry for every
`dialogue_events` id, in source order, and nothing else — no narration ids,
no invented ids, no dropped line.
- Every `dialogue_events` id maps to a canonical character name or the reserved
  name `Anonymous`. Dialogue must NEVER map to Narrator, even when the speaker is
  uncertain, even for a greeting, and even when nobody in the line is named.
- The ONE exception: a quoted span that is not somebody talking. A title, a
  technique, a term, a panel label, a song name — `cuốn sách "Khải hoàn"`, a
  quoted skill in a system panel, `Tràng "cuồng phong bạo vũ"`. The preparer
  called it dialogue only because a quote mark opened it; it has to be given to
  somebody, and that somebody would be invented. Judge these from the context
  beside the span, never from the words alone: a title sits inside prose that
  continues the sentence on both sides, while a real speech is followed by a
  tag (`hắn hỏi`) or stands alone as a person's line. When a span is one, list
  its id in `not_speech`; the code then reads it as narration and ignores
  whatever `speakers` says about it, so list an id only when you mean it.
  Retracting the last line a speaker had makes that speaker unused, so drop it
  from `roster` in the same answer — a roster entry nobody speaks is refused.
  Quote marks alone decide nothing here: the same words spoken aloud
  (`"Ngươi đọc 'Yêu Đại Giới' chưa?"`) are real dialogue and stay with a
  character.
- A quoted hail that names only the person it is addressed to — `\"Dịch sư
  phụ.\"`, `\"Sư tôn.\"`, `\"Đồ nhi!\"` — is spoken BY someone else TO that
  person, so it is a person and never Narrator. If no cast member is tagged
  saying it, it is crowd dialogue: give it `Anonymous`. A name occurring inside
  a quote is never the reason to choose a speaker.
- Two dialogue lines may spell out exactly the same text — a street hailing the
  same person twice on two consecutive lines. They are two events with two ids:
  answer both, and never merge, drop, or reuse one answer for the other.
- `Anonymous` is the one speaker for a person the source never names — a street
  crowd, a shopkeeper, a voice in the dark. Every unnamed speaker is `Anonymous`;
  never number them, never invent a second one, and never describe them as a
  character. Someone unnamed is not a Bible character and `Anonymous` never
  appears in `mentions`. Choose a named cast member whenever the dialogue tag,
  self-reference or surrounding action identifies one — `Anonymous` is the
  answer to "nobody is named", not to "I am unsure".
- Resolve the speaker in this order: an explicit named dialogue tag in the
  event immediately AFTER the quote; then a tag in the event immediately BEFORE
  it; then self-reference, action, and the wider scene. A following tag such as
  `Lạc Lan Tuyết vội vàng hỏi.` proves the preceding quote is hers, even when
  the quote only addresses `Sư tôn`. The context objects beside each quote are
  evidence for that id; they are never themselves speaker-map entries.
- Never attribute by the addressee, by a name merely occurring inside the quote,
  or by a chapter-wide `mentions` entry. `Đồ nhi`, `đệ tử`, `sư tôn`, and similar
  forms are scenario-dependent: the same word can address different people even
  inside one chapter. Omit such ambiguous forms from `mentions`.
- Quoted game-system notifications are dialogue for the canonical `Hệ thống`
  character when the bible contains it; prose about the system remains narration.
- `roster` contains Narrator when narration exists, every named speaker used, and
  `Anonymous` when the chapter has an unnamed speaker. It must not contain a
  character who never speaks.
- Correctness priority is `speakers` first, title second, and cast metadata last.
  A named speaker omitted from `new_characters` is synthesized by code. Never emit
  a nameless character object. `mentions` is optional evidence; omit uncertain
  rows rather than inventing an owner. Free-form `voice_hint` text is accepted.
"#;
    apply_continuity(&mut body, continuity, Pass::Attribution);
    // The one cross-chapter memory the attribution pass gets. Identity is the
    // bible's business (names, aliases), but the bible holds no *events*: a
    // stranger the prose has not named yet, a reveal, a disguise still on —
    // that is what the previous excerpt carries, and what an analyzer
    // without it resolves by guessing. Absent means the block is not
    // appended at all, so a first chapter or an out-of-order one is the
    // pre-excerpt prompt byte for byte.
    if let Some(previously) = previously {
        body.push_str(&format!(
            "\n---PREVIOUSLY--- (the chapter before this one; identity context only — resolve \
             names and strangers against it, but answer only for the events in front of you)\n\
             {previously}\n"
        ));
    }
    Ok(format!("{body}\n{contract}"))
}

/// The previous chapters' excerpts, as the attribution prompt's memory.
///
/// Depth is `excerpt_window` from settings — 1 is chapter *n−1* only, 0 is
/// off — and each excerpt is read from the stored script of the chapter it
/// summarizes. A chapter with no stored predecessor (the first one, an
/// out-of-order one, a book digested before the field existed) contributes
/// nothing: fewer lines, not a failure, the same "if any" the bible's own
/// partial order has always had.
fn previous_excerpts(layout: &Layout, n: u32) -> Option<String> {
    let window = Settings::load(&layout.settings()).excerpt_window;
    if window == 0 {
        return None;
    }
    let mut lines = Vec::new();
    for d in 1..=window {
        let m = n.saturating_sub(d);
        if m == 0 {
            break;
        }
        let Ok(script) = crate::read_json::<Value>(&layout.script(m)) else {
            continue;
        };
        let Some(excerpt) = script.get("excerpt").and_then(Value::as_str) else {
            continue;
        };
        if excerpt.trim().is_empty() {
            continue;
        }
        lines.push(format!("CH {m}: {excerpt}"));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Build the audio-staging pass. Speaker assignment is supplied as immutable
/// data and the model never returns it; code attaches it after generation.
///
/// `continuity` is the part this prompt is for when the chapter was split, and
/// `None` for a chapter that fits one call — see [`Continuity`].
fn build_staging_prompt(
    layout: &Layout,
    engine: &str,
    bible: &Value,
    context: &Value,
    prepared: &PreparedChapter,
    continuity: Option<&Continuity>,
) -> Result<String> {
    let path = layout.script_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let map = load_map(layout)?;
    let palette = crate::ambience::palette_prompt(&map);
    let scene_words = crate::ambience::scene_prompt(&map);
    let pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let effects = crate::ambience::effect_tags(&pool).join(", ");
    let injects = crate::ambience::inject_prompt(&crate::audio_pool::load_pool(
        &layout.assets().join("inject-pool.json"),
    ));
    let mut missed: Vec<String> = Vec::new();
    // The acting-mood vocabulary, rendered from the one table the mixer reads
    // (`assemble::MOOD_TAKE`) so the prompt can only offer words `mood_cluster`
    // resolves. It was previously written into the file's TASK block, which the
    // override below deletes — so the analyzer saw no mood list at all and any
    // coined word silently fell back to `neutral`.
    let mood_palette = crate::assemble::mood_palette();
    let mut body = template;
    replace_or_miss(
        &mut body,
        "INPUT 2 — the CAST of THIS chapter, already resolved by the context pass, and the\nonly speaker labels you may use. `mentions` maps every surface form the chapter\nuses to its canonical name — use it ONLY to resolve WHO a dialogue tag names,\nnever to decide who speaks a line: a sentence merely containing \"nàng\" or a\ncharacter's name is not spoken by them. Never invent a speaker who is\nnot on the cast list.",
        "INPUT 2 — the chapter cast and `fixed_speakers`, the complete immutable source-id to speaker map returned by the attribution pass. Do not infer, change, or return a speaker.",
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "INPUT 3 — one raw chapter text (Vietnamese). Mixes narration and dialogue in \"...\"\nquotes, with pronouns and descriptive aliases instead of names.",
        "INPUT 3 — the same prepared source events shown to the attribution pass. `kind` is authoritative; speakers are already fixed.",
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "{bible_json}",
        &bible_context(bible),
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "{cast_json}",
        &cast_context(context),
        &mut missed,
    );
    replace_or_miss(&mut body, "{music_palette}", &palette, &mut missed);
    // `{mood_palette}` is new: profiles written before it lack the placeholder
    // and that is not a miss worth warning about, so it is replaced outright.
    body = body.replace("{mood_palette}", &mood_palette);
    // `{effect_tags}` is deprecated: the effect layer reads `scene` labels, and
    // rule 9 that used this placeholder is gone. Profiles written before that
    // still carry it, so it is replaced outright rather than reported as a miss.
    body = body.replace("{effect_tags}", &effects);
    // Same for `{scene_words}`, and for the same reason plus a second: this path
    // shares its template with the script pass, so a placeholder only the other
    // one replaced would survive into the prompt as a literal the model copies.
    body = body.replace("{scene_words}", &scene_words);
    replace_or_miss(&mut body, "{inject_sounds}", &injects, &mut missed);
    replace_or_miss(
        &mut body,
        "{chapter_text}",
        &prepared.prompt_json,
        &mut missed,
    );
    render_nonverbal(&mut body, engine, &mut missed);
    body = body.replace("{\"speaker\": \"Narrator\", ", "{\"");
    if let Some(task) = body.find("TASK:") {
        let end = body
            .find("\nRULES:")
            .filter(|rules| *rules > task)
            .unwrap_or(body.len());
        body.replace_range(
            task..end,
            "TASK: return only the strict staging JSON defined at the end of this prompt.\n",
        );
    } else {
        missed.push("TASK:".into());
    }
    if !replace_prompt_section(
        &mut body,
        "1. Split on speaker turns:",
        "4. Keep segments short for TTS:",
        "1. Cover every prepared source event exactly once and in source order. A source\n   event may be split into consecutive segments for a long TTS line or a sound seam;\n   every split carries the same `source_id`. Never merge source events.\n2. `kind` is already decided. Use it only to understand the text. Do not return a\n   `kind`, `speaker`, roster, cast, or attribution field; the immutable map in INPUT 2\n   is attached by code after you return.\n3. Narrate every word exactly once. Never include the source headline. A dialogue\n   event and its surrounding narration are already separate source events.\n",
    ) {
        missed.push("rules 1-3 (staging)".into());
    }
    warn_missing_sections("staging prompt", &missed);

    let contract = r#"
---STAGING OUTPUT CONTRACT---
Return ONE strict JSON object containing only `segments` and `fixes`:
{
  "segments": [{
    "source_id": "e0001",
    "text": "exact speakable text for this source event",
    "mood": "one token from the mood palette below",
    "scene": "English place-time label",
    "music": "one token from the music palette in rule 8",
    "sound_after": "sound name or none",
    "stop_after": "sound name or none"
  }],
  "fixes": []
}

Emit a field only where it CHANGES. `mood`, `scene` and `music` are carried
forward: omit the key and the previous segment's value is used. `text` is
carried forward too — omit it when you neither split the source event nor fix a
typo, and code fills it from the prepared event. A value you do emit replaces
the carried one for that segment and every segment after it until the next.

---MOOD PALETTE--- (one token, copied exactly)
{mood_palette}

Do not return `speaker`: `fixed_speakers` is authoritative and code attaches it.
The staging pass may split a source event but may never change its identity,
drop an event, duplicate one, or move one out of source order.

A split PARTITIONS its event: the halves, in source order, concatenated, must
spell the source text exactly. Never repeat the whole line on both halves, and
never drop words — a split exists to put a sound seam between two different
halves of one line. Two different source events may carry identical text (a
street crowd hailing the same phrase on two lines); that is two events, not a
duplicate — answer each, and never merge them.

Follow every audio, grammar, TTS, music and sound rule in this prompt.
"#;
    let contract = contract.replace("{mood_palette}", &mood_palette);
    apply_continuity(&mut body, continuity, Pass::Staging);
    Ok(format!("{body}\n{contract}"))
}

// ---------------------------------------------------------------------------
// parts: a chapter staged in more than one call
// ---------------------------------------------------------------------------

/// One part of a chapter, staged.
///
/// `from`/`to` are the window's event bounds, kept so a stored part can be
/// checked against the plan it would be resumed into: a part is reusable only
/// where the current plan puts the same events in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Part {
    from: usize,
    to: usize,
    /// What this part established, in the attribution answer's own words. It is
    /// the whole of what the parts after it know about it.
    summary: String,
    context: Value,
    script: Value,
}

/// The on-disk shape of a half-staged chapter.
#[derive(Serialize, Deserialize)]
struct StoredParts {
    key: String,
    parts: Vec<Part>,
}

/// The parts of one chapter that are already staged, and the file that survives
/// a restart.
///
/// A long chapter is up to sixteen calls, so losing the last one to a rate limit
/// or a closed laptop costs the fifteen before it. The parts *are* the answer:
/// each one is written only after both its rounds parsed and validated, so what a
/// restart resumes from is work that would have been accepted — never work in
/// progress, which is why a resumed part is never re-validated.
struct Parts {
    done: Vec<Part>,
    path: PathBuf,
    key: String,
    /// Whether there is a boundary to resume from at all. A one-window chapter
    /// has none — its two rounds are one part, and a part is stored when it is
    /// *finished* — so it never writes the file.
    store: bool,
}

impl Parts {
    /// Open the checkpoint for one chapter, keeping the leading run of stored
    /// parts that still match the plan.
    fn open(
        layout: &Layout,
        n: u32,
        text: &str,
        bible: &Value,
        windows: &[Window],
        settings: &Settings,
    ) -> Parts {
        let path = layout.data().join(format!(".digest-parts-ch{n}.json"));
        let key = parts_key(text, bible, windows, settings);
        let store = windows.len() > 1;
        let done = if store {
            load_parts(&path, &key, windows)
        } else {
            Vec::new()
        };
        Parts {
            done,
            path,
            key,
            store,
        }
    }

    fn len(&self) -> usize {
        self.done.len()
    }

    /// Every finished part's summary, oldest first: the `PLOT SO FAR` the next
    /// part is handed.
    fn summaries(&self) -> Vec<String> {
        self.done.iter().map(|p| p.summary.clone()).collect()
    }

    fn push(&mut self, part: Part) -> Result<()> {
        self.done.push(part);
        self.save()
    }

    fn save(&self) -> Result<()> {
        if !self.store {
            return Ok(());
        }
        let stored = StoredParts {
            key: self.key.clone(),
            parts: self.done.clone(),
        };
        atomic_write(&self.path, &serde_json::to_string_pretty(&stored)?)
    }

    /// Forget the checkpoint. Called when the chapter is finished, so a
    /// re-digest — or the operator taking the chapter over by hand — starts
    /// clean instead of resuming into parts of a script that already exists.
    fn clear(&self) {
        if self.store {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The leading run of stored parts the current plan still answers for.
fn load_parts(path: &Path, key: &str, windows: &[Window]) -> Vec<Part> {
    let stored = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<StoredParts>(&text).ok());
    let Some(stored) = stored else {
        return Vec::new();
    };
    if stored.key != key {
        return Vec::new();
    }
    let mut kept = Vec::new();
    for (i, part) in stored.parts.into_iter().enumerate() {
        match windows.get(i) {
            Some(w) if w.from == part.from && w.to == part.to => kept.push(part),
            // A part that does not line up with the plan at its own index means
            // the plan moved, and everything after it answers for a chapter that
            // is no longer this one.
            _ => break,
        }
    }
    kept
}

/// What a stored part has to match to be reusable: the chapter text, the bible
/// it was staged against, and the plan of windows it belongs to.
///
/// Not a security boundary — a **stale-work check**. A chapter edited under a
/// half-finished digest, another chapter's bible merge, or a `chunk_sentences`
/// change all leave stored parts answering a question nobody is asking any more,
/// and the cost of finding that out at the end is every call it was meant to
/// save.
fn parts_key(text: &str, bible: &Value, windows: &[Window], settings: &Settings) -> String {
    let mut h = Sha256::new();
    h.update(b"bm-digest-parts-v1");
    h.update([0]);
    h.update(text.as_bytes());
    h.update([0]);
    h.update(serde_json::to_string(bible).unwrap_or_default().as_bytes());
    h.update([0]);
    for w in windows {
        h.update(format!("{}..{}", w.from, w.to).as_bytes());
        h.update([0]);
    }
    h.update(
        format!(
            "{}-{}-{}",
            settings.digest.chunk_sentences, settings.digest.chunk_chars, settings.digest.answer_tokens
        )
        .as_bytes(),
    );
    format!("{:x}", h.finalize())
}

/// The part a round belongs to, as `(1-based index, total)`, or `None` for a
/// chapter that did not split.
///
/// One place decides, so every label, dump file name, progress line and gate
/// message agrees about whether there are parts at all — and so a one-window
/// chapter's log reads exactly as it did before windows existed.
fn part_of(index: usize, total: usize) -> Option<(usize, usize)> {
    (total > 1).then_some((index + 1, total))
}

/// `part 2/5: ` when a message is about one part of a split chapter, and nothing
/// when it is about the chapter itself.
fn part_prefix(part: Option<(usize, usize)>) -> String {
    match part {
        None => String::new(),
        Some((index, total)) => format!("part {index}/{total}: "),
    }
}

/// `-part2of5` for a dump file's name, and nothing when the chapter did not
/// split. Debug dumps are read by eye next to each other, so the part is in the
/// name rather than only in the file.
fn part_suffix(part: Option<(usize, usize)>) -> String {
    match part {
        None => String::new(),
        Some((index, total)) => format!("-part{index}of{total}"),
    }
}

/// The progress line for one round of one part. A one-window chapter's line is
/// the string it has always been.
fn round_label(n: u32, analyzer: &str, round: &str, part: Option<(usize, usize)>) -> String {
    match part {
        None => format!("digest ch{n} via {analyzer}: {round}"),
        Some((index, total)) => {
            format!("digest ch{n} via {analyzer}: {round} (part {index}/{total})")
        }
    }
}

/// The events a part answers for, as `e0001–e0241`.
fn part_span(prepared: &PreparedChapter, w: &Window) -> String {
    if w.to <= w.from {
        return "no events".into();
    }
    let first = &prepared.events[w.from].id;
    let last = &prepared.events[w.to - 1].id;
    if first == last {
        first.clone()
    } else {
        format!("{first}–{last}")
    }
}

/// One part's prose as a single string, for rule 2's cue scan.
///
/// The part's own events, so a cue can only fail a part that contains it — which
/// is the whole reason rule 2 is checked per part once a chapter has split.
fn window_text(prepared: &PreparedChapter, w: &Window) -> String {
    prepared.events[w.from..w.to]
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plan as one line: what this chapter costs, and why.
fn plan_line(
    n: u32,
    analyzer: &str,
    windows: &[Window],
    prepared: &PreparedChapter,
    settings: &Settings,
) -> String {
    let chars = weight(&prepared.events);
    format!(
        "digest ch{n} via {analyzer}: {} parts — {} events, {chars} chars ≈ {}k tokens of answer \
         against a {}-token budget",
        windows.len(),
        prepared.events.len(),
        tokens(chars) / 1000,
        settings.digest.answer_tokens
    )
}

/// Where the cuts fall, as one line, before any call is made.
fn plan_detail(windows: &[Window], prepared: &PreparedChapter) -> String {
    let spans: Vec<String> = windows.iter().map(|w| part_span(prepared, w)).collect();
    format!("parts: {}", spans.join(", "))
}

/// The plan as the ledger sees it: one line for the chapter and one per part.
///
/// The only place a long chapter's cost is visible. Five parts and sixteen calls
/// produce the same script as one call, and only these lines say which happened
/// — which is what an operator looking at "the digest is slow today" needs.
fn part_lines(windows: &[Window], prepared: &PreparedChapter, settings: &Settings) -> Vec<String> {
    let chars = weight(&prepared.events);
    let mut out = vec![format!(
        "   staged in {} parts: {} events, {chars} chars ≈ {}k tokens of answer against a \
         {}-token budget",
        windows.len(),
        prepared.events.len(),
        tokens(chars) / 1000,
        settings.digest.answer_tokens
    )];
    let total = windows.len();
    for (i, w) in windows.iter().enumerate() {
        out.push(format!(
            "   part {}/{}: {} ({} events, {} chars)",
            i + 1,
            total,
            part_span(prepared, w),
            w.events,
            w.chars
        ));
    }
    out
}

/// The one script a chapter is, out of the parts that made it.
///
/// `segments` and `fixes` concatenate and nothing else does: speakers are
/// attached per part by code, and a fix is a `before`/`after` pair applied to the
/// chapter's text as a whole — which is exactly what a single-call answer's
/// fixes were. Part order is source order, so the merged array is the array one
/// staging answer would have returned.
fn merge_scripts<'a>(scripts: impl IntoIterator<Item = &'a Value>) -> Value {
    let mut segments = Vec::new();
    let mut fixes = Vec::new();
    for script in scripts {
        if let Some(list) = script.get("segments").and_then(Value::as_array) {
            segments.extend(list.iter().cloned());
        }
        if let Some(list) = script.get("fixes").and_then(Value::as_array) {
            fixes.extend(list.iter().cloned());
        }
    }
    json!({"segments": segments, "fixes": fixes})
}

/// What the sound-design gates complain about, for the parts staged so far —
/// with a not-yet-stored answer standing in as a part of its own.
///
/// One function for the worker's final check and the operator's, so the two
/// cannot disagree about whether a chapter is finished. Rule 1 runs on the
/// merged script (a bed opened in one part and closed in the next is closed);
/// then rule 2 runs part by part against that part's own prose. The index that
/// comes back is the part whose prompt answers for the complaint: the part that
/// placed the surviving bed, or the part whose own text stages a cue and whose
/// own segments place none.
///
/// `what` names the subject in rule 2's message — `chapter` when there is one
/// part, `part` when there are more — the same way `classify_fetch` is told the
/// noun it is describing.
fn sound_gap(
    scripts: &[&Value],
    texts: &[&str],
    pool: &crate::audio_pool::ClipPool,
    what: &str,
) -> Option<(String, usize)> {
    // A chapter that did not split goes through [`sound_design_gap`] itself, in
    // its own order, so the single-call digest's gate is the same code it always
    // was rather than a re-implementation that agrees with it today.
    if scripts.len() <= 1 {
        let script = scripts.first().copied().unwrap_or(&Value::Null);
        let text = texts.first().copied().unwrap_or("");
        return sound_design_gap(script, text, pool).map(|gap| (gap, 0));
    }
    let merged = merge_scripts(scripts.iter().copied());
    if let Some(bed) = open_beds(&merged, pool).into_iter().next() {
        let owner = bed_owner(&bed, scripts);
        return Some((unclosed_beds(&merged, pool).unwrap_or_default(), owner));
    }
    for (i, script) in scripts.iter().enumerate() {
        let text = texts.get(i).copied().unwrap_or("");
        if let Some(gap) = silent_design(script, text, what) {
            return Some((gap, i));
        }
    }
    None
}

/// The identity half of a chapter staged in parts: what the parts' attribution
/// answers together say about who is in it.
///
/// `title` and `atmosphere` are the **first** part's, because a chapter's title
/// and its opening mood are set by its opening; the summaries carry the rest.
/// The **excerpt is the last** non-empty part's, for the opposite reason: it
/// is a statement of the state the chapter *ends* in, and the last part is
/// the only author that has seen the whole arc — its own slice plus the plot
/// the earlier parts handed it. Everything else is a union — `roster` in
/// first-seen order, `mentions`, `new_aliases` and `speakers` by key,
/// `new_characters` by canonical name with a later part filling only the
/// fields an earlier one left blank. A one-window chapter goes through this
/// too and comes out with exactly what its single answer said.
///
/// Disagreements are returned rather than silently resolved. Two parts naming
/// different owners for one surface form is the one thing a union cannot fix
/// (`mentions` is a map, and a map holds one value per key), and it is a real
/// signal: the alias table owes somebody a decision. The earlier part wins, and
/// the operator is told.
fn merge_contexts(parts: &[Part]) -> (Value, Vec<String>) {
    let mut title = String::new();
    let mut atmosphere = String::new();
    let mut excerpt = String::new();
    let mut roster: Vec<String> = Vec::new();
    let mut characters: Vec<Value> = Vec::new();
    let mut mentions = serde_json::Map::new();
    let mut aliases = serde_json::Map::new();
    let mut speakers = serde_json::Map::new();
    let mut conflicts = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let c = &part.context;
        if title.is_empty() {
            title = c
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        if atmosphere.is_empty() {
            atmosphere = c
                .get("atmosphere")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        // Last non-empty wins: the excerpt describes the chapter's end
        // state, and only the last part has seen the whole arc.
        if let Some(e) = c.get("excerpt").and_then(Value::as_str) {
            if !e.trim().is_empty() {
                excerpt = e.to_string();
            }
        }
        union_names(&mut roster, c.get("roster"));
        union_map(
            &mut mentions,
            c.get("mentions"),
            "mention",
            i + 1,
            &mut conflicts,
        );
        union_map(
            &mut aliases,
            c.get("new_aliases"),
            "alias",
            i + 1,
            &mut conflicts,
        );
        if let Some(map) = c.get("speakers").and_then(Value::as_object) {
            for (id, who) in map {
                speakers.insert(id.clone(), who.clone());
            }
        }
        union_characters(&mut characters, c.get("new_characters"));
    }
    let merged = json!({
        "title": title,
        "atmosphere": atmosphere,
        "excerpt": excerpt,
        "roster": roster,
        "mentions": Value::Object(mentions),
        "new_characters": characters,
        "new_aliases": Value::Object(aliases),
        "speakers": Value::Object(speakers),
    });
    (merged, conflicts)
}

/// Union of a name list: first-seen order, no duplicates.
fn union_names(into: &mut Vec<String>, from: Option<&Value>) {
    for name in from
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !into.iter().any(|seen| seen == name) {
            into.push(name.to_string());
        }
    }
}

/// Union of a surface-form map: the first part's owner wins, and the
/// disagreement is named.
fn union_map(
    into: &mut serde_json::Map<String, Value>,
    from: Option<&Value>,
    what: &str,
    part: usize,
    conflicts: &mut Vec<String>,
) {
    for (form, owner) in from.and_then(Value::as_object).into_iter().flatten() {
        match into.get(form) {
            None => {
                into.insert(form.clone(), owner.clone());
            }
            Some(previous) if previous == owner => {}
            Some(previous) => conflicts.push(format!(
                "{what} {form:?} is {previous} here and {owner} in part {part} — the earlier part \
                 wins, and the alias table owes one of them a decision"
            )),
        }
    }
}

/// Union of the declared new characters, by canonical name.
///
/// A later part fills fields an earlier one left blank and never overwrites one
/// it stated: two parts describing the same new character is the ordinary case,
/// and the earlier description is the one written closest to meeting them.
fn union_characters(into: &mut Vec<Value>, from: Option<&Value>) {
    for candidate in from.and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = candidate.get("name").and_then(Value::as_str) else {
            continue;
        };
        let existing = into
            .iter_mut()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name));
        let Some(existing) = existing else {
            into.push(candidate.clone());
            continue;
        };
        let (Some(target), Some(source)) = (existing.as_object_mut(), candidate.as_object()) else {
            continue;
        };
        for (key, value) in source {
            if field_is_blank(target, key) {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Which part to re-ask when a looping bed is still open at the end of a
/// chapter: the part that placed the last `sound` for it.
///
/// The last, not the first, because a bed can be closed and opened again — the
/// surviving copy is the one nobody stopped, and a repair aimed at an earlier
/// copy would go to a model whose own part is already correct.
fn bed_owner(bed: &str, scripts: &[&Value]) -> usize {
    let mut owner = 0;
    for (i, script) in scripts.iter().enumerate() {
        let placed = script
            .get("segments")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("sound").and_then(Value::as_str) == Some(bed))
            });
        if placed {
            owner = i;
        }
    }
    owner
}

/// One part's two rounds: attribution, then staging against that map.
///
/// **The rounds do not change because a chapter was split.** Same prompt
/// builders, same validators, same one-repair rule, same retry policy — only the
/// events in front of the model differ, plus the continuity block that tells it
/// there are other parts. A part is therefore staged to exactly the standard a
/// whole chapter was, which is what makes the merged script indistinguishable
/// from one the single-call digest would have written.
#[allow(clippy::too_many_arguments)]
async fn stage_part(
    layout: &Layout,
    n: u32,
    analyzer: &str,
    settings: &Settings,
    bible: &Value,
    vocab: &Vocabulary,
    slice: &PreparedChapter,
    continuity: Option<&Continuity<'_>>,
    part: Option<(usize, usize)>,
    progress: &mut (dyn FnMut(f32, String) + Send),
    from: f32,
    mid: f32,
    to: f32,
) -> Result<(Value, Value)> {
    let suffix = part_suffix(part);
    progress(from, round_label(n, analyzer, "attribution", part));
    let previously = previous_excerpts(layout, n);
    let attribution_prompt =
        build_attribution_prompt(layout, bible, slice, continuity, previously.as_deref())?;
    let raw = generate_retrying(&attribution_prompt, analyzer, settings, progress, from, mid).await?;
    dump_raw(layout, &format!("digest-attribution{suffix}"), &raw);
    let context = match parse_attribution(&raw, bible, slice, continuity.is_some()) {
        Ok(context) => context,
        Err(e) => {
            progress(
                mid,
                format!(
                    "{}invalid attribution, asking for one repair: {e}",
                    part_prefix(part)
                ),
            );
            let again = repair_once(&attribution_prompt, &e, analyzer, settings).await?;
            dump_raw(layout, &format!("digest-attribution{suffix}-retry"), &again);
            parse_attribution(&again, bible, slice, continuity.is_some()).map_err(|e2| {
                let dump = layout.data().join(".last-analyze-raw.json");
                let _ = atomic_write(&dump, &again);
                anyhow::anyhow!(
                    "digest attribution invalid ({e2}); raw saved to {}",
                    dump.display()
                )
            })?
        }
    };

    progress(mid, round_label(n, analyzer, "staging", part));
    let staging_prompt = build_staging_prompt(
        layout,
        &settings.engine,
        bible,
        &context,
        slice,
        continuity,
    )?;
    let raw = generate_retrying(&staging_prompt, analyzer, settings, progress, mid, to).await?;
    dump_raw(layout, &format!("digest-staging{suffix}"), &raw);
    let parse = |raw: &str| parse_staged_script(raw, bible, &context, slice, vocab);
    let script = match parse(&raw) {
        Ok(script) => script,
        Err(e) => {
            progress(
                to,
                format!(
                    "{}invalid staging, asking for one repair: {e}",
                    part_prefix(part)
                ),
            );
            let again = repair_once(&staging_prompt, &e, analyzer, settings).await?;
            dump_raw(layout, &format!("digest-staging{suffix}-retry"), &again);
            parse(&again).map_err(|e2| {
                let dump = layout.data().join(".last-analyze-raw.json");
                let _ = atomic_write(&dump, &again);
                anyhow::anyhow!(
                    "digest staging invalid ({e2}); raw saved to {}",
                    dump.display()
                )
            })?
        }
    };
    Ok((context, script))
}

/// Re-ask one part's staging round with a gate's complaint appended.
///
/// The prompt is re-rendered rather than carried: a part's prompt is a few
/// hundred KB of string, and two `read_to_string`s cost less than holding every
/// part's prompt for the length of a chapter. It is rendered from **that part's
/// own slice and cast**, so a repair can only ever answer for events it was
/// shown, and its answer goes through the same validators every other answer
/// does.
#[allow(clippy::too_many_arguments)]
async fn reask_staging(
    layout: &Layout,
    analyzer: &str,
    settings: &Settings,
    bible: &Value,
    vocab: &Vocabulary,
    slice: &PreparedChapter,
    context: &Value,
    index: usize,
    total: usize,
    plot: &[String],
    part: Option<(usize, usize)>,
    complaint: &str,
) -> Result<Value> {
    let continuity = (total > 1).then_some(Continuity {
        index,
        total,
        plot,
    });
    let prompt = build_staging_prompt(
        layout,
        &settings.engine,
        bible,
        context,
        slice,
        continuity.as_ref(),
    )?;
    let again = repair_once(
        &prompt,
        &anyhow::anyhow!(complaint.to_string()),
        analyzer,
        settings,
    )
    .await?;
    dump_raw(layout, &format!("digest-staging-retry{}", part_suffix(part)), &again);
    parse_staged_script(&again, bible, context, slice, vocab).map_err(|e| {
        let dump = layout.data().join(".last-analyze-raw.json");
        let _ = atomic_write(&dump, &again);
        anyhow::anyhow!(
            "digest {complaint} not fixed by one repair ({e}); raw saved to {}",
            dump.display()
        )
    })
}

/// Ask the analyzer for one chapter through two constrained passes, once per
/// window when the chapter is too long for one answer to carry.
///
/// Attribution is generated and validated first. The staging pass receives that
/// map as data and never emits speakers, so a small model cannot regress a
/// mechanically separated dialogue event back to Narrator while it is choosing
/// scenes and sounds.
///
/// That is the whole contract, and splitting the chapter does not weaken it: a
/// window is a contiguous run of the same prepared events, the two rounds run on
/// it unchanged, and the parts are merged back into one script and one bible
/// delta. See [`window`] for where the cuts fall and [`Parts`] for what survives
/// a restart.
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
    let prepared = prepare_chapter(&text);
    let vocab = vocabulary(layout)?;
    let windows = plan_windows(&prepared, &settings.digest);
    let total = windows.len();
    let mut parts = Parts::open(layout, n, &text, bible, &windows, settings);
    if total > 1 {
        // Before any call, because the number of calls is the operator's
        // business: a 40 KB chapter is sixteen of them, not two, and a digest
        // that looks stuck is only diagnosable once the plan said so.
        progress(
            0.05,
            plan_line(n, analyzer, &windows, &prepared, settings),
        );
        progress(0.06, plan_detail(&windows, &prepared));
        if parts.len() > 0 {
            progress(
                0.07,
                format!(
                    "resuming at part {} of {total} from the checkpoint",
                    parts.len() + 1
                ),
            );
        }
    }

    // 0.08..0.82, spent evenly across the parts, so the bar moves at the same
    // rate whether this chapter is one part or sixteen. Within a part the two
    // rounds split the band: attribution first, and staging — the longer and
    // more expensive of the two — the rest of it.
    let per = 0.74 / total as f32;
    for (i, window) in windows.iter().enumerate().skip(parts.len()) {
        let at = part_of(i, total);
        let from = 0.08 + i as f32 * per;
        let mid = from + per * 0.45;
        let to = from + per;
        let summaries = parts.summaries();
        let continuity = (total > 1).then_some(Continuity {
            index: i,
            total,
            plot: &summaries,
        });
        let slice = window.prepared(&prepared);
        let (context, script) = stage_part(
            layout,
            n,
            analyzer,
            settings,
            bible,
            &vocab,
            &slice,
            continuity.as_ref(),
            at,
            progress,
            from,
            mid,
            to,
        )
        .await?;
        // Stored only once both rounds parsed and validated, so what a restart
        // resumes from is work that would have been accepted — not work in
        // progress, which is why a resume never has to re-validate a part.
        let summary = context
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        parts.push(Part {
            from: window.from,
            to: window.to,
            summary,
            context,
            script,
        })?;
    }

    // ---- the sound-design gates, once every part is staged -----------------
    //
    // Rule 1 (a looping bed opened and never stopped) is a fact about the
    // **whole chapter**, so it is checked on the merged segments: a bed opened
    // at the end of one part and closed at the start of the next is closed, and
    // a per-part check would refuse exactly the long scene this feature exists
    // for. Its repair goes to the part that placed the surviving `sound`.
    //
    // Rule 2 (the text stages sounds and the script places none) is a fact about
    // one **part**, so it is checked part by part against that part's own prose —
    // a cue can only fail the text that contains it. For a chapter that did not
    // split, this is `sound_design_gap`'s two rules in their original order
    // against the whole chapter, which is what keeps the single-call digest's
    // behaviour intact.
    //
    // The cue scan is a heuristic about prose, not a contradiction: an idiom
    // trips it with nothing staged, and a gate that can never be satisfied is a
    // deadlock — every repair burns an LLM call and the chapter refuses 100% of
    // correct answers. So the block decays: 90%, then -25% per consecutive
    // failure, and below a coin flip the script is accepted with a loud warning
    // instead of refused. Deterministic (no dice): the same chapter always takes
    // the same path, and the loop always terminates within four evaluations.
    // An unclosed bed is still corruption rather than judgment — but the decay
    // covers its repair too, because a model that cannot close a bed after four
    // asks is not going to on the fifth, and a refused chapter helps nobody.
    let what = if total == 1 { "chapter" } else { "part" };
    // Each part's own prose, for rule 2 — or the chapter's own text when it did
    // not split, which is the text the single-call digest has always scanned.
    let texts: Vec<String> = if total == 1 {
        vec![text.clone()]
    } else {
        windows.iter().map(|w| window_text(&prepared, w)).collect()
    };
    let mut soft_released: Option<String> = None;
    let mut attempt = 0u32;
    loop {
        let scripts: Vec<&Value> = parts.done.iter().map(|p| &p.script).collect();
        let scopes: Vec<&str> = texts.iter().map(String::as_str).collect();
        let Some((gap, owner)) = sound_gap(&scripts, &scopes, &vocab.injects, what) else {
            break;
        };
        let at = part_of(owner, total);
        progress(
            0.88,
            format!(
                "{}sound design incomplete, asking for repairs: {gap}",
                part_prefix(at)
            ),
        );
        // 0.90, 0.68, 0.51, then 0.38: below a coin flip, accept.
        if gap_block_p(attempt) < 0.5 {
            let msg = format!(
                "ch{n} {}sound-design gate soft-released ({gap})",
                part_prefix(at)
            );
            progress(0.88, format!("WARN: {msg}"));
            eprintln!("WARN: {msg}");
            soft_released = Some(msg);
            break;
        }
        let summaries = parts.summaries();
        let slice = windows[owner].prepared(&prepared);
        let script = reask_staging(
            layout,
            analyzer,
            settings,
            bible,
            &vocab,
            &slice,
            &parts.done[owner].context,
            owner,
            total,
            &summaries,
            at,
            &gap,
        )
        .await?;
        parts.done[owner].script = script;
        attempt += 1;
    }

    let (context, conflicts) = merge_contexts(&parts.done);
    let script = merge_scripts(parts.done.iter().map(|p| &p.script));
    let mut outcome = assemble_outcome(bible, &context, &script, &text)?;
    // A disagreement between parts is the operator's to settle, so it is said
    // out loud rather than resolved quietly: `mentions` is a map, a map holds one
    // value per key, and two parts naming different owners for one surface form
    // is a decision the alias table owes somebody.
    for w in conflicts {
        outcome.log.push(format!("   WARN: {w}"));
        outcome.warnings.push(w);
    }
    // `warnings` is dropped by the agent today; `log` is what the worker
    // prints, so the release lands in both — one for future readers, one
    // for the operator watching now.
    if let Some(w) = soft_released {
        outcome.log.push(format!("   WARN: {w}"));
        outcome.warnings.push(w);
    }
    if total > 1 {
        // The plan, in the log the worker prints: the only place the cost of a
        // long chapter is visible. Five parts and sixteen calls are the same
        // script as one call, and only these lines say which one happened.
        for (i, line) in part_lines(&windows, &prepared, settings)
            .into_iter()
            .enumerate()
        {
            outcome.log.insert(1 + i, line);
        }
    }
    // From here the chapter is a finished script and `digest_chapter` writes it,
    // so the checkpoint has done its job. Cleared rather than left: a re-digest,
    // or the operator taking the chapter over by hand, has to start clean
    // instead of resuming into parts of a script that already exists.
    parts.clear();
    progress(1.0, format!("digest ch{n} done"));
    Ok(outcome)
}

/// Everything after the two answers have parsed: merge the rounds, check the
/// grammar fixes against the chapter, build the script and the bible delta, and
/// describe what came out.
///
/// **Shared by the worker's automatic path and the operator's manual one, and
/// that is the point.** The manual route exists to be *the same digest* with a
/// person standing in for the model, not a second, looser one. One function,
/// rather than two that agree today. The `sound_design_gap` check stays in
/// the callers, because they answer it differently: the worker asks the model
/// again, the operator is told and gets to paste a better answer.
fn assemble_outcome(
    bible: &Value,
    context: &Value,
    script: &Value,
    text: &str,
) -> Result<DigestOutcome> {
    let data = merge_rounds(context, script);

    let mut log = Vec::new();
    // First line, before anything the model said. The split is decided from the
    // text alone, so this is the earliest a bad crawl is visible, and the only
    // place a *silent* one is, since a chapter with no dialogue has nothing for
    // any validator to object to. It is here rather than in the automatic path
    // because the manual path is where a person is standing there able to act
    // on it, and both paths need the same answer.
    log.push(prepare_chapter(text).split_summary());
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
        // this, so it is the mp3's filename *and* the spoken headline, one
        // value, two consumers, no chance of them disagreeing.
        "title": data.get("title").cloned().unwrap_or(json!("")),
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
        // The chapter's end-state summary, the next chapter's attribution
        // prompt reads back as ---PREVIOUSLY---. Lives in the script root
        // beside title and atmosphere: no new file, and a re-digest of this
        // chapter rewrites it where the next one reads it.
        "excerpt": data.get("excerpt").cloned().unwrap_or(json!("")),
        "roster": data.get("roster").cloned().unwrap_or(json!([])),
        "mentions": data.get("mentions").cloned().unwrap_or(json!({})),
        "speakers": data.get("speakers").cloned().unwrap_or(json!({})),
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
        "speakers": data.get("speakers").cloned().unwrap_or(json!({})),
        "segments": script.get("segments").cloned().unwrap_or(json!([])),
    });

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
    write_script(layout, n, &out.script)?;
    out.log.push(format!("-> {}", layout.script(n).display()));
    Ok(out)
}

/// Which half of the two-round digest an answer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    Cast,
    Script,
}

impl Round {
    pub fn as_str(self) -> &'static str {
        match self {
            Round::Cast => "cast",
            Round::Script => "script",
        }
    }
}

/// Which part of a chapter a manual round is for.
///
/// `None` on every chapter that fits one answer, which is what the prompt's
/// `part` field holds for a short chapter — so a manual digest of a chapter that
/// did not split is the two-round gesture it has always been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualPart {
    /// 1-based.
    pub index: usize,
    pub total: usize,
}

/// The prompt for one manual round, ready to be carried to any model.
#[derive(Debug, Clone)]
pub struct ManualPrompt {
    pub round: Round,
    pub text: String,
    /// The part this round is for, when the chapter is staged in parts. The
    /// front end shows it (`part 2/3`), because an operator pasting into a long
    /// chapter has to know how many rounds it still owes.
    pub part: Option<ManualPart>,
}

/// What a pasted answer produced.
#[derive(Debug, Clone)]
pub struct ManualAnswer {
    /// The next prompt to ask, when there is one: round 2 after a cast answer,
    /// or round 1 of the next part after a part's script.
    pub prompt: Option<ManualPrompt>,
    /// Round 1's validated cast, set exactly when `prompt` is round 2: it is
    /// what round 2 was rendered against and what its answer is checked
    /// against, so the caller has to carry it forward.
    pub cast: Option<Value>,
    /// Every part staged and merged: the finished chapter. Never set together
    /// with `prompt`.
    pub outcome: Option<DigestOutcome>,
}

/// The vocabulary the script validators check against, read from the same files
/// the prompt was rendered from.
///
/// One loader for both directions, so what the prompt *offered* and what the
/// validator *accepts* cannot drift: a tag the prompt listed but the validator
/// rejected would fail a chapter for a reason nobody could see.
struct Vocabulary {
    palette: Vec<String>,
    effects: Vec<String>,
    injects: crate::audio_pool::ClipPool,
    aliases: TagAliases,
}

fn vocabulary(layout: &Layout) -> Result<Vocabulary> {
    let effect_pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let palette = crate::ambience::palette_names(&load_map(layout)?);
    let effects = crate::ambience::effect_tags(&effect_pool);
    let injects = crate::audio_pool::load_pool(&layout.assets().join("inject-pool.json"));
    let aliases = TagAliases::load(&layout.assets().join("tag-aliases.json"))?;
    aliases.validate(&palette, &effects, injects.keys().cloned())?;
    Ok(Vocabulary {
        palette,
        effects,
        injects,
        aliases,
    })
}

/// The chapter text and the bible, as the manual path needs them.
fn manual_inputs(layout: &Layout, n: u32) -> Result<(Value, String)> {
    let chapter_path = layout.chapter_txt(n);
    let text = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    Ok((load_bible(&layout.bible()), text))
}

/// Build the prompt for a manual round.
///
/// **The same two prompts the worker's automatic digest builds**, rendered from
/// the same functions: round 1 is [`build_attribution_prompt`] and round 2 is
/// [`build_staging_prompt`]. A manual digest is the automatic one with a person
/// (or a backup model) standing in for the analyzer, so a hand-driven chapter
/// must not be dramatized by a second, looser contract, that was the legacy
/// `build_prompt` / `build_script_prompt` pair, which no longer runs here.
///
/// `cast` is the validated answer to round 1 and is required for round 2: the
/// staging prompt is rendered *against that immutable speaker map*, exactly as
/// the worker's is, so an operator who skipped round 1 gets an error rather than
/// a prompt that quietly asks for the wrong thing.
///
/// A chapter too long for one answer is asked for **one part at a time**, from
/// the same plan the worker's digest uses. The parts already staged are read
/// from the worker's own checkpoint, so an operator picking up a chapter the
/// cluster half-finished continues at the same boundary instead of starting
/// over — and a chapter the cluster could not finish is one the operator can.
pub fn manual_prompt(
    layout: &Layout,
    engine: &str,
    n: u32,
    cast: Option<&Value>,
) -> Result<ManualPrompt> {
    let session = ManualSession::open(layout, n)?;
    let index = session.parts.len();
    let slice = session.slice(index).ok_or_else(|| {
        anyhow::anyhow!(
            "ch{n} is already staged in full — re-digest it instead of asking for another round"
        )
    })?;
    let summaries = session.parts.summaries();
    let continuity = session.continuity(index, &summaries);
    let text = match cast {
        None => build_attribution_prompt(
            layout,
            &session.bible,
            &slice,
            continuity.as_ref(),
            previous_excerpts(layout, n).as_deref(),
        )?,
        Some(context) => build_staging_prompt(
            layout,
            engine,
            &session.bible,
            context,
            &slice,
            continuity.as_ref(),
        )?,
    };
    Ok(ManualPrompt {
        round: match cast {
            None => Round::Cast,
            Some(_) => Round::Script,
        },
        text,
        part: session.part(index),
    })
}

/// One chapter as the manual flow needs it: the text, the bible, and the plan —
/// including the parts the worker's digest may already have staged.
///
/// The plan is read from the workspace's `settings.json` rather than handed in,
/// and that is deliberate: the cuts have to fall where the *worker's* digest put
/// them, or an operator picking up a half-staged chapter would continue a
/// different plan from the one the checkpoint was written for. Every front end —
/// the TUI, the headless backup runner — would otherwise have to thread a knob
/// that only changes the shape of a prompt.
struct ManualSession {
    bible: Value,
    text: String,
    prepared: PreparedChapter,
    windows: Vec<Window>,
    parts: Parts,
    settings: Settings,
}

impl ManualSession {
    fn open(layout: &Layout, n: u32) -> Result<ManualSession> {
        let (bible, text) = manual_inputs(layout, n)?;
        let prepared = prepare_chapter(&text);
        let settings = Settings::load(&layout.settings());
        let windows = plan_windows(&prepared, &settings.digest);
        let parts = Parts::open(layout, n, &text, &bible, &windows, &settings);
        Ok(ManualSession {
            bible,
            text,
            prepared,
            windows,
            parts,
            settings,
        })
    }

    fn total(&self) -> usize {
        self.windows.len()
    }

    /// The events of the part a round belongs to.
    fn slice(&self, index: usize) -> Option<PreparedChapter> {
        self.windows.get(index).map(|w| w.prepared(&self.prepared))
    }

    fn part(&self, index: usize) -> Option<ManualPart> {
        part_of(index, self.total()).map(|(index, total)| ManualPart { index, total })
    }

    /// The continuity block for a part, or `None` for a chapter that fits one
    /// answer — which is what keeps a short chapter's prompts the ones the
    /// single-call digest builds.
    fn continuity<'a>(&self, index: usize, plot: &'a [String]) -> Option<Continuity<'a>> {
        (self.total() > 1).then_some(Continuity {
            index,
            total: self.total(),
            plot,
        })
    }
}

/// Check a pasted answer for one round, and assemble what it yields.
///
/// **The same validators the worker's answers go through, and that is the whole
/// design.** A manual digest is the automatic one with a person standing in for
/// the model, so an answer the worker's path would have refused is refused here
/// too, with the validator's own complaint as the message, because the operator
/// is the one who can act on it.
///
/// Nothing is written to the chapter itself. Committing is [`write_script`],
/// called by the caller, so what lands is one write site rather than two that
/// could differ. The one file this does write is the **parts checkpoint**, the
/// same one the worker's digest uses: a part is stored when it is accepted, so a
/// chapter handed from the cluster to an operator — or the other way round —
/// continues instead of restarting.
pub fn manual_accept(
    layout: &Layout,
    n: u32,
    round: Round,
    pasted: &str,
    cast: Option<&Value>,
) -> Result<ManualAnswer> {
    let mut session = ManualSession::open(layout, n)?;
    let index = session.parts.len();
    let slice = session.slice(index).ok_or_else(|| {
        anyhow::anyhow!(
            "ch{n} is already staged in full — re-digest it instead of pasting another round"
        )
    })?;
    let summaries = session.parts.summaries();
    let continuity = session.continuity(index, &summaries);
    match round {
        Round::Cast => Ok(ManualAnswer {
            // Round 2's prompt is **not** built here: it carries the engine's
            // non-verbal vocabulary, and the engine is the caller's to name (the
            // TUI's manual digest runs against the engine the run screen shows).
            // The cast comes back instead, and the caller asks for round 2 with
            // it — which is the hand-off the worker makes between its own two
            // calls.
            prompt: None,
            cast: Some(parse_attribution(
                pasted,
                &session.bible,
                &slice,
                continuity.is_some(),
            )?),
            outcome: None,
        }),
        Round::Script => {
            let context = cast.ok_or_else(|| {
                anyhow::anyhow!("round 2 needs round 1's cast — paste the cast answer first")
            })?;
            let vocab = vocabulary(layout)?;
            let script = parse_staged_script(pasted, &session.bible, context, &slice, &vocab)?;
            // Every part's own prose, for rule 2 — or the chapter's own text when
            // it did not split, which is the text the single-call digest has
            // always scanned.
            let texts: Vec<String> = if session.total() == 1 {
                vec![session.text.clone()]
            } else {
                session
                    .windows
                    .iter()
                    .map(|w| window_text(&session.prepared, w))
                    .collect()
            };
            let what = if session.total() == 1 { "chapter" } else { "part" };
            // The gates run **before** the part is stored, with the pasted answer
            // standing in as a part of its own. That ordering is the whole reason
            // the operator can act on a refusal: the checkpoint has not moved on,
            // so the complaint is about the answer in their clipboard and they can
            // paste a better one for the same round.
            let scripts: Vec<&Value> = session
                .parts
                .done
                .iter()
                .map(|p| &p.script)
                .chain(std::iter::once(&script))
                .collect();
            let scopes: Vec<&str> = texts.iter().map(String::as_str).collect();
            if let Some((gap, owner)) = sound_gap(&scripts, &scopes, &vocab.injects, what) {
                match session.part(owner) {
                    Some(part) => anyhow::bail!("part {}/{}: {gap}", part.index, part.total),
                    None => anyhow::bail!("{gap}"),
                }
            }
            let summary = context
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            session.parts.push(Part {
                from: session.windows[index].from,
                to: session.windows[index].to,
                summary,
                context: context.clone(),
                script,
            })?;
            if session.parts.len() < session.total() {
                let next = session.parts.len();
                let next_slice = session.slice(next).ok_or_else(|| {
                    anyhow::anyhow!("ch{n} has no part {}", next + 1)
                })?;
                let next_summaries = session.parts.summaries();
                let next_continuity = session.continuity(next, &next_summaries);
                return Ok(ManualAnswer {
                    prompt: Some(ManualPrompt {
                        round: Round::Cast,
                        text: build_attribution_prompt(
                            layout,
                            &session.bible,
                            &next_slice,
                            next_continuity.as_ref(),
                            previous_excerpts(layout, n).as_deref(),
                        )?,
                        part: session.part(next),
                    }),
                    cast: None,
                    outcome: None,
                });
            }
            let (merged, conflicts) = merge_contexts(&session.parts.done);
            let merged_script = merge_scripts(session.parts.done.iter().map(|p| &p.script));
            let mut outcome =
                assemble_outcome(&session.bible, &merged, &merged_script, &session.text)?;
            for w in conflicts {
                outcome.log.push(format!("   WARN: {w}"));
                outcome.warnings.push(w);
            }
            if session.total() > 1 {
                for (i, line) in part_lines(&session.windows, &session.prepared, &session.settings)
                    .into_iter()
                    .enumerate()
                {
                    outcome.log.insert(1 + i, line);
                }
            }
            session.parts.clear();
            Ok(ManualAnswer {
                prompt: None,
                cast: None,
                outcome: Some(outcome),
            })
        }
    }
}

/// Write a chapter's script where every consumer reads it.
///
/// One write site, so the worker's path and the operator's cannot land the same
/// artifact differently.
pub fn write_script(layout: &Layout, n: u32, script: &Value) -> Result<()> {
    atomic_write(&layout.script(n), &serde_json::to_string_pretty(script)?)
}

/// One generation, retried through rate limits.
///
/// Split out because the digest makes two calls now and the retry policy must
/// not differ between them, a round that gave up sooner than the other would
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
            Ok((t, backend)) => {
                // The configured backend and the one that ran are not the same
                // thing whenever the gemini chain falls back. Say which one
                // answered, so the operator's screen stops naming a backend that
                // had already given up, this is the label that read "via gemini"
                // while opencode was the thing hanging.
                if backend.as_str() != analyzer {
                    progress(
                        to,
                        format!(
                            "{analyzer} gave up — this round was answered by {}",
                            backend.as_str()
                        ),
                    );
                }
                return Ok(t);
            }
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
        // The backend that answered a repair is not re-labelled here: this path
        // has no progress sink, and the fallback has already said so in the log.
        Ok((t, _backend)) => Ok(t),
        Err(GenError::RateLimited(m)) => anyhow::bail!("repair attempt rate-limited: {m}"),
        Err(GenError::Fatal(e)) => Err(e),
    }
}

/// Dump a round's raw answer when `BM_DIGEST_RAW` is set.
///
/// The digest throws the model's text away once it parses, which is right for a
/// run and useless for a post-mortem: "the analyzer placed no sounds" is a
/// symptom, and the raw is the only place the cause is visible, whether it
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
/// rather than as an item of its own, and that is deliberate: given an array
/// of objects, a model fills every field of every object and will not
/// introduce an object it was not handed (asked for sound items directly it
/// returns none at all; asked for a field it fills the field).
///
/// So the pipeline does the moving. What lands on disk is still a sibling
/// `{"sound": ...}` item at the seam, so no renderer is ever handed a line
/// with a sound on it and `text` is never touched. The fields are removed
/// from every line whether or not the name is any good: a bad one then fails
/// validation with the message that explains it, instead of sitting on a line
/// being read by nobody.
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

/// Fill fields the staging answer carried forward by omission.
///
/// The prompt asks for `mood`/`scene`/`music` only where they change and for
/// `text` only where it changes, so a segment that omits one inherits the
/// previous segment's value. `text` falls back to the prepared event's own text
/// by `source_id` — the split/fix cases must still emit it, which the contract
/// says outright. This runs before the validators, so they see the same fully
/// populated segments an older, verboser answer would have produced and nothing
/// downstream has to know the model-facing shape got smaller.
fn carry_forward_fields(data: &mut Value, prepared: &PreparedChapter) {
    let source_text: std::collections::HashMap<&str, &str> = prepared
        .events
        .iter()
        .map(|e| (e.id.as_str(), e.text.as_str()))
        .collect();
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    let mut last: std::collections::HashMap<&'static str, Value> = std::collections::HashMap::new();
    for segment in segments.iter_mut() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let Some(obj) = segment.as_object_mut() else {
            continue;
        };
        for key in ["mood", "scene", "music"] {
            if field_is_blank(obj, key) {
                if let Some(carried) = last.get(key) {
                    obj.insert(key.to_string(), carried.clone());
                }
            } else {
                last.insert(key, obj.get(key).cloned().unwrap_or(Value::Null));
            }
        }
        if field_is_blank(obj, "text") {
            if let Some(text) = obj
                .get("source_id")
                .and_then(Value::as_str)
                .and_then(|id| source_text.get(id))
            {
                obj.insert("text".into(), json!(*text));
            }
        }
    }

    // The *head* of the chapter, which has nothing to inherit. The prompt asks
    // for `music` where it changes, so a chapter whose first bed arrives at line
    // 12 legitimately omits the field at lines 1-11 — and the validator refuses
    // any blank once a single segment declares one, which is what `segment 0:
    // missing music` was: 2 of ch386's 15 attempts. An empty value already means
    // "no bed" to the mixer (`resolve_music`), so filling the blanks before the
    // first declaration only says out loud what the mix does anyway.
    //
    // Only when something *is* declared: a script with no `music` at all
    // predates the field and has to keep taking the legacy merge path rather
    // than become a chapter of explicit silence.
    let first_declared = segments.iter().position(|s| {
        !crate::util::is_sound_item(s)
            && !field_is_blank(s.as_object().unwrap_or(&serde_json::Map::new()), "music")
    });
    if let Some(first) = first_declared {
        for segment in segments.iter_mut().take(first) {
            if crate::util::is_sound_item(segment) {
                continue;
            }
            if let Some(obj) = segment.as_object_mut() {
                if field_is_blank(obj, "music") {
                    obj.insert("music".into(), json!("none"));
                }
            }
        }
    }
}

/// Whether a carried field is absent or empty, the two ways a model declines to
/// state it. An empty array counts, so a deliberate `["rain"]` is a value and a
/// bare `[]` is not mistaken for one.
fn field_is_blank(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    match obj.get(key) {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// Phrases from rule 10's own sweep that are literal on the page in this genre.
///
/// Narrow on purpose: a hit here can fail a chapter, so a word that is usually a
/// metaphor does not belong on the list. `dao` alone is out for that reason
/// `dao phay` is in. Bare `chém` is out for the same reason: ch262's only hit
/// was the idiom "muốn chém muốn giết" (kill me if you want), no slash staged,
/// and the gate refused every correct answer. The compounds (`rút kiếm`,
/// `vung kiếm`) stay; they name an action, not a figure of speech.
const SOUND_CUES: [&str; 20] = [
    "phun ra",
    "máu tươi",
    "máu văng",
    "thổ huyết",
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
    "đâm",
];

/// Block probability after `failures` consecutive same-gap failures: 90%,
/// then -25% each time (0.90, 0.68, 0.51, 0.38...). Pure so the curve is
/// pinned without spending LLM calls; the caller accepts below a coin flip.
fn gap_block_p(failures: u32) -> f64 {
    0.9 * 0.75f64.powi(failures as i32)
}

/// Two sound-design answers that cannot be right, checked in that order.
///
/// Both are things the prompt says in as many words and the model does anyway,
/// and both are silent failures: the chapter merges, sounds fine at a glance,
/// and has no sound design where the prose staged one. Neither is a judgment
/// call, which is why they can be gated at all, a chapter that places three
/// sounds and misses a fourth is the model's business, and no word list can
/// second-guess it.
///
/// 1. A `loop`ed bed started and never stopped. The prompt calls this "the one
///    way to get a bed wrong": the clip plays once and stops dead. Measured on
///    ch9, a 25 s bed opened into a 130 s kitchen, then digital silence.
/// 2. A chapter that stages a sound and places none at all, the cue list from
///    rule 10's own last check, matched against the chapter text.
fn sound_design_gap(
    script: &Value,
    chapter_text: &str,
    pool: &crate::audio_pool::ClipPool,
) -> Option<String> {
    unclosed_beds(script, pool).or_else(|| silent_design(script, chapter_text, "chapter"))
}

/// Rule 1 alone: looping beds a script opened and never stopped, **in the order
/// they opened**.
///
/// Split out of [`sound_design_gap`] for the windowed digest, and the split is
/// load-bearing rather than tidiness. This rule is a fact about the *whole
/// chapter*: a bed started at the end of one part and stopped at the start of
/// the next is closed, and a check that ran per part would refuse a chapter
/// whose long scene is simply wider than one prompt. The repair it asks for goes
/// to the part that placed the surviving `sound` — the only repair that does not
/// re-stage prose nobody complained about.
fn unclosed_beds(script: &Value, pool: &crate::audio_pool::ClipPool) -> Option<String> {
    let open = open_beds(script, pool);
    if open.is_empty() {
        return None;
    }
    Some(format!(
        "{} is a looping bed started with no `stop_after` — it plays once and stops dead. \
         Close it on the line where the scene moves on",
        open.join(", ")
    ))
}

/// Rule 1's finding on its own: the looping beds still open when the script
/// ends, in the order they opened.
///
/// Separated from the sentence it becomes because a windowed digest needs the
/// **name**, not the complaint: the part to re-ask is the part that placed the
/// surviving `sound`, and only the name finds it.
fn open_beds(script: &Value, pool: &crate::audio_pool::ClipPool) -> Vec<String> {
    let Some(segments) = script.get("segments").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
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
    open.into_iter().map(str::to_string).collect()
}

/// Rule 2 alone: the text stages sounds and this script places none.
///
/// A fact about one **part** of a chapter, which is why it is the half a
/// windowed digest checks window by window: `chapter_text` here is the text that
/// window's staging answer was actually answering, so the complaint it produces
/// is about prose the model saw rather than prose it was never shown. `what`
/// names the subject in the message — `chapter`, or `part` for a window — the
/// same way `classify_fetch` is told the noun it is describing.
fn silent_design(script: &Value, chapter_text: &str, what: &str) -> Option<String> {
    let segments = script.get("segments").and_then(|s| s.as_array())?;
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
        "this {what} stages sounds ({}) and this {what}'s script places none — the last check in \
         rule 10 was skipped. Place a sound for each moment the text stages",
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
        "speakers",
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

/// The one reserved speaker name for a person the source never names.
///
/// A crowd is a chorus, not a cast: every unnamed speaker in a chapter shares
/// this name and speaks in the Narrator's voice, which is what the script, the
/// cast file and the mix all show. Numbered slots (`anonymous:anon-1`) are gone
/// from new digests, near-identical one-off clones for a street greeting were
/// the loudest thing in a scene, but they still resolve here, and are still
/// voiced by the Narrator, so chapters already on disk keep rendering.
pub(crate) const ANONYMOUS_SPEAKER: &str = "Anonymous";

pub(crate) fn is_anonymous_speaker(speaker: &str) -> bool {
    if speaker == ANONYMOUS_SPEAKER {
        return true;
    }
    let Some(number) = speaker.strip_prefix("anonymous:anon-") else {
        return false;
    };
    number
        .parse::<u32>()
        .is_ok_and(|n| n > 0 && number == n.to_string())
}

fn fixed_speakers(data: &Value) -> Result<BTreeMap<String, String>> {
    let speakers = data
        .get("speakers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("attribution answer has no speakers object"))?;
    speakers
        .iter()
        .map(|(id, speaker)| {
            let speaker = speaker
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("source {id:?} has an empty speaker"))?;
            Ok((id.clone(), speaker.to_string()))
        })
        .collect()
}

/// What an event is, once the attribution pass's retraction is applied.
///
/// One definition for both gates. A retracted id reads as narration everywhere,
/// so the speaker rule, the delimiter rule and the prompt's own wording cannot
/// disagree about whether a span is still dialogue.
fn effective_kind<'a>(event: &'a PreparedEvent, not_speech: &HashSet<String>) -> &'a str {
    if not_speech.contains(&event.id) {
        "narration"
    } else {
        &event.kind
    }
}

/// The ids the attribution answer claims are quoted non-speech.
///
/// Optional: an answer without the field is an answer that agrees with the
/// preparer, which is what every stored script before this field existed means.
/// A non-string entry is refused rather than skipped, so a model that meant to
/// retract a line cannot have it silently ignored.
fn not_speech_ids(data: &Value) -> Result<HashSet<String>> {
    let Some(list) = data.get("not_speech") else {
        return Ok(HashSet::new());
    };
    if list.is_null() {
        return Ok(HashSet::new());
    }
    let list = list.as_array().ok_or_else(|| {
        anyhow::anyhow!("attribution `not_speech` must be an array of source ids")
    })?;
    list.iter()
        .map(|id| {
            id.as_str().map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("attribution `not_speech` holds a non-string: {id:?}")
            })
        })
        .collect()
}

/// Validate the complete source-id to speaker map against the preparer's
/// mechanical narration/dialogue classification.
///
/// **The one thing the model may overrule is "this quote is speech",** and only
/// downwards. `prepare_chapter` calls a quoted span dialogue because a
/// delimiter opened it, with no notion of a title or a term — so
/// `"sánh ngang với thần"`, a skill name, has to be spoken by somebody. The
/// attribution pass already holds the span with the narration on both sides,
/// which is the evidence a title needs and a keyword list cannot supply: a
/// title is bracketed by prose that continues the sentence, a speech is
/// followed by a tag. So `not_speech` lets the model say so.
///
/// **The direction is what makes this safe.** Narration is still written here
/// in code, never read from the answer, and the reverse — prose promoted to
/// dialogue — is not expressible. The model can retract the preparer's guess;
/// it cannot invent a speaker for prose, nor talk a real character out of a
/// line it is given.
fn validate_attributions(
    data: &Value,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Result<BTreeMap<String, String>> {
    let mut speakers = fixed_speakers(data)?;
    let not_speech = not_speech_ids(data)?;
    // Narration is mechanical, so it is written here rather than read from the
    // answer: the prompt never asks about these ids, and a model that answers
    // anyway cannot change who speaks prose.
    for event in prepared.events.iter().filter(|e| e.kind != "dialogue") {
        speakers.insert(event.id.clone(), "Narrator".to_string());
    }
    let roster = data
        .get("roster")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("attribution answer has no roster"))?;
    let roster: Vec<&str> = roster.iter().filter_map(Value::as_str).collect();
    let expected: HashSet<&str> = prepared.events.iter().map(|e| e.id.as_str()).collect();
    for id in speakers.keys() {
        if !expected.contains(id.as_str()) {
            anyhow::bail!("attribution names unknown source id {id:?}");
        }
    }
    // `not_speech` wins over `speakers` for the ids it lists, rather than the
    // two having to agree.
    //
    // A live run is why. The model listed four ids, agreed with itself on
    // three, and gave the fourth a character. Requiring agreement refused the
    // whole chapter, and the one repair pass made it worse — it dropped an
    // unrelated event — so the chapter failed outright over one ambiguous id.
    // A gate that can deadlock is worse than the mistake it prevents, which is
    // the same lesson `sound_design_gap` encodes.
    //
    // So the listing is taken as the decision and the speaker is written here,
    // exactly as narration's is. Which half the model actually meant is unknow-
    // able, and this way the cost of guessing wrong is one narrated span
    // rather than a refused chapter.
    for id in &not_speech {
        speakers.insert(id.clone(), "Narrator".to_string());
    }

    for event in &prepared.events {
        let speaker = speakers.get(&event.id).ok_or_else(|| {
            // The text goes in the complaint, not just the id. This string is
            // the whole of what the one repair pass is told, and a bare
            // `e0076` asks the model to act on a label it can no longer look
            // up — the prepared view is thousands of characters back in the
            // prompt by now. ch347 failed on exactly this: the model dropped
            // `e0076` ("A!"), was told only the id, dropped it again, and the
            // chapter burned every racer. Naming the words lets one repair
            // actually repair.
            anyhow::anyhow!(
                "attribution dropped source event {:?} — it is dialogue and reads {:?}; give it a speaker",
                event.id,
                crate::util::head_chars(&event.text, 80)
            )
        })?;
        // A dialogue event the model retracted is narration now, and is held
        // to the narration rule instead. The check below cannot read as
        // "dialogue assigned Narrator" for an id that was retracted, or every
        // legitimate retraction would be refused.
        let kind = effective_kind(event, &not_speech);
        match kind {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {:?} is narration but attribution assigns {speaker:?}; narration must be Narrator",
                event.id
            ),
            "dialogue" if speaker == "Narrator" => anyhow::bail!(
                "source {:?} is dialogue but attribution assigns Narrator; use a canonical character or the reserved `Anonymous` — a hail nobody on cast is tagged saying belongs to the crowd, not to Narrator. Only a quoted span that is not somebody talking (a title, a term, a panel label) may be Narrator, and it must also be listed in `not_speech`",
                event.id
            ),
            _ => {}
        }
        if !roster.contains(&speaker.as_str()) {
            if speaker == "Narrator" {
                anyhow::bail!(
                    "source {:?} is narration and this chapter has narration, but `roster` omits \"Narrator\" — add it to roster",
                    event.id
                );
            }
            // The same reasoning as the dropped-event complaint: a repair only
            // works if it can see the line it is being asked to fix. ch347 hit
            // this too, on a different id, in the same failing chapter.
            anyhow::bail!(
                "source {:?} assigns {speaker:?}, but that speaker is not in the chapter roster — \
                 add {speaker:?} to `roster` or give the line another speaker. The line reads {:?}",
                event.id,
                crate::util::head_chars(&event.text, 80)
            );
        }
    }
    for (i, name) in roster.iter().enumerate() {
        if is_anonymous_speaker(name) && !speakers.values().any(|speaker| speaker == *name) {
            anyhow::bail!(
                "roster[{i}]: {name:?} is never used; drop it when nobody in the chapter is unnamed"
            );
        }
    }
    validate_digest_identity(data, bible)?;
    Ok(speakers)
}

fn word_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphabetic())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Rewrite free-form model voice descriptions into the one prefix the cast
/// validator understands, while preserving the description after the colon.
fn canonical_voice_hint(hint: &str) -> String {
    let hint = hint.trim();
    let words = word_set(hint);
    let has = |word: &str| words.contains(word);
    let current = hint
        .split([',', ':', '-', '–'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if matches!(
        current.as_str(),
        "adult male" | "adult female" | "boy" | "girl" | "elderly male" | "elderly female"
    ) {
        return current;
    }

    let female = ["female", "woman", "lady", "nữ", "cô", "chị", "gái", "bà"]
        .iter()
        .any(|word| has(word));
    let male = ["male", "man", "nam", "ông", "anh", "trai", "lão"]
        .iter()
        .any(|word| has(word));
    let head = if has("elderly") || has("old") {
        if female {
            "elderly female"
        } else {
            "elderly male"
        }
    } else if has("girl") {
        "girl"
    } else if has("boy") {
        "boy"
    } else if female {
        "adult female"
    } else if male {
        "adult male"
    } else {
        // A system voice or an under-described newcomer still needs a speakable
        // identity. The default is explicit and stable; later metadata may
        // change it without blocking the chapter.
        "adult male"
    };
    if hint.is_empty() {
        head.to_string()
    } else {
        format!("{head}: {hint}")
    }
}

/// Make the cast bookkeeping deterministic without touching the model's
/// semantic decisions.
///
/// A free model will occasionally omit a character from `new_characters`, use
/// `aliases` for `proper_aliases`, write `young female` instead of the six
/// accepted prefixes, or leave an empty object behind. None of those changes
/// who spoke the prepared events. The old behavior shelved every racer for them;
/// this pass repairs the metadata and leaves speaker/source validation strict.
fn normalize_attribution_metadata(data: &mut Value, bible: &Value, prepared: &PreparedChapter) {
    let mut roster: Vec<String> = data
        .get("roster")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    roster.dedup();
    if let Some(speakers) = data.get("speakers").and_then(Value::as_object) {
        roster.retain(|name| {
            !is_anonymous_speaker(name)
                || speakers
                    .values()
                    .any(|speaker| speaker.as_str() == Some(name.as_str()))
        });
    }

    let mut new_characters: Vec<Value> = Vec::new();
    let mut new_names: HashSet<String> = HashSet::new();
    for mut character in data
        .get("new_characters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let Some(name) = character
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && !is_anonymous_speaker(name))
            .map(str::to_string)
        else {
            continue;
        };
        if !new_names.insert(name.clone()) {
            continue;
        }

        let original_hint = character
            .get("voice_hint")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let voice_hint = canonical_voice_hint(original_hint);
        let mut tags = tags::normalise_tags(character.get("tags"));
        if tags.is_empty() {
            tags = crate::pool::tags_from_hint(original_hint);
        }
        if tags.is_empty() {
            tags = crate::pool::tags_from_hint(&voice_hint);
        }

        let aliases = character
            .get("proper_aliases")
            .or_else(|| character.get("aliases"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut aliases = aliases;
        if !aliases.iter().any(|alias| alias == &name) {
            aliases.insert(0, name.clone());
        }
        aliases.dedup();

        character["name"] = json!(name);
        character["personality"] = character
            .get("personality")
            .cloned()
            .filter(Value::is_string)
            .unwrap_or(json!(""));
        character["voice_hint"] = json!(voice_hint);
        character["tags"] = json!(tags);
        character["proper_aliases"] = json!(aliases);
        character.as_object_mut().map(|o| o.remove("aliases"));
        new_characters.push(character);
    }

    // Every named roster entry needs a persisted identity. The model may have
    // omitted one while still assigning that character correctly; synthesize a
    // minimal adult-male entry rather than rejecting every worker for the same
    // metadata omission.
    let known_bible_name = |name: &str| {
        bible
            .get("characters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|c| c.get("name").and_then(Value::as_str) == Some(name))
            || resolve_speaker(bible, name) != name
    };
    for name in roster.clone() {
        if name == "Narrator"
            || is_anonymous_speaker(&name)
            || known_bible_name(&name)
            || new_names.contains(&name)
        {
            continue;
        }
        new_names.insert(name.clone());
        new_characters.push(json!({
            "name": name,
            "personality": "",
            "voice_hint": "adult male",
            "tags": ["male"],
            "proper_aliases": [name]
        }));
    }

    // A mention map is evidence for later books, not a join table needed by this
    // chapter's already-fixed speakers. Keep only literal, stable name-bearing
    // forms with a known named owner; discard rows that can neither resolve nor
    // be audited, and forms whose referent depends on the local scene.
    let source = prepared
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let known_owner = |owner: &str| {
        owner == "Narrator"
            || roster.iter().any(|name| name == owner)
            || known_bible_name(owner)
            || new_names.contains(owner)
    };
    let mentions = data
        .get("mentions")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter_map(|(form, owner)| {
            let form = form.trim();
            let owner = owner.as_str()?.trim();
            // A mentions object has chapter scope but no scenario key, so it
            // cannot faithfully represent a form whose referent changes within
            // the chapter (`Đồ nhi`, `sư tôn`, pronouns). Keep only stable,
            // name-bearing forms; the source-aware prompt uses the actual
            // neighboring narration to resolve those ambiguous references.
            (!form.is_empty()
                && !canon::is_scenario_dependent(form)
                && source.contains(form)
                && known_owner(owner))
            .then(|| (form.to_string(), json!(owner)))
        })
        .collect::<serde_json::Map<_, _>>();

    data["roster"] = json!(roster);
    data["mentions"] = Value::Object(mentions);
    data["new_characters"] = Value::Array(new_characters);
}

/// `split` says the answer is for one part of a chapter staged in windows,
/// which is the only thing it changes: a part's answer must carry a `summary`,
/// because that summary is what the next part is handed. A one-window chapter
/// is validated exactly as before — the field is neither asked for nor
/// required, which is what keeps a short chapter's answer identical to the
/// pre-window digest's.
/// The excerpt's cap, in characters. Two to four sentences of state is the
/// contract; the cap is what a runaway answer costs when the model ignores
/// it — a paragraph, not a second chapter riding into every future prompt.
const EXCERPT_CHARS: usize = 600;

fn parse_attribution(
    raw: &str,
    bible: &Value,
    prepared: &PreparedChapter,
    split: bool,
) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "attribution is not valid JSON".to_string())?;
    normalize_attribution_metadata(&mut data, bible, prepared);
    // The excerpt is a **soft** field: absent, blank, or over-long is
    // squeezed and capped, never a refusal. `speakers` is the product and is
    // hard-validated; a missing excerpt only means the next chapter runs
    // with one less memory, which is the ordinary degradation and the same
    // "if any" the prompt side already accepts.
    let excerpt = data
        .get("excerpt")
        .and_then(Value::as_str)
        .map(|s| head_chars(&squeeze_ws(s), EXCERPT_CHARS))
        .unwrap_or_default();
    data["excerpt"] = json!(excerpt);
    validate_context(&data, bible)?;
    validate_title(&data)?;
    if split {
        take_summary(&data)?;
    }
    // The validated map is written back with the narration rows the preparer
    // owns, because staging reads `speakers` from this same object.
    let speakers = validate_attributions(&data, bible, prepared)?;
    data["speakers"] = json!(speakers);
    Ok(data)
}

/// The `summary` a part's attribution answer must carry: what the parts after it
/// need to know and cannot look up.
///
/// Refused rather than defaulted when blank, the same way every other required
/// field here is, and for a stronger reason than most: the summary is the whole
/// of what a later part knows about this one, so a part that omits it leaves
/// the rest of the chapter staged against nothing — which is the failure the
/// field exists to remove, and it would be silent. The complaint reaches the
/// model as the ordinary one repair, so a model that skipped the field gets a
/// second chance in the same round.
fn take_summary(data: &Value) -> Result<String> {
    data.get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "attribution has no `summary` — this chapter is longer than one pass, so each \
                 part's answer must say what happened in it; the parts after this one are handed \
                 that summary and never see these events"
            )
        })
}

/// Attach the already validated speaker map to staging output. A speaker emitted
/// or changed by the staging model is ignored; identity belongs to pass one.
fn attach_fixed_speakers(data: &mut Value, speakers: &BTreeMap<String, String>) -> Result<()> {
    let segments = data
        .get_mut("segments")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("staging answer has no segments"))?;
    for (i, segment) in segments.iter_mut().enumerate() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let id = segment
            .get("source_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing source_id"))?;
        let speaker = speakers
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: source id {id:?} has no attribution"))?;
        segment["speaker"] = Value::String(speaker.clone());
    }
    Ok(())
}

/// Parse the staging answer after attaching immutable speakers, then run the
/// ordinary script checks and the source-integrity gate.
fn parse_staged_script(
    raw: &str,
    bible: &Value,
    context: &Value,
    prepared: &PreparedChapter,
    vocab: &Vocabulary,
) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let mut data =
        parse_json_repaired(cleaned).with_context(|| "staging is not valid JSON".to_string())?;
    carry_forward_fields(&mut data, prepared);
    if let Some(segs) = data.get("segments").and_then(|s| s.as_array()).cloned() {
        data["segments"] = json!(expand_sound_fields(&segs)?);
    }
    apply_tag_aliases(&mut data, &vocab.aliases);
    discard_unknown_effect_tags(&mut data, &vocab.effects);
    attach_fixed_speakers(&mut data, &fixed_speakers(context)?)?;
    collapse_redundant_sounds(&mut data);
    validate_script(&data, bible, context, &vocab.palette)?;
    validate_effect_tags(&data, &vocab.effects)?;
    validate_injects(&data, &vocab.injects)?;
    validate_source_alignment(&data, prepared, &not_speech_ids(context)?)?;
    Ok(data)
}

/// Collapse a written non-verbal sound the answer left beside its tag.
///
/// The source gate builds its `expected` with `retag_text`, so the answer has
/// to come through the same door: `"[hắng giọng] Khụ khụ khụ, ban đầu…"` is
/// stored as `"[hắng giọng] ban đầu…"`. Leaving it is wrong twice over, the
/// tag *and* the words get spoken, and refusing it stalls a chapter whose
/// model is otherwise right, which is exactly what ch22 did across every racer.
/// `retag_text` is idempotent and word-boundary disciplined, and text with no
/// written sound is untouched.
fn collapse_redundant_sounds(data: &mut Value) {
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    for segment in segments.iter_mut() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let Some(text) = segment.get("text").and_then(Value::as_str) else {
            continue;
        };
        if let Some(collapsed) = retag_text(text) {
            segment["text"] = json!(collapsed);
        }
    }
}

/// The attribution contract is stricter than the legacy manual contract: every
/// named label is canonical, Narrator is reserved, and anonymous dialogue uses
/// the validated reusable `anonymous:anon-N` namespace.
fn validate_digest_identity(data: &Value, bible: &Value) -> Result<()> {
    let roster = data
        .get("roster")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("digest attribution has no roster"))?;
    let new_names: Vec<&str> = data
        .get("new_characters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect();
    if let Some(name) = new_names.iter().find(|name| is_anonymous_speaker(name)) {
        anyhow::bail!(
            "new_character name {name:?} uses the reserved anonymous speaker namespace; unnamed dialogue is not a Bible character"
        );
    }
    let bible_names: Vec<&str> = bible
        .get("characters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect();
    let characters = bible
        .get("characters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (i, name) in roster.iter().filter_map(Value::as_str).enumerate() {
        if name != "Narrator"
            && !is_anonymous_speaker(name)
            && !bible_names.contains(&name)
            && !new_names.contains(&name)
        {
            // `roster` is a join key, not a display list: `speaker` is matched
            // against it, and the cast is keyed by the canonical name, so an
            // alias here resolves to no voice. Naming the canonical spelling
            // is what lets the repair converge, without it the model retries
            // the same alias until the racers give up on the chapter.
            let owners = alias_owners(&characters, name);
            if let Some(canonical) = owners.first().filter(|_| owners.len() == 1) {
                anyhow::bail!(
                    "roster[{i}]: {name:?} is an alias of {canonical:?} — put the canonical name in roster and keep {name:?} in mentions"
                );
            }
            anyhow::bail!("roster[{i}]: {name:?} is not a canonical known/new character name");
        }
    }
    if let Some(mentions) = data.get("mentions").and_then(Value::as_object) {
        for (form, owner) in mentions {
            let owner = owner.as_str().unwrap_or("");
            if is_anonymous_speaker(owner) {
                anyhow::bail!(
                    "mention {form:?} cannot be owned by the reserved anonymous speaker {owner:?}; mentions map named identities only"
                );
            }
            // Deliberately *not* checked against `roster`. `roster` is the
            // speaker list, while a `mentions` entry may name a character who is
            // only mentioned in this chapter and never speaks, which is
            // ordinary: ch22 names Vũ Kiệt in narration and nowhere else, so
            // there was no legal owner for the form until this check went.
            // `validate_context` has already refused an owner that no bible
            // character or declared new character claims, and it resolves an
            // alias through the bible on purpose.
            let owners = alias_owners(&characters, form);
            if owners.len() > 1 {
                anyhow::bail!("mention {form:?} is ambiguous between {owners:?}");
            }
            if !owners.is_empty() && !owners.contains(owner) {
                anyhow::bail!(
                    "mention {form:?} is owned by {owners:?}, not {owner:?} — a mention's owner must be that character's canonical name"
                );
            }
        }
    }
    for (i, segment) in data
        .get("segments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let speaker = segment.get("speaker").and_then(Value::as_str).unwrap_or("");
        if speaker != "Narrator"
            && !roster
                .iter()
                .filter_map(Value::as_str)
                .any(|name| name == speaker)
        {
            anyhow::bail!("segment {i}: speaker {speaker:?} is not a canonical roster name");
        }
    }
    Ok(())
}

/// Bible characters that claim `form` through `proper_aliases`, compared under
/// `canon_key`. Exactly one owner means the alias has a single legitimate
/// canonical spelling, which is what a repair message needs to be able to name.
fn alias_owners(characters: &[Value], form: &str) -> std::collections::BTreeSet<String> {
    let key = canon_key(form);
    characters
        .iter()
        .filter(|c| {
            c.get("proper_aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|alias| canon_key(alias) == key)
        })
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Apply the answer's explicitly declared grammar fixes to one source event.
fn corrected_source(event: &PreparedEvent, fixes: &[Value]) -> String {
    let mut text = event.text.clone();
    for fix in fixes {
        let (Some(before), Some(after)) = (
            fix.get("before").and_then(Value::as_str),
            fix.get("after").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !before.is_empty() {
            text = text.replace(before, after);
        }
    }
    // The prompt permits the three engine voice tags to replace a written
    // non-verbal sound. That is a rendering transformation, not lost source.
    retag_text(&text).unwrap_or(text)
}
fn normalized_source(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["[cười]", "[thở dài]", "[hắng giọng]"] {
        // The tag, *and the punctuation it swallowed when it took the sound's
        // place*. `retag_text` truncates `"…trượt tay, ha ha."` to
        // `"…trượt tay, [cười]"` — the sentence period goes with the sound — so a
        // model that writes the same line with its period (`"…[cười]."`) differs
        // by one mark and was refused for it: 4 of ch386's 15 attempts died
        // here, and the model's version is the more correct one.
        //
        // Punctuation only, never the adjacent words: `"…, [cười] ha ha."`
        // still differs from `"…, [cười]"` after this, which is what keeps
        // "the tag *and* the words it stands for" the error rule 7 says it is.
        for punct in [",", ";", ":", ".", "…", "!", "?"] {
            out = out.replace(&format!("{tag}{punct}"), " ");
            out = out.replace(&format!("{punct}{tag}"), " ");
        }
        out = out.replace(tag, " ");
    }
    crate::util::squeeze_ws(&out)
}

/// Text with every *written* non-verbal sound, and every tag standing in for
/// one, removed, so two texts can be compared while ignoring how, or whether,
/// they spell laughter, sighs and coughs.
///
/// Longest spellings first: `"thở dài một hơi"` before `"thở dài"`, and a
/// repeated run before the single word, or a stub survives the pass. The tag
/// list is in here too so the function is correct on raw text, not only on
/// input a caller already normalized.
///
/// A removed sound takes its punctuation with it: `"…mình, haha, vừa…"` has to
/// reduce to the words of `"…mình, [cười] vừa…"`, because putting the tag in
/// the sound's place swallowed the comma after it.
fn source_without_written_sound(text: &str) -> String {
    let mut out = text.to_lowercase();
    for sound in [
        "[cười]",
        "[thở dài]",
        "[hắng giọng]",
        "thở dài một hơi rồi",
        "thở dài một tiếng",
        "thở dài một hơi",
        "thở dài",
        "ha ha ha",
        "ha ha",
        "haha",
        "hắc hắc",
        "hô hô",
        "haizz",
        "haiz",
        "khụ khụ khụ",
        "khụ khụ",
        "khụ",
    ] {
        for punct in [",", ";", ":", ".", "…", "!", "?"] {
            out = out.replace(&format!("{sound}{punct}"), " ");
        }
        out = out.replace(sound, " ");
    }
    crate::util::squeeze_ws(&out)
}

fn source_text_matches(expected: &str, actual: &[String]) -> bool {
    let expected = normalized_source(expected);
    let joined = actual
        .iter()
        .map(|s| normalized_source(s))
        .collect::<Vec<_>>();
    normalized_source(&joined.concat()) == expected
        || normalized_source(&joined.join(" ")) == expected
        || source_without_written_sound(&expected)
            == normalized_source(&joined.concat()).to_lowercase()
        || source_without_written_sound(&expected)
            == normalized_source(&joined.join(" ")).to_lowercase()
}

/// The tag whose written sound is still sitting in the text, as the corpus
/// actually spells it. `retag_text` only trims a literal run *immediately*
/// after its tag, so a model that hoists the tag to the head of the line
/// leaves the words behind, this is the shape being named.
fn leftover_written_sound(text: &str) -> Option<(&'static str, &'static str)> {
    let lower = text.to_lowercase();
    for (tag, spellings) in [
        ("[cười]", &["haha", "ha ha", "hắc hắc", "hô hô"][..]),
        // Longest spelling first, for the same reason the matcher does it: the
        // sigh the corpus writes out (`thở dài một tiếng`) contains the bare
        // form, and a hint that named the stub would send the repair to delete
        // two words of its own sentence.
        (
            "[thở dài]",
            &[
                "thở dài một hơi rồi",
                "thở dài một tiếng",
                "thở dài một hơi",
                "thở dài",
                "haizz",
                "haiz",
            ][..],
        ),
        ("[hắng giọng]", &["khụ khụ", "khụ"][..]),
    ] {
        if !lower.contains(tag) {
            continue;
        }
        if let Some(spelling) = spellings.iter().find(|spelling| lower.contains(**spelling)) {
            return Some((tag, spelling));
        }
    }
    None
}

/// Explain a mismatch that is *only* about written non-verbal sound.
///
/// This class fails a chapter across every racer, the model adds the tag and
/// keeps the words it stands for, and the generic "was changed" message gives
/// the repair nothing to act on. It fires only when the two texts agree once
/// written sounds are ignored on both sides, so any other disagreement keeps
/// the honest generic message.
fn written_sound_hint(expected: &str, actual: &str) -> Option<String> {
    let (tag, literal) = leftover_written_sound(actual)?;
    if source_without_written_sound(expected) != source_without_written_sound(actual) {
        return None;
    }
    Some(format!(
        "text still contains the written sound {literal:?} that {tag} stands for; the tag goes \
         exactly where the sound was and its words are deleted — never both, and never moved to \
         the start of the line"
    ))
}

/// Check the source contract independently of the LLM's interpretation.
///
/// `source_id` is deliberately mandatory here, unlike the legacy script
/// validator. It is the join key that makes a dropped paragraph, a duplicated
/// quote, or a reordered chapter visible before the script is persisted.
fn validate_source_alignment(
    data: &Value,
    prepared: &PreparedChapter,
    not_speech: &HashSet<String>,
) -> Result<()> {
    let segments = data
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("digest script has no segments"))?;
    let fixes = data
        .get("fixes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let events: std::collections::HashMap<&str, &PreparedEvent> = prepared
        .events
        .iter()
        .map(|event| (event.id.as_str(), event))
        .collect();

    let mut by_id: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut speakers: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut last_index = 0usize;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, segment) in segments.iter().enumerate() {
        if crate::util::is_sound_item(segment) {
            continue;
        }
        let id = segment
            .get("source_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing source_id"))?;
        let event = events.get(id).ok_or_else(|| {
            anyhow::anyhow!(
                "segment {i}: unknown source_id {id:?} — use an id from the prepared chapter"
            )
        })?;
        let index = prepared
            .events
            .iter()
            .position(|candidate| candidate.id == event.id)
            .expect("event came from the prepared chapter");
        if index < last_index {
            anyhow::bail!("segment {i}: source order regressed at {id:?}");
        }
        last_index = index;
        seen.insert(id);
        let text = segment
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing text"))?;
        by_id.entry(id).or_default().push(text.to_string());

        let speaker = segment.get("speaker").and_then(Value::as_str).unwrap_or("");
        if let Some(previous) = speakers.insert(id, speaker.to_string()) {
            if previous != speaker {
                anyhow::bail!(
                    "source {id:?} is split across speakers {previous:?} and {speaker:?}; a sound or TTS split cannot change the speaker"
                );
            }
        }
        match effective_kind(event, not_speech) {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {id:?} is narration but segment {i} is assigned to {speaker:?}; narration must be Narrator"
            ),
            "dialogue" if speaker == "Narrator" => anyhow::bail!(
                "source {id:?} is dialogue but segment {i} is assigned to Narrator"
            ),
            _ => {}
        }
        // A retracted span is narration now, so it is a narration segment and
        // must not carry a delimiter — same rule, reached through the same
        // effective kind the speaker check above used.
        if effective_kind(event, not_speech) == "dialogue"
            && (text.contains('"') || text.contains('“') || text.contains('”'))
        {
            anyhow::bail!("source {id:?}: quote delimiters must not be merged into segment {i}");
        }
    }

    for event in &prepared.events {
        if !seen.contains(event.id.as_str()) {
            anyhow::bail!("source event {:?} was dropped", event.id);
        }
        let actual = by_id.get(event.id.as_str()).cloned().unwrap_or_default();
        let expected = corrected_source(event, &fixes);
        if !source_text_matches(&expected, &actual) {
            if let Some(hint) = written_sound_hint(&expected, &actual.concat()) {
                anyhow::bail!("source event {:?}: {hint}", event.id);
            }
            anyhow::bail!(
                "source event {:?} was changed, duplicated, or split out of order (expected {:?}, got {:?})",
                event.id,
                expected,
                actual
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bible that knows one character, for the tests that need a named
    /// speaker on the roster: `validate_digest_identity` refuses a roster name
    /// that is neither in the bible nor declared in `new_characters`.
    fn bible_with_phong() -> Value {
        json!({"characters": [
            {"name": "Dịch Phong", "personality": "wry", "voice_hint": "young male",
             "tags": ["male"]}
        ]})
    }

    /// The source gate with nothing retracted, which is what every test that
    /// is not *about* the retraction field means. Named rather than spelled
    /// inline so a future test that does retract something has to say so.
    fn validate_source_alignment_no_retractions(
        data: &Value,
        prepared: &PreparedChapter,
    ) -> Result<()> {
        validate_source_alignment(data, prepared, &HashSet::new())
    }

    /// The chain the whole program rests on, as one test: **a crawler's output
    /// decides whether the model is asked a question at all.**
    ///
    /// `prepare_chapter` decides narration-vs-dialogue from quote marks alone
    /// `"`, `“`, `「`. So a crawler that returns a container with no quote marks
    /// in it, or that picks a site which marks speech some other way, hands the
    /// digest one long run of narration. From there *nothing complains*: the
    /// attribution answer is complete, the source gate passes, the chapter
    /// renders, every ledger row is green, and the book is read in one voice.
    ///
    /// That is why the split is printed. A validator can only catch a model
    /// disagreeing with the text it was given; it cannot catch text that never
    /// offered a speaker to disagree with.
    #[test]
    fn the_digest_reports_a_chapter_whose_crawler_kept_no_quote_marks() {
        // The clean shape, for contrast: a real quote mark is found, and the
        // chapter genuinely has two people in it.
        let clean = "Chương 1: Gặp gỡ\n\nHắn đứng đợi. \"Ừm?\" hắn hỏi.";
        let p = prepare_chapter(clean);
        assert!(p.dialogue_count() > 0, "a real quote mark must be seen");
        let s = p.split_summary();
        assert!(s.contains("1 dialogue"), "{s}");
        assert!(!s.contains("no dialogue found"), "a false alarm: {s}");

        // The broken shape: the same prose with the quote marks gone, which is
        // what a crawler selecting the wrong container returns.
        let stripped = "Chương 1: Gặp gỡ\n\nHắn đứng đợi. Ừm? hắn hỏi.";
        let p = prepare_chapter(stripped);
        assert_eq!(
            p.dialogue_count(),
            0,
            "the whole point: with no quote mark there is no dialogue to find"
        );
        let s = p.split_summary();
        assert!(s.contains("0 dialogue"), "{s}");
        // …and the line points at the thing to check, because a count on its
        // own is trivia.
        assert!(s.contains("crawler's container selector"), "{s}");
    }

    /// The same line, for a chapter that is legitimately all narration, must
    /// *not* blame the crawler, or it stops being read.
    #[test]
    fn the_split_report_does_not_blame_the_crawler_on_a_quiet_chapter() {
        let p = prepare_chapter("Chương 2: Một cảnh\n\nHắn lật trang sách.");
        assert_eq!(p.dialogue_count(), 0);
        let s = p.split_summary();
        assert!(s.contains("0 dialogue"), "{s}");
        // Conditional, because a single-voice chapter is a real thing: the line
        // has to concede it before naming the alternative.
        assert!(
            s.contains("correct if the chapter really is narration"),
            "the caveat must come before the suggestion: {s}"
        );
        assert!(!s.contains("left the crawler"), "no accusation, ever: {s}");
    }

    /// An empty chapter says so rather than claiming zero dialogue of a chapter
    /// that does not exist.
    #[test]
    fn the_split_report_says_so_when_there_is_nothing_to_split() {
        let p = prepare_chapter("");
        assert!(
            p.split_summary().contains("nothing to attribute"),
            "{}",
            p.split_summary()
        );
    }

    /// The line as it actually reaches an operator: first entry in the log of
    /// `assemble_outcome`, so it is present for the worker's run *and* for the
    /// by-hand one. Asserted on the log rather than on `split_summary` because
    /// the placement is the part that can silently regress.
    #[test]
    fn the_split_is_the_first_thing_the_digest_log_says() {
        let text = "Chương 1: Gặp gỡ\n\nHắn đứng đợi. \"Ừm?\" hắn hỏi.";
        let bible = serde_json::json!({});
        let context = serde_json::json!({"speakers": {}, "roster": []});
        let script = serde_json::json!({
            "segments": [{
                "source_id": "e0001", "speaker": "Narrator",
                "text": "Hắn đứng đợi.", "mood": "calm", "scene": "room"
            }]
        });
        let out = assemble_outcome(&bible, &context, &script, text).expect("assembles");
        assert!(
            out.log[0].contains("event(s):") && out.log[0].contains("dialogue"),
            "the split must be the first thing said, got {:?}",
            out.log[0]
        );
    }

    #[test]
    fn fences_are_stripped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("\u{feff}{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn json_repair_escapes_literal_quotes_inside_values() {
        let value =
            parse_json_repaired(r#"{"text":"trên biển khắc một chữ "Võ", rồi chữ đó biến mất."}"#)
                .unwrap();
        assert_eq!(
            value["text"],
            json!("trên biển khắc một chữ \"Võ\", rồi chữ đó biến mất.")
        );

        let quoted = parse_json_repaired(r#"{"text":"he said "hello", then left"}"#).unwrap();
        assert_eq!(quoted["text"], json!("he said \"hello\", then left"));
    }

    #[test]
    fn json_repair_handles_common_model_syntax_mistakes() {
        let value = parse_json_repaired(
            "{\n  \"text\": \"first line\nsecond line\\tand a tab\",\n  \"items\": [1, 2,],\n}",
        )
        .unwrap();
        assert_eq!(value["text"], json!("first line\nsecond line\tand a tab"));
        assert_eq!(value["items"], json!([1, 2]));

        // An apostrophe is ordinary text, not an invitation to rewrite quoting.
        let apostrophe = parse_json_repaired(r#"{"text":"Dịch Phong's shop"}"#).unwrap();
        assert_eq!(apostrophe["text"], json!("Dịch Phong's shop"));
    }

    /// ch79's staging answer: a `text` value carrying both an unescaped `"` and
    /// a raw newline.
    ///
    /// The control-character escaper believes it is outside a string from the
    /// stray quote onward, so it emits the newline raw; the quote repair cannot
    /// fix a control-character complaint without corrupting an unrelated key.
    /// Quotes first, then control characters, is the only order that gets this
    /// through, and it failed on this input and on its own repair before, which
    /// is how a chapter was lost to `control character found while parsing a
    /// string` twice over.
    #[test]
    fn json_repair_settles_quotes_before_control_characters() {
        let broken = "{\n  \"segments\": [\n    {\"text\": \"Trên bia khắc một chữ \"Võ\", rồi chữ đó biến mất.\nTiếng gõ phía sau vang lên.\"}\n  ]\n}";
        assert!(
            serde_json::from_str::<Value>(broken).is_err(),
            "the shape this fixes must be a parse error to begin with"
        );
        let value = parse_json_repaired(broken).unwrap();
        assert_eq!(
            value["segments"][0]["text"],
            json!(
                "Trên bia khắc một chữ \"Võ\", rồi chữ đó biến mất.\nTiếng gõ phía sau vang lên."
            )
        );
    }

    /// The complaint the repair prompt quotes has to say what to write, not
    /// which byte class serde tripped over.
    #[test]
    fn a_json_complaint_carries_its_own_remedy() {
        let err = json_failure(&serde_json::from_str::<Value>("{\"a\":").unwrap_err());
        let msg = err.to_string();
        assert!(msg.contains("EOF") || msg.contains("end of file"), "{msg}");
        assert!(
            msg.contains("whole object"),
            "and says what a model should do instead: {msg}"
        );
    }

    /// A literal newline inside a string is ch79's failure in miniature, and it
    /// is *repaired* rather than reported, the alternative is burning a model
    /// call on output this code can fix.
    #[test]
    fn a_raw_control_character_is_repaired_rather_than_reported() {
        let value = parse_json_repaired("{\"a\": \"one\ntwo\"}").unwrap();
        assert_eq!(value["a"], json!("one\ntwo"));
    }

    #[test]
    fn json_repair_does_not_hide_genuinely_invalid_json() {
        assert!(parse_json_repaired("{\"a\":1").is_err());
        assert!(parse_json_repaired("not JSON").is_err());
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

    /// The staging shape asks for `mood`/`scene`/`music`/`text` only where they
    /// change; code fills the rest. This is the pass that makes the smaller
    /// answer identical to the older, fully-populated one before any validator
    /// or the mixer ever sees it.
    #[test]
    fn omitted_staging_fields_carry_forward_from_the_previous_segment() {
        let prepared = prepare_chapter("\"Một.\"\n\nNàng gật đầu.");
        let narration = prepared
            .events
            .iter()
            .find(|e| e.kind == "narration")
            .expect("the prose event")
            .text
            .clone();
        let mut data = json!({
            "segments": [
                {"source_id": "e0001", "text": "Một.", "mood": "calm",
                 "scene": "room", "music": "quiet"},
                {"source_id": "e0002"}
            ]
        });
        carry_forward_fields(&mut data, &prepared);
        let segs = data["segments"].as_array().unwrap();
        assert_eq!(segs[1]["mood"], json!("calm"));
        assert_eq!(segs[1]["scene"], json!("room"));
        assert_eq!(segs[1]["music"], json!("quiet"));
        // `text` is filled from the prepared event, so the model never re-types
        // the chapter it was handed.
        assert_eq!(segs[1]["text"], json!(narration));

        // A segment that states its own value never inherits the previous one —
        // this is how a split keeps its two different halves.
        let mut split = json!({
            "segments": [
                {"source_id": "e0001", "text": "Một.", "mood": "calm"},
                {"source_id": "e0001", "text": "Hai.", "mood": "urgent"},
                {"source_id": "e0001"}
            ]
        });
        carry_forward_fields(&mut split, &prepared);
        let segs = split["segments"].as_array().unwrap();
        assert_eq!(segs[1]["mood"], json!("urgent"));
        assert_eq!(segs[2]["mood"], json!("urgent"));
        assert_eq!(segs[1]["text"], json!("Hai."));
    }

    /// The head of a chapter whose first bed arrives mid-way.
    ///
    /// Rule 8 asks for `music` where it *changes*, so the lines before the
    /// first change carry no value at all — there is nothing to inherit, and the
    /// validator refuses a blank the moment any segment declares a bed. That was
    /// `segment 0: missing music` on ch386, and it is a chapter the prompt told
    /// the model to write exactly as it did.
    #[test]
    fn the_lines_before_the_first_music_declaration_are_none() {
        let prepared = prepare_chapter("Một.\n\nHai.\n\nBa.");
        let mut data = json!({
            "segments": [
                {"source_id": "e0001", "text": "Một."},
                {"source_id": "e0002", "text": "Hai.", "music": "battle"},
                {"source_id": "e0003", "text": "Ba."}
            ]
        });
        carry_forward_fields(&mut data, &prepared);
        let segs = data["segments"].as_array().unwrap();
        assert_eq!(segs[0]["music"], json!("none"), "the silent head is `none`");
        assert_eq!(segs[1]["music"], json!("battle"));
        assert_eq!(segs[2]["music"], json!("battle"), "and it still carries on");

        // A chapter with no `music` anywhere is left alone: it predates the
        // field, and the legacy merge path is what reads it.
        let mut legacy = json!({"segments": [
            {"source_id": "e0001", "text": "Một."},
            {"source_id": "e0002", "text": "Hai."}
        ]});
        carry_forward_fields(&mut legacy, &prepared);
        for seg in legacy["segments"].as_array().unwrap() {
            assert!(
                seg.get("music").is_none(),
                "nothing to say about a legacy script"
            );
        }
    }

    /// A reworded template must surface as a miss, not vanish. This is the
    /// mechanism that let `analyze.txt`'s rule 1 drift out of sync unnoticed.
    #[test]
    fn a_missing_section_marker_is_reported_not_silently_skipped() {
        let mut body = String::from("1. something else entirely\n2. new_characters\n");
        assert!(!replace_prompt_section(
            &mut body,
            "1. mentions records",
            "2. new_characters",
            "NEW"
        ));
        assert_eq!(body, "1. something else entirely\n2. new_characters\n");

        let mut body = String::from("1. mentions records\n2. new_characters\n");
        assert!(replace_prompt_section(
            &mut body,
            "1. mentions records",
            "2. new_characters",
            "NEW"
        ));
        assert!(body.starts_with("NEW"), "{body}");

        let mut miss = Vec::new();
        let mut body = String::from("no marker here");
        replace_or_miss(&mut body, "{chapter_text}", "text", &mut miss);
        assert_eq!(miss.len(), 1);
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
            "{music_palette}|{scene_words}|{effect_tags}|{inject_sounds}|{cast_json}|{bible_json}|{chapter_text}",
        )
        .unwrap();
        std::fs::write(
            dir.join("assets/scene-map.json"),
            // A rule whose match word is deliberately NOT a bed tag, because
            // that is the case the place vocabulary exists for: `palace` is in
            // no pool, and it still has to reach the prompt.
            r#"{"rules": [{"match": ["palace", "jade pavilion"], "effect": [], "level": 0.0}],
                "music_palette": {"quiet": {"tags": ["soft"], "note": "low"}}}"#,
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
        let context =
            json!({"roster": ["Narrator", "Dịch Phong"], "mentions": {"hắn": "Dịch Phong"}});

        // The cast prompt: bible and chapter, and nothing else.
        let cast = build_prompt(&layout, &bible, "text").unwrap();
        for ph in ["{bible_json}", "{chapter_text}"] {
            assert!(!cast.contains(ph), "placeholder leaked: {ph}");
        }
        assert!(
            !cast.contains("{music_palette}") && !cast.contains("{inject_sounds}"),
            "the cast prompt must not carry sound vocabulary: {cast}"
        );

        // The automatic contracts keep dialogue identity separate from staging.
        let prepared = prepare_chapter("Chương 1: Một chuyến gặp\n\n\"Ừm!\"");
        let attribution = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
        assert!(attribution.contains("---ATTRIBUTION OUTPUT CONTRACT---"));
        assert!(attribution.contains("Dialogue must NEVER map to Narrator"));
        assert!(attribution.contains("Anonymous"));
        // The answerable list is dialogue with nearby source context; narration
        // ids never enter the answer map.
        assert!(attribution.contains("narration_ids"), "{attribution}");
        assert!(attribution.contains("`dialogue_events`"), "{attribution}");
        assert!(attribution.contains("following_context"), "{attribution}");
        assert!(
            attribution.contains("explicit named dialogue tag"),
            "{attribution}"
        );
        assert!(attribution.contains("scenario-dependent"), "{attribution}");
        let fixed = json!({
            "roster": ["Narrator", "anonymous:anon-1"],
            "mentions": {},
            "speakers": {"e0001": "anonymous:anon-1"}
        });
        let staging =
            build_staging_prompt(&layout, "vieneu", &bible, &fixed, &prepared, None).unwrap();
        assert!(staging.contains("---STAGING OUTPUT CONTRACT---"));
        assert!(staging.contains("fixed_speakers"));
        assert!(staging.contains("Do not return `speaker`"));
        // The staging path shares its template with the script path, so a
        // placeholder only one of them replaced would reach the model intact.
        assert!(
            !staging.contains("{scene_words}") && !staging.contains("{effect_tags}"),
            "placeholder leaked into the staging prompt: {staging}"
        );

        // The script prompt: the four vocabularies and the resolved cast.
        let p = build_script_prompt(&layout, "vieneu", &bible, &context, "text").unwrap();
        assert!(p.contains("quiet (soft; low)"), "{p}");
        assert!(p.contains("night"), "{p}");
        // The PLACE vocabulary reaches the prompt, and `palace` is in no pool,
        // so it can only have come from the rules.
        assert!(p.contains("jade pavilion, palace"), "{p}");
        // the inject vocabulary renders the clip's own mode first
        assert!(p.contains("coin (hit; coin, metal; 0.6s)"), "{p}");
        assert!(p.contains("\"roster\""), "{p}");
        assert!(p.contains("Dịch Phong"), "{p}");
        assert!(p.contains("hắn"), "{p}");
        for ph in [
            "{effect_tags}",
            "{scene_words}",
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

    #[test]
    fn attribution_is_complete_deterministic_and_rejects_narrator_dialogue() {
        let prepared = prepare_chapter(
            "Chương 1: Một chuyến gặp\n\nDịch Phong đứng yên.\n\n\"Ừm!\"\n\nHắn gật đầu.",
        );
        assert_eq!(
            prepared
                .events
                .iter()
                .map(|e| (e.id.as_str(), e.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("e0001", "narration"),
                ("e0002", "dialogue"),
                ("e0003", "narration"),
            ]
        );
        let mut data = json!({
            "roster": ["Narrator", "Dịch Phong"],
            "speakers": {
                "e0001": "Narrator",
                "e0002": "Dịch Phong",
                "e0003": "Narrator"
            }
        });
        let bible = json!({"characters": [{"name": "Dịch Phong"}]});
        validate_attributions(&data, &bible, &prepared).unwrap();

        data["speakers"]["e0002"] = json!("Narrator");
        let err = validate_attributions(&data, &bible, &prepared).unwrap_err();
        assert!(err.to_string().contains("dialogue"), "{err}");
        // The message names the answer the repair round must reach for.
        assert!(err.to_string().contains("Anonymous"), "{err}");
    }

    #[test]
    fn attribution_normalizes_optional_cast_metadata_without_touching_speakers() {
        let prepared = prepare_chapter(
            "Chương 1: Làm cái một đời tông sư\n\nĐồ nhi Dịch Phong đứng trước Huyền Vũ tông.\n\n\"Ừm!\"\n\n\"Ký chủ: Dịch Phong.\"\n\n\"Tỷ tỷ, muội muốn mua sách không?\"\n\n\"Mở cửa!\"",
        );
        let raw = json!({
            "title": "Võ Quán Phàm Nhân",
            "atmosphere": "A quiet martial shop at dawn.",
            "roster": [
                "Narrator", "Dịch Phong", "Hệ thống", "Lạc Lan Tuyết",
                "anonymous:anon-1", "Anonymous"
            ],
            "mentions": {
                "Dịch Phong": "Dịch Phong",
                "Huyền Vũ tông": "Huyền Vũ tông",
                "Đồ nhi": "Dịch Phong",
                "not in source": "Dịch Phong"
            },
            "new_characters": [
                {
                    "name": "Dịch Phong",
                    "personality": "calm",
                    "voice_hint": "young adult male: reserved",
                    "aliases": ["Dịch Phong"]
                },
                {
                    "name": "Hệ thống",
                    "personality": "mechanical",
                    "voice_hint": "neutral non-binary middle-aged, mechanical",
                    "aliases": ["Hệ thống"]
                },
                {"personality": "nameless junk", "voice_hint": "adult male", "tags": []}
            ],
            "new_aliases": {},
            "speakers": {
                "e0001": "Narrator",
                "e0002": "Dịch Phong",
                "e0003": "Hệ thống",
                "e0004": "Lạc Lan Tuyết",
                "e0005": "anonymous:anon-1"
            }
        })
        .to_string();

        let data = parse_attribution(&raw, &json!({"characters": []}), &prepared, false).unwrap();
        let names = data["new_characters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Dịch Phong", "Hệ thống", "Lạc Lan Tuyết"]);
        assert!(!names.contains(&"nameless junk"));
        assert_eq!(
            data["new_characters"][0]["voice_hint"],
            json!("adult male: young adult male: reserved")
        );
        assert!(data["new_characters"][0]["tags"].is_array());
        assert!(data["new_characters"][1]["proper_aliases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "Hệ thống"));
        assert_eq!(data["mentions"], json!({"Dịch Phong": "Dịch Phong"}));
        assert!(
            !data["roster"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "Anonymous"),
            "an unused anonymous placeholder is metadata, not a cast decision"
        );
        assert_eq!(data["speakers"]["e0003"], json!("Hệ thống"));
    }

    /// The excerpt is **soft**: whitespace and length are cleaned, never
    /// refused — it is memory for the next chapter, not the product — and on
    /// the merge it is the **last** non-empty part's, because the excerpt
    /// describes the state the chapter *ends* in and only the last part has
    /// seen the whole arc.
    #[test]
    fn the_excerpt_is_soft_on_parse_and_last_part_wins_on_merge() {
        let prepared = prepare_chapter(
            "Chương 1: Làm cái một đời tông sư\n\nĐồ nhi Dịch Phong đứng trước Huyền Vũ tông.\n\n\"Ừm!\"",
        );
        let mut raw = json!({
            "title": "Võ Quán Phàm Nhân",
            "atmosphere": "A quiet martial shop at dawn.",
            "excerpt": "  The  chapter  ends   with the stranger\n\n still unnamed, traveling with the party. ",
            "roster": ["Narrator", "Dịch Phong"],
            "speakers": {"e0001": "Narrator", "e0002": "Dịch Phong"}
        });
        let data =
            parse_attribution(&raw.to_string(), &json!({"characters": []}), &prepared, false)
                .unwrap();
        let excerpt = data["excerpt"].as_str().unwrap();
        assert!(excerpt.starts_with("The chapter ends"), "{excerpt}");
        assert!(!excerpt.contains('\n'), "squeezed: {excerpt}");
        assert!(excerpt.ends_with("party."), "{excerpt}");

        // A runaway answer is capped, not chattered at.
        raw["excerpt"] = json!("dạ ".repeat(EXCERPT_CHARS));
        let data =
            parse_attribution(&raw.to_string(), &json!({"characters": []}), &prepared, false)
                .unwrap();
        assert_eq!(
            data["excerpt"].as_str().unwrap().chars().count(),
            EXCERPT_CHARS
        );

        // Absent is as good as blank: the field simply comes back empty.
        raw.as_object_mut().unwrap().remove("excerpt");
        let data =
            parse_attribution(&raw.to_string(), &json!({"characters": []}), &prepared, false)
                .unwrap();
        assert_eq!(data["excerpt"], json!(""));

        // Last non-empty wins, and an empty tail part does not erase it.
        let base = json!({
            "title": "Tiếng Hỏi Trong Sân",
            "atmosphere": "An empty courtyard at dusk.",
            "roster": [],
            "mentions": {},
            "new_characters": [],
            "new_aliases": {},
            "speakers": {}
        });
        let mut first = base.clone();
        first["excerpt"] = json!("part one ends quietly");
        let mut second = base.clone();
        second["excerpt"] = json!("part two: the reveal lands");
        let third = base;
        let parts = vec![
            staged_part(0, 4, first, json!([])),
            staged_part(4, 8, second, json!([])),
            staged_part(8, 12, third, json!([])),
        ];
        let (merged, _) = merge_contexts(&parts);
        assert_eq!(merged["excerpt"], json!("part two: the reveal lands"));
    }

    #[test]
    fn anonymous_dialogue_uses_reusable_slots_and_never_becomes_a_character() {
        let prepared = prepare_chapter("Chương 1: Tiếng gọi\n\n\"Mở cửa!\"");
        let data = json!({
            "roster": ["Narrator", "anonymous:anon-1"],
            "speakers": {"e0001": "anonymous:anon-1"}
        });
        validate_attributions(&data, &json!({"characters": []}), &prepared).unwrap();

        let reserved_name = json!({
            "roster": ["Narrator", "anonymous:anon-1"],
            "speakers": {"e0001": "anonymous:anon-1"},
            "new_characters": [{
                "name": "anonymous:anon-1",
                "personality": "stranger",
                "voice_hint": "adult male",
                "tags": ["male"]
            }]
        });
        let err = validate_digest_identity(&reserved_name, &json!({"characters": []})).unwrap_err();
        assert!(err.to_string().contains("reserved anonymous"), "{err}");

        assert!(!is_anonymous_speaker("anonymous:anon-0"));
        assert!(!is_anonymous_speaker("anonymous:anon-01"));
        // Legacy scripts keep their numbered ids, and the current name is the
        // bare reserved one.
        assert!(is_anonymous_speaker("anonymous:anon-12"));
        assert!(is_anonymous_speaker(ANONYMOUS_SPEAKER));
        assert!(!is_anonymous_speaker("anonymous"));
        assert!(!is_anonymous_speaker("Người lạ"));
    }

    /// ch6's opening: prose, then a street hailing the same phrase twice on two
    /// consecutive lines. Both are dialogue events, and the answer map holds
    /// only them, narration is not the model's to answer.
    #[test]
    fn narration_is_attached_by_code_and_the_map_holds_only_dialogue() {
        let prepared = prepare_chapter(
            "Chương 6: Xem như chó hoang\n\nDịch Phong bước ra khỏi cửa.\n\n\"Dịch sư phụ.\"\n\n\"Dịch sư phụ.\"",
        );
        assert_eq!(
            prepared
                .events
                .iter()
                .map(|e| (e.id.as_str(), e.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("e0001", "narration"),
                ("e0002", "dialogue"),
                ("e0003", "dialogue"),
            ]
        );

        let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
        assert_eq!(view["narration_ids"], json!(["e0001"]));
        assert_eq!(view["dialogue_events"][0]["id"], json!("e0002"));
        assert_eq!(view["dialogue_events"][1]["id"], json!("e0003"));
        assert_eq!(view["dialogue_events"][1]["text"], json!("Dịch sư phụ."));
        assert!(view["dialogue_events"][0]["previous_context"]["text"]
            .as_str()
            .unwrap()
            .contains("Dịch Phong"));
        assert!(view["dialogue_events"][0]["following_context"].is_null());

        let bible = json!({"characters": []});
        let mut data = json!({
            "roster": ["Narrator", "anonymous:anon-1"],
            "speakers": {"e0002": "anonymous:anon-1", "e0003": "anonymous:anon-1"}
        });
        let speakers = validate_attributions(&data, &bible, &prepared).unwrap();
        assert_eq!(speakers["e0001"], "Narrator");
        assert_eq!(speakers["e0002"], "anonymous:anon-1");

        // A model that answers a narration id anyway cannot change who speaks
        // prose: the id is rewritten rather than trusted.
        data["speakers"]["e0001"] = json!("anonymous:anon-1");
        let speakers = validate_attributions(&data, &bible, &prepared).unwrap();
        assert_eq!(speakers["e0001"], "Narrator");
    }

    #[test]
    fn a_named_tag_after_a_quote_is_attribution_evidence() {
        let prepared = prepare_chapter(
            "Chương 6: Gặp lại\n\nDịch Phong bước ra khỏi cửa.\n\n\"Sư tôn, chính là nơi này.\" Lạc Lan Tuyết vẻ mặt trịnh trọng nói.",
        );
        let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
        let dialogue = &view["dialogue_events"][0];
        assert_eq!(dialogue["id"], json!("e0002"));
        assert_eq!(dialogue["text"], json!("Sư tôn, chính là nơi này."));
        assert_eq!(dialogue["previous_context"]["id"], json!("e0001"));
        assert_eq!(dialogue["following_context"]["id"], json!("e0003"));
        assert_eq!(
            dialogue["following_context"]["text"],
            json!("Lạc Lan Tuyết vẻ mặt trịnh trọng nói.")
        );
    }

    #[test]
    fn staging_cannot_change_the_fixed_speaker() {
        let speakers = BTreeMap::from([
            ("e0001".to_string(), "Narrator".to_string()),
            ("e0002".to_string(), "anonymous:anon-1".to_string()),
        ]);
        let mut staging = json!({"segments": [
            {"source_id": "e0001", "speaker": "Dịch Phong", "text": "Trời sáng."},
            {"source_id": "e0002", "speaker": "Narrator", "text": "Ai đó?"}
        ]});
        attach_fixed_speakers(&mut staging, &speakers).unwrap();
        assert_eq!(staging["segments"][0]["speaker"], json!("Narrator"));
        assert_eq!(staging["segments"][1]["speaker"], json!("anonymous:anon-1"));
    }

    #[test]
    fn source_gate_requires_complete_ordered_attribution() {
        let prepared = prepare_chapter(
            "Chương 1: Một chuyến gặp\n\nDịch Phong nói với Bành Anh.\n\n\"Anh nhi, xong chưa?\" Vũ Kiệt hỏi.",
        );
        assert_eq!(prepared.events.len(), 3);
        assert_eq!(prepared.events[0].kind, "narration");
        assert_eq!(prepared.events[1].kind, "dialogue");
        let line = |id: &str, speaker: &str, text: &str| json!({"source_id": id, "speaker": speaker, "text": text});
        let good = json!({"segments": [
            line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
            line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa?"),
            line("e0003", "Narrator", "Vũ Kiệt hỏi."),
        ], "fixes": []});
        validate_source_alignment_no_retractions(&good, &prepared).unwrap();

        let dropped = json!({"segments": [
            line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
        ], "fixes": []});
        let err = validate_source_alignment_no_retractions(&dropped, &prepared).unwrap_err();
        assert!(err.to_string().contains("dropped"), "{err}");

        let wrong_owner = json!({"segments": [
            line("e0001", "Bành Anh", "Dịch Phong nói với Bành Anh."),
            line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa?"),
            line("e0003", "Narrator", "Vũ Kiệt hỏi."),
        ], "fixes": []});
        let err = validate_source_alignment_no_retractions(&wrong_owner, &prepared).unwrap_err();
        assert!(err.to_string().contains("narration"), "{err}");

        let merged = json!({"segments": [
            line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
            line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa? Vũ Kiệt hỏi."),
        ], "fixes": []});
        let err = validate_source_alignment_no_retractions(&merged, &prepared).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
    }

    /// Chapter 99's rotated script, as the regression case: three consecutive
    /// events came back with their speakers shifted one step — the order's
    /// dialogue on Narrator, the narration on a bystander, the prisoners'
    /// plea on Narrator. Every row of that shape must refuse, so a future
    /// rotation fails the chapter instead of shipping voices on wrong lines.
    #[test]
    fn source_gate_refuses_a_rotated_speaker_row() {
        let prepared = prepare_chapter(
            "\"Người đâu, mang ba tên hỗn xược kia lên đây cho ta!\" Diệp Bắc khoát tay nói.\n\nRất nhanh, ba tên Võ Linh kia liền bị dẫn lên, vừa nhìn thấy Diệp Bắc liền lớn tiếng kêu: \"Bang chủ, ngươi làm vậy là có ý gì?\"",
        );
        assert_eq!(prepared.events.len(), 4, "{:?}", prepared.events);
        assert_eq!(prepared.events[0].kind, "dialogue");
        assert_eq!(prepared.events[1].kind, "narration");
        assert_eq!(prepared.events[2].kind, "narration");
        assert_eq!(prepared.events[3].kind, "dialogue");
        let line =
            |id: &str, speaker: &str, text: &str| json!({"source_id": id, "speaker": speaker, "text": text});
        // The rotation, verbatim in shape: dialogue on Narrator, narration on
        // a character, dialogue on Narrator again.
        let rotated = json!({"segments": [
            line("e0001", "Narrator", "Người đâu, mang ba tên hỗn xược kia lên đây cho ta!"),
            line("e0002", "Narrator", "Diệp Bắc khoát tay nói."),
            line("e0003", "Diệp Bắc", "Rất nhanh, ba tên Võ Linh kia liền bị dẫn lên, vừa nhìn thấy Diệp Bắc liền lớn tiếng kêu:"),
            line("e0004", "Narrator", "Bang chủ, ngươi làm vậy là có ý gì?"),
        ], "fixes": []});
        let err = validate_source_alignment_no_retractions(&rotated, &prepared).unwrap_err();
        assert!(
            err.to_string().contains("Narrator"),
            "a rotated row must name the Narrator violation, got: {err}"
        );
    }

    #[test]
    fn quote_separators_that_are_only_punctuation_are_not_prepared_as_speech() {
        let prepared = prepare_chapter("Những câu như:\n\n\"Một câu.\", \"Câu tiếp theo.\"");

        assert_eq!(prepared.events.len(), 3, "{:?}", prepared.events);
        assert_eq!(prepared.events[0].text, "Những câu như:");
        assert_eq!(prepared.events[1].text, "Một câu.");
        assert_eq!(prepared.events[2].text, "Câu tiếp theo.");
        assert_eq!(prepared.events[2].id, "e0003");
        assert!(prepared
            .events
            .iter()
            .all(|event| crate::util::has_speakable_content(&event.text)));

        let aligned = json!({"segments": [
            {"source_id": "e0001", "speaker": "Narrator", "text": "Những câu như:"},
            {"source_id": "e0002", "speaker": "Anonymous", "text": "Một câu."},
            {"source_id": "e0003", "speaker": "Anonymous", "text": "Câu tiếp theo."}
        ], "fixes": []});
        validate_source_alignment_no_retractions(&aligned, &prepared).unwrap();

        let mut corrected_separator = prepare_chapter("\"Một câu.\", \"Câu tiếp theo.\"");
        corrected_separator.events[0].text.push(',');
        let aligned = json!({"segments": [
            {"source_id": "e0001", "speaker": "Anonymous", "text": "Một câu.,"},
            {"source_id": "e0002", "speaker": "Anonymous", "text": "Câu tiếp theo."}
        ], "fixes": []});
        validate_source_alignment_no_retractions(&aligned, &corrected_separator).unwrap();
    }

    #[test]
    fn an_entity_bearing_chapter_prepares_to_the_decoded_text() {
        // ch79 as crawled by the pre-fix crawler: numeric entities raw on
        // disk. The model reads `&#x27;` and answers `'`, so the prepared
        // events must carry the decoded form, or the source gate refuses the
        // chapter on every racer and it can never digest (the stuck-chapter
        // shape the inductor log showed for ch79/85/91/93/96/100).
        let prepared = prepare_chapter(
            "Chương 79: Cánh cửa\n\nQuả nhiên, cánh cửa nhỏ &#x27;két&#x27; một tiếng, nhẹ nhàng khẽ mở.",
        );
        assert_eq!(prepared.events.len(), 1);
        assert_eq!(
            prepared.events[0].text,
            "Quả nhiên, cánh cửa nhỏ 'két' một tiếng, nhẹ nhàng khẽ mở."
        );
        // The model's natural, decoded answer now matches the gate.
        let data = json!({"segments": [
            {"source_id": "e0001", "speaker": "Narrator", "text": "Quả nhiên, cánh cửa nhỏ 'két' một tiếng, nhẹ nhàng khẽ mở."}
        ], "fixes": []});
        validate_source_alignment_no_retractions(&data, &prepared).unwrap();
    }

    #[test]
    fn the_digest_does_not_edit_a_chapters_words() {
        // The digest used to strip Storya's furniture here, on the way into the
        // prompt. It does not any more: the crawler owns that, and a stored
        // chapter is whatever the crawler (or the operator who pasted it) wrote.
        //
        // What still has to hold is the *source contract*, every sentence in
        // the chapter is an event the model must cover, and the alignment gate
        // below it is unchanged by any of this.
        let prepared = prepare_chapter(
            "Chương 81: Liền phòng ngự\n\nCài đặt đọc\n\nHắn đã hoàn thành nhiệm vụ.\n\nHệ thống thực thể dưới dạng chiếc đỉnh. Truyện đã hoàn thành",
        );

        // The furniture is present because nobody here was asked to remove it …
        assert!(
            prepared.prompt_json.contains("Cài đặt đọc"),
            "the digest is not the place that knows what a site prints: {}",
            prepared.prompt_json
        );
        // … and it is *owed*, not skipped: every line is an event, and a
        // response that quietly left one out is refused by the gate.
        assert_eq!(prepared.events.len(), 3);
        assert_eq!(prepared.events[0].text, "Cài đặt đọc");
        assert_eq!(prepared.events[1].text, "Hắn đã hoàn thành nhiệm vụ.");
    }

    #[test]
    fn a_decoded_quot_becomes_a_dialogue_boundary() {
        // `&quot;` survived the old crawler too, only as raw markup. Decoding
        // turns it into a real quote delimiter, so prepare_chapter splits the
        // dialogue out exactly as it would for a properly crawled chapter
        // and the gate keeps demanding the delimiter-free speech span.
        let prepared =
            prepare_chapter("Chương 1: Gặp gỡ\n\n&quot;Ừm.&quot; hắn đáp, &quot;xong rồi.&quot;");
        assert_eq!(prepared.events.len(), 3);
        assert_eq!(prepared.events[0].kind, "dialogue");
        assert_eq!(prepared.events[0].text, "Ừm.");
        assert_eq!(prepared.events[1].kind, "narration");
        assert_eq!(prepared.events[1].text, "hắn đáp,");
        assert_eq!(prepared.events[2].kind, "dialogue");
        assert_eq!(prepared.events[2].text, "xong rồi.");
        let data = json!({"segments": [
            {"source_id": "e0001", "speaker": "Vũ Kiệt", "text": "Ừm."},
            {"source_id": "e0002", "speaker": "Narrator", "text": "hắn đáp,"},
            {"source_id": "e0003", "speaker": "Vũ Kiệt", "text": "xong rồi."}
        ], "fixes": []});
        validate_source_alignment_no_retractions(&data, &prepared).unwrap();
    }

    /// ch248's real tail, verbatim. The crawler cut the line mid-speech and
    /// left a dangling backslash, so the closing `"` never arrived — and
    /// every span after the last matched pair became one dialogue event. The
    /// chapter read as a single voice with a green ledger row, which is the
    /// mirror of the no-quotes case and the reason this is a warning.
    #[test]
    fn an_unclosed_quote_is_reported_rather_than_read_as_one_voice() {
        let text = concat!(
            "\"Tiền bối, không thể nói như thế chứ, hắn đi tới Nam Sa chúng ta, ",
            "dù sao cũng phải có chút thể hiện chứ!\"\n\n",
            "Hắn lắc đầu, thở dài một tiếng.\n\n",
            "\"Đúng vậy đúng vậy, cũng không thể phụ lòng nhiệt tình của chúng ta chứ!\\\n",
        );
        let prepared = prepare_chapter(text);
        assert!(
            prepared.unbalanced,
            "the trailing quote is never closed, so the chapter is unbalanced"
        );
        // Everything after the last matched pair became dialogue, which is the
        // damage: prose the scanner can no longer see as prose.
        assert!(prepared
            .events
            .iter()
            .any(|e| e.kind == "dialogue" && e.text.starts_with("Đúng vậy")));
        let summary = prepared.split_summary();
        assert!(
            summary.contains("still open"),
            "the summary must name the open quote: {summary}"
        );
    }

    /// A balanced chapter must not be warned about, or the operator learns to
    /// ignore the line on every chapter that is fine.
    #[test]
    fn a_balanced_chapter_says_nothing_about_quotes() {
        let prepared = prepare_chapter("Hắn lật trang sách.\n\n\"Ngươi đọc xong chưa?\" hắn hỏi.");
        assert!(!prepared.unbalanced);
        let summary = prepared.split_summary();
        assert!(!summary.contains("still open"), "{summary}");
        assert!(!summary.contains("no narration at all"), "{summary}");
    }

    /// The other end of the same blind spot: a chapter that is one quoted
    /// system panel has no narration at all. Legal and real, so it is asked
    /// about rather than refused.
    #[test]
    fn an_all_dialogue_chapter_is_asked_about_not_refused() {
        let prepared = prepare_chapter("\"Ký chủ: Dịch Phong.\"\n\n\"Tuổi tác: 20.\"");
        assert!(!prepared.unbalanced);
        assert_eq!(
            prepared
                .events
                .iter()
                .filter(|e| e.kind == "narration")
                .count(),
            0
        );
        assert!(prepared.split_summary().contains("no narration at all"));
    }

    /// The defect this field exists for. A title inside narration is a quoted
    /// span with nobody talking, and the preparer cannot tell it from a hail —
    /// so it became a dialogue event, which `validate_attributions` then
    /// *forbade* from being Narrator. The only legal answer was a character or
    /// `Anonymous`, and 101 spans in this corpus were read that way: skill
    /// names, a panel label, a guqin piece title, each in a stranger's voice.
    #[test]
    fn a_quoted_title_in_narration_can_be_retracted_to_the_narrator() {
        let prepared = prepare_chapter(
            "Hắn lật ra cuốn sách \"Khải hoàn\" bất ngờ với nội dung bên trong.\n\nDịch Phong ngẩng đầu.",
        );
        // The preparer calls it dialogue, and that is the whole problem: the
        // evidence is in the context, not in the two words.
        let title = prepared
            .events
            .iter()
            .find(|e| e.text == "Khải hoàn")
            .expect("the title is a prepared event");
        assert_eq!(title.kind, "dialogue");
        assert_eq!(title.id, "e0002");

        // The model retracts it and assigns Narrator: both halves agree, so it
        // is accepted and the segment is read by the narrator.
        let data = json!({
            "roster": ["Narrator"],
            "not_speech": ["e0002"],
            "speakers": {"e0002": "Narrator"}
        });
        let speakers = validate_attributions(&data, &bible_with_phong(), &prepared).unwrap();
        assert_eq!(speakers["e0002"], "Narrator");

        let script = json!({"segments": [
            {"source_id": "e0001", "speaker": "Narrator", "text": "Hắn lật ra cuốn sách"},
            {"source_id": "e0002", "speaker": "Narrator", "text": "Khải hoàn"},
            {"source_id": "e0003", "speaker": "Narrator", "text": "bất ngờ với nội dung bên trong."},
            {"source_id": "e0004", "speaker": "Narrator", "text": "Dịch Phong ngẩng đầu."}
        ], "fixes": []});
        validate_source_alignment(&script, &prepared, &not_speech_ids(&data).unwrap()).unwrap();
    }

    /// The case a keyword list would get wrong, and the reason this is a
    /// model call rather than a list: the *same* words, quoted aloud, really
    /// are dialogue. A technique name inside narration is a title; a technique
    /// name inside a question is speech. Only the surrounding context tells
    /// them apart, so nothing about "Yêu Đại Giới" itself may decide it.
    #[test]
    fn the_same_words_spoken_aloud_stay_dialogue() {
        let prepared = prepare_chapter(
            "Hắn lật ra cuốn sách \"Khải hoàn\" bất ngờ với nội dung bên trong.\n\n\"Ngươi đọc 'Yêu Đại Giới' chưa?\" hắn hỏi.",
        );
        let spoken = prepared
            .events
            .iter()
            .find(|e| e.text.contains("Yêu Đại Giới"))
            .expect("the spoken title is a prepared event");
        let quoted_title = prepared
            .events
            .iter()
            .find(|e| e.text == "Khải hoàn")
            .expect("the quoted title is a prepared event");
        assert_ne!(spoken.id, quoted_title.id, "two distinct events");

        // Absent from `not_speech`, the spoken one is ordinary dialogue and is
        // held to the ordinary rule: somebody on cast has to be speaking it.
        // The title beside it is retracted, exactly as in the previous test.
        let data = json!({
            "roster": ["Narrator", "Dịch Phong"],
            "not_speech": [quoted_title.id.clone()],
            "speakers": {
                quoted_title.id.clone(): "Narrator",
                spoken.id.clone(): "Dịch Phong"
            }
        });
        let not_speech = not_speech_ids(&data).unwrap();
        assert!(
            !not_speech.contains(&spoken.id),
            "the spoken title is not retracted"
        );
        let speakers = validate_attributions(&data, &bible_with_phong(), &prepared).unwrap();
        assert_eq!(speakers[&spoken.id], "Dịch Phong");

        // And retracting *this* one is not a free pass: it would narrate a
        // real speech, so it has to be claimed as non-speech, which the
        // context above does not support — the model has to actually say so.
        let retracted = json!({
            "roster": ["Narrator", "Dịch Phong"],
            "not_speech": [quoted_title.id.clone(), spoken.id.clone()],
            "speakers": {
                quoted_title.id.clone(): "Narrator",
                spoken.id.clone(): "Narrator"
            }
        });
        let speakers = validate_attributions(&retracted, &bible_with_phong(), &prepared).unwrap();
        assert_eq!(speakers[&spoken.id], "Narrator");
    }

    /// A disagreement resolves in favour of the listing, and does not fail the
    /// chapter. This is the live ch161 shape: the model listed four ids, agreed
    /// with itself on three and gave the fourth a character. Requiring
    /// agreement refused the chapter, and the single repair pass made it worse
    /// by dropping an unrelated event — one ambiguous id cost a whole chapter.
    /// The listing wins instead, so the worst case is one narrated span.
    #[test]
    fn a_retraction_wins_over_a_disagreeing_speaker() {
        let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
        let contradiction = json!({
            "roster": ["Narrator", "Dịch Phong"],
            "not_speech": ["e0002"],
            "speakers": {"e0002": "Dịch Phong"}
        });
        let speakers =
            validate_attributions(&contradiction, &bible_with_phong(), &prepared).unwrap();
        assert_eq!(
            speakers["e0002"], "Narrator",
            "the listing is the decision; the speaker it contradicts is overwritten"
        );

        // Listed with no speaker at all is the same case, and used to be
        // blamed as a dropped event.
        let orphan = json!({
            "roster": ["Narrator"],
            "not_speech": ["e0002"],
            "speakers": {}
        });
        let speakers =
            validate_attributions(&orphan, &json!({"characters": []}), &prepared).unwrap();
        assert_eq!(speakers["e0002"], "Narrator");
    }

    /// Narration still cannot be promoted to dialogue, and an unlisted
    /// dialogue event still cannot be narrated. The retraction moves one way
    /// only, which is what makes it safe to hand the model at all.
    #[test]
    fn the_retraction_is_one_way_only() {
        let prepared = prepare_chapter("Hắn bước ra cửa.\n\n\"Ngươi đi đâu?\" hắn hỏi.");
        let narration = prepared
            .events
            .iter()
            .find(|e| e.kind == "narration")
            .expect("there is narration");
        let speech = prepared
            .events
            .iter()
            .find(|e| e.kind == "dialogue")
            .expect("there is dialogue");

        // Listing a narration id does not make it answerable as speech. The
        // speaker it claims is overwritten rather than trusted, exactly as
        // before this field existed — a model cannot promote prose into a
        // line somebody has to deliver, however it asks.
        let hoisted = json!({
            "roster": ["Narrator", "Dịch Phong"],
            "not_speech": [narration.id.clone()],
            "speakers": {
                narration.id.clone(): "Dịch Phong",
                speech.id.clone(): "Dịch Phong"
            }
        });
        let speakers = validate_attributions(&hoisted, &bible_with_phong(), &prepared).unwrap();
        assert_eq!(
            speakers[&narration.id], "Narrator",
            "a claimed speaker for prose must be rewritten, not honoured"
        );
        assert_eq!(speakers[&speech.id], "Dịch Phong");

        // And a dialogue id with no retraction and no speaker is still the
        // old refusal, so an answer that ignores the field entirely behaves
        // exactly as it did before the field existed.
        let ignored = json!({"roster": ["Narrator"], "speakers": {}});
        let err =
            validate_attributions(&ignored, &json!({"characters": []}), &prepared).unwrap_err();
        assert!(err.to_string().contains("dropped source event"), "{err}");
    }

    /// The complaint is the entire input to the one repair pass, so it has to
    /// carry the line it is complaining about. ch347 failed on this: the model
    /// dropped `e0076`, was told only the bare id, dropped it again, and the
    /// chapter burned every racer in a loop. A label the model cannot look up
    /// is not a repairable instruction.
    #[test]
    fn a_dropped_event_complaint_quotes_the_line_it_names() {
        let prepared = prepare_chapter(
            "Gấu đen rụt đầu vào trong khe.\n\n\"Các ngươi không thấy ta đâu.\"\n\n\"A!\"",
        );
        // One event answered, one dropped, so the complaint is about a known id
        // rather than whichever happens to come first.
        let short = prepared
            .events
            .iter()
            .find(|e| e.text == "A!")
            .expect("the exclamation is a prepared dialogue event");
        let other = prepared
            .events
            .iter()
            .find(|e| e.kind == "dialogue" && e.id != short.id)
            .expect("there is a second dialogue event");
        let err = validate_attributions(
            &json!({
                "roster": ["Narrator", "anonymous:anon-1"],
                "speakers": {other.id.clone(): "anonymous:anon-1"}
            }),
            &json!({"characters": []}),
            &prepared,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&short.id), "{msg}");
        assert!(
            msg.contains("A!"),
            "the complaint must quote the dropped line so one repair can fix it: {msg}"
        );
    }

    /// Same for the not-in-roster complaint, which ch347 hit on a second id in
    /// the same chapter. Naming the speaker is not enough; the model has to be
    /// able to see which line it is being asked to re-attribute.
    #[test]
    fn a_roster_complaint_quotes_the_line_too() {
        let prepared = prepare_chapter("Hắn ngồi xuống.\n\n\"Ngươi đi đâu đấy?\" hắn hỏi.");
        let speech = prepared
            .events
            .iter()
            .find(|e| e.kind == "dialogue")
            .expect("there is dialogue");
        let err = validate_attributions(
            &json!({"roster": ["Narrator"], "speakers": {speech.id.clone(): "Ghost"}}),
            &json!({"characters": [{"name": "Ghost", "personality": "x",
                                    "voice_hint": "adult male", "tags": ["male"]}]}),
            &prepared,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Ngươi đi đâu đấy?"),
            "the complaint must quote the line: {err}"
        );
    }

    /// A malformed retraction is refused, not skipped. Silently ignoring a
    /// non-string entry would let a model believe it retracted a line when the
    /// code did not.
    #[test]
    fn a_malformed_retraction_is_refused_rather_than_ignored() {
        let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
        let err = validate_attributions(
            &json!({"roster": ["Narrator"], "not_speech": "e0002", "speakers": {}}),
            &json!({"characters": []}),
            &prepared,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be an array"), "{err}");

        let err = validate_attributions(
            &json!({"roster": ["Narrator"], "not_speech": [7], "speakers": {}}),
            &json!({"characters": []}),
            &prepared,
        )
        .unwrap_err();
        assert!(err.to_string().contains("non-string"), "{err}");
    }

    /// An absent field means the answer agrees with the preparer, which is
    /// what every script digested before this field existed means. The 345
    /// stored scripts stay valid and nothing re-digests.
    #[test]
    fn an_absent_retraction_field_means_no_retraction() {
        assert!(not_speech_ids(&json!({})).unwrap().is_empty());
        assert!(not_speech_ids(&json!({"not_speech": null}))
            .unwrap()
            .is_empty());
        assert!(not_speech_ids(&json!({"not_speech": []}))
            .unwrap()
            .is_empty());
    }

    /// **This is the test that would have caught the field not working.**
    ///
    /// The first live run put `not_speech` in the view's `note` and the model
    /// never emitted it. The cause was not the note: the template's own output
    /// contract listed the allowed keys and said "Dialogue must NEVER map to
    /// Narrator" with no exception, and a model reads the contract, not the
    /// footnote. The field was unreachable however the note was worded.
    ///
    /// So the retraction is pinned in *both* places, and the one that matters
    /// is the contract — asserted here on the string the model actually reads.
    #[test]
    fn the_prompt_contract_offers_the_retraction_not_only_the_note() {
        // The contract is appended in code, not only in the profile template,
        // so it has to be asserted on the string the model is actually handed.
        // Resolved, not built: which tree the prompts come from is the *binding's*
        // answer now (`adapters/<adapter>/prompts/`), and `Layout::new` knows only
        // the pre-split one at the root — which is why this reads the real
        // checkout rather than a fixture.
        let root = crate::paths::Layout::find_root().unwrap();
        let layout = crate::paths::Layout::resolve(root).unwrap();
        let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
        let prompt = build_attribution_prompt(&layout, &json!({}), &prepared, None, None).unwrap();
        assert!(
            prompt.contains("not_speech"),
            "the schema block must show the field, or a model returning the \
             documented shape has no way to reach it"
        );
        assert!(
            prompt.contains("The ONE exception"),
            "the absolute 'dialogue is NEVER Narrator' rule needs its exception \
             next to it, not in a note"
        );
        // The exception must be an exception and not a replacement: a real
        // speech still goes to a character, said aloud or not. Matched on
        // folded whitespace so rewrapping the paragraph cannot silently
        // un-assert this.
        let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("are real dialogue and stay with a character"),
            "the contract must still forbid Narrator for real speech"
        );
        // And the view's note must not restate the rules, or the two can
        // disagree — the first live run did exactly that.
        let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
        let note = view["note"].as_str().unwrap();
        assert!(
            !note.contains("NEVER"),
            "rules live in the contract: {note}"
        );
    }

    /// The prompt has to offer the field, or the model cannot use it. The
    /// `note` is the only place that describes the answer, so this is what
    /// makes the retraction reachable.
    #[test]
    fn the_attribution_view_offers_the_retraction() {
        let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
        let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
        let note = view["note"].as_str().unwrap();
        assert!(note.contains("not_speech"), "{note}");
        // And it must keep saying the rest, or the field reads as a licence
        // to answer nothing at all.
        assert!(note.contains("every `dialogue_events` id"), "{note}");
    }

    #[test]
    fn a_written_sound_is_collapsed_rather_than_left_beside_its_tag() {
        let mut data = json!({"segments": [
            {"source_id": "e0001", "speaker": "Narrator", "text": "[hắng giọng] Khụ khụ khụ, ban đầu ta cầm bảo đao."},
            {"source_id": "e0002", "speaker": "Narrator", "text": "Khụ khụ khụ, ban đầu ta cầm bảo đao."},
            {"source_id": "e0003", "speaker": "Narrator", "text": "Hắn lật trang sách."},
        ]});
        collapse_redundant_sounds(&mut data);
        // The tag and the words it stands for are spoken as one cough, and the
        // source verbatim is normalized the same way, one door for both.
        for i in [0, 1] {
            assert_eq!(
                data["segments"][i]["text"], "[hắng giọng] ban đầu ta cầm bảo đao.",
                "segment {i}"
            );
        }
        assert_eq!(data["segments"][2]["text"], "Hắn lật trang sách.");
    }

    #[test]
    fn written_laughter_is_recognized_wherever_it_sits_in_the_line() {
        // The engine counts `haha`, one word, and a laugh in the middle of a
        // line. Rule 7 used to describe only "Ha ha" leading a line, which is
        // how ch22 failed on every racer.
        assert_eq!(
            retag_text("Vài ngày nữa trời lạnh, haha, vừa sạch sẽ."),
            Some("Vài ngày nữa trời lạnh, [cười] vừa sạch sẽ.".into())
        );
        assert_eq!(
            retag_text("Khụ khụ khụ, ban đầu ta cầm bảo đao."),
            Some("[hắng giọng] ban đầu ta cầm bảo đao.".into())
        );
        // Ignoring written sound is symmetric now: the stripper runs on both
        // sides, not on the already-retagged expected alone.
        assert_eq!(
            source_without_written_sound("…cho mình, haha, vừa sạch sẽ."),
            source_without_written_sound("…cho mình, [cười] vừa sạch sẽ.")
        );
    }

    #[test]
    fn vietnamese_sigh_quantifiers_normalize_like_the_engine_tag() {
        for (written, tagged) in [
            (
                "Bành Anh thở dài một tiếng, nói:",
                "Bành Anh [thở dài] nói:",
            ),
            (
                "Bành Anh thở dài một hơi rồi mới cất lời:",
                "Bành Anh [thở dài] mới cất lời:",
            ),
        ] {
            let mut data = json!({"segments": [
                {"source_id": "e0015", "speaker": "Bành Anh", "text": written}
            ]});
            collapse_redundant_sounds(&mut data);
            assert_eq!(data["segments"][0]["text"], json!(tagged));
            assert!(
                source_text_matches(written, &[tagged.to_string()]),
                "source gate must accept the engine tag as the written sigh: {written}"
            );
        }

        let mut model_shape = json!({"segments": [
            {"source_id": "e0015", "speaker": "Bành Anh", "text": "Bành Anh [thở dài] một tiếng, nói:"}
        ]});
        collapse_redundant_sounds(&mut model_shape);
        assert_eq!(
            model_shape["segments"][0]["text"],
            json!("Bành Anh [thở dài] nói:")
        );
    }

    /// The sentence's own period, which went with the sound on the way in.
    ///
    /// `retag_text` truncates `"…trượt tay, ha ha."` to `"…trượt tay, [cười]"`,
    /// so a model that writes the line back with its period — the more correct
    /// of the two — differed from the expectation by one mark and was refused
    /// for it. ch386 lost 4 of its 15 attempts to exactly this, and no repair
    /// could act on it: the message asks the model to delete a period it never
    /// added. `written_sound_hint` has always compared the two texts with the
    /// sounds stripped on *both* sides; this is the same courtesy for the mark
    /// the tag swallowed.
    #[test]
    fn a_period_beside_an_engine_tag_is_not_a_source_change() {
        let expected = "Cũng may, cũng may không bị trượt tay, [cười]";
        assert!(
            source_text_matches(
                expected,
                &["Cũng may, cũng may không bị trượt tay, [cười].".to_string()]
            ),
            "the tag's own period must not read as a change"
        );
        assert!(source_text_matches(
            "Hắn gầm lên, [hắng giọng]",
            &["Hắn gầm lên, [hắng giọng]!".to_string()]
        ));
        // The tag *and* the words it stands for is still the error it is: only
        // punctuation around a tag is forgiven, never the text beside it.
        assert!(!source_text_matches(
            expected,
            &["Cũng may, cũng may không bị trượt tay, [cười] ha ha.".to_string()]
        ));
    }

    #[test]
    fn source_gate_names_a_leftover_written_sound() {
        let prepared = prepare_chapter(
            "Chương 1: Một chuyến gặp\n\n\"Khụ khụ khụ, ban đầu ta cầm bảo đao.\" Thanh Sơn lão tổ nói.",
        );
        let dialogue = prepared
            .events
            .iter()
            .find(|e| e.kind == "dialogue")
            .expect("the cough line is prepared");
        let tail = prepared
            .events
            .iter()
            .find(|e| e.kind == "narration")
            .expect("its tag is prepared");
        assert!(corrected_source(dialogue, &[]).starts_with("[hắng giọng]"));

        let answer = |cough: &str| {
            json!({"segments": [
                {"source_id": dialogue.id, "speaker": "Thanh Sơn lão tổ", "text": cough},
                {"source_id": tail.id, "speaker": "Narrator", "text": tail.text},
            ], "fixes": []})
        };

        // Hoisting the tag to the head of the line and keeping the words is
        // refused, and the error names the sound instead of saying only that
        // the event "was changed".
        let hoisted = answer("[hắng giọng] Khụ khụ khụ, ban đầu ta cầm bảo đao.");
        let err = validate_source_alignment_no_retractions(&hoisted, &prepared).unwrap_err();
        assert!(err.to_string().contains("written sound"), "{err}");

        // The same answer passes once it comes through `retag_text`, which is
        // what `parse_staged_script` does before it validates and persists.
        let mut collapsed = hoisted.clone();
        collapse_redundant_sounds(&mut collapsed);
        validate_source_alignment_no_retractions(&collapsed, &prepared).unwrap();

        // A genuine change keeps the honest generic message: the hint fires
        // only when written sound is the whole disagreement.
        let rewritten = answer("Đêm ấy trời trở gió.");
        let err = validate_source_alignment_no_retractions(&rewritten, &prepared).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert!(!err.to_string().contains("written sound"), "{err}");
    }

    #[test]
    fn roster_entry_that_is_a_bible_alias_names_the_canonical_form() {
        let bible = json!({"characters": [
            {"name": "Vũ Kiệt", "proper_aliases": ["Vu Vũ Kiệt"]},
        ]});
        // The alias has no voice to resolve to: `speaker` is matched against
        // `roster` and the cast is keyed by the canonical name.
        let aliased = json!({
            "roster": ["Narrator", "Vu Vũ Kiệt"],
            "segments": [{"speaker": "Vu Vũ Kiệt", "text": "Anh nhi, xong chưa?"}],
        });
        let err = validate_digest_identity(&aliased, &bible).unwrap_err();
        assert!(err.to_string().contains("alias"), "{err}");
        assert!(err.to_string().contains("Vũ Kiệt"), "{err}");

        // Canonical name in `roster`, alias in `mentions`: the contract.
        let canonical = json!({
            "roster": ["Narrator", "Vũ Kiệt"],
            "mentions": {"Vu Vũ Kiệt": "Vũ Kiệt"},
            "segments": [{"speaker": "Vũ Kiệt", "text": "Anh nhi, xong chưa?"}],
        });
        validate_digest_identity(&canonical, &bible).unwrap();
    }

    #[test]
    fn a_mention_may_name_a_character_who_does_not_speak_here() {
        let bible = json!({"characters": [
            {"name": "Vũ Kiệt", "proper_aliases": ["Vu Vũ Kiệt"]},
            {"name": "Thanh Sơn lão tổ", "proper_aliases": []},
        ]});
        // `roster` is the speaker list, so a character named only in the
        // narration is genuinely absent from it, and that must not make the
        // mention illegal.
        let data = json!({
            "roster": ["Narrator", "Thanh Sơn lão tổ"],
            "mentions": {"Vu Vũ Kiệt": "Vũ Kiệt"},
            "segments": [{"speaker": "Narrator", "text": "Thanh Sơn lão tổ hiện ra trước Vu Vũ Kiệt."}],
        });
        validate_digest_identity(&data, &bible).unwrap();

        // The owner still has to be the canonical name, never the surface form.
        let self_mapped = json!({
            "roster": ["Narrator", "Thanh Sơn lão tổ"],
            "mentions": {"Vu Vũ Kiệt": "Vu Vũ Kiệt"},
            "segments": [{"speaker": "Narrator", "text": "Thanh Sơn lão tổ hiện ra trước Vu Vũ Kiệt."}],
        });
        let err = validate_digest_identity(&self_mapped, &bible).unwrap_err();
        assert!(err.to_string().contains("owned by"), "{err}");
        assert!(err.to_string().contains("Vũ Kiệt"), "{err}");
    }

    #[test]
    fn source_gate_allows_sound_splits_under_one_id() {
        let prepared = prepare_chapter("Chương 1: Một cảnh\n\nHắn lật trang sách.");
        let data = json!({"segments": [
            {"source_id": "e0001", "speaker": "Narrator", "text": "Hắn lật"},
            {"source_id": "e0001", "speaker": "Narrator", "text": "trang sách."},
        ], "fixes": []});
        validate_source_alignment_no_retractions(&data, &prepared).unwrap();
    }

    #[test]
    fn source_gate_rejects_a_split_that_changes_speaker() {
        let prepared = prepare_chapter("Chương 1: Một cảnh\n\n\"Anh nhi, xong chưa?\"");
        let data = json!({"segments": [
            {"source_id": "e0001", "speaker": "Vũ Kiệt", "text": "Anh nhi,"},
            {"source_id": "e0001", "speaker": "Bành Anh", "text": "xong chưa?"},
        ], "fixes": []});
        let err = validate_source_alignment_no_retractions(&data, &prepared).unwrap_err();
        assert!(err.to_string().contains("split across speakers"), "{err}");
    }

    #[test]
    fn source_gate_ignores_repeated_standalone_headlines() {
        let prepared = prepare_chapter(
            "Chương 1: Một cảnh\n\nHắn bước đi.\n\nChương 1: Một cảnh\n\nHắn dừng lại.",
        );
        assert_eq!(prepared.events.len(), 2);
        assert_eq!(prepared.events[0].text, "Hắn bước đi.");
        assert_eq!(prepared.events[1].text, "Hắn dừng lại.");
    }

    /// The sound fields come off the lines and become items at their seams.
    ///
    /// This is the concession the whole shape rests on, so it is pinned: the
    /// field must never survive onto a line, `text` must never be touched, a
    /// *bad* name must still be lifted so the validator can refuse it, and a
    /// blank must be refused here, a blank is a line nobody decided about, and
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
        // The sound lands between the halves, not after the whole line.
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
        assert!(gap.contains("phòng bếp"), "{gap}");

        // ch15 names a cleaver while asking for one to be forged later; an
        // object mentioned in dialogue is not a chopping action to sound now.
        let chapter15 = "Dịch Phong vươn tay lấy ra con dao phay nói: nhớ lần trước bá mẫu nói qua, nhờ ta rèn một con dao phay lúc rảnh rỗi, giúp ta mang cho họ nhé!";
        let mention_only = json!({"segments": [line(chapter15)]});
        assert!(
            sound_design_gap(&mention_only, chapter15, &pool).is_none(),
            "a mentioned cleaver must not require a chopping sound"
        );

        // 2. The bed opened and never closed, ch9's exact answer.
        let unclosed = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "food-prep"},
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
        ]});
        let gap = sound_design_gap(&unclosed, chapter, &pool).expect("an unclosed bed");
        assert!(
            gap.contains("food-prep") && gap.contains("stops dead"),
            "{gap}"
        );

        // 3. Closed: both halves satisfied.
        let closed = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "food-prep"},
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
            {"stop": "food-prep"},
        ]});
        assert!(
            sound_design_gap(&closed, chapter, &pool).is_none(),
            "closed must pass"
        );

        // 4. A one-shot needs no stop, only a `looped` sound does.
        let oneshot = json!({"segments": [
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "coin"},
            line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
        ]});
        assert!(sound_design_gap(&oneshot, chapter, &pool).is_none());

        // 5. A chapter that stages nothing is allowed to place nothing.
        assert!(sound_design_gap(&silent, "Trời hôm nay đẹp.", &pool).is_none());
    }

    /// The soft-release curve: three blocks, then accept. Pinned because the
    /// loop's termination hangs on it — a curve that never drops below half
    /// is the deadlock back again.
    #[test]
    fn gap_block_p_releases_on_the_fourth_failure() {
        assert!((gap_block_p(0) - 0.9).abs() < 1e-9);
        assert!(gap_block_p(1) >= 0.5);
        assert!(gap_block_p(2) >= 0.5);
        assert!(gap_block_p(3) < 0.5);
    }

    /// ch262's false positive, pinned: bare `chém` is an idiom
    /// ("muốn chém muốn giết"), not a staged slash, and must not fail a
    /// chapter that places nothing. The compounds still catch real blades.
    #[test]
    fn bare_chem_is_no_sound_cue() {
        use crate::audio_pool::ClipPool;
        let pool = ClipPool::new();
        let line = |t: &str| json!({"speaker": "Narrator", "text": t});
        let chapter = "Muốn chém muốn giết, người cứ nói thẳng ra một lời.";
        let silent = json!({"segments": [line(chapter)]});
        assert!(sound_design_gap(&silent, chapter, &pool).is_none());
        let chapter2 = "Hắn rút kiếm chém xuống.";
        let silent2 = json!({"segments": [line(chapter2)]});
        assert!(sound_design_gap(&silent2, chapter2, &pool).is_some());
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
            "speakers": {"e0001": "Narrator"},
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
        assert_eq!(merged["speakers"], json!({"e0001": "Narrator"}));
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
            "speakers",
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
        let p = build_script_prompt(&layout, "vieneu", &bible, &context, &text).unwrap();
        for ph in [
            "{music_palette}",
            "{effect_tags}",
            "{inject_sounds}",
            "{cast_json}",
            "{bible_json}",
            "{chapter_text}",
            "{voice_tags}",
            "{tag_laugh}",
            "{tag_sigh}",
            "{tag_throat}",
        ] {
            assert!(!p.contains(ph), "placeholder leaked: {ph}");
        }
        assert!(p.contains("quiet (soft, calm;"), "{p}");
        assert!(p.contains("battle, birds, calm"), "{p}");
        assert!(p.contains("blood-spatter (hit; blood"), "{p}");

        // The non-verbal vocabulary is VieNeu's, so its tags render — and an
        // engine that voices none gets the *rule* removed rather than negated:
        // a rule that says "none" still teaches the model to write brackets.
        assert!(p.contains("[cười] [thở dài] [hắng giọng]"), "{p}");
        let none = build_script_prompt(&layout, "gemini", &bible, &context, &text).unwrap();
        assert!(
            !none.contains("NON-VERBAL"),
            "another engine must not be taught the rule at all: {none}"
        );
        assert!(!none.contains("[cười]"), "{none}");
        assert!(!none.contains("{tag_"), "placeholder leaked: {none}");
    }

    /// The manual path's contract, against the fixture.
    ///
    /// **The manual rounds are the worker's two prompts, not a second pair.** A
    /// chapter finished by hand must be dramatized by the same contract the
    /// automatic path enforces, or "the same digest with a person standing in for
    /// the model" is not true of it. So this pins the *identity*: round 1 is
    /// `build_attribution_prompt` (dialogue events with source ids to answer),
    /// round 2 is `build_staging_prompt` (the same events, the speaker map fixed,
    /// staging only), and the hand-off between them carries that map.
    #[test]
    fn the_manual_rounds_ask_for_the_right_prompt_and_refuse_a_paste_out_of_order() {
        let dir = std::env::temp_dir().join("bm-manual-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        crate::profile::install_fixture(&dir).expect("fixture profile");
        let layout = Layout::new(&dir);
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(
            layout.chapter_txt(51),
            "Chương 51: Fixture\n\nHắn gật đầu.\n\n\"Ừm!\"\n",
        )
        .unwrap();

        // Round 1 is the attribution pass, and the fixture's template is a stub
        // ("production prompts live in the profile"), so the assertions are on
        // substitution and on the contract appended to it.
        let first = manual_prompt(&layout, "vieneu", 51, None).unwrap();
        assert_eq!(first.round, Round::Cast);
        assert!(
            first.text.contains("Fixture dramatization prompt"),
            "the attribution template, as the fixture ships it: {}",
            head_chars(&first.text, 120)
        );
        // The chapter arrives as prepared events: the quote is answerable, the
        // prose around it is evidence, and no raw text is handed over.
        assert!(first.text.contains("dialogue_events"), "{}", first.text);
        assert!(first.text.contains("Ừm!"), "the line it must attribute");
        assert!(
            first.text.contains("---ATTRIBUTION OUTPUT CONTRACT---"),
            "the worker's own contract: {}",
            head_chars(&first.text, 200)
        );
        assert!(
            !first.text.contains("{chapter_text}"),
            "no placeholder leaked"
        );

        // A paste for round 2 with no cast is refused by name, rather than
        // rendering a staging prompt against a cast that does not exist.
        let err = manual_accept(&layout, 51, Round::Script, "{}", None)
            .expect_err("round 2 needs round 1");
        assert!(err.to_string().contains("round 1's cast"), "{err}");

        // A garbage paste fails the *worker's* validator, the same one, and
        // says so in words the operator can paste back into their model.
        let err =
            manual_accept(&layout, 51, Round::Cast, "not json at all", None).expect_err("not JSON");
        assert!(err.to_string().contains("not valid JSON"), "{err:#}");

        // With a cast in hand, round 2 renders the *staging* prompt against it.
        let cast = json!({
            "roster": ["Narrator", "Anonymous"],
            "mentions": {},
            "speakers": {"e0002": "Anonymous"}
        });
        let second = manual_prompt(&layout, "vieneu", 51, Some(&cast)).unwrap();
        assert_eq!(second.round, Round::Script);
        assert_ne!(second.text, first.text, "a different pass, not a repeat");
        assert!(second.text.contains("---STAGING OUTPUT CONTRACT---"));
        assert!(
            second.text.contains("fixed_speakers"),
            "round 2 is rendered against the map round 1 fixed"
        );
        for ph in ["{cast_json}", "{music_palette}", "{inject_sounds}"] {
            assert!(!second.text.contains(ph), "placeholder leaked: {ph}");
        }

        // A chapter with no text fails by path, so the operator knows which file
        // the crawl never produced rather than reading a bare "no such file".
        let err = manual_prompt(&layout, "vieneu", 999, None).expect_err("no chapter text");
        assert!(err.to_string().contains("ch999"), "{err:#}");
    }

    /// The happy path, end to end, without a model.
    ///
    /// Two pastes and a finished chapter, the flow the TUI drives with `c` and
    /// `v` and the backup digestor drives with a model, exercised through
    /// `manual_accept` so the seam between the rounds is real rather than
    /// assumed. What this buys that the per-part tests cannot: it proves the
    /// round-1 answer is *usable* as round 2's input.
    ///
    /// The staging answer names **no speaker at all**, it cannot, and saying so
    /// in the fixture is the point: the map round 1 fixed is attached by code,
    /// and the finished script shows both of its decisions (Narrator for the
    /// prose, the anonymous slot for the quote). The `calm` alias must land as
    /// `quiet`, proving the alias is applied on the real manual-validation path
    /// rather than only in the normalization unit test.
    #[test]
    fn a_valid_pair_of_pastes_finishes_the_chapter() {
        let dir = std::env::temp_dir().join("bm-manual-happy");
        let _ = std::fs::remove_dir_all(&dir);
        crate::profile::install_fixture(&dir).expect("fixture profile");
        let layout = Layout::new(&dir);
        std::fs::create_dir_all(layout.chapters()).unwrap();
        std::fs::write(
            layout.chapter_txt(51),
            "Chương 51: Fixture\n\nHắn gật đầu.\n\n\"Ừm!\"\n",
        )
        .unwrap();

        // Round 1: the attribution answer, one entry per *dialogue* event, in
        // source order. Narration is not the model's to answer; code owns it.
        let cast = manual_accept(
            &layout,
            51,
            Round::Cast,
            r#"{"title": "Dao Phay Trong Bếp", "atmosphere": "A quiet kitchen at dusk.",
                "roster": ["Narrator", "Anonymous"], "mentions": {},
                "new_characters": [], "new_aliases": {},
                "speakers": {"e0002": "Anonymous"}}"#,
            None,
        )
        .expect("a well-formed attribution answer");
        let cast = cast.cast.expect("round 1 yields the cast");
        assert!(cast.get("outcome").is_none(), "and nothing finished");
        assert_eq!(cast["speakers"]["e0002"], json!("Anonymous"));
        // Narration is attached by code, so it is in the map the *next* round
        // reads even though the model never answered for it.
        assert_eq!(cast["speakers"]["e0001"], json!("Narrator"));

        // Round 2: staging, carrying no speaker, checked against that map.
        let done = manual_accept(
            &layout,
            51,
            Round::Script,
            r#"{"segments": [
                {"source_id": "e0001", "text": "Hắn gật đầu.", "music": "calm"},
                {"source_id": "e0002", "text": "Ừm!", "music": "calm"}],
                "fixes": []}"#,
            Some(&cast),
        )
        .expect("a well-formed staging answer against the map it was given");
        let outcome = done.outcome.expect("round 2 finishes the chapter");

        assert_eq!(outcome.segments, 2, "two spoken segments");
        assert_eq!(outcome.script["title"], json!("Dao Phay Trong Bếp"));
        let segments = outcome.script["segments"].as_array().unwrap();
        assert_eq!(segments[0]["speaker"], json!("Narrator"));
        assert_eq!(segments[1]["speaker"], json!("Anonymous"));
        assert_eq!(segments[0]["music"], json!("quiet"));
        // The delta is what the inductor merges into the bible, a manual digest
        // has to produce one, or the next chapter would not know this cast.
        assert!(outcome.delta.get("roster").is_some(), "{:?}", outcome.delta);
        assert!(
            outcome.log.iter().any(|l| l.contains("segments=2")),
            "{:?}",
            outcome.log
        );

        // **And the hand-off is real, not decorative.** A staging answer that
        // drops an event is refused by round 2, otherwise the source gate is
        // decorative and a chapter can ship with words the novel never said.
        let err = manual_accept(
            &layout,
            51,
            Round::Script,
            r#"{"segments": [{"source_id": "e0001", "text": "Hắn gật đầu."}], "fixes": []}"#,
            Some(&cast),
        )
        .expect_err("an event the source gate never saw cannot land");
        assert!(
            err.to_string().contains("dropped") || err.to_string().contains("source"),
            "and says which source contract broke: {err:#}"
        );
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

    // -----------------------------------------------------------------------
    // windows: one chapter staged in more than one call
    // -----------------------------------------------------------------------

    /// A chapter long enough to need parts, **built rather than committed**.
    ///
    /// The shape is what the tests are about — one paragraph per event, a quoted
    /// line every third paragraph, and no cue word from rule 10's list, so the
    /// sound-design gates stay quiet — and a 40 KB blob in the repository would
    /// only pin the bytes. What it models is the real case: a novel chapter about
    /// three times the length of the longest chapter in the sample corpus.
    fn long_chapter(paragraphs: usize) -> String {
        let mut text = String::new();
        for i in 0..paragraphs {
            text.push_str(&format!(
                "Đoạn {i} kể rằng buổi chiều hôm ấy trời trở gió, và người trong sân vẫn đứng im \
                 như tượng đá trước hiên nhà, chẳng ai dám lên tiếng trước.\n"
            ));
            if i % 3 == 1 {
                text.push_str("\"Ngươi có nghe thấy tiếng gì không?\" người ấy hỏi.\n");
            }
        }
        text
    }

    /// A fixture workspace with one long chapter in it.
    fn long_layout(tag: &str, paragraphs: usize) -> (std::path::PathBuf, Layout, String) {
        let dir = std::env::temp_dir().join(format!("bm-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        crate::profile::install_fixture(&dir).expect("fixture profile");
        let layout = Layout::new(&dir);
        std::fs::create_dir_all(layout.chapters()).unwrap();
        let text = long_chapter(paragraphs);
        std::fs::write(layout.chapter_txt(51), &text).unwrap();
        (dir, layout, text)
    }

    /// A part's attribution answer as a model would write it: one speaker per
    /// *dialogue* event in the part, a roster naming only who speaks, and the
    /// `summary` a later part is handed.
    fn cast_answer(slice: &PreparedChapter) -> String {
        let speakers: serde_json::Map<String, Value> = slice
            .events
            .iter()
            .filter(|e| e.kind == "dialogue")
            .map(|e| (e.id.clone(), json!("Anonymous")))
            .collect();
        let roster = if speakers.is_empty() {
            json!(["Narrator"])
        } else {
            json!(["Narrator", "Anonymous"])
        };
        json!({
            "title": "Tiếng Hỏi Trong Sân",
            "atmosphere": "An empty courtyard at dusk.",
            "roster": roster,
            "mentions": {},
            "new_characters": [],
            "new_aliases": {},
            "not_speech": [],
            "speakers": speakers,
            "summary": "The courtyard falls quiet and someone asks a question.",
        })
        .to_string()
    }

    /// A part's staging answer: every event of the part, once, in source order,
    /// with its own text and no speaker — which is what the contract asks for and
    /// what the source gate proves.
    fn script_answer(slice: &PreparedChapter) -> String {
        let segments: Vec<Value> = slice
            .events
            .iter()
            .map(|e| json!({"source_id": e.id, "text": e.text}))
            .collect();
        json!({"segments": segments, "fixes": []}).to_string()
    }

    fn staged_part(from: usize, to: usize, context: Value, segments: Value) -> Part {
        Part {
            from,
            to,
            summary: format!("part {from}..{to}"),
            context,
            script: json!({"segments": segments, "fixes": []}),
        }
    }

    /// **The parity promise, in bytes.** A chapter that fits one call is asked
    /// the prompt it was always asked — no part note, no plot, and no `summary`
    /// field — so it cannot digest differently because windows exist. The same
    /// chapter asked as a part *does* carry the block, and part 2 is handed what
    /// part 1 said.
    #[test]
    fn a_chapter_that_fits_one_call_is_asked_the_prompt_it_always_was() {
        let (_dir, layout, text) = long_layout("one-call-prompt", 3);
        let prepared = prepare_chapter(&text);
        let bible = json!({"characters": []});
        let one = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
        for absent in ["PART 1 OF", "PLOT SO FAR", "\"summary\""] {
            assert!(!one.contains(absent), "{absent} reached a one-call prompt");
        }

        let first = Continuity {
            index: 0,
            total: 2,
            plot: &[],
        };
        let part_one = build_attribution_prompt(&layout, &bible, &prepared, Some(&first), None).unwrap();
        assert!(part_one.contains("PART 1 OF 2"), "{}", head_chars(&part_one, 40));
        assert!(part_one.contains("\"summary\""), "the field a part must return");
        assert!(
            !part_one.contains("PLOT SO FAR"),
            "the first part has nothing behind it"
        );

        let plot = vec!["They reach the courtyard and nobody speaks.".to_string()];
        let second = Continuity {
            index: 1,
            total: 2,
            plot: &plot,
        };
        let part_two = build_attribution_prompt(&layout, &bible, &prepared, Some(&second), None).unwrap();
        assert!(part_two.contains("PART 2 OF 2"));
        assert!(
            part_two.contains("They reach the courtyard"),
            "part 2 is handed part 1's own words"
        );

        // Staging has its own note, and the rule that matters for it is the bed
        // that may be closed by the part after this one.
        let cast = json!({"roster": ["Narrator"], "speakers": {"e0001": "Narrator"}});
        let quiet = build_staging_prompt(&layout, "vieneu", &bible, &cast, &prepared, None).unwrap();
        assert!(!quiet.contains("PART 1 OF"), "no part note when there is one part");
        let split = build_staging_prompt(
            &layout,
            "vieneu",
            &bible,
            &cast,
            &prepared,
            Some(&second),
        )
        .unwrap();
        assert!(split.contains("PART 2 OF 2"));
        assert!(
            split.contains("`loop`ed bed may run past the end of your part"),
            "the bed rule, told to the round that places beds"
        );
        assert!(split.contains("write no ending"), "and no rounding off");
    }

    /// The one cross-chapter memory: a stored predecessor's excerpt rides
    /// into the attribution prompt as `---PREVIOUSLY---`, and no predecessor
    /// means no block at all — the ordinary first chapter is the pre-excerpt
    /// prompt byte for byte.
    #[test]
    fn the_previous_excerpt_rides_as_a_previously_block() {
        let (_dir, layout, text) = long_layout("excerpt-prompt", 3);
        let prepared = prepare_chapter(&text);
        let bible = json!({"characters": []});

        let bare = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
        assert!(
            !bare.contains("PREVIOUSLY"),
            "no block without a predecessor: {}",
            head_chars(&bare, 40)
        );

        let with = build_attribution_prompt(
            &layout,
            &bible,
            &prepared,
            None,
            Some("CH 41: The white-robed swordswoman is still unnamed; she left with the party."),
        )
        .unwrap();
        assert!(with.contains("---PREVIOUSLY---"), "{}", head_chars(&with, 40));
        assert!(with.contains("still unnamed"), "the memory itself");
        assert!(
            with.contains("identity context only"),
            "the block names what it is for: resolve, not answer"
        );
    }

    /// The chain reads the stored scripts: an excerpt is picked up from
    /// `script(n-1)` the moment it exists, gaps are skipped, and the window
    /// is honored — the default depth of 1 never reaches past the previous
    /// chapter.
    #[test]
    fn previous_excerpts_read_stored_scripts_and_skip_gaps() {
        let (_dir, layout, _text) = long_layout("excerpt-store", 3);
        std::fs::create_dir_all(layout.script(41).parent().unwrap()).unwrap();
        std::fs::write(
            layout.script(41),
            r#"{"excerpt": "The stranger is still unnamed."}"#,
        )
        .unwrap();

        let got = previous_excerpts(&layout, 42).unwrap();
        assert_eq!(got, "CH 41: The stranger is still unnamed.");

        // The default window is 1: chapter 43 asks for script(42), which was
        // never written, and gets nothing — a gap is silence, not an error.
        assert!(previous_excerpts(&layout, 43).is_none());
        assert!(
            previous_excerpts(&layout, 41).is_none(),
            "the first chapter has no predecessor by definition"
        );
    }

    /// A part has to say what happened in it, because that summary is the whole
    /// of what the parts after it know. A chapter that did not split is validated
    /// exactly as before, which is the other half of the parity promise.
    #[test]
    fn a_part_of_a_chapter_has_to_say_what_happened_in_it() {
        let prepared = prepare_chapter("Hắn gật đầu.\n\n\"Ừm!\"\n");
        let bible = json!({"characters": []});
        let answer = |extra: &str| {
            format!(
                "{{\"title\": \"Tiếng Hỏi Trong Sân\", \"atmosphere\": \"Quiet.\", \"roster\": \
                 [\"Narrator\", \"Anonymous\"], \"mentions\": {{}}, \"new_characters\": [], \
                 \"new_aliases\": {{}}, \"speakers\": {{\"e0002\": \"Anonymous\"}}{extra}}}"
            )
        };
        parse_attribution(&answer(""), &bible, &prepared, false)
            .expect("a one-call chapter is asked for no summary");
        let err = parse_attribution(&answer(""), &bible, &prepared, true)
            .expect_err("a part without a summary is a part the rest continues blind");
        assert!(err.to_string().contains("summary"), "{err:#}");
        let ok = parse_attribution(
            &answer(", \"summary\": \"Hắn đồng ý.\""),
            &bible,
            &prepared,
            true,
        )
        .unwrap();
        assert_eq!(ok["summary"], json!("Hắn đồng ý."));
    }

    /// The merge: source order, one identity per person, and every speaker from
    /// every part in one map.
    #[test]
    fn merging_parts_keeps_the_source_order_and_every_identity() {
        let a = staged_part(
            0,
            2,
            json!({
                "title": "Tiếng Hỏi Trong Sân",
                "atmosphere": "A courtyard at dusk.",
                "roster": ["Narrator"],
                "mentions": {"hắn": "Dịch Phong"},
                "new_characters": [{"name": "Dịch Phong", "personality": "wry"}],
                "new_aliases": {"Lão Phong": "Dịch Phong"},
                "speakers": {"e0001": "Narrator"},
            }),
            json!([{"source_id": "e0001", "text": "Hắn gật đầu."}]),
        );
        let b = staged_part(
            2,
            3,
            json!({
                "title": "Something Else",
                "atmosphere": "A hall at noon.",
                "roster": ["Narrator", "Anonymous"],
                "mentions": {"hắn": "Dịch Phong"},
                "new_characters": [{"name": "Dịch Phong", "voice_hint": "low"}],
                "new_aliases": {},
                "speakers": {"e0003": "Anonymous"},
            }),
            json!([{"source_id": "e0003", "text": "Ừm!"}]),
        );
        let script = merge_scripts([&a.script, &b.script]);
        let (merged, conflicts) = merge_contexts(&[a, b]);
        assert!(conflicts.is_empty(), "{conflicts:?}");

        // The script is the parts in order, which is source order.
        let segments = script["segments"].as_array().unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0]["source_id"], json!("e0001"));
        assert_eq!(segments[1]["source_id"], json!("e0003"));

        // The opening part names the chapter and sets its mood; the rest is the
        // union of what the parts knew.
        assert_eq!(merged["title"], json!("Tiếng Hỏi Trong Sân"));
        assert_eq!(merged["atmosphere"], json!("A courtyard at dusk."));
        assert_eq!(merged["roster"], json!(["Narrator", "Anonymous"]));
        assert_eq!(merged["speakers"]["e0001"], json!("Narrator"));
        assert_eq!(merged["speakers"]["e0003"], json!("Anonymous"));
        assert_eq!(merged["mentions"]["hắn"], json!("Dịch Phong"));
        assert_eq!(merged["new_aliases"]["Lão Phong"], json!("Dịch Phong"));

        // One character declared twice is one character, and the later part fills
        // the field the earlier one left blank without overwriting what it said.
        let characters = merged["new_characters"].as_array().unwrap();
        assert_eq!(characters.len(), 1, "{characters:?}");
        assert_eq!(characters[0]["personality"], json!("wry"));
        assert_eq!(characters[0]["voice_hint"], json!("low"));
    }

    /// Two parts owning one surface form differently is the one thing a union
    /// cannot fix, so it is named rather than swallowed.
    #[test]
    fn two_parts_owning_one_form_differently_is_reported() {
        let part = |owner: &str| {
            staged_part(
                0,
                1,
                json!({"mentions": {"hắn": owner}}),
                json!([{"source_id": "e0001", "text": "x"}]),
            )
        };
        let (merged, conflicts) = merge_contexts(&[part("Dịch Phong"), part("Vũ Kiệt")]);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].contains("Dịch Phong") && conflicts[0].contains("Vũ Kiệt"));
        assert_eq!(
            merged["mentions"]["hắn"],
            json!("Dịch Phong"),
            "the earlier part wins"
        );
        // Agreement is not a conflict, however many parts say the same thing.
        let (_, conflicts) = merge_contexts(&[part("Dịch Phong"), part("Dịch Phong")]);
        assert!(conflicts.is_empty(), "{conflicts:?}");
    }

    /// The checkpoint is what makes a sixteen-call chapter survivable: a part is
    /// only in it once it is finished, and it is only honoured while the chapter
    /// text, the bible and the plan still say what they said when it was staged.
    #[test]
    fn the_parts_checkpoint_resumes_what_matches_and_forgets_what_does_not() {
        let (_dir, layout, text) = long_layout("parts-checkpoint", 300);
        let settings = Settings::load(&layout.settings());
        let bible = json!({"characters": []});
        let prepared = prepare_chapter(&text);
        let windows = plan_windows(&prepared, &settings.digest);
        assert!(windows.len() > 1, "{}", windows.len());

        let mut parts = Parts::open(&layout, 51, &text, &bible, &windows, &settings);
        assert_eq!(parts.len(), 0, "nothing is staged before the first call");
        let first = staged_part(
            windows[0].from,
            windows[0].to,
            json!({"title": "T", "summary": "câu chuyện mở ra"}),
            json!([{"source_id": prepared.events[windows[0].from].id, "text": "x"}]),
        );
        parts.push(first.clone()).unwrap();
        assert!(!parts.path.exists() || parts.path.metadata().is_ok());

        let reopened = Parts::open(&layout, 51, &text, &bible, &windows, &settings);
        assert_eq!(reopened.len(), 1, "the finished part is resumed, not re-asked");
        assert_eq!(reopened.summaries().len(), 1);

        // A different plan is a different chapter's work: the stored parts are
        // answers to a question nobody is asking any more.
        let chunkier = crate::config::DigestSettings {
            chunk_sentences: 4,
            chunk_chars: 0,
            answer_tokens: 0,
        };
        let other = plan_windows(&prepared, &chunkier);
        assert_ne!(other.len(), windows.len());
        assert_eq!(
            Parts::open(&layout, 51, &text, &bible, &other, &settings).len(),
            0
        );
        // As is an edited chapter, even under the same plan.
        assert_eq!(
            Parts::open(&layout, 51, &format!("{text} x"), &bible, &windows, &settings).len(),
            0
        );
        // As is another bible, which is what makes later parts' casts safe.
        assert_eq!(
            Parts::open(
                &layout,
                51,
                &text,
                &json!({"characters": [{"name": "Dịch Phong"}]}),
                &windows,
                &settings,
            )
            .len(),
            0
        );
        // And the plan moving under a stored part drops it, rather than staging
        // a script against events the part never saw.
        let shifted: Vec<Window> = windows
            .iter()
            .skip(1)
            .map(|w| Window {
                from: w.from - windows[0].events,
                to: w.to - windows[0].events,
                ..*w
            })
            .collect();
        assert_eq!(
            Parts::open(&layout, 51, &text, &bible, &shifted, &settings).len(),
            0
        );
    }

    /// The gates name the part that owes an answer: the one that opened the
    /// surviving bed, or the one whose own prose stages a cue and whose own
    /// segments place none.
    #[test]
    fn the_gate_names_the_part_that_owes_an_answer() {
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

        // Rule 1 across a part boundary: opened in part 2, never closed. The
        // part that opened it is the part to re-ask, and the merged check is what
        // knows the chapter as a whole.
        let quiet = json!({"segments": [line("Trời tối."), {"sound": "coin"}]});
        let opens = json!({"segments": [line("Hai người đến phòng bếp."), {"sound": "food-prep"}, line("Rồi đi ra.")]});
        let scripts = [&quiet, &opens];
        let (gap, owner) = sound_gap(&scripts, &["Trời tối.", "Hai người."], &pool, "part")
            .expect("an open bed");
        assert_eq!(owner, 1, "the part that opened it");
        assert!(gap.contains("food-prep") && gap.contains("stops dead"), "{gap}");

        // Rule 2 is per part, and this is why: part 1's prose stages a kitchen
        // and part 1's segments place nothing, while part 2 did place a sound.
        // A merged check would have seen a placement and passed the chapter.
        let silent = json!({"segments": [line("Hắn đi đến phòng bếp.")]});
        let placed = json!({"segments": [line("Trời tối."), {"sound": "coin"}]});
        let scripts = [&silent, &placed];
        let (gap, owner) = sound_gap(
            &scripts,
            &["Hắn đi đến phòng bếp.", "Trời tối."],
            &pool,
            "part",
        )
        .expect("a part that staged a cue and placed none");
        assert_eq!(owner, 0);
        assert!(gap.contains("phòng bếp"), "{gap}");
        assert!(gap.contains("part"), "the message names what it is about");

        // A chapter that did not split goes through `sound_design_gap` itself, so
        // its message still says `chapter`.
        let scripts = [&silent];
        let gap = sound_gap(&scripts, &["Hắn đi đến phòng bếp."], &pool, "chapter")
            .expect("a staged chapter with none");
        assert!(gap.0.contains("chapter"), "{}", gap.0);
    }

    /// **The end-to-end proof, without a model.** A 40 KB chapter — three times
    /// the longest chapter in the sample corpus — is planned into parts, and the
    /// whole manual flow is driven over it part by part: round 1, round 2, on to
    /// the next part, until the chapter finishes. What it establishes that the
    /// per-unit tests cannot:
    ///
    /// 1. the answer to one call would not have fit (so the split is the reason a
    ///    long chapter is digestible at all, not a preference), while every part
    ///    fits under the budget with room to spare;
    /// 2. every part's prompt is a fraction of the one the whole chapter would
    ///    have needed;
    /// 3. the merged script is the script the single-call digest would have
    ///    written: one segment per source event, in source order, nothing dropped
    ///    at a part boundary;
    /// 4. the checkpoint exists while the chapter is half-staged and is gone
    ///    once it is written.
    #[test]
    fn a_long_chapter_is_staged_in_parts_and_merges_into_one_script() {
        let (_dir, layout, text) = long_layout("manual-parts", 220);
        let prepared = prepare_chapter(&text);
        let settings = Settings::load(&layout.settings());
        let windows = plan_windows(&prepared, &settings.digest);
        assert!(
            windows.len() > 1,
            "a {}-char chapter has to need parts: {} windows",
            text.chars().count(),
            windows.len()
        );

        // (1) Why parts exist, in the numbers the backends impose: the answer to
        // one call is over the hard cap every backend sets, so today it would be
        // truncated mid-JSON and refused after a repair that fails the same way.
        let whole_chars = weight(&prepared.events);
        assert!(
            tokens(whole_chars) > 16_384,
            "the fixture must be over the hard cap in one call: {} tokens",
            tokens(whole_chars)
        );
        for (i, w) in windows.iter().enumerate() {
            assert!(
                tokens(w.chars) < settings.digest.answer_tokens as usize,
                "part {} does not fit the budget either: {} tokens",
                i + 1,
                tokens(w.chars)
            );
        }

        // (2) The prompt shrinks with the part, which is the truncation being
        // prevented on the *input* side as well — one backend shares the context
        // between prompt and answer.
        let bible = load_bible(&layout.bible());
        let whole_prompt = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
        let checkpoint = layout.data().join(".digest-parts-ch51.json");

        // (3) The flow, part by part, exactly as the TUI and the backup runner
        // drive it.
        let mut cast: Option<Value> = None;
        let mut outcome = None;
        let mut prompts: Vec<ManualPrompt> = Vec::new();
        let mut part_prompts: Vec<String> = Vec::new();
        for i in 0..windows.len() {
            let slice = windows[i].prepared(&prepared);

            let step = manual_prompt(&layout, "vieneu", 51, cast.as_ref()).unwrap();
            assert_eq!(step.round, Round::Cast, "every part opens on its cast");
            let part = ManualPart {
                index: i + 1,
                total: windows.len(),
            };
            assert_eq!(step.part, Some(part), "the round knows which part it is");
            part_prompts.push(step.text.clone());
            prompts.push(step);

            let accepted =
                manual_accept(&layout, 51, Round::Cast, &cast_answer(&slice), None).unwrap();
            assert!(accepted.prompt.is_none(), "round 2 is asked for by the caller");
            cast = accepted.cast;
            assert!(cast.is_some(), "a part's cast is validated and handed back");

            let second = manual_prompt(&layout, "vieneu", 51, cast.as_ref()).unwrap();
            assert_eq!(second.round, Round::Script);
            assert_eq!(second.part, Some(part));
            part_prompts.push(second.text.clone());
            if i > 0 {
                assert!(
                    second.text.contains("PLOT SO FAR"),
                    "part {} is told what the parts before it established",
                    i + 1
                );
            }

            let accepted = manual_accept(
                &layout,
                51,
                Round::Script,
                &script_answer(&slice),
                cast.as_ref(),
            )
            .unwrap();
            cast = None;
            if i + 1 < windows.len() {
                assert!(
                    checkpoint.exists(),
                    "part {} is on disk the moment it is accepted",
                    i + 1
                );
                let next = accepted.prompt.expect("the next part's cast prompt");
                assert_eq!(next.round, Round::Cast);
                assert_eq!(
                    next.part,
                    Some(ManualPart {
                        index: i + 2,
                        total: windows.len()
                    })
                );
                assert!(accepted.outcome.is_none(), "the chapter is not finished yet");
            } else {
                assert!(accepted.prompt.is_none());
                outcome = accepted.outcome;
            }
        }
        let outcome = outcome.expect("the last part finishes the chapter");

        // Every part's prompt is a fraction of the one the whole chapter would
        // have needed, and none of them is the whole chapter.
        for (i, prompt) in part_prompts.iter().enumerate() {
            assert!(
                prompt.len() < whole_prompt.len(),
                "part prompt {i} is not smaller than the whole chapter's"
            );
        }

        // (4) The chapter is written, so the parts it was built from are gone.
        assert!(!checkpoint.exists(), "the checkpoint is cleared at the end");

        // (3) The merged script: one segment per source event, in source order,
        // with the text of the event it answers. Nothing was dropped or reordered
        // at a part boundary — which is what makes this the single-call digest.
        let segments = outcome.script["segments"].as_array().unwrap();
        assert_eq!(segments.len(), prepared.events.len());
        for (segment, event) in segments.iter().zip(&prepared.events) {
            assert_eq!(segment["source_id"], json!(event.id));
            assert_eq!(segment["text"], json!(event.text));
        }
        assert_eq!(outcome.segments, prepared.events.len());
        assert_eq!(
            outcome.delta["segments"].as_array().unwrap().len(),
            prepared.events.len(),
            "the delta the inductor merges carries the whole chapter"
        );
        assert_eq!(outcome.script["roster"], json!(["Narrator", "Anonymous"]));

        // The ledger line says what the chapter cost, part by part — the only
        // place a sixteen-call chapter is visible as sixteen calls.
        assert!(
            outcome.log.iter().any(|l| l.contains("staged in")),
            "{:?}",
            outcome.log
        );
        assert_eq!(
            outcome.log.iter().filter(|l| l.contains("   part ")).count(),
            windows.len()
        );
    }

    /// A part that leaves a looping bed open is refused **before** the checkpoint
    /// advances, so the operator can paste a better answer for the round they are
    /// on instead of being stuck one part further on.
    #[test]
    fn a_part_that_leaves_a_bed_open_is_refused_without_moving_on() {
        let (_dir, layout, _text) = long_layout("manual-gate", 300);
        let settings = Settings::load(&layout.settings());
        let text = std::fs::read_to_string(layout.chapter_txt(51)).unwrap();
        let prepared = prepare_chapter(&text);
        let windows = plan_windows(&prepared, &settings.digest);
        assert!(windows.len() > 1);
        for (i, w) in windows.iter().enumerate() {
            let slice = w.prepared(&prepared);
            let accepted =
                manual_accept(&layout, 51, Round::Cast, &cast_answer(&slice), None).unwrap();
            let cast = accepted.cast.unwrap();
            // The last part's staging answer opens a bed and never closes it.
            let mut answer: Value = serde_json::from_str(&script_answer(&slice)).unwrap();
            if i == windows.len() - 1 {
                answer["segments"][0]["sound_after"] = json!("food-prep");
            }
            let result = manual_accept(
                &layout,
                51,
                Round::Script,
                &answer.to_string(),
                Some(&cast),
            );
            if i == windows.len() - 1 {
                let err = result.expect_err("an unclosed bed is refused").to_string();
                assert!(err.contains("food-prep"), "{err}");
                assert!(
                    err.starts_with(&format!("part {}/{}", i + 1, windows.len())),
                    "the complaint names the part to fix: {err}"
                );
            } else {
                result.expect("the earlier parts are fine");
            }
        }
    }
}
