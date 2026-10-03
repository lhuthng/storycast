use super::excerpt::content_language;
use super::excerpt::excerpt_rule;
use super::prompts::PreparedChapter;
use super::quotes::attributes_speech;
use super::quotes::hands_off_to_quote;
use super::*;
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
pub(crate) fn attribution_view(prepared: &PreparedChapter) -> String {
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
pub(crate) fn replace_prompt_section(
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
pub(crate) fn replace_or_miss(
    body: &mut String,
    needle: &str,
    replacement: &str,
    missed: &mut Vec<String>,
) {
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
pub(crate) fn warn_missing_sections(which: &str, missed: &[String]) {
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
pub(crate) enum Pass {
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
pub(crate) struct Continuity<'a> {
    /// 0-based window index.
    pub(crate) index: usize,
    pub(crate) total: usize,
    /// Every earlier part's summary, oldest first. Empty for the first part,
    /// which is the only part with nothing behind it.
    pub(crate) plot: &'a [String],
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
pub(crate) fn apply_continuity(body: &mut String, continuity: Option<&Continuity>, pass: Pass) {
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
pub(crate) fn build_attribution_prompt(
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
