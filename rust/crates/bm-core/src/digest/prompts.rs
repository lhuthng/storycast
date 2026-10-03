use super::attribution::apply_continuity;
use super::attribution::replace_or_miss;
use super::attribution::replace_prompt_section;
use super::attribution::warn_missing_sections;
use super::attribution::Continuity;
use super::attribution::Pass;
use super::*;
/// Read the scene map, refusing a map that declares no music palette: without
pub(crate) fn load_map(layout: &Layout) -> Result<crate::ambience::SceneMap> {
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
pub fn build_prompt(layout: &Layout, bible: &Value, chapter_text: &str) -> Result<String> {
    let path = layout.prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    Ok(template
        .replace("{bible_json}", &bible_context(bible))
        .replace("{chapter_text}", chapter_text))
}

/// What the context pass found, rendered for the script pass: the cast it must
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
fn render_nonverbal(body: &mut String, engine: &str, missed: &mut Vec<String>) {
    // The declaration API, not a name test: whichever engine is bound answers
    let tags = crate::voices::nonverbals(engine);
    if tags.is_empty() {
        if !replace_prompt_section(body, NONVERBAL_RULE, MUSIC_RULE, "") {
            missed.push("rule 7 (non-verbal)".into());
        }
        // Backstop for a template numbered differently: a placeholder that
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedEvent {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) text: String,
    pub(crate) at: usize,
    pub(crate) end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedChapter {
    pub(crate) events: Vec<PreparedEvent>,
    /// The machine-readable form placed in the attribution and staging prompts.
    pub(crate) prompt_json: String,
    /// Where a quote delimiter was still open when the text ran out: a byte
    /// `"` upstream is read start-to-finish in a single voice — the mirror of
    /// the no-quotes case below, and just as silent.
    ///
    /// An offset, not a line number, because the sanitized text is not the file
    /// the operator has open: `sanitize_chapter_text` drops blank lines and
    /// joins paragraphs with a blank one, so a line counted here is not a line
    /// they can go to. [`quote_fault`] turns this into something they can.
    pub(crate) unbalanced_at: Option<usize>,
}

impl PreparedChapter {
    /// How many events are dialogue, as decided by the quote delimiters alone.
    pub(crate) fn dialogue_count(&self) -> usize {
        self.events.iter().filter(|e| e.kind == "dialogue").count()
    }

    /// How many events are thoughts carved out of narration. Counted apart from
    /// dialogue: a thought has no delimiters to check a crawler against, and
    /// counting it as speech would mask the one-voice warning below.
    pub(crate) fn thought_count(&self) -> usize {
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
    pub(crate) fn split_summary(&self) -> String {
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

pub(crate) fn prepared_event(
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

/// Build the audio-staging pass. Speaker assignment is supplied as immutable
pub(crate) fn build_staging_prompt(
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
    body = body.replace("{mood_palette}", &mood_palette);
    // `{effect_tags}` is deprecated: the effect layer reads `scene` labels, and
    body = body.replace("{effect_tags}", &effects);
    // Same for `{scene_words}`, and for the same reason plus a second: this path
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
