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
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

mod canon;
mod llm;
mod reconcile;
mod tags;

pub use canon::{
    apply_merges, canon_key, canonicalize_script, merge_bible, resolve_speaker,
    scrub_ambiguous_aliases, BibleMerge,
};
pub use llm::{generate, parse_retry_delay, GenError};
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
    let s = raw.trim().trim_start_matches('\u{feff}');
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Parse model-produced JSON, repairing only a small, well-understood set of
/// common mistakes before preserving the normal parse error.
///
/// Prompt answers occasionally put literal quotation marks inside a Vietnamese
/// `text` value (`... khắc một chữ "Võ" ...`). The first unescaped quote makes
/// serde treat the rest of the sentence as JSON source and fail. Feeding each
/// parse error back into the input is safer than guessing from punctuation: the
/// quote immediately before the error is escaped, and the next parse confirms
/// whether that interpretation produces valid JSON. Bounded retries keep truly
/// malformed output from being rewritten indefinitely.
///
/// **The unambiguous repairs run on every pass, not once.** That is the fix for
/// ch79's staging answer, which died of `control character found while parsing a
/// string` on the *repair* attempt as well as the first: escaping control
/// characters requires knowing which quotes open a string, so one unescaped
/// `"` inside a `text` value puts the scanner outside the string and every later
/// newline is emitted raw. Escaping quotes afterwards moves that boundary
/// again — so a pass that only fixes quotes can expose control characters the
/// first pass could not see, and a pass that only escapes sees the wrong
/// boundaries. Alternating them until neither changes anything is the only order
/// that converges on this input.
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
    // escapes — which is how a repair pass can turn one broken answer into a
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
/// no longer a string — `key must be a string`, a new error one step further
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
    /// dialogue are told apart by quote marks and nothing else — `"`, `“` and
    /// `「`. So a chapter that arrives with no quote marks in it is one long run
    /// of narration, and from there *nothing downstream complains*: the
    /// attribution answer is complete, the source gate is satisfied, the
    /// chapter renders, and every ledger row is green — while the whole book is
    /// read in a single voice. The validators can only catch a model that
    /// disagrees with *the text it was given*; they cannot catch text that never
    /// offered a speaker to disagree with.
    ///
    /// Which is why the message is worded as a thing to check and not an
    /// accusation. A genuinely single-voice chapter is a real thing — a scene
    /// description, a dream sequence — and blaming the crawler on every one of
    /// them would train the operator to ignore the line exactly when it matters.
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
        }
        s
    }
}

fn prepared_event(id: usize, kind: &str, text: &str) -> Option<PreparedEvent> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(PreparedEvent {
        id: format!("e{id:04}"),
        kind: kind.to_string(),
        text: text.to_string(),
    })
}

/// Split source paragraphs into dialogue and narration spans without changing
/// their text. A quote delimiter is punctuation, not part of the speakable
/// span, so the model is not asked to reproduce it in a segment.
fn prepare_chapter(text: &str) -> PreparedChapter {
    // Older workspaces can contain raw HTML entities and Storya's promo/footer
    // metadata. Sanitize at the same boundary the crawler and local reader use,
    // so those artifacts never receive source ids or become obligations for the
    // model. A decoded `&quot;` becomes a real quote delimiter, which
    // `prepare_chapter` then splits on — exactly what a properly crawled
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
        "note": "Return `speakers` for every `dialogue_events` id only. Context events are evidence for resolving that id; all context and every id in `narration_ids` are spoken by Narrator and are not yours to answer. An explicit named speech tag in `following_context` is the strongest speaker evidence.",
    });
    serde_json::to_string_pretty(&view).unwrap_or_else(|_| "[]".into())
}

/// Replace one bounded prompt section when the live template still has it.
///
/// Profiles may be older than the binary, so absent section markers are not an
/// error: the appended contract remains authoritative and placeholder
/// substitution still works for fixture/custom templates.
fn replace_prompt_section(
    body: &mut String,
    start_marker: &str,
    end_marker: &str,
    replacement: &str,
) {
    let Some(start) = body.find(start_marker) else {
        return;
    };
    let Some(end) = body[start..].find(end_marker).map(|n| start + n) else {
        return;
    };
    body.replace_range(start..end, &format!("{replacement}\n"));
}

/// Build the constrained attribution pass.
///
/// Dialogue detection is not a model decision: `prepare_chapter` has already
/// marked every event, and narration is attached to `Narrator` by code. The
/// chapter is therefore shown as answerable `dialogue_events` — each beside its
/// nearest source narration — plus `narration_ids` the model must not answer. The
/// answer map stays small while the tags that actually identify speakers remain
/// visible. The remaining identity fields are the chapter's own.
fn build_attribution_prompt(
    layout: &Layout,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Result<String> {
    let path = layout.prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let mut body = template
        .replace(
            "INPUT 2 — one raw chapter text (Vietnamese). Mixes narration and dialogue in \"...\"\nquotes, with pronouns and descriptive aliases instead of names.",
            "INPUT 2 — the prepared chapter as two lists, in exact source order. `narration_ids` are prose events: the preparer has already spoken them as `Narrator` and they are NOT yours to answer. `dialogue_events` are the quoted lines, each with the stable `id` your answer keys on and its text without quote delimiters.",
        )
        .replace(
            "This is the CONTEXT pass: you read one\nchapter and report WHO is in it and WHAT it is about — the cast and the story.\nYou do NOT write the script. A second pass does that, and it is handed your answer\nas its cast list, so be exact about names and about the surface forms the chapter\nuses: everything downstream is resolved against what you return here.",
            "This is the ATTRIBUTION pass: prepared narration and dialogue events are already separated deterministically. Resolve the chapter cast and assign one immutable speaker to every event. You do NOT stage audio, choose music, or write segments; the next pass is handed this exact speaker map.",
        )
        .replace(
            "The second pass attributes every line against\n   this map",
            "The attribution map uses this evidence",
        )
        .replace(
            "`roster` is the cast list the second pass must attribute against: canonical\n   names only, plus \"Narrator\" when the chapter has narration.",
            "`roster` is the cast list the next pass consumes: canonical names and the\n   reserved `Anonymous` speaker, plus \"Narrator\" when the chapter has narration.",
        )
        .replace("{bible_json}", &bible_context(bible))
        .replace("{chapter_text}", &attribution_view(prepared));

    if let Some(task) = body.find("TASK:") {
        let end = body
            .find("\nRULES:")
            .filter(|rules| *rules > task)
            .unwrap_or(body.len());
        body.replace_range(
            task..end,
            "TASK: return only the strict attribution JSON defined at the end of this prompt.\n",
        );
    }
    replace_prompt_section(
        &mut body,
        "1. mentions records",
        "2. new_characters",
        "1. `mentions` is chapter-local evidence, not a chapter-wide identity table.\n   Include only exact, name-bearing surface forms that identify the same owner\n   wherever they occur. Omit pronouns and context-dependent role or address terms\n   such as `Đồ nhi`, `đệ tử`, `sư tôn`, or `sư phụ`: different scenes in one chapter\n   can give the same form different owners. A mention never determines who speaks\n   a quote; use the explicit tag in the quote's nearby narration first.\n",
    );
    replace_prompt_section(
        &mut body,
        "2. new_characters",
        "3. TITLE:",
        "2. `new_characters` and `new_aliases` are for established proper identities only.\n   A quote whose speaker cannot be identified is an anonymous dialogue speaker, not\n   a new character. Never create a Bible character for a pronoun, generic role, or\n   anonymous passer-by.\n",
    );

    let contract = r#"
---ATTRIBUTION OUTPUT CONTRACT---
Return ONE strict JSON object, never markdown or commentary:
{
  "title": "3-8 word Vietnamese chapter title; do not start it with `Chương`",
  "atmosphere": "1-2 English sentences",
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
  "speakers": {
    "e0002": "canonical character name",
    "e0003": "Anonymous"
  }
}

The prepare step's split is authoritative: `narration_ids` are already spoken by
`Narrator` and are attached by code, so return exactly one `speakers` entry for
every `dialogue_events` id, in source order, and nothing else — no narration ids,
no invented ids, no dropped line.
- Every `dialogue_events` id maps to a canonical character name or the reserved
  name `Anonymous`. Dialogue must NEVER map to Narrator, even when the speaker is
  uncertain, even for a greeting, and even when nobody in the line is named.
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
    Ok(format!("{body}\n{contract}"))
}

/// Build the audio-staging pass. Speaker assignment is supplied as immutable
/// data and the model never returns it; code attaches it after generation.
fn build_staging_prompt(
    layout: &Layout,
    bible: &Value,
    context: &Value,
    prepared: &PreparedChapter,
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
    let mut body = template
        .replace(
            "INPUT 2 — the CAST of THIS chapter, already resolved by the context pass, and the\nonly speaker labels you may use. `mentions` maps every surface form the chapter\nuses to its canonical name — use it ONLY to resolve WHO a dialogue tag names,\nnever to decide who speaks a line: a sentence merely containing \"nàng\" or a\ncharacter's name is not spoken by them. Never invent a speaker who is\nnot on the cast list.",
            "INPUT 2 — the chapter cast and `fixed_speakers`, the complete immutable source-id to speaker map returned by the attribution pass. Do not infer, change, or return a speaker.",
        )
        .replace(
            "INPUT 3 — one raw chapter text (Vietnamese). Mixes narration and dialogue in \"...\"\nquotes, with pronouns and descriptive aliases instead of names.",
            "INPUT 3 — the same prepared source events shown to the attribution pass. `kind` is authoritative; speakers are already fixed.",
        )
        .replace("{bible_json}", &bible_context(bible))
        .replace("{cast_json}", &cast_context(context))
        .replace("{music_palette}", &palette)
        .replace("{effect_tags}", &effects)
        .replace("{inject_sounds}", &injects)
        .replace("{chapter_text}", &prepared.prompt_json)
        .replace("{\"speaker\": \"Narrator\", ", "{\"");
    if let Some(task) = body.find("TASK:") {
        let end = body
            .find("\nRULES:")
            .filter(|rules| *rules > task)
            .unwrap_or(body.len());
        body.replace_range(
            task..end,
            "TASK: return only the strict staging JSON defined at the end of this prompt.\n",
        );
    }
    replace_prompt_section(
        &mut body,
        "1. Split on speaker turns:",
        "4. Keep segments short for TTS:",
        "1. Cover every prepared source event exactly once and in source order. A source\n   event may be split into consecutive segments for a long TTS line or a sound seam;\n   every split carries the same `source_id`. Never merge source events.\n2. `kind` is already decided. Use it only to understand the text. Do not return a\n   `kind`, `speaker`, roster, cast, or attribution field; the immutable map in INPUT 2\n   is attached by code after you return.\n3. Narrate every word exactly once. Never include the source headline. A dialogue\n   event and its surrounding narration are already separate source events.\n",
    );

    let contract = r#"
---STAGING OUTPUT CONTRACT---
Return ONE strict JSON object containing only `segments` and `fixes`:
{
  "segments": [{
    "source_id": "e0001",
    "text": "exact speakable text for this source event",
    "mood": "English mood",
    "scene": "English place-time label",
    "music": "one token from the palette",
    "effect": ["tags from the effect palette"],
    "sound_after": "sound name or none",
    "stop_after": "sound name or none"
  }],
  "fixes": []
}

Do not return `speaker`: `fixed_speakers` is authoritative and code attaches it.
The staging pass may split a source event but may never change its identity,
drop an event, duplicate one, or move one out of source order.

A split PARTITIONS its event: the halves, in source order, concatenated, must
spell the source text exactly. Never repeat the whole line on both halves, and
never drop words — a split exists to put a sound seam between two different
halves of one line. Two different source events may carry identical text (a
street crowd hailing the same phrase on two lines); that is two events, not a
duplicate — answer each, and never merge them.

Follow every audio, grammar, TTS, music, effect, and sound rule in this prompt.
"#;
    Ok(format!("{body}\n{contract}"))
}

/// Ask the analyzer for one chapter through two constrained passes.
///
/// Attribution is generated and validated first. The staging pass receives that
/// map as data and never emits speakers, so a small model cannot regress a
/// mechanically separated dialogue event back to Narrator while it is choosing
/// scenes and sounds.
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

    progress(0.08, format!("digest ch{n} via {analyzer}: attribution"));
    let attribution_prompt = build_attribution_prompt(layout, bible, &prepared)?;
    let raw = generate_retrying(
        &attribution_prompt,
        analyzer,
        settings,
        progress,
        0.08,
        0.36,
    )
    .await?;
    dump_raw(layout, "digest-attribution", &raw);
    let context = match parse_attribution(&raw, bible, &prepared) {
        Ok(context) => context,
        Err(e) => {
            progress(
                0.38,
                format!("invalid attribution, asking for one repair: {e}"),
            );
            let again = repair_once(&attribution_prompt, &e, analyzer, settings).await?;
            dump_raw(layout, "digest-attribution-retry", &again);
            parse_attribution(&again, bible, &prepared).map_err(|e2| {
                let dump = layout.data().join(".last-analyze-raw.json");
                let _ = atomic_write(&dump, &again);
                anyhow::anyhow!(
                    "digest attribution invalid ({e2}); raw saved to {}",
                    dump.display()
                )
            })?
        }
    };

    progress(0.42, format!("digest ch{n} via {analyzer}: staging"));
    let staging_prompt = build_staging_prompt(layout, bible, &context, &prepared)?;
    let raw = generate_retrying(&staging_prompt, analyzer, settings, progress, 0.42, 0.82).await?;
    dump_raw(layout, "digest-staging", &raw);
    let parse_staging = |raw: &str| parse_staged_script(raw, bible, &context, &prepared, &vocab);
    let mut script = match parse_staging(&raw) {
        Ok(script) => script,
        Err(e) => {
            progress(0.84, format!("invalid staging, asking for one repair: {e}"));
            let again = repair_once(&staging_prompt, &e, analyzer, settings).await?;
            dump_raw(layout, "digest-staging-retry", &again);
            parse_staging(&again).map_err(|e2| {
                let dump = layout.data().join(".last-analyze-raw.json");
                let _ = atomic_write(&dump, &again);
                anyhow::anyhow!(
                    "digest staging invalid ({e2}); raw saved to {}",
                    dump.display()
                )
            })?
        }
    };

    if let Some(gap) = sound_design_gap(&script, &text, &vocab.injects) {
        progress(
            0.88,
            format!("sound design incomplete, asking for one repair: {gap}"),
        );
        let again = repair_once(&staging_prompt, &anyhow::anyhow!(gap), analyzer, settings).await?;
        dump_raw(layout, "digest-sound-retry", &again);
        script = parse_staging(&again)?;
        if let Some(gap) = sound_design_gap(&script, &text, &vocab.injects) {
            anyhow::bail!("digest staging left the sound design incomplete: {gap}");
        }
    }

    let data = merge_rounds(&context, &script);
    let outcome = assemble_outcome(bible, &data, &data, &text)?;
    progress(1.0, format!("digest ch{n} done"));
    Ok(outcome)
}

/// Everything after the two answers have parsed: merge the rounds, check the
/// grammar fixes against the chapter, build the script and the bible delta, and
/// describe what came out.
///
/// **Shared by the worker's automatic path and the operator's manual one, and
/// that is the point.** A manual digest that assembled its script differently
/// would put a chapter into the library that the automatic path would have
/// refused — and the manual route exists to be *the same digest* with a person
/// standing in for the model, not a second, looser one. One function, rather
/// than two that agree today.
///
/// The `sound_design_gap` check stays in the callers, because they answer it
/// differently: the worker asks the model again, the operator is told and gets
/// to paste a better answer.
fn assemble_outcome(
    bible: &Value,
    context: &Value,
    script: &Value,
    text: &str,
) -> Result<DigestOutcome> {
    let data = merge_rounds(context, script);

    let mut log = Vec::new();
    // First line, before anything the model said. The split is decided from the
    // text alone, so this is the earliest a bad crawl is visible — and the only
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
        // this, so it is the mp3's filename *and* the spoken headline — one
        // value, two consumers, no chance of them disagreeing.
        "title": data.get("title").cloned().unwrap_or(json!("")),
        "atmosphere": data.get("atmosphere").cloned().unwrap_or(json!("")),
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

/// The prompt for one manual round, ready to be carried to any model.
#[derive(Debug, Clone)]
pub struct ManualPrompt {
    pub round: Round,
    pub text: String,
}

/// What a pasted answer produced. Exactly one field is set.
#[derive(Debug, Clone)]
pub struct ManualAnswer {
    /// Round 1: the cast, to be handed back when asking for round 2.
    pub cast: Option<Value>,
    /// Round 2: the finished outcome — script assembled, delta ready.
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
/// must not be dramatized by a second, looser contract — that was the legacy
/// `build_prompt` / `build_script_prompt` pair, which no longer runs here.
///
/// `cast` is the validated answer to round 1 and is required for round 2: the
/// staging prompt is rendered *against that immutable speaker map*, exactly as
/// the worker's is, so an operator who skipped round 1 gets an error rather than
/// a prompt that quietly asks for the wrong thing.
pub fn manual_prompt(layout: &Layout, n: u32, cast: Option<&Value>) -> Result<ManualPrompt> {
    let (bible, text) = manual_inputs(layout, n)?;
    let prepared = prepare_chapter(&text);
    match cast {
        None => Ok(ManualPrompt {
            round: Round::Cast,
            text: build_attribution_prompt(layout, &bible, &prepared)?,
        }),
        Some(context) => Ok(ManualPrompt {
            round: Round::Script,
            text: build_staging_prompt(layout, &bible, context, &prepared)?,
        }),
    }
}

/// Check a pasted answer for one round, and assemble what it yields.
///
/// **The same validators the worker's answers go through, and that is the whole
/// design.** A manual digest is the automatic one with a person standing in for
/// the model, so an answer the worker's path would have refused is refused here
/// too — with the validator's own complaint as the message, because the operator
/// is the one who can act on it.
///
/// Nothing is written. Committing is [`write_script`], called by the caller, so
/// what lands is one write site rather than two that could differ.
pub fn manual_accept(
    layout: &Layout,
    n: u32,
    round: Round,
    pasted: &str,
    cast: Option<&Value>,
) -> Result<ManualAnswer> {
    let (bible, text) = manual_inputs(layout, n)?;
    let prepared = prepare_chapter(&text);
    match round {
        Round::Cast => Ok(ManualAnswer {
            cast: Some(parse_attribution(pasted, &bible, &prepared)?),
            outcome: None,
        }),
        Round::Script => {
            let context = cast.ok_or_else(|| {
                anyhow::anyhow!("round 2 needs round 1's cast — paste the cast answer first")
            })?;
            let vocab = vocabulary(layout)?;
            let script = parse_staged_script(pasted, &bible, context, &prepared, &vocab)?;
            // The worker asks the model again at this point; the operator is
            // simply told, so they can paste an answer that places the sounds it
            // staged. Same rule, different remedy.
            if let Some(gap) = sound_design_gap(&script, &text, &vocab.injects) {
                anyhow::bail!("{gap}");
            }
            Ok(ManualAnswer {
                cast: None,
                outcome: Some(assemble_outcome(&bible, context, &script, &text)?),
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
            Ok((t, backend)) => {
                // The configured backend and the one that ran are not the same
                // thing whenever the gemini chain falls back. Say which one
                // answered, so the operator's screen stops naming a backend that
                // had already given up — this is the label that read "via gemini"
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
const SOUND_CUES: [&str; 21] = [
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
/// from new digests — near-identical one-off clones for a street greeting were
/// the loudest thing in a scene — but they still resolve here, and are still
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

/// Validate the complete source-id to speaker map against the preparer's
/// mechanical narration/dialogue classification.
fn validate_attributions(
    data: &Value,
    bible: &Value,
    prepared: &PreparedChapter,
) -> Result<BTreeMap<String, String>> {
    let mut speakers = fixed_speakers(data)?;
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

    for event in &prepared.events {
        let speaker = speakers
            .get(&event.id)
            .ok_or_else(|| anyhow::anyhow!("attribution dropped source event {:?}", event.id))?;
        match event.kind.as_str() {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {:?} is narration but attribution assigns {speaker:?}; narration must be Narrator",
                event.id
            ),
            "dialogue" if speaker == "Narrator" => anyhow::bail!(
                "source {:?} is dialogue but attribution assigns Narrator; use a canonical character or the reserved `Anonymous` — a hail nobody on cast is tagged saying belongs to the crowd, not to Narrator",
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
            anyhow::bail!(
                "source {:?} assigns {speaker:?}, but that speaker is not in the chapter roster",
                event.id
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

fn parse_attribution(raw: &str, bible: &Value, prepared: &PreparedChapter) -> Result<Value> {
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "attribution is not valid JSON".to_string())?;
    normalize_attribution_metadata(&mut data, bible, prepared);
    validate_context(&data, bible)?;
    validate_title(&data)?;
    // The validated map is written back with the narration rows the preparer
    // owns, because staging reads `speakers` from this same object.
    let speakers = validate_attributions(&data, bible, prepared)?;
    data["speakers"] = json!(speakers);
    Ok(data)
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
    validate_source_alignment(&data, prepared)?;
    Ok(data)
}

/// Collapse a written non-verbal sound the answer left beside its tag.
///
/// The source gate builds its `expected` with `retag_text`, so the answer has
/// to come through the same door: `"[hắng giọng] Khụ khụ khụ, ban đầu…"` is
/// stored as `"[hắng giọng] ban đầu…"`. Leaving it is wrong twice over — the
/// tag *and* the words get spoken — and refusing it stalls a chapter whose
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
            // is what lets the repair converge — without it the model retries
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
    let text = text
        .replace("[cười]", " ")
        .replace("[thở dài]", " ")
        .replace("[hắng giọng]", " ");
    crate::util::squeeze_ws(&text)
}

/// Text with every *written* non-verbal sound — and every tag standing in for
/// one — removed, so two texts can be compared while ignoring how, or whether,
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
/// leaves the words behind — this is the shape being named.
fn leftover_written_sound(text: &str) -> Option<(&'static str, &'static str)> {
    let lower = text.to_lowercase();
    for (tag, spellings) in [
        ("[cười]", &["haha", "ha ha", "hắc hắc", "hô hô"][..]),
        ("[thở dài]", &["haizz", "haiz"][..]),
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
/// This class fails a chapter across every racer — the model adds the tag and
/// keeps the words it stands for — and the generic "was changed" message gives
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
fn validate_source_alignment(data: &Value, prepared: &PreparedChapter) -> Result<()> {
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
        match event.kind.as_str() {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {id:?} is narration but segment {i} is assigned to {speaker:?}; narration must be Narrator"
            ),
            "dialogue" if speaker == "Narrator" => anyhow::bail!(
                "source {id:?} is dialogue but segment {i} is assigned to Narrator"
            ),
            _ => {}
        }
        if event.kind == "dialogue"
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

    /// The chain the whole program rests on, as one test: **a crawler's output
    /// decides whether the model is asked a question at all.**
    ///
    /// `prepare_chapter` decides narration-vs-dialogue from quote marks alone —
    /// `"`, `“`, `「`. So a crawler that returns a container with no quote marks
    /// in it, or that picks a site which marks speech some other way, hands the
    /// digest one long run of narration. From there *nothing complains*: the
    /// attribution answer is complete, the source gate passes, the chapter
    /// renders, every ledger row is green — and the book is read in one voice.
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
    /// *not* blame the crawler — or it stops being read.
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
    /// through — and it failed on this input and on its own repair before, which
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
    /// is *repaired* rather than reported — the alternative is burning a model
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
        let attribution = build_attribution_prompt(&layout, &bible, &prepared).unwrap();
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
        let staging = build_staging_prompt(&layout, &bible, &fixed, &prepared).unwrap();
        assert!(staging.contains("---STAGING OUTPUT CONTRACT---"));
        assert!(staging.contains("fixed_speakers"));
        assert!(staging.contains("Do not return `speaker`"));

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

        let data = parse_attribution(&raw, &json!({"characters": []}), &prepared).unwrap();
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
    /// only them — narration is not the model's to answer.
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
        validate_source_alignment(&good, &prepared).unwrap();

        let dropped = json!({"segments": [
            line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
        ], "fixes": []});
        let err = validate_source_alignment(&dropped, &prepared).unwrap_err();
        assert!(err.to_string().contains("dropped"), "{err}");

        let wrong_owner = json!({"segments": [
            line("e0001", "Bành Anh", "Dịch Phong nói với Bành Anh."),
            line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa?"),
            line("e0003", "Narrator", "Vũ Kiệt hỏi."),
        ], "fixes": []});
        let err = validate_source_alignment(&wrong_owner, &prepared).unwrap_err();
        assert!(err.to_string().contains("narration"), "{err}");

        let merged = json!({"segments": [
            line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
            line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa? Vũ Kiệt hỏi."),
        ], "fixes": []});
        let err = validate_source_alignment(&merged, &prepared).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
    }

    #[test]
    fn an_entity_bearing_chapter_prepares_to_the_decoded_text() {
        // ch79 as crawled by the pre-fix crawler: numeric entities raw on
        // disk. The model reads `&#x27;` and answers `'` — so the prepared
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
        validate_source_alignment(&data, &prepared).unwrap();
    }

    #[test]
    fn site_metadata_never_enters_the_source_contract() {
        let prepared = prepare_chapter(
            "Chương 81: Liền phòng ngự\n\n81. Chương 81: Liền phòng ngự\n\nCài đặt đọc\n\nNgười Trên Vạn Người\n\nNgười Trên Vạn Người thuộc thể loại Xuyên Không, chương 81 tiếp tục diễn biến hấp dẫn của câu chuyện. Đọc online miễn phí, cập nhật nhanh nhất tại Storya - nền tảng đọc truyện chất lượng cao.\n\nHắn đã hoàn thành nhiệm vụ.\n\nHệ thống thực thể dưới dạng chiếc đỉnh. Main bá, không hậu cung. Truyện đã hoàn thành\n\nPS: sẽ cập nhật sau.",
        );

        assert_eq!(prepared.events.len(), 1);
        assert_eq!(prepared.events[0].text, "Hắn đã hoàn thành nhiệm vụ.");
        assert!(!prepared.prompt_json.contains("Storya"));
        assert!(!prepared.prompt_json.contains("Truyện đã hoàn thành"));
        assert!(!prepared.prompt_json.contains("PS:"));
    }

    #[test]
    fn a_decoded_quot_becomes_a_dialogue_boundary() {
        // `&quot;` survived the old crawler too, only as raw markup. Decoding
        // turns it into a real quote delimiter, so prepare_chapter splits the
        // dialogue out exactly as it would for a properly crawled chapter —
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
        validate_source_alignment(&data, &prepared).unwrap();
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
        // source verbatim is normalized the same way — one door for both.
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
        // The engine counts `haha` — one word — and a laugh in the middle of a
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
        let err = validate_source_alignment(&hoisted, &prepared).unwrap_err();
        assert!(err.to_string().contains("written sound"), "{err}");

        // The same answer passes once it comes through `retag_text`, which is
        // what `parse_staged_script` does before it validates and persists.
        let mut collapsed = hoisted.clone();
        collapse_redundant_sounds(&mut collapsed);
        validate_source_alignment(&collapsed, &prepared).unwrap();

        // A genuine change keeps the honest generic message: the hint fires
        // only when written sound is the whole disagreement.
        let rewritten = answer("Đêm ấy trời trở gió.");
        let err = validate_source_alignment(&rewritten, &prepared).unwrap_err();
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
        // narration is genuinely absent from it — and that must not make the
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
        validate_source_alignment(&data, &prepared).unwrap();
    }

    #[test]
    fn source_gate_rejects_a_split_that_changes_speaker() {
        let prepared = prepare_chapter("Chương 1: Một cảnh\n\n\"Anh nhi, xong chưa?\"");
        let data = json!({"segments": [
            {"source_id": "e0001", "speaker": "Vũ Kiệt", "text": "Anh nhi,"},
            {"source_id": "e0001", "speaker": "Bành Anh", "text": "xong chưa?"},
        ], "fixes": []});
        let err = validate_source_alignment(&data, &prepared).unwrap_err();
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
        assert!(gap.contains("phòng bếp"), "{gap}");

        // ch15 names a cleaver while asking for one to be forged later; an
        // object mentioned in dialogue is not a chopping action to sound now.
        let chapter15 = "Dịch Phong vươn tay lấy ra con dao phay nói: nhớ lần trước bá mẫu nói qua, nhờ ta rèn một con dao phay lúc rảnh rỗi, giúp ta mang cho họ nhé!";
        let mention_only = json!({"segments": [line(chapter15)]});
        assert!(
            sound_design_gap(&mention_only, chapter15, &pool).is_none(),
            "a mentioned cleaver must not require a chopping sound"
        );

        // 2. The bed opened and never closed — ch9's exact answer.
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
        let first = manual_prompt(&layout, 51, None).unwrap();
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

        // A garbage paste fails the *worker's* validator — the same one — and
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
        let second = manual_prompt(&layout, 51, Some(&cast)).unwrap();
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
        let err = manual_prompt(&layout, 999, None).expect_err("no chapter text");
        assert!(err.to_string().contains("ch999"), "{err:#}");
    }

    /// The happy path, end to end, without a model.
    ///
    /// Two pastes and a finished chapter — the flow the TUI drives with `c` and
    /// `v` and the backup digestor drives with a model, exercised through
    /// `manual_accept` so the seam between the rounds is real rather than
    /// assumed. What this buys that the per-part tests cannot: it proves the
    /// round-1 answer is *usable* as round 2's input.
    ///
    /// The staging answer names **no speaker at all** — it cannot, and saying so
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

        // Round 1: the attribution answer — one entry per *dialogue* event, in
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
        // The delta is what the inductor merges into the bible — a manual digest
        // has to produce one, or the next chapter would not know this cast.
        assert!(outcome.delta.get("roster").is_some(), "{:?}", outcome.delta);
        assert!(
            outcome.log.iter().any(|l| l.contains("segments=2")),
            "{:?}",
            outcome.log
        );

        // **And the hand-off is real, not decorative.** A staging answer that
        // drops an event is refused by round 2 — otherwise the source gate is
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
}
