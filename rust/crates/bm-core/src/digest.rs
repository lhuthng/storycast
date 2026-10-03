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
///
/// `at`/`end` are where the span sits in the sanitized text — the quote gates
/// use them to say *where* a finding is and to re-read the span **raw**: the
/// published `text` is trimmed, and a trim would hide exactly the leading
/// paragraph break that proves a speech swallowed the paragraph after it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedEvent {
    id: String,
    kind: String,
    text: String,
    at: usize,
    end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedChapter {
    events: Vec<PreparedEvent>,
    /// The machine-readable form placed in the attribution and staging prompts.
    /// It contains the chapter text exactly once, split into ordered events.
    prompt_json: String,
    /// Where a quote delimiter was still open when the text ran out: a byte
    /// offset into the **sanitized** text, or `None` when every delimiter paired.
    ///
    /// Not cosmetic. An unclosed quote makes the scanner treat *every*
    /// remaining span as one dialogue event, so a chapter that lost a final
    /// `"` upstream is read start-to-finish in a single voice — the mirror of
    /// the no-quotes case below, and just as silent.
    ///
    /// An offset, not a line number, because the sanitized text is not the file
    /// the operator has open: `sanitize_chapter_text` drops blank lines and
    /// joins paragraphs with a blank one, so a line counted here is not a line
    /// they can go to. [`quote_fault`] turns this into something they can.
    unbalanced_at: Option<usize>,
}

impl PreparedChapter {
    /// How many events are dialogue, as decided by the quote delimiters alone.
    fn dialogue_count(&self) -> usize {
        self.events.iter().filter(|e| e.kind == "dialogue").count()
    }

    /// How many events are thoughts carved out of narration. Counted apart from
    /// dialogue: a thought has no delimiters to check a crawler against, and
    /// counting it as speech would mask the one-voice warning below.
    fn thought_count(&self) -> usize {
        self.events.iter().filter(|e| e.kind == "thought").count()
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
        let thought = self.thought_count();
        let narration = self.events.len() - dialogue - thought;
        let mut s = format!(
            "   prepared {} event(s): {narration} narration, {dialogue} dialogue, {thought} thought",
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
        } else if self.unbalanced_at.is_some() {
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

fn prepared_event(
    id: usize,
    kind: &str,
    text: &str,
    at: usize,
    end: usize,
) -> Option<PreparedEvent> {
    let text = text.trim();
    if text.is_empty() || !crate::util::has_speakable_content(text) {
        return None;
    }
    Some(PreparedEvent {
        id: format!("e{id:04}"),
        kind: kind.to_string(),
        text: text.to_string(),
        at,
        end,
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

/// A quoted span too small to be a spoken line: `“rear palace”`, `“flower
/// garden”` — translated terms and scare quotes, not dialogue. The ceiling
/// is deliberately low because the error only runs one way: a span kept as
/// narration is never offered a speaker, while a span split out as dialogue
/// can still be retracted by the attribution pass via `not_speech`. The
/// known blind spot is a bare quote as a verb complement mid-sentence
/// (`said "come here" and left`): nothing structural tells it from a term,
/// so it reads as narration. Zero instances in 31 chapters of the live book.
fn is_quoted_term(span: &str) -> bool {
    let t = span.trim();
    if t.is_empty() || t.contains('\n') {
        return false;
    }
    if t.chars().any(|c| matches!(c, '.' | '!' | '?' | '…')) {
        return false;
    }
    if t.chars().count() > 24 {
        return false;
    }
    t.split_whitespace().count() <= 4
}

/// Prose glued into running text on the same line: `the “rear palace”:
/// the residence` is an appositive inside narration. A quote handed over
/// from a sentence end, a comma, a colon or a dash (`said: "I understand,"`,
/// `"Cacao," she replied` after `?"`) is speech changing hands, and so is a
/// quote with nothing before it on the line (`"Just leave it there."
/// Within, …`). Only a letter or digit touching the opener means the quote
/// never left the sentence.
///
/// A headline glued straight onto the quote (`Chapter 25: Wine "What
/// terrible news,"`) hands over too: without this the merged event would
/// start with the headline and the headline filter would drop the line with
/// it. Headlines further back don't count — sentence punctuation or a closed
/// quote since means real prose intervenes, and the merged event starts after
/// it.
fn embedded_in_prose(text: &str, opener_at: usize, after_closer: usize) -> bool {
    let line_start = text[..opener_at].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line_end = text[after_closer..]
        .find('\n')
        .map(|p| after_closer + p)
        .unwrap_or(text.len());
    let before = text[line_start..opener_at].trim_end();
    if before.is_empty() {
        return false;
    }
    if crate::assemble::is_headline(before)
        && !before.chars().any(|c| matches!(c, '.' | '!' | '?' | '…'))
        && !before.chars().any(|c| matches!(c, '"' | '”' | '」'))
    {
        return false;
    }
    if !before
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric())
    {
        return false;
    }
    !text[after_closer..line_end].trim().is_empty()
}

/// First-person markers and English second person: the `I`/`my`/`you` voice
/// of a thought, in the content languages. Vietnamese second person stays
/// out: `bạn` is as often "friend" as "you", and `ngươi` sits inside `con
/// ngươi` (pupil) — both would carve constantly, and constant false carves
/// are quota the model spends retracting.
const THOUGHT_MARKERS: &[&str] = &[
    "i",
    "i'd",
    "i'll",
    "i'm",
    "i've",
    "my",
    "me",
    "mine",
    "myself",
    "we",
    "us",
    "our",
    "ours",
    "let's",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "tôi",
    "tao",
    "tớ",
];

/// The markers that make a thought the thinker's own voice: first person
/// singular. A narrator aside to the reader is `we`/`us`/`you`-voiced (`let us
/// call them…`, `you see`), which is why those stay out — they are the
/// retraction's legitimate targets, and the `I` a thought is made of is not.
const FIRST_PERSON_SINGULAR: &[&str] = &[
    "i", "i'd", "i'll", "i'm", "i've", "my", "me", "mine", "myself",
];

/// Whether a passage is voiced first person singular, the same word-boundaried
/// and folded way [`is_thought_sentence`] reads its markers.
fn first_person_singular(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        let folded = word
            .trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
            .to_lowercase()
            .replace('’', "'");
        FIRST_PERSON_SINGULAR.contains(&folded.as_str())
    })
}

/// Whether a narration sentence is voiced `I`/`you`: an unquoted
/// first- or second-person passage is an inner thought, not speech. Word-boundaried and
/// case-folded; curly apostrophes fold to straight ones (`I’ll` reads as
/// `i'll`).
///
/// **A marker buried under two commas does not count.** A thought announces
/// itself where its sentence starts (`I need to…`, `You know…`, `Hope my old
/// man's…`); a third-clause `you` is prose talking about somebody. The sentence
/// that proved it: `But Maomao, who had been making her way just fine as an
/// apothecary, thank you very much, saw it solely as so much trouble.` — two
/// commas before the `you` of `thank you very much`, so it carved as a thought
/// and the attribution pass handed narration about Maomao to Maomao. An
/// interpolated aside is a comment *inside* the narrator's sentence, not the
/// sentence's own voice, and one comma of headroom keeps the real openings
/// (`In that case, I'll go.`) while refusing the third-clause shape.
fn is_thought_sentence(sentence: &str) -> bool {
    let mut commas = 0usize;
    for word in sentence.split_whitespace() {
        let folded = word
            .trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
            .to_lowercase()
            .replace('’', "'");
        if commas < 2 && THOUGHT_MARKERS.contains(&folded.as_str()) {
            return true;
        }
        commas += word.matches(',').count();
    }
    false
}

/// Split a narration run into sentences at `. ! ? …`, keeping the mark.
/// Over-splits abbreviations (`Mr.`); harmless, because only thought
/// sentences leave the run and the rest rejoin below.
fn narration_sentences(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let (at, ch) = chars[i];
        if matches!(ch, '.' | '!' | '?' | '…') {
            let mut end = at + ch.len_utf8();
            while text[end..]
                .chars()
                .next()
                .is_some_and(|c| matches!(c, '.' | '!' | '?' | '…'))
            {
                end += text[end..].chars().next().unwrap().len_utf8();
            }
            // Only a boundary when whitespace or the end follows, so `3.5`
            // stays whole — same rule the TTS splitter uses.
            if end >= text.len()
                || text[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                out.push((start, end));
                start = end;
            }
        }
        i += 1;
    }
    if start < text.len() {
        out.push((start, text.len()));
    }
    out
}

/// Carve unquoted first-person sentences out of narration as thought events
/// (`thought` kind, no delimiters): `I need to just get this job done.` is
/// Maomao thinking, and voicing her needs an event the attribution pass can
/// see. Everything else rejoins into whole narration events, so a chapter with
/// no thoughts prepares exactly as before.
///
/// `thought` is a kind of its own rather than a dialogue event without quote
/// marks: the attribution view lists thoughts apart from spoken lines so the
/// model resolves a thinker and not a speaker, the script carries
/// `"kind": "thought"` on the segment, and the mixer keys the pack's thought
/// stinger on that marker (`scene-map.json` → `thought.sound`). A dialogue
/// event that happens to have no delimiters could say none of that.
///
/// The chapter-level guard is the whole ballgame: a first-person novel is
/// voiced `I` throughout, and carving it would turn the book into dialogue.
/// Intrusions are rare by definition — a fifth of the narration thinking
/// aloud is a narrator, not a thought — so nothing carves once three such
/// sentences make up a fifth or more of it. Below three there is no evidence
/// of a voice either way, so isolated intrusions always carve. A narrator
/// aside that still matches (`let us call them…`) carves as a thought event
/// the model retracts via `not_speech`, the same escape hatch quoted titles
/// use.
fn carve_thoughts(events: Vec<PreparedEvent>) -> Vec<PreparedEvent> {
    let mut sentences = 0usize;
    let mut marked = 0usize;
    for event in events.iter().filter(|e| e.kind == "narration") {
        for (from, to) in narration_sentences(&event.text) {
            sentences += 1;
            if is_thought_sentence(&event.text[from..to]) {
                marked += 1;
            }
        }
    }
    if sentences == 0 || (marked >= 3 && marked * 5 >= sentences) {
        return events;
    }
    let mut out: Vec<PreparedEvent> = Vec::with_capacity(events.len());
    for event in events {
        if event.kind != "narration" {
            out.push(event);
            continue;
        }
        let mut run_from: Option<usize> = None;
        for (from, to) in narration_sentences(&event.text) {
            if is_thought_sentence(&event.text[from..to]) {
                if let Some(rs) = run_from.take() {
                    if let Some(nar) = prepared_event(
                        out.len() + 1,
                        "narration",
                        &event.text[rs..from],
                        event.at + rs,
                        event.at + from,
                    ) {
                        out.push(nar);
                    }
                }
                if let Some(thought) = prepared_event(
                    out.len() + 1,
                    "thought",
                    &event.text[from..to],
                    event.at + from,
                    event.at + to,
                ) {
                    out.push(thought);
                }
            } else if run_from.is_none() {
                run_from = Some(from);
            }
        }
        if let Some(rs) = run_from {
            if let Some(nar) = prepared_event(
                out.len() + 1,
                "narration",
                &event.text[rs..],
                event.at + rs,
                event.end,
            ) {
                out.push(nar);
            }
        }
    }
    out
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
        if let Some(event) = prepared_event(events.len() + 1, kind, raw, from, to) {
            events.push(event);
        }
    };

    let mut i = 0usize;
    // A pending opener whose span may be a quoted term: the narration before
    // it is not pushed until the closer decides. (opener byte index, byte
    // index just inside it)
    let mut pending: Option<(usize, usize)> = None;
    while i < chars.len() {
        let (at, ch) = chars[i];
        let is_open = ch == '"' || ch == '“' || ch == '「';
        let is_close = ch == '"' || ch == '”' || ch == '」';
        if is_open && quote.is_none() {
            quote = Some((ch, at));
            pending = Some((at, at + ch.len_utf8()));
            kind = "dialogue";
        } else if is_close && quote.is_some() {
            let (opener_at, inner_start) = pending.unwrap_or((at, at));
            let span = &text[inner_start..at];
            if is_quoted_term(span) && embedded_in_prose(&text, opener_at, at + ch.len_utf8()) {
                // A translated term, not speech: the delimiters stay in the
                // narration flow and no event is split. Longer quoted
                // non-speech (titles, panels) still splits out for the
                // attribution pass to retract via `not_speech`.
                kind = "narration";
            } else {
                push(start, opener_at, "narration", &mut events);
                push(inner_start, at, "dialogue", &mut events);
                start = at + ch.len_utf8();
                kind = "narration";
            }
            quote = None;
            pending = None;
        } else if quote.is_none() && ch == '\n' {
            push(start, at, kind, &mut events);
            start = at + ch.len_utf8();
        }
        i += 1;
    }
    // An unclosed opener splits like before: prose before it is narration,
    // everything after is one speech.
    if let Some((opener_at, inner_start)) = pending {
        push(start, opener_at, "narration", &mut events);
        start = inner_start;
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
    // Thoughts carve after the headline filter: a heading can itself carry a
    // marker (`Chapter 12: What You Mean`), and carving first would split it
    // into pieces the filter no longer recognizes.
    let events = carve_thoughts(content);
    let mut events = events;
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
        unbalanced_at: quote.map(|(_, at)| at),
    }
}

/// One structural problem the quote scan found, in coordinates an operator and
/// a repair prompt can both use.
///
/// The kinds, and what each proves:
///
/// * `"unclosed quote"` — a delimiter opened and never closed. The **net**
///   damage: the scanner is still inside a speech when the text ends.
/// * `"swallowed paragraph"` — one dialogue span contains a paragraph break.
///   The scanner never splits speech on a newline, so in a healthy chapter a
///   dialogue span cannot cross one. This is the check that catches what a
///   quote *count* cannot: two dialogues each missing a single mark keep the
///   count even, and every window of the text reads fine — but the mispaired
///   opener still drags a paragraph of narration into a speech, and that
///   spanning is a local, visible fact.
/// * `"welded prose"` — a long speech begins right after running prose with no
///   colon in front of it. Either the opener was never written, or a closer
///   was lost and prose got welded to the next span; both mis-split the
///   chapter. Punctuation before the mark (`. ? ! …`) is how a normal sentence
///   hands over, so only prose itself touching the quote fires this — and only
///   for spans longer than a quoted term ever gets, so `Tràng "cuồng phong bạo
///   vũ"` mid-sentence stays legal while a swallowed sentence does not.
///
/// **Structural facts, not a count** — which is why they survive the
/// even-count case, and why each one names its paragraph: that is the input the
/// repair pass needs to fix a mark it cannot otherwise find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteFinding {
    /// 1-based paragraph number, in the order the chapter reads.
    pub paragraph: usize,
    /// The paragraph (or, for a swallowed one, the head of the whole span).
    pub text: String,
    /// Which structural rule fired, as above.
    pub kind: &'static str,
}

/// Scan a chapter for unbalanced quotation structure. Empty means clean.
///
/// **A global fact about the whole chapter, which is why it lives in code and
/// not in a prompt.** A model asked to stage segments cannot see the imbalance —
/// every local view of a mispaired chapter reads fine — and its answer passes
/// every validator there is, because swallowing the narration after a mispaired
/// opener is *consistent* with the text it was given. The scan is the only
/// place this is catchable, and it costs one linear pass.
pub fn quote_findings(text: &str) -> Vec<QuoteFinding> {
    let prepared = prepare_chapter(text);
    // `prepare_chapter` sanitizes internally and sanitation is idempotent, so
    // this is the same string every event offset indexes.
    let clean = crate::crawl::sanitize_chapter_text(text);
    let paragraph_of = |at: usize| {
        let before = &clean[..at.min(clean.len())];
        let mut n = before.lines().filter(|l| !l.trim().is_empty()).count();
        // The partial line the cursor sits in is not a completed paragraph —
        // and when a span starts right after its opening quote, that partial
        // line is the paragraph the finding belongs to.
        if !before.is_empty() && !before.ends_with('\n') {
            n -= 1;
        }
        n + 1
    };
    let mut findings = Vec::new();

    // The net check first: a delimiter still open at the end. Every event after
    // it is one long speech, so the structural checks below would fire on the
    // same span anyway — this one names the opener directly.
    if let Some(at) = prepared.unbalanced_at {
        findings.push(QuoteFinding {
            paragraph: paragraph_of(at),
            text: head_chars(&clean[at..], 200),
            kind: "unclosed quote",
        });
    }

    for event in &prepared.events {
        if event.kind != "dialogue" {
            continue;
        }
        // Gate 1 — a speech that contains a paragraph break, read **raw**: the
        // published text is trimmed, and a leading `\n\n` after the opening
        // mark is exactly what a swallowed paragraph looks like. In a healthy
        // chapter the scanner never produces such a span.
        let raw = &clean[event.at..event.end];
        let interior_break = raw
            .split_once('\n')
            .is_some_and(|(_, rest): (&str, &str)| !rest.trim().is_empty());
        if interior_break {
            findings.push(QuoteFinding {
                paragraph: paragraph_of(event.at),
                text: head_chars(&event.text, 200),
                kind: "swallowed paragraph",
            });
            continue;
        }
        // Gate 2 — prose runs straight into a LONG quote. A real handover is a
        // colon or sentence punctuation; a word welded to a long span means a
        // mark is missing. The check steps back OVER the opening delimiter —
        // `event.at` is inside the span, so the character it must judge sits
        // one delimiter before it. The length guard keeps legitimate quoted
        // terms from firing: `Tràng "cuồng phong bạo vũ"` mid-sentence is a
        // healthy chapter, and only a span a quoted term never reaches is
        // evidence of a lost mark.
        if event.at > 0 && event.text.chars().count() > 120 {
            let before = clean[..event.at].trim_end();
            let before = before
                .strip_suffix('"')
                .or_else(|| before.strip_suffix('\u{201c}'))
                .or_else(|| before.strip_suffix('\u{300c}'))
                .unwrap_or(before)
                .trim_end();
            let hands_over = before
                .chars()
                .last()
                .is_none_or(|c| c == ':' || !c.is_alphanumeric());
            if !hands_over {
                findings.push(QuoteFinding {
                    paragraph: paragraph_of(event.at),
                    text: head_chars(&event.text, 200),
                    kind: "welded prose",
                });
            }
        }
    }
    findings
}

/// The text a chapter's digest should actually read: the sidecar a previous
/// repair wrote, when that sidecar is present and balanced.
///
/// So a repair is paid for once. The original file is never rewritten — it is
/// crawled source, and the operator's copy of it is worth more than the
/// convenience — but the second digest of the same chapter reads the balanced
/// text rather than paying for the repair again. An absent or still-unbalanced
/// sidecar falls back to the original, which is what sends the chapter to
/// [`repair_quotes`] once more.
fn effective_text(layout: &Layout, n: u32, original: &str) -> String {
    let sidecar = repaired_txt(layout, n);
    std::fs::read_to_string(&sidecar)
        .ok()
        .filter(|fixed| quote_findings(fixed).is_empty())
        .unwrap_or_else(|| original.to_string())
}

/// Where a repaired chapter is kept: beside the chapter, never over it.
fn repaired_txt(layout: &Layout, n: u32) -> PathBuf {
    layout.data().join(format!("ch{n:02}-repaired.txt"))
}

/// The proofread pass, asked only when the gate tripped.
///
/// A **separate pass, on purpose**, and not a rule inside the staging
/// instructions. The staging pass never sees the imbalance — its window reads
/// fine — so a rule there is advice about a fault the model cannot observe,
/// which is the thing that already failed. Handed the whole chapter at once,
/// with the gate's own paragraph, parity is a question the model can actually
/// check, because the chapter fits in one context.
///
/// The prose is `prompts/repair.txt` beside the other two templates, so it is
/// editable and per-language like everything else. What the code owns is the
/// part an operator must not soften: the contract appended below, and the
/// facts only the scan has — which paragraphs are broken and how. A template
/// that dropped a placeholder still renders, and the miss is warned about
/// rather than silently costing the model the one thing it needs.
fn build_repair_prompt(layout: &Layout, text: &str, complaint: &str) -> Result<String> {
    let path = layout.repair_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let mut missed: Vec<String> = Vec::new();
    let mut body = template;
    replace_or_miss(&mut body, "{fault_line}", complaint, &mut missed);
    replace_or_miss(&mut body, "{chapter_text}", text, &mut missed);
    warn_missing_sections("repair prompt", &missed);
    // Appended, never in the file: the answer is checked by code on return, and
    // a template that made that optional would make the whole gate optional.
    body.push_str(
        "\n---REPAIR OUTPUT CONTRACT---\nReturn ONE strict JSON object, never markdown or \
         commentary:\n{\"text\": \"the full corrected chapter text\", \"changes\": [\"one short \
         line per change\"]}\n\nYour `text` is REJECTED unless every alphanumeric character of it, in \
         order, is identical to the input's. Punctuation, quote marks and whitespace are the \
         only things you may move.\n",
    );
    Ok(body)
}

/// The gated proofread ladder, run once per chapter, only when the scan finds
/// something. Returns the repaired text, or `Ok(None)` to digest the original.
///
/// The order is the ladder, not a single ask:
///
/// 1. **LLM FIX** — the whole chapter, the scan's findings, one proofread.
/// 2. **light gate** — the deterministic verifier: parseable JSON, a `text`
///    field, and every alphanumeric character identical to the input. Most
///    answers clear it and stop here.
/// 3. **LLM + Gate 1** — re-ask carrying the complaint, when a speech still
///    spans a paragraph break.
/// 4. **LLM + Gate 2** — one more ask if prose is still welded to a speech.
///
/// Three answers, not one, because a model handed a complaint about its own
/// last answer fixes it far more often than a fresh ask guesses. Every
/// candidate is judged by the same closure — the alphanumeric filter plus the
/// two structural gates — so a creative model cannot buy its way past a gate by
/// rewriting, and no round is ever more lenient than the last. A chapter
/// that still fails after the ladder digests the **original**: a bad read, but
/// a rewrite is a different book, and the operator is the one who may decide.
///
/// `Err` is reserved for a **broken install** — a missing `prompts/repair.txt`.
/// That is fatal rather than a silent fallback, because digesting an unbalanced
/// chapter without ever saying so is the exact failure this pass exists to
/// prevent, and it would do it quietly.
#[allow(clippy::too_many_arguments)]
async fn repair_quotes(
    layout: &Layout,
    n: u32,
    text: &str,
    findings: &[QuoteFinding],
    analyzer: &str,
    settings: &Settings,
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<Option<String>> {
    let list = |fs: &[QuoteFinding]| {
        fs.iter()
            .map(|f| {
                format!(
                    "  - {}: paragraph {}: {}",
                    f.kind,
                    f.paragraph,
                    head_chars(&f.text, 120)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let ask = |complaint: String| {
        build_repair_prompt(layout, text, &complaint).map_err(|e| {
            eprintln!("ch{n} repair pass unavailable: {e}");
            e
        })
    };
    // Spent from the chapter's budget like any other G, so the phrase pass's
    // three rungs are counted against the cap rather than being free calls.
    async fn call(
        layout: &Layout,
        n: u32,
        analyzer: &str,
        settings: &Settings,
        prompt: &str,
    ) -> Option<String> {
        dump_raw(layout, "digest-repair", prompt);
        match generate(prompt, analyzer, settings).await {
            Ok((raw, _)) => Some(raw),
            Err(GenError::RateLimited(m)) => {
                eprintln!("ch{n} proofread rate-limited: {m}; digesting the original");
                None
            }
            Err(GenError::Fatal(e)) => {
                eprintln!("ch{n} proofread failed ({e}); digesting the original");
                None
            }
        }
    }
    // The verdict on one answer. `Fixed` ends the ladder; anything else says
    // exactly which gate the candidate still fails, which is the complaint the
    // next ask carries.
    enum Verdict {
        /// Accepted text plus the model's own change list, for the sidecar.
        Fixed(String, Value),
        /// The answer was unusable (no JSON) or a rewrite: stop, fall back.
        Reject(String),
        /// Words untouched, but a structural gate still fires: the complaint
        /// the next ask carries.
        Gated(String),
    }
    // ...and the one place every answer is judged, so the ladder cannot drift
    // into believing an answer one round and refusing the same shape the next.
    let judge = |raw: &str| -> Verdict {
        let Some(parsed) = parse_json_repaired(raw).ok() else {
            return Verdict::Reject("no usable JSON".to_string());
        };
        let Some(fixed) = parsed
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Verdict::Reject("no text field".to_string());
        };
        if strip_punctuation(&fixed) != strip_punctuation(text) {
            return Verdict::Reject("changed more than punctuation".to_string());
        }
        let changes = parsed.get("changes").cloned().unwrap_or(Value::Null);
        // Gate 1 (spanning) and Gate 2 (welded prose) on the candidate. The
        // net check is deliberately NOT here: it has already done its job by
        // naming the damage, and a candidate that fixed both structural faults
        // but traded one mark for another is still every word it was given,
        // correctly split — the digest's own validators say the rest.
        let gated = quote_findings(&fixed)
            .into_iter()
            .filter(|f| f.kind == "swallowed paragraph" || f.kind == "welded prose")
            .collect::<Vec<_>>();
        if let Some(f) = gated.first() {
            return Verdict::Gated(format!(
                "{} at paragraph {}: {}",
                f.kind,
                f.paragraph,
                head_chars(&f.text, 120)
            ));
        }
        Verdict::Fixed(fixed, changes)
    };
    let keep = |verdict: Verdict,
                note: &str,
                progress_at: f32,
                progress: &mut (dyn FnMut(f32, String) + Send)|
     -> Option<String> {
        match verdict {
            Verdict::Fixed(fixed, changes) => {
                // The sidecar is the audit trail: the repaired text an
                // operator can diff against the chapter the crawl produced,
                // and the text the next digest of this chapter reuses.
                let _ = atomic_write(
                    &repaired_txt(layout, n),
                    &format!(
                        "{}\n{}",
                        serde_json::to_string_pretty(&changes).unwrap_or_default(),
                        fixed
                    ),
                );
                progress(progress_at, format!("ch{n} {note}"));
                Some(fixed)
            }
            Verdict::Reject(why) | Verdict::Gated(why) => {
                eprintln!("ch{n} not used: {why}");
                None
            }
        }
    };

    // 1. LLM FIX — the whole chapter, with every finding named.
    progress(
        0.02,
        format!(
            "ch{n} quote structure is broken ({} finding(s)); asking for a proofread:\n{}",
            findings.len(),
            list(findings)
        ),
    );
    let prompt = ask(format!(
        "The chapter's speech quotation marks do not pair correctly. Findings:\n{}",
        list(findings)
    ))?;
    if calls.spend("phrase").is_err() {
        return Ok(None);
    }
    let Some(raw) = call(layout, n, analyzer, settings, &prompt).await else {
        return Ok(None);
    };
    // 2. light gate — most answers clear it and the ladder ends here.
    match judge(&raw) {
        v @ Verdict::Fixed(..) => Ok(keep(
            v,
            "quotes repaired, proofreading it kept punctuation only",
            0.03,
            progress,
        )),
        Verdict::Reject(why) => {
            eprintln!("ch{n} proofread {why}; digesting the original");
            Ok(None)
        }
        Verdict::Gated(complaint) => {
            // 3. LLM + Gate 1 — re-ask, carrying the structural complaint.
            progress(
                0.03,
                format!("ch{n} proofread still fails the gate ({complaint}); re-asking"),
            );
            let prompt = ask(format!(
                "Your last answer was rejected: {complaint}. Fix exactly that and return the \
                 full corrected text again."
            ))?;
            if calls.spend("phrase").is_err() {
                return Ok(None);
            }
            let Some(raw2) = call(layout, n, analyzer, settings, &prompt).await else {
                return Ok(None);
            };
            match judge(&raw2) {
                v @ Verdict::Fixed(..) => Ok(keep(
                    v,
                    "quotes repaired on the second ask, punctuation only",
                    0.04,
                    progress,
                )),
                Verdict::Reject(why) => {
                    eprintln!("ch{n} second proofread {why}; digesting the original");
                    Ok(None)
                }
                Verdict::Gated(complaint2) => {
                    // 4. LLM + Gate 2 — the last ask.
                    progress(
                        0.04,
                        format!(
                            "ch{n} still failing after the second ask ({complaint2}); last attempt"
                        ),
                    );
                    let prompt = ask(format!(
                        "Your last answer was rejected: {complaint2}. Fix exactly that and \
                         return the full corrected text again."
                    ))?;
                    if calls.spend("phrase").is_err() {
                        return Ok(None);
                    }
                    let Some(raw3) = call(layout, n, analyzer, settings, &prompt).await else {
                        return Ok(None);
                    };
                    match judge(&raw3) {
                        v @ Verdict::Fixed(..) => {
                            Ok(keep(v, "quotes repaired on the third ask", 0.05, progress))
                        }
                        Verdict::Reject(why) | Verdict::Gated(why) => {
                            eprintln!(
                                "ch{n} proofread still failing after the ladder ({why}); \
                                 digesting the original — the chapter probably lost a mark \
                                 the source no longer has: {why}"
                            );
                            Ok(None)
                        }
                    }
                }
            }
        }
    }
}

/// Every alphanumeric character of a text, in order.
///
/// The verifier's whole idea. Punctuation and whitespace are what a proofread
/// is allowed to move, so dropping them leaves the part that must not change —
/// if this is equal, nothing was rewritten; if it is not, the answer is a
/// rewrite wearing a proofread's clothes.
fn strip_punctuation(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
}

/// Whether a narration event ends by handing the floor to the speech that
/// comes after it — a speech verb and its colon, `…từng chữ từng câu hỏi:`.
///
/// Both sides of a quote look alike in the view: a `previous_context` that ends
/// this way is the tag for the quote in hand, and a `following_context` that
/// ends this way is the tag for the *next* dialogue event in the chapter. On
/// ch51 of beyond-myriads the model was handed the second while looking at the
/// first, and gave the sect elder's line about his own clan's treasure to the
/// woman being scolded, because the narration after it ended by handing the
/// floor to her reply. Which side of the quote the verb sits on is knowable
/// here and not from the text, so it is handed over as a flag rather than left
/// to the model.
fn hands_off_to_quote(text: &str) -> bool {
    text.trim_end().ends_with(':')
}

/// Whether a narration attributes a quote to somebody at all — a speech verb
/// anywhere in it, `Lạc Lan Tuyết vẻ mặt trịnh trọng nói.` Yes, and `Trời tối
/// dần.` No.
///
/// The distinction matters for the *following* side only, and it is what keeps
/// [`attribution_view`]'s `decided_by` from calling any narration after a quote
/// a tag. It is not a tag because it follows; it is a tag because it says
/// somebody spoke. Prose that merely continues the scene is evidence of nothing
/// and must not be named as the answer’s source.
const SPEECH_VERBS: &[&str] = &[
    " nói",
    " hỏi",
    " đáp",
    " kêu",
    " rằng",
    " quát",
    " thốt",
    " hét",
    " gào",
    " than",
    " khấn",
    " dặn",
    " bảo",
    " thưa",
    " đọc",
    " nói tiếp",
    " hỏi lại",
    " đáp lại",
    " trả lời",
    " lên tiếng",
    " tiếp lời",
    " ngắt lời",
    " thì thầm",
    " lẩm bẩm",
    " cười nói",
];

fn attributes_speech(text: &str) -> bool {
    let folded: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let folded = format!(" {folded}");
    SPEECH_VERBS.iter().any(|verb| folded.contains(verb))
}

/// The attribution prompt's view of the chapter: dialogue and thought events it
/// must attribute, plus the nearest narration immediately before and after each
/// one.
///
/// Splitting the answerable events from narration keeps the map small. Keeping
/// the adjacent narration is nevertheless essential: Vietnamese web novels
/// routinely put the speaker tag *after* the quote (`"Sư tôn..." Lạc Lan Tuyết
/// ... nói.`). The old view kept narration ids but removed their text, so the
/// model was explicitly told to use surrounding narration it could not see. On
/// ch6 it assigned Lạc Lan Tuyết's three tagged lines to Chung Thanh. Context
/// beside each quote restores that evidence while leaving only dialogue ids in
/// the answer map.
///
/// Thoughts are a list of their own rather than dialogue without quote marks.
/// The answer for a thought is a *thinker*, not a speaker, and the model can
/// only know which it is being asked for if the view says so — the same words
/// in one list are a line to cast and in the other an interior voice to own.
fn attribution_view(prepared: &PreparedChapter) -> String {
    let mut narration_ids = Vec::new();
    let mut dialogue_events = Vec::new();
    let mut thought_events = Vec::new();
    for (i, event) in prepared.events.iter().enumerate() {
        if !matches!(event.kind.as_str(), "dialogue" | "thought") {
            narration_ids.push(json!(event.id));
            continue;
        }

        // Whether the narration at `at` is *a speech tag handing the floor to
        // the quote after it*: it ends the way a tag ends, and a quote is
        // actually there to take the floor. A trailing colon at the end of the
        // chapter introduces nobody, and calling that a tag would make this a
        // guess about punctuation rather than a fact about the chapter.
        let hands_off = |at: usize| -> bool {
            prepared.events.get(at).is_some_and(|candidate| {
                candidate.kind == "narration"
                    && hands_off_to_quote(&candidate.text)
                    && prepared.events[at + 1..]
                        .first()
                        .is_some_and(|next| matches!(next.kind.as_str(), "dialogue" | "thought"))
            })
        };
        let next_narration = prepared.events[i + 1..]
            .iter()
            .position(|candidate| candidate.kind == "narration")
            .map(|offset| i + 1 + offset);
        // **The tag, from THIS event's point of view.** The old field was a
        // property of the narration — `hands_off_to_next_quote` — and read
        // inside a `previous_context` its own name says "not this one", which
        // is the exact opposite of what it means there. On ch51 that was worth
        // ten answers in twelve. A side of the quote is decidable in code, so it
        // is decided in code: `previous` is the narration whose speech verb
        // hands the floor to this quote, `following` is the narration reacting
        // to it, and `null` is neither.
        // **Named `decided_by`, and that name is load-bearing twice over.**
        // First, it spells the answer rather than a code for it: the value is
        // the *key* of the context to read, so there is no `"previous"` →
        // `previous_context` hop to get wrong. Second, `serde_json` writes a
        // `Value`'s object keys **alphabetically**, and `decided_by` sorts
        // before `following_context`, so this is the first field of every event
        // the model reads. Order is not cosmetic here: ch51's line was answered
        // correctly 5 times in 12 with this field last and 12 times in 12 with it
        // first, byte-for-byte identical otherwise. A test pins the ordering,
        // because a rename that sorted later would silently undo this.
        let tag_context = if (i > 0) && hands_off(i - 1) {
            json!("previous_context")
        } else if next_narration.is_some_and(|at| {
            // Reacting to this quote *by attributing it*. A narration that only
            // continues the scene is not evidence about who spoke, and naming
            // it as this quote's tag would hand the model an answer that is not
            // there — `"Đi thôi." Trời tối dần.` has no tag at all.
            !hands_off(at)
                && prepared
                    .events
                    .get(at)
                    .is_some_and(|n| attributes_speech(&n.text))
        }) {
            json!("following_context")
        } else {
            Value::Null
        };

        let context = |range: std::ops::Range<usize>| {
            let start = range.start;
            prepared.events[range]
                .iter()
                .position(|candidate| candidate.kind == "narration")
                .map(|offset| {
                    let candidate = &prepared.events[start + offset];
                    json!({"id": candidate.id, "text": candidate.text})
                })
                .unwrap_or(Value::Null)
        };
        let entry = json!({
            "decided_by": tag_context,
            "id": event.id,
            "text": event.text,
            "previous_context": context(i.saturating_sub(1)..i),
            "following_context": context(i + 1..prepared.events.len()),
        });
        if event.kind == "thought" {
            thought_events.push(entry);
        } else {
            dialogue_events.push(entry);
        }
    }
    let view = json!({
        "narration_ids": narration_ids,
        "dialogue_events": dialogue_events,
        "thought_events": thought_events,
        // The rules themselves live in the prompt template, where the rest of
        // the output contract is. This says only what the JSON is, so a model
        // reading the view and a model reading the contract are never told two
        // different things about the same field.
        "note": "Return `speakers` for every `dialogue_events` and `thought_events` id, except any you also list in `not_speech` — a span that is not somebody talking or thinking: a quoted title or term, or an unquoted narrator aside — judged from the context around it. A `thought_events` entry is an unquoted passage in the first or second person: answer with the character thinking it, never Narrator and never the addressee. Context events are evidence for resolving an id; all context and every id in `narration_ids` are spoken by Narrator and are not yours to answer. Each entry's first field, `decided_by`, names the context that holds that quote's own tag: `previous_context`, `following_context`, or null when neither side tags it. Read it before anything else in the entry.",
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
        "INPUT 2 — the prepared chapter as three lists, in exact source order. `narration_ids` are prose events: the preparer has already spoken them as `Narrator` and they are NOT yours to answer. `dialogue_events` are the quoted lines, and `thought_events` are unquoted first- or second-person passages carved out of narration — a thought to be assigned a thinker, not a line to be assigned a speaker. Each carries the stable `id` your answer keys on and its text without quote delimiters.",
        &mut missed,
    );
    replace_or_miss(
        &mut body,
        "This is the CONTEXT pass: you read one\nchapter and report WHO is in it and WHAT it is about — the cast and the story.\nYou do NOT write the script. A second pass does that, and it is handed your answer\nas its cast list, so be exact about names and about the surface forms the chapter\nuses: everything downstream is resolved against what you return here.",
        "This is the ATTRIBUTION pass: prepared narration, dialogue and thought events are already separated deterministically. Resolve the chapter cast and assign one immutable voice to every event. You do NOT stage audio, choose music, or write segments; the next pass is handed this exact speaker map.",
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

    let content_language = content_language(layout);

    let contract = r#"
---ATTRIBUTION OUTPUT CONTRACT---
Return ONE strict JSON object, never markdown or commentary:
{
  "title": "3-8 word chapter title in {content_language}, as rule 3 of this prompt defines it; do not start it with the source's chapter-heading word (`Chương`, `Chapter`)",
  "atmosphere": "1-2 sentences in {content_language}",
  "excerpt": "{excerpt}",
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
`dialogue_events` and `thought_events` id, in source order, and nothing else —
no narration ids, no invented ids, no dropped line.
- Every `dialogue_events` id maps to a canonical character name or the reserved
  name `Anonymous`. Dialogue must NEVER map to Narrator, even when the speaker is
  uncertain, even for a greeting, and even when nobody in the line is named.
- Every `thought_events` id is an inner thought — an unquoted first- or
  second-person passage, the viewpoint character thinking in their own voice —
  so it maps to the character **thinking** it, using the same `speakers` map:
  never Narrator, never the addressee. The thinker is the `I` in the surrounding
  action — the `you` of `You know no one is going to come visit you` mused over a
  consort is hers, not a stranger's. If no cast member thinks it, give it
  `Anonymous`.
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
- A passage voiced `I`/`my`/`me` is that thinker's own voice and stays a
  thought, whatever the prose around it does: a third-person narrator rendering
  it does not make it narration. `I need to just get this job done.` and `Hope
  my old man's eating properly.` are the thinker, never the Narrator — never
  list them in `not_speech`.
- Retract only these two shapes. A passage that **names its thinker in third
  person** (`Maomao's thinking was…`, `But Maomao, who had been making her way
  just fine as an apothecary, thank you very much, saw it solely as so much
  trouble.`) is narration about them, not their thought — even with an aside
  tucked inside — so list it in `not_speech`.
  Never voice a character saying their own name in third person.
- Or a narrator aside to the reader (`let us call them…`, `you see`, a `you reap
  what you sow` maxim): list it in `not_speech` like a title. These are
  `we`/`you`-voiced, never `I`-voiced.
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
- **`decided_by` is already resolved for you, and it is the first thing to
  read.** It names the context holding this quote's own explicit named dialogue
  tag: `"previous_context"` means the narration before the quote hands the floor
  to it, `"following_context"` means the narration after it attributes the quote
  just made, and `null` means neither side has a tag and you resolve the speaker
  from the quote itself and the scene. Read that context. `"following_context"`
  proves the line belongs to that tag's subject even when the quote only
  addresses `Sư tôn` — `"Sư tôn, chính là nơi này." Lạc Lan Tuyết vẻ mặt trịnh
  trọng nói.` is hers. A narration on the other side which ends in a speech verb
  and a colon (`… nàng đành kiên trì gật đầu nói:`) is that *next* quote's tag, so
  it names the speaker of the line after this one, never of this one, and
  `decided_by` will not point at it. The context objects beside each quote are
  evidence for that id; they are never themselves speaker-map entries.
- **A name inside the quote is evidence when the quote claims it in the first
  person, and no evidence otherwise.** `chí bảo của Huyền Vũ tông ta` — "my sect
  Huyền Vũ" — is the speaker saying whose house they belong to, so the speaker is
  `Huyền Vũ lão tổ`, and that is stronger than any name in the narration around
  it. The same goes for `đệ tử của ta`, `Chấn Thiên Thạch của ta`, `sư phụ ta`.
  What is *not* evidence is a name the quote merely addresses or mentions in the
  second person: `"Dịch sư phụ."` or `"Sư tôn, chính là nơi này."` names the
  LISTENER, so never the speaker. `Đồ nhi`, `đệ tử`, `sư tôn` and similar forms
  are scenario-dependent, so they are never a reason on their own and belong in
  no `mentions` entry.
- Worked example, one chapter's own words, `decided_by` deciding it:
  ```
  e0009 narration  "… Ninh Huyền Vũ … nhìn chằm chằm Yêu Linh Nhi từng chữ từng câu hỏi:"
  e0010 dialogue   "Ngươi nói Chấn Thiên Thạch của ta, chí bảo của Huyền Vũ tông ta, bị hắn lấy ra lấp bậc thang ư?"
  e0011 narration  "Nhìn vẻ nổi giận của sư tôn mình, Yêu Linh Nhi … nàng đành kiên trì gật đầu nói:"
  ```
  `e0010.decided_by` is `"previous_context"`, so e0009 tags it and the answer is
  `Huyền Vũ lão tổ` — even though e0009's last name before the verb is Yêu Linh
  Nhi, and even though e0011 opens by naming Yêu Linh Nhi. e0011 ends in `nói:`
  and tags e0012, not e0010. The quote's own `Huyền Vũ tông ta` agrees. Answering
  `Yêu Linh Nhi` here is wrong twice over: it takes the next line's tag, and it
  reads the addressee as the speaker.
- Quoted game-system notifications are dialogue for the canonical `Hệ thống`
  character when the bible contains it; prose about the system remains narration.
- `roster` contains Narrator when narration exists, every named speaker used, and
  `Anonymous` when the chapter has an unnamed speaker. It must not contain a
  character who never speaks.
- Every value in `speakers` and every entry in `roster` is the character's
  **`name` from INPUT 1, copied exactly** — never a form the chapter happens to
  use. When a character's `proper_aliases` list holds the form you can see in the
  prose (`Ninh Huyền Vũ` under `Huyền Vũ lão tổ`, `Lạc Ly` under `Doãn Lạc Ly`,
  `Sở Cuồng sư` under `Sở Cuồng`), answer with the canonical `name` and put the
  surface form in `mentions`. An alias where a canonical name belongs is refused,
  which costs the whole round: a rejected answer is re-asked from scratch, so
  copying the chapter's spelling is slower than reading the bible.
- Correctness priority is `speakers` first, title second, and cast metadata last.
  A named speaker omitted from `new_characters` is synthesized by code. Never emit
  a nameless character object. `mentions` is optional evidence; omit uncertain
  rows rather than inventing an owner. Free-form `voice_hint` text is accepted.
"#;
    let contract = contract
        .replace("{content_language}", &content_language)
        .replace("{excerpt}", &excerpt_rule(&content_language));
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
    // **The rules go above the data, not below it.** The contract used to be
    // appended after the chapter — which on this book is tens of thousands of
    // characters of events and context — so the model had answered before it
    // ever read what it was asked for. Orders of magnitude, measured on ch51's
    // elder line with everything else held byte-for-byte identical: contract
    // last, 3 in 12; contract first, 40 in 40. It is the same text in the same
    // prompt, and the only difference is which end the reader reaches first.
    //
    // Placed immediately before `---CHAPTER---` rather than at the very top, so
    // the contract still follows the prompt it modifies and the bible it is
    // resolved against, and `---PREVIOUSLY---` keeps its place at the end.
    Ok(match body.find("---CHAPTER---") {
        Some(at) => format!("{}{}\n{}", &body[..at], contract, &body[at..]),
        None => format!("{body}\n{contract}"),
    })
}

/// The previous chapters' excerpts chapter `n` is fed, newest first, each
/// paired with the chapter it summarizes.
///
/// `pub` because the TUI's excerpt view draws the same chain the attribution
/// prompt is built from: one definition of the window, so the screen can never
/// show a different memory than the model was handed. Depth is
/// `excerpt_window` from settings — 1 is chapter *n−1* only, 0 is off — and
/// each excerpt is read from the stored script of the chapter it summarizes. A
/// chapter with no stored predecessor (the first one, an out-of-order one, a
/// book digested before the field existed) contributes nothing: fewer entries,
/// not a failure, the same "if any" the bible's own partial order has always
/// had.
pub fn excerpt_chain(layout: &Layout, n: u32) -> Vec<(u32, String)> {
    let window = Settings::load(&layout.settings()).excerpt_window;
    if window == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
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
        out.push((m, excerpt.to_string()));
    }
    out
}

/// The prompt half of [`excerpt_chain`]: the chain as `CH m: excerpt` lines.
/// `None` when there is none, which is what keeps a windowless prompt
/// byte-for-byte the pre-excerpt one.
fn previous_excerpts(layout: &Layout, n: u32) -> Option<String> {
    let chain = excerpt_chain(layout, n);
    if chain.is_empty() {
        return None;
    }
    Some(
        chain
            .iter()
            .map(|(m, e)| format!("CH {m}: {e}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// The language the digest's prose fields are written in.
///
/// The contract's language is the ADAPTER's, not a constant: `atmosphere` and
/// `excerpt` used to say "English sentences" for every book on every checkout,
/// which was true exactly once and silently wrong for every other adapter — and
/// a Vietnamese title instruction shipped beside them for a while, which is how
/// an English book ended up titled in Vietnamese even after its prompts were.
/// What is declared in `adapter.json` is the one fact the fork line rests on
/// ("an adapter has one language, and it is both the source's and the
/// target's"), so that is what the wording follows; an adapter that claims
/// nothing falls back to the chapter's own language, the same fact said per
/// chapter instead of per manifest.
fn content_language(layout: &Layout) -> String {
    crate::adapter::in_force(layout)
        .ok()
        .flatten()
        .map(|m| m.language.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "the chapter's own language".into())
}

/// The excerpt instruction, verbatim.
///
/// One wording, two callers: the attribution contract embeds it as one field of
/// the strict JSON it asks for, and [`build_excerpt_prompt`] asks for it alone.
/// Keeping it a single string is what makes a backfilled excerpt the same field
/// the digest would have written instead of a second, drifted definition.
fn excerpt_rule(content_language: &str) -> String {
    format!(
        "2-4 sentences in {content_language} on the state this chapter ENDS in: who is \
         present, identity reveals (X is Y), disguises, deaths, and any stranger the prose \
         still has not named — written for the NEXT chapter's analyzer, who has not seen \
         this chapter and resolves its cast against it. State, not plot."
    )
}

/// The **excerpt-only** prompt: the attribution pass's excerpt, asked for on its
/// own.
///
/// [`build_attribution_prompt`] asks for the excerpt as one field of a cast
/// answer and pays for the whole attribution gate to get it. A book digested
/// before the field existed has scripts but no excerpts, and re-digesting it to
/// recover a two-sentence memory would re-decide every speaker, invalidate
/// segments and land a second bible delta. This asks the same question against
/// the same bible and the same `---PREVIOUSLY---` chain and nothing else, so the
/// answer is the field the digest would have kept — same instruction, same
/// window — without touching the cast.
///
/// `text` is the raw chapter, the same chapter the attribution pass is handed.
/// The block order mirrors the pipeline's: context and rules first, the chapter
/// last, because a model that reads the data before the question has already
/// answered.
pub fn build_excerpt_prompt(layout: &Layout, n: u32, text: &str) -> Result<String> {
    let bible = load_bible(&layout.bible());
    let language = content_language(layout);
    let mut prompt = format!(
        "You are completing ONE field of the story digest for chapter {n} of a serialized \
         novel. Read the chapter below and write only its excerpt — the state it ends in, \
         for the next chapter's analyzer.\n\n{}\n\nINPUT 1 — the story so far (identity only):\n{}\n",
        excerpt_rule(&language),
        bible_context(&bible),
    );
    if let Some(previously) = previous_excerpts(layout, n) {
        prompt.push_str(&format!(
            "\n---PREVIOUSLY--- (the chapter before this one; identity context only — \
             resolve names and strangers against it, but write only this chapter's \
             excerpt)\n{previously}\n"
        ));
    }
    prompt.push_str(&format!(
        "\n---CHAPTER---\n{text}\n\nReturn ONE strict JSON object, never markdown or \
         commentary:\n{{\"excerpt\": \"...\"}}\n"
    ));
    Ok(prompt)
}

/// Read an excerpt answer, tolerantly.
///
/// The strict `{"excerpt": "..."}` object is what the prompt asks for, but a
/// model sometimes returns the prose alone. The field is soft in the digest
/// (blank or over-long is squeezed and capped, never refused) and it is soft
/// here for the same reason, so the only failure is an answer with nothing in
/// it — and that is what `None` says, which is what the caller repairs.
///
/// White space is squeezed to single spaces so an excerpt read back from disk
/// is byte-identical to the one the digest would have stored.
pub fn parse_excerpt(raw: &str) -> Option<String> {
    let cleaned = strip_fences(raw);
    // `strip_fences` knows ```` ```json ```` and a trailing fence; a bare
    // opener with no language tag is common enough in a model answer that the
    // excerpt reader undoes it too, rather than reading the fence as prose.
    let unfenced = match cleaned.strip_prefix("```") {
        Some(rest) => rest.split_once('\n').map(|(_, body)| body).unwrap_or(rest),
        None => cleaned,
    };
    let cleaned = unfenced.strip_suffix("```").unwrap_or(unfenced).trim();
    let from_json = parse_json_repaired(cleaned)
        .ok()
        .and_then(|v| v.get("excerpt").and_then(Value::as_str).map(str::to_string));
    let text = head_chars(
        &squeeze_ws(from_json.as_deref().unwrap_or(cleaned)),
        EXCERPT_CHARS,
    );
    (!text.is_empty()).then_some(text)
}

/// Write one chapter's excerpt back into its stored script, and nothing else.
///
/// The script holds the segment plan, the cast and the speakers the render
/// reads; recovering a missing memory must not rewrite any of them. Only the
/// `excerpt` key is touched, and the write goes through [`write_script`] like
/// every other script write, so the artifact on disk cannot land differently
/// from one a digest wrote.
pub fn write_excerpt(layout: &Layout, n: u32, excerpt: &str) -> Result<()> {
    let path = layout.script(n);
    let mut script =
        crate::read_json::<Value>(&path).with_context(|| format!("reading {}", path.display()))?;
    script
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not a JSON object", path.display()))?
        .insert("excerpt".to_string(), json!(excerpt));
    write_script(layout, n, &script)
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

A source event whose `kind` is `thought` is a thought being thought, not a line
being spoken: voice it as its speaker like any line, reproducing it exactly —
never add quote marks around it, and never fold it into the narration. Thoughts
read as interiority; a reflective mood suits them unless the feeling says
otherwise. Do not write a `sound_after` for a thought just to mark it: the
pack's own thought sound, if it declares one, is attached by code.

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
            settings.digest.chunk_sentences,
            settings.digest.chunk_chars,
            settings.digest.answer_tokens
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
/// Everything one part's G's share, so a G is a step plus a context rather than
/// ten arguments — and so the driver can hold one of these while it decides
/// which step to run next.
struct PartCtx<'a> {
    layout: &'a Layout,
    n: u32,
    analyzer: &'a str,
    settings: &'a Settings,
    bible: &'a Value,
    vocab: &'a Vocabulary,
    slice: &'a PreparedChapter,
    continuity: Option<&'a Continuity<'a>>,
    part: Option<(usize, usize)>,
    progress: &'a mut (dyn FnMut(f32, String) + Send),
    /// The band this part spends, split between the two steps.
    from: f32,
    mid: f32,
    to: f32,
    /// How far the bar has been pushed, so a step that runs twice — because a
    /// gate handed the work back to it — never rewinds what the operator has
    /// already been shown. Re-casting a chapter is still forward progress.
    hi: f32,
}

impl PartCtx<'_> {
    /// The band one step spends, clamped to what has already been shown.
    fn band(&self, round: Round) -> (f32, f32) {
        let (a, b) = match round {
            Round::Attribution => (self.from, self.mid),
            Round::Staging => (self.mid, self.to),
        };
        (a.max(self.hi), b.max(self.hi))
    }

    fn report(&mut self, at: f32, msg: String) {
        self.hi = self.hi.max(at);
        (self.progress)(self.hi, msg);
    }
}

/// Ask the analyzer to stage one window, running each step as a G and handing
/// back to an earlier one whenever a gate says the fault is not there.
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
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
    from: f32,
    mid: f32,
    to: f32,
) -> Result<(Value, Value)> {
    let mut ctx = PartCtx {
        layout,
        n,
        analyzer,
        settings,
        bible,
        vocab,
        slice,
        continuity,
        part,
        progress,
        from,
        mid,
        to,
        hi: from,
    };
    loop {
        // G_attribution. Nothing can blame a later step from here, so a Back is
        // a gate that named a step this loop cannot reach — a bug, not a
        // chapter, and it is reported as one rather than looped on.
        let context = match run_g(Round::Attribution, &mut ctx, None, calls).await {
            Ok(context) => context,
            Err(Fail::Dead(e)) => return Err(e),
            Err(Fail::Back(why)) => {
                return Err(anyhow::anyhow!(
                    "digest attribution gate blamed another step ({why})"
                ))
            }
        };
        match run_g(Round::Staging, &mut ctx, Some(&context), calls).await {
            Ok(script) => return Ok((context, script)),
            Err(Fail::Dead(e)) => return Err(e),
            Err(Fail::Back(why)) => ctx.report(
                ctx.to,
                format!(
                    "{}{why}; the cast owns that, so attribution runs again",
                    part_prefix(ctx.part)
                ),
            ),
        }
    }
}

/// **One G**: build the prompt, spend a call from the chapter's budget, ask,
/// and let the gate judge the answer.
///
/// Two rungs, because a failure means one of only two things. The gate blamed
/// this step, so it gets one more ask here carrying the complaint — a model
/// told exactly what it got wrong fixes it far more often than a fresh ask
/// guesses, which is why this is a retry in place and not a jump back to
/// itself. The gate blamed an **earlier** step, so this answer is abandoned at
/// once: it was never going to be right, and spending a call to be sure twice
/// is the expensive way to learn it.
async fn run_g(
    round: Round,
    ctx: &mut PartCtx<'_>,
    context: Option<&Value>,
    calls: &mut GCalls,
) -> Result<Value, Fail> {
    let suffix = part_suffix(ctx.part);
    let mut complaint: Option<String> = None;
    for attempt in 0..2 {
        let (from, to) = ctx.band(round);
        // Re-rendered per attempt rather than carried, for the reason
        // `reask_staging` gives: a part's prompt is a few hundred KB, and two
        // reads cost less than holding every part's prompt for a chapter.
        let prompt = match round {
            Round::Attribution => {
                let previously = previous_excerpts(ctx.layout, ctx.n);
                build_attribution_prompt(
                    ctx.layout,
                    ctx.bible,
                    ctx.slice,
                    ctx.continuity,
                    previously.as_deref(),
                )
            }
            Round::Staging => build_staging_prompt(
                ctx.layout,
                &ctx.settings.engine,
                ctx.bible,
                context.expect("staging is only ever run against a cast"),
                ctx.slice,
                ctx.continuity,
            ),
        };
        let prompt = match prompt {
            Ok(prompt) => prompt,
            Err(e) => return Err(Fail::Dead(e)),
        };
        if let Err(e) = calls.spend(round.as_str()) {
            return Err(Fail::Dead(e));
        }
        ctx.report(
            from,
            round_label(ctx.n, ctx.analyzer, round.as_str(), ctx.part),
        );
        let raw = if attempt == 0 {
            generate_retrying(
                &prompt,
                ctx.analyzer,
                ctx.settings,
                &mut *ctx.progress,
                from,
                to,
            )
            .await
        } else {
            // The one repair, in place, carrying what the gate said.
            repair_once(
                &prompt,
                &anyhow::anyhow!(complaint.clone().unwrap_or_default()),
                ctx.analyzer,
                ctx.settings,
            )
            .await
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(e) => return Err(Fail::Dead(e)),
        };
        let dump = if attempt == 0 {
            format!("digest-{round}{suffix}")
        } else {
            format!("digest-{round}{suffix}-retry")
        };
        dump_raw(ctx.layout, &dump, &raw);
        let judged = match round {
            Round::Attribution => {
                parse_attribution(&raw, ctx.bible, ctx.slice, ctx.continuity.is_some())
            }
            Round::Staging => parse_staged_script(
                &raw,
                ctx.bible,
                context.expect("staging is only ever run against a cast"),
                ctx.slice,
                ctx.vocab,
            ),
        };
        match judged {
            Ok(value) => return Ok(value),
            Err(c) => match route(c.blame, round, attempt) {
                // Someone else's fault: the answer is dropped without a second
                // ask, and the step that owns it runs again.
                Route::Back => {
                    return Err(Fail::Back(format!(
                        "{}: {} (the {} step owns that)",
                        round.as_str(),
                        c.why,
                        c.blame.as_str()
                    )))
                }
                Route::Die => {
                    let dump = ctx.layout.data().join(".last-analyze-raw.json");
                    let _ = atomic_write(&dump, &raw);
                    return Err(Fail::Dead(anyhow::anyhow!(
                        "digest {} invalid ({}); raw saved to {}",
                        round.as_str(),
                        c.why,
                        dump.display()
                    )));
                }
                Route::Retry => {
                    ctx.report(
                        to,
                        format!(
                            "{}{} gate said {why}, asking for one repair",
                            part_prefix(ctx.part),
                            round.as_str(),
                            why = c.why,
                        ),
                    );
                    complaint = Some(c.why);
                }
            },
        }
    }
    // The loop returns on attempt 0 (accepted, or handed back) or attempt 1
    // (accepted or dead), so this is arithmetic rather than a path.
    unreachable!("run_g leaves on its first or second attempt")
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
    calls: &mut GCalls,
) -> Result<Value> {
    let continuity = (total > 1).then_some(Continuity { index, total, plot });
    let prompt = build_staging_prompt(
        layout,
        &settings.engine,
        bible,
        context,
        slice,
        continuity.as_ref(),
    )?;
    // A G like any other: it spends from the same chapter budget, so a chapter
    // whose sound design never converges cannot buy repairs past the cap.
    calls.spend(Round::Staging.as_str())?;
    let again = repair_once(
        &prompt,
        &anyhow::anyhow!(complaint.to_string()),
        analyzer,
        settings,
    )
    .await?;
    dump_raw(
        layout,
        &format!("digest-staging-retry{}", part_suffix(part)),
        &again,
    );
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
    let original = std::fs::read_to_string(&chapter_path)
        .with_context(|| format!("reading {}", chapter_path.display()))?;
    // One budget for every G this chapter runs, including the phrase pass below.
    let mut calls = GCalls::new(n);
    // The gate, before anything expensive. A sidecar from an earlier repair
    // means the question is already answered, and this chapter costs nothing
    // extra; otherwise an unbalanced chapter spends one proofread call here and
    // then runs the same digest it always does.
    let text = effective_text(layout, n, &original);
    let text = match quote_findings(&text).first() {
        None => text,
        Some(_) => match repair_quotes(
            layout,
            n,
            &text,
            &quote_findings(&text),
            analyzer,
            settings,
            &mut calls,
            progress,
        )
        .await?
        {
            Some(fixed) => fixed,
            None => original.clone(),
        },
    };
    let prepared = prepare_chapter(&text);
    let vocab = vocabulary(layout)?;
    let windows = plan_windows(&prepared, &settings.digest);
    let total = windows.len();
    // The parts are known, so the rest of the budget can be sized against them:
    // two calls each is the floor, and the surplus is what the gates are for.
    calls.allow_parts(total);
    let mut parts = Parts::open(layout, n, &text, bible, &windows, settings);
    if total > 1 {
        // Before any call, because the number of calls is the operator's
        // business: a 40 KB chapter is sixteen of them, not two, and a digest
        // that looks stuck is only diagnosable once the plan said so.
        progress(0.05, plan_line(n, analyzer, &windows, &prepared, settings));
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
            &mut calls,
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
            &mut calls,
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

/// The digest's steps, named for what each one is asked to do rather than for
/// the shape of its answer.
///
/// **Every step is a G: one LLM call plus the gate that judges it.** `Round` is
/// both the identity the operator's manual session drives and the step a
/// failure gets blamed on, because those are the same set of steps — there is
/// no fourth kind of thing that can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    /// Who speaks which event. Generated and validated first, and read as data
    /// by every step after it.
    Attribution,
    /// Which scenes, sounds and beds the events become.
    Staging,
}

impl Round {
    pub fn as_str(self) -> &'static str {
        match self {
            Round::Attribution => "attribution",
            Round::Staging => "staging",
        }
    }
}

impl std::fmt::Display for Round {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A gate failure, tagged with the step that **owns** the fault.
///
/// The tag is the whole point. A gate failure is not always the running step's
/// fault, and when it is not, asking again cannot help: staging reads the cast
/// as data and never emits a speaker, so a script answer that trips over a
/// missing attribution is not a staging mistake — no number of staging retries
/// will ever produce the row that only the attribution step can write. Retrying
/// in place there burns an expensive call and then refuses the chapter anyway,
/// which is a real failure this codebase has already lived through.
///
/// So a gate says who is wrong, and the driver either retries the step that
/// failed (its own fault, and one more ask is the ordinary remedy) or hands
/// back to the step named here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Complaint {
    /// The step that can fix this.
    pub blame: Round,
    /// What is wrong, in the words the model needs to fix it.
    pub why: String,
}

impl Complaint {
    pub fn new(blame: Round, why: impl std::fmt::Display) -> Self {
        Self {
            blame,
            why: why.to_string(),
        }
    }
}

impl std::fmt::Display for Complaint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

impl std::error::Error for Complaint {}

/// What a failed G does to the chapter.
enum Fail {
    /// No call from here can fix it. The chapter is over, with a reason.
    Dead(anyhow::Error),
    /// Another step owns the fault. Re-run that step, then this one again.
    Back(String),
}

/// What one gate failure means for the chapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// The running step's own fault, and it has an ask left.
    Retry,
    /// An earlier step's fault. The answer is dropped without a second call.
    Back,
    /// Its own fault, and one ask did not fix it.
    Die,
}

/// **The severity rule, in one place and four lines.**
///
/// This is the whole difference between a ladder and a retry loop. A failure
/// the running step owns is worth exactly one more ask *in place* — a model
/// told what it got wrong fixes it far more often than a fresh ask guesses. A
/// failure an earlier step owns is worth **none**, at any attempt: the answer
/// was never going to be right, because the step that must change did not run.
/// Asking again spends a call to be sure twice, which is the expensive way to
/// learn what the blame already said.
fn route(blame: Round, running: Round, attempt: u32) -> Route {
    if blame != running {
        return Route::Back;
    }
    if attempt == 0 {
        Route::Retry
    } else {
        Route::Die
    }
}

/// The chapter's LLM-call budget, shared by every G it runs.
///
/// Routing between steps is worth doing — but a chapter whose gates keep
/// handing work backwards must end, and ending it with a clear reason beats
/// ending it whenever the provider decides to bill the operator. The allowance
/// is generous on purpose: two calls per part is the floor, and the rest is
/// headroom for the retries that are the point of having gates.
struct GCalls {
    chapter: u32,
    left: usize,
    spent: usize,
}

impl GCalls {
    /// A chapter that has not split yet gets the phrase pass's own three asks.
    fn new(chapter: u32) -> Self {
        Self {
            chapter,
            left: 3,
            spent: 0,
        }
    }

    /// Add a part's worth of calls once the windows are known. Called once.
    fn allow_parts(&mut self, parts: usize) {
        self.left += 4 * parts;
    }

    /// Spend one call, or refuse the chapter rather than make it. The label is
    /// the step's own name, which for the phrase pass is a pass rather than a
    /// [`Round`] — it is not one of the two steps a script is made of.
    fn spend(&mut self, label: &str) -> Result<()> {
        if self.left == 0 {
            anyhow::bail!(
                "ch{} spent its budget of {} LLM calls without a passing gate; \
                 the last failure decides the chapter, and another call is not \
                 going to change it",
                self.chapter,
                self.spent
            );
        }
        self.left -= 1;
        self.spent += 1;
        eprintln!("ch{} {label} call {}", self.chapter, self.spent);
        Ok(())
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
    /// `scene-map.json`'s `thought.sound`, when the pack declares one. Checked
    /// against `injects` here so the prompt's vocabulary, the inject validator
    /// and the lift that writes the item all read the same one name.
    thought_stinger: Option<String>,
}

fn vocabulary(layout: &Layout) -> Result<Vocabulary> {
    let effect_pool = crate::audio_pool::load_pool(&layout.assets().join("effect-pool.json"));
    let map = load_map(layout)?;
    let palette = crate::ambience::palette_names(&map);
    let effects = crate::ambience::effect_tags(&effect_pool);
    let injects = crate::audio_pool::load_pool(&layout.assets().join("inject-pool.json"));
    let aliases = TagAliases::load(&layout.assets().join("tag-aliases.json"))?;
    aliases.validate(&palette, &effects, injects.keys().cloned())?;
    let thought_stinger = thought_stinger(layout, &map, &injects)?;
    Ok(Vocabulary {
        palette,
        effects,
        injects,
        aliases,
        thought_stinger,
    })
}

/// The pack's declared thought sound, checked against the inject pool.
///
/// Checked while the vocabulary is loaded rather than at the lift, for the
/// same reason the `music` palette is closed: a declared name with no clip
/// would otherwise be lifted into the script and refused by
/// `validate_injects` on every attempt, leaving the chapter stalling on a pack
/// typo instead of one line that names it.
fn thought_stinger(
    layout: &Layout,
    map: &crate::ambience::SceneMap,
    injects: &crate::audio_pool::ClipPool,
) -> Result<Option<String>> {
    let name = map.thought.sound.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if !injects.contains_key(name) {
        anyhow::bail!(
            "{} declares `thought.sound` = {name:?}, but assets/inject-pool.json has no such \
             sound — every thought would be lifted with a clip nobody has, so remove the rule or \
             add the clip to the inject pool",
            layout.assets().join("scene-map.json").display()
        );
    }
    Ok(Some(name.to_string()))
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
            None => Round::Attribution,
            Some(_) => Round::Staging,
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
        let (bible, original) = manual_inputs(layout, n)?;
        // The same gate the worker's digest runs, and for the same reason: the
        // prompts handed to an operator carry the same prepared events the
        // model gets, so a mispaired quote makes the *manual* rounds stage a
        // swallowed paragraph too — and by hand that is worse, because nobody
        // is watching for it.
        //
        // No repair call here: the manual path exists so a person stands in
        // for the model, and spending one on the operator's key behind their
        // back would make the cost invisible. A repaired sidecar is honoured
        // (one proofread, already paid for); a still-unbalanced chapter says
        // so and names the line, which is the part the operator can act on.
        let text = effective_text(layout, n, &original);
        if let Some(f) = quote_findings(&text).first() {
            anyhow::bail!(
                "ch{n} quote structure is broken: {} at paragraph {} ({:?}), so speech and \
                 narration are mis-split. Run the automatic digest once to proofread it, or \
                 fix the quote in {} by hand",
                f.kind,
                f.paragraph,
                head_chars(&f.text, 60),
                layout.chapter_txt(n).display()
            );
        }
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
        Round::Attribution => Ok(ManualAnswer {
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
        Round::Staging => {
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
            let what = if session.total() == 1 {
                "chapter"
            } else {
                "part"
            };
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
                let next_slice = session
                    .slice(next)
                    .ok_or_else(|| anyhow::anyhow!("ch{n} has no part {}", next + 1))?;
                let next_summaries = session.parts.summaries();
                let next_continuity = session.continuity(next, &next_summaries);
                return Ok(ManualAnswer {
                    prompt: Some(ManualPrompt {
                        round: Round::Attribution,
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
///
/// `pub` because the backup digestor asks its own rounds outside the worker's
/// [`call`], and it is precisely the dry run that has no other record: a
/// `--dry-run` reported a chapter's segment count and kept nothing, so a
/// question about what the model actually said could only be answered by
/// re-spending the call.
pub fn dump_raw(layout: &Layout, round: &str, raw: &str) {
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
/// (`title`, `atmosphere`, `excerpt`, `roster`, `mentions`, `new_characters`,
/// `new_aliases`) and the script pass owns the speech (`segments`, `fixes`).
/// Neither can overwrite the other's keys, so a round that helpfully invents a
/// `title` of its own is ignored rather than silently believed.
fn merge_rounds(context: &Value, script: &Value) -> Value {
    let mut out = serde_json::Map::new();
    // `excerpt` belongs to the cast pass like `title` and `atmosphere` do: it
    // is the cast answer's statement of the state the chapter ends in. It was
    // missing here, and because the miss is silent — the writer at the script
    // build reads `data.get("excerpt")` and falls back to `""` — every chapter
    // was digested with a working excerpt that never reached its script, and
    // the next chapter's `---PREVIOUSLY---` block was always empty.
    for key in [
        "title",
        "atmosphere",
        "excerpt",
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
/// The kinds the attribution pass has to give a voice to: a spoken line and a
/// thought. Narration is mechanical, so it is written by code and never asked
/// about — anything this returns false for gets `Narrator` without the model's
/// answer having a say.
fn is_voiced_kind(kind: &str) -> bool {
    matches!(kind, "dialogue" | "thought")
}

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
/// delimiter opened it — except a span too short to be speech embedded in
/// running prose, which stays narration outright — so a longer quoted title
/// or term still has to be spoken by somebody. The attribution pass already holds the span with the narration
/// on both sides, which is the evidence a title needs and a keyword list
/// cannot supply: a title is bracketed by prose that continues the sentence,
/// a speech is followed by a tag. So `not_speech` lets the model say so.
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
    // A first-person-singular passage is the thinker's own voice, so the
    // retraction does not apply to it. A live ch1 run is why: the answer listed
    // `I need to just get this job done.` and `Hope my old man's eating
    // properly.` in `not_speech`, and the Narrator read Maomao's thoughts
    // aloud. The escape hatch is for quoted non-speech and for narrator asides
    // to the reader — `we`/`you`-voiced, never the `I` a thought is made of.
    for id in &not_speech {
        if let Some(event) = prepared.events.iter().find(|e| &e.id == id) {
            if event.kind == "thought" && first_person_singular(&event.text) {
                anyhow::bail!(
                    "source {id:?} is a first-person thought ({:?}) and cannot be retracted as \
                     narration — give it the character thinking it, never Narrator. A narrator aside \
                     to the reader is `we`/`you`-voiced, not `I`",
                    crate::util::head_chars(&event.text, 80)
                );
            }
        }
    }
    // Narration is mechanical, so it is written here rather than read from the
    // answer: the prompt never asks about these ids, and a model that answers
    // anyway cannot change who speaks prose.
    for event in prepared.events.iter().filter(|e| !is_voiced_kind(&e.kind)) {
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
                "attribution dropped source event {:?} — it is dialogue or a thought and reads {:?}; give it a speaker",
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
            "dialogue" | "thought" if speaker == "Narrator" => anyhow::bail!(
                "source {:?} is {kind} but attribution assigns Narrator; use a canonical character or the reserved `Anonymous` — a hail nobody on cast is tagged saying belongs to the crowd, not to Narrator, and a thought belongs to its thinker, not to the narrator. Only a span that is not somebody talking or thinking (a quoted title, a term, a panel label, a third-person sentence about its own subject) may be Narrator, and it must also be listed in `not_speech`",
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
) -> Result<Value, Complaint> {
    // Everything here is the attribution answer's own doing, so the blame is
    // uniform: no amount of staging can fix a roster that names a stranger.
    let mine = |e: anyhow::Error| Complaint::new(Round::Attribution, e);
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "attribution is not valid JSON".to_string())
        .map_err(mine)?;
    normalize_attribution_metadata(&mut data, bible, prepared);
    // Unambiguous aliases are corrected here, before a single check runs, and
    // never excused afterwards: `validate_digest_identity` stays exactly as
    // strict as it was, it is just no longer handed a name that has one
    // legitimate canonical spelling. Announced, because a silent rewrite is a
    // bug of its own.
    for fix in canonicalize_aliases(&mut data, bible) {
        eprintln!("attribution alias corrected: {fix}");
    }
    for fix in complete_roster(&mut data, bible, prepared) {
        eprintln!("attribution roster completed: {fix}");
    }
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
    validate_context(&data, bible).map_err(mine)?;
    validate_title(&data).map_err(mine)?;
    if split {
        take_summary(&data).map_err(mine)?;
    }
    // The validated map is written back with the narration rows the preparer
    // owns, because staging reads `speakers` from this same object.
    let speakers = validate_attributions(&data, bible, prepared).map_err(mine)?;
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

/// Attach the already validated speaker map — and the thought marker — to
/// staging output. A speaker emitted or changed by the staging model is
/// ignored, and so is a `kind` it invented; identity belongs to pass one.
///
/// `thoughts` is the id set the preparer carved as thoughts minus the ones the
/// attribution pass retracted (`not_speech`), so the marker the mixer keys the
/// thought sound on always agrees with the kind the source gate checks.
fn attach_fixed_speakers(
    data: &mut Value,
    speakers: &BTreeMap<String, String>,
    thoughts: &HashSet<String>,
) -> Result<()> {
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
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: missing source_id"))?;
        let speaker = speakers
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("segment {i}: source id {id:?} has no attribution"))?;
        segment["speaker"] = Value::String(speaker.clone());
        if thoughts.contains(&id) {
            segment["kind"] = Value::String("thought".into());
        } else if let Some(object) = segment.as_object_mut() {
            object.remove("kind");
        }
    }
    Ok(())
}

/// Lift the pack's declared thought sound into a sound item at each thought's
/// seam.
///
/// A thought is a line like any other to the renderer, so the marker plus this
/// lift is what gives it a sound without teaching any layer downstream about
/// thoughts: the item is the same sibling `{"sound": …}` the staging pass
/// writes (`expand_sound_fields`), placed after the event's **last** segment —
/// a thought split for a TTS run fires one stinger, at its end — and skipped
/// when a `sound` the answer wrote already sits there, so a sting the analyzer
/// chose is never doubled. A `stop` is not a replacement: it closes a running
/// bed, and the declared thought sound still fires beside it.
///
/// `sound` comes from `scene-map.json`'s `thought.sound` and is checked against
/// the inject pool when the vocabulary is loaded, so a name that reaches here
/// is one `validate_injects` will accept.
fn lift_thought_stingers(data: &mut Value, sound: Option<&str>) {
    let Some(sound) = sound.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) else {
        return;
    };
    let mut lifted: Vec<Value> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        let thought = !crate::util::is_sound_item(segment)
            && segment.get("kind").and_then(Value::as_str) == Some("thought");
        let source = segment.get("source_id").and_then(Value::as_str);
        let next = segments.get(i + 1);
        let continues = next.and_then(|n| n.get("source_id").and_then(Value::as_str)) == source;
        let start_already = next
            .filter(|n| crate::util::is_sound_item(n))
            .map(|n| n.get("sound").is_some())
            .unwrap_or(false);
        lifted.push(segment.clone());
        if thought && !continues && !start_already {
            lifted.push(json!({ "sound": sound }));
        }
    }
    *segments = lifted;
}

/// Parse the staging answer after attaching immutable speakers, then run the
/// ordinary script checks and the source-integrity gate.
fn parse_staged_script(
    raw: &str,
    bible: &Value,
    context: &Value,
    prepared: &PreparedChapter,
    vocab: &Vocabulary,
) -> Result<Value, Complaint> {
    // Two blames, and the split is the point. Most of this is the staging
    // answer's own doing, so a second ask is the remedy. But two checks read
    // the **cast** rather than the answer: if the attribution pass never gave
    // an event a speaker, the staging model was handed an event it had no
    // business staging, and no staging retry will invent the missing row.
    let mine = |e: anyhow::Error| Complaint::new(Round::Staging, e);
    let theirs = |e: anyhow::Error| Complaint::new(Round::Attribution, e);
    let cleaned = strip_fences(raw);
    let mut data = parse_json_repaired(cleaned)
        .with_context(|| "staging is not valid JSON".to_string())
        .map_err(mine)?;
    carry_forward_fields(&mut data, prepared);
    if let Some(segs) = data.get("segments").and_then(|s| s.as_array()).cloned() {
        data["segments"] = json!(expand_sound_fields(&segs).map_err(mine)?);
    }
    apply_tag_aliases(&mut data, &vocab.aliases);
    discard_unknown_effect_tags(&mut data, &vocab.effects);
    let not_speech = not_speech_ids(context).map_err(theirs)?;
    let thoughts: HashSet<String> = prepared
        .events
        .iter()
        .filter(|e| e.kind == "thought" && !not_speech.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();
    attach_fixed_speakers(
        &mut data,
        &fixed_speakers(context).map_err(theirs)?,
        &thoughts,
    )
    .map_err(theirs)?;
    collapse_redundant_sounds(&mut data);
    lift_thought_stingers(&mut data, vocab.thought_stinger.as_deref());
    validate_script(&data, bible, context, &vocab.palette).map_err(mine)?;
    validate_effect_tags(&data, &vocab.effects).map_err(mine)?;
    validate_injects(&data, &vocab.injects).map_err(mine)?;
    validate_source_alignment(&data, prepared, &not_speech).map_err(mine)?;
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

/// Rewrite the names in an attribution answer that are an **unambiguous
/// alias** of a bible character into that character's canonical name.
///
/// The gate refused these, and refusing was expensive in a way that did not
/// match the size of the mistake: ch51 came back with `Ninh Huyền Vũ` where the
/// bible's canonical name is `Huyền Vũ lão tổ`, the person was right, the
/// spelling was the chapter's own, and the whole round was thrown away and
/// re-asked for it. A name that has exactly one canonical spelling is not a
/// fact the model has to get right — it is one the bible already knows.
///
/// **This corrects the answer, it does not relax the check.** Nothing here
/// excuses an error: `validate_digest_identity` still refuses an unknown name,
/// still refuses an ambiguous one, and still refuses everything else it refused
/// before. The pass only rewrites what the bible can resolve on its own, which
/// is why an ambiguous form — two characters claiming the same alias — is left
/// exactly as it was, to fail with the message that names both owners.
///
/// `Narrator` and the reserved anonymous speakers are never touched: neither is
/// a bible character, and both are legitimate names in their own right.
fn canonicalize_aliases(data: &mut Value, bible: &Value) -> Vec<String> {
    let characters: Vec<Value> = bible
        .get("characters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if characters.is_empty() {
        return Vec::new();
    }
    // The names that are already canonical — from the bible, and from the
    // characters this very answer declares as new. A canonical name is never
    // rewritten, even when some other character lists it as an alias.
    let canonical: HashSet<String> = characters
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .chain(
            data.get("new_characters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string),
        )
        .collect();

    let resolve = |name: &str| -> Option<String> {
        if name == "Narrator" || is_anonymous_speaker(name) || canonical.contains(name) {
            return None;
        }
        let owners = alias_owners(&characters, name);
        if owners.len() != 1 {
            return None;
        }
        let one = owners.into_iter().next().expect("length just checked");
        (one != name).then_some(one)
    };

    let mut fixes = Vec::new();
    // `roster` is a join key, so it has to end up canonical *and* de-duplicated:
    // an answer that listed both spellings would otherwise carry one character
    // twice and be refused for a duplicate the rewrite itself created.
    if let Some(roster) = data.get_mut("roster").and_then(Value::as_array_mut) {
        for slot in roster.iter_mut() {
            let Some(name) = slot.as_str().map(str::to_string) else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("roster {name:?} -> {canon:?}"));
                *slot = json!(canon);
            }
        }
        let mut seen = HashSet::new();
        roster.retain(|v| match v.as_str() {
            Some(name) => seen.insert(name.to_string()),
            None => true,
        });
    }
    if let Some(speakers) = data.get_mut("speakers").and_then(Value::as_object_mut) {
        for slot in speakers.values_mut() {
            let Some(name) = slot.as_str().map(str::to_string) else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("speakers -> {canon:?} (was {name:?})"));
                *slot = json!(canon);
            }
        }
    }
    if let Some(mentions) = data.get_mut("mentions").and_then(Value::as_object_mut) {
        for slot in mentions.values_mut() {
            let Some(name) = slot.as_str().map(str::to_string) else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("mentions -> {canon:?} (was {name:?})"));
                *slot = json!(canon);
            }
        }
    }
    // A staged segment carries the speaker too, and it is checked against the
    // same roster. Missing this is how the first rewrite pass turned a refusal
    // into a different refusal: `segment 0: speaker "Ninh Huyền Vũ" is not a
    // canonical roster name`, with the roster already corrected above it.
    if let Some(segments) = data.get_mut("segments").and_then(Value::as_array_mut) {
        for segment in segments.iter_mut() {
            let Some(name) = segment
                .get("speaker")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if let Some(canon) = resolve(&name) {
                fixes.push(format!("segment speaker -> {canon:?} (was {name:?})"));
                segment["speaker"] = json!(canon);
            }
        }
    }
    fixes
}

/// Add the speakers an answer actually uses to the `roster` it returned.
///
/// The other half of the same class of mistake as [`canonicalize_aliases`].
/// `roster` is the join key the speaker map is resolved against, and the gate
/// refuses a line whose speaker is missing from it — but the roster is also
/// *derivable* from the answer: every name the answer assigns, plus `Narrator`
/// for the narration the preparer owns, is a name that speaks. ch51's `Được!`
/// line cost a whole round to a roster that simply forgot to list the
/// `Anonymous` it had just used.
///
/// **Only names the chapter can legitimately speak are added.** A name the
/// bible does not carry and the answer did not declare as new is left out, so it
/// still fails the canonical-name check with the message that says so: this
/// completes bookkeeping, it never admits a stranger, and it never rewrites
/// `speakers` itself.
fn complete_roster(data: &mut Value, bible: &Value, prepared: &PreparedChapter) -> Vec<String> {
    let legit: HashSet<String> = bible
        .get("characters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .chain(
            data.get("new_characters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string),
        )
        .collect();
    let speaks =
        |name: &str| name == "Narrator" || is_anonymous_speaker(name) || legit.contains(name);

    // Every name the answer puts on a line.
    let mut used: Vec<String> = data
        .get("speakers")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    // Narration is spoken by the preparer, not the answer, so its speaker is
    // never in `speakers` — but it is still in the chapter, and the gate checks
    // the roster for every event.
    if prepared.events.iter().any(|e| !is_voiced_kind(&e.kind)) {
        used.push("Narrator".to_string());
    }
    // A retracted span is read as narration too, which is the same `Narrator`.
    if !not_speech_ids(data).unwrap_or_default().is_empty() {
        used.push("Narrator".to_string());
    }

    let Some(roster) = data.get_mut("roster").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let mut have: HashSet<String> = roster
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut fixes = Vec::new();
    for name in used {
        if !speaks(&name) || !have.insert(name.clone()) {
            continue;
        }
        fixes.push(format!("roster += {name:?}"));
        roster.push(json!(name));
    }
    fixes
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
        let kind = effective_kind(event, not_speech);
        // The thought marker is code-attached (`attach_fixed_speakers`), so a
        // mismatch here is a hand edit or a stale script — and either way the
        // mixer would fire the pack's thought stinger on a spoken line, or stay
        // silent over a thought, without saying anything. Read through the same
        // effective kind as the speaker rule, so a retracted thought has to be
        // unmarked like any other narration.
        match (kind, segment.get("kind").and_then(Value::as_str)) {
            ("thought", Some("thought")) => {}
            ("thought", other) => anyhow::bail!(
                "source {id:?} is a thought but segment {i} carries kind {other:?} — a thought's \
                 segment must carry `\"kind\": \"thought\"`"
            ),
            (_, Some(marker)) => anyhow::bail!(
                "source {id:?} is {kind} but segment {i} carries kind {marker:?} — only a thought \
                 event's segment may carry `\"kind\": \"thought\"`"
            ),
            (_, None) => {}
        }
        match kind {
            "narration" if speaker != "Narrator" => anyhow::bail!(
                "source {id:?} is narration but segment {i} is assigned to {speaker:?}; narration must be Narrator"
            ),
            "dialogue" | "thought" if speaker == "Narrator" => anyhow::bail!(
                "source {id:?} is {kind} but segment {i} is assigned to Narrator"
            ),
            _ => {}
        }
        // A retracted span is narration now, so it is a narration segment and
        // must not carry a delimiter — same rule, reached through the same
        // effective kind the speaker check above used. A thought has no
        // delimiters by definition, so the same check covers it: quote marks
        // merged into either would be spoken aloud.
        if matches!(kind, "dialogue" | "thought")
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
mod tests;

#[cfg(test)]
mod quote_gate_tests;

#[cfg(test)]
mod repair_template_tests;
