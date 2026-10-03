use super::attribution::Continuity;
use super::manual::Vocabulary;
use super::prompts::PreparedChapter;
use super::run::run_g;
use super::run::Fail;
use super::run::GCalls;
use super::run::Round;
use super::sound_fields::field_is_blank;
use super::sound_fields::open_beds;
use super::sound_fields::silent_design;
use super::sound_fields::sound_design_gap;
use super::sound_fields::unclosed_beds;
use super::*;
/// One part of a chapter, staged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Part {
    pub(crate) from: usize,
    pub(crate) to: usize,
    /// What this part established, in the attribution answer's own words. It is
    pub(crate) summary: String,
    pub(crate) context: Value,
    pub(crate) script: Value,
}

/// The on-disk shape of a half-staged chapter.
#[derive(Serialize, Deserialize)]
struct StoredParts {
    key: String,
    parts: Vec<Part>,
}

/// The parts of one chapter that are already staged, and the file that survives
pub(crate) struct Parts {
    pub(crate) done: Vec<Part>,
    pub(crate) path: PathBuf,
    pub(crate) key: String,
    /// Whether there is a boundary to resume from at all. A one-window chapter
    pub(crate) store: bool,
}

impl Parts {
    /// Open the checkpoint for one chapter, keeping the leading run of stored
    pub(crate) fn open(
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

    pub(crate) fn len(&self) -> usize {
        self.done.len()
    }

    /// Every finished part's summary, oldest first: the `PLOT SO FAR` the next
    pub(crate) fn summaries(&self) -> Vec<String> {
        self.done.iter().map(|p| p.summary.clone()).collect()
    }

    pub(crate) fn push(&mut self, part: Part) -> Result<()> {
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
    pub(crate) fn clear(&self) {
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
            _ => break,
        }
    }
    kept
}

/// What a stored part has to match to be reusable: the chapter text, the bible
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
pub(crate) fn part_of(index: usize, total: usize) -> Option<(usize, usize)> {
    (total > 1).then_some((index + 1, total))
}

/// `part 2/5: ` when a message is about one part of a split chapter, and nothing
pub(crate) fn part_prefix(part: Option<(usize, usize)>) -> String {
    match part {
        None => String::new(),
        Some((index, total)) => format!("part {index}/{total}: "),
    }
}

/// `-part2of5` for a dump file's name, and nothing when the chapter did not
pub(crate) fn part_suffix(part: Option<(usize, usize)>) -> String {
    match part {
        None => String::new(),
        Some((index, total)) => format!("-part{index}of{total}"),
    }
}

/// The progress line for one round of one part. A one-window chapter's line is
pub(crate) fn round_label(
    n: u32,
    analyzer: &str,
    round: &str,
    part: Option<(usize, usize)>,
) -> String {
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
pub(crate) fn window_text(prepared: &PreparedChapter, w: &Window) -> String {
    prepared.events[w.from..w.to]
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plan as one line: what this chapter costs, and why.
pub(crate) fn plan_line(
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
pub(crate) fn plan_detail(windows: &[Window], prepared: &PreparedChapter) -> String {
    let spans: Vec<String> = windows.iter().map(|w| part_span(prepared, w)).collect();
    format!("parts: {}", spans.join(", "))
}

/// The plan as the ledger sees it: one line for the chapter and one per part.
pub(crate) fn part_lines(
    windows: &[Window],
    prepared: &PreparedChapter,
    settings: &Settings,
) -> Vec<String> {
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
pub(crate) fn merge_scripts<'a>(scripts: impl IntoIterator<Item = &'a Value>) -> Value {
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
pub(crate) fn sound_gap(
    scripts: &[&Value],
    texts: &[&str],
    pool: &crate::audio_pool::ClipPool,
    what: &str,
) -> Option<(String, usize)> {
    // A chapter that did not split goes through [`sound_design_gap`] itself, in
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
pub(crate) fn merge_contexts(parts: &[Part]) -> (Value, Vec<String>) {
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
#[allow(clippy::too_many_arguments)]
/// Everything one part's G's share, so a G is a step plus a context rather than
pub(crate) struct PartCtx<'a> {
    pub(crate) layout: &'a Layout,
    pub(crate) n: u32,
    pub(crate) analyzer: &'a str,
    pub(crate) settings: &'a Settings,
    pub(crate) bible: &'a Value,
    pub(crate) vocab: &'a Vocabulary,
    pub(crate) slice: &'a PreparedChapter,
    pub(crate) continuity: Option<&'a Continuity<'a>>,
    pub(crate) part: Option<(usize, usize)>,
    pub(crate) progress: &'a mut (dyn FnMut(f32, String) + Send),
    /// The band this part spends, split between the two steps.
    pub(crate) from: f32,
    pub(crate) mid: f32,
    pub(crate) to: f32,
    /// How far the bar has been pushed, so a step that runs twice — because a
    pub(crate) hi: f32,
}

impl PartCtx<'_> {
    /// The band one step spends, clamped to what has already been shown.
    pub(crate) fn band(&self, round: Round) -> (f32, f32) {
        let (a, b) = match round {
            Round::Attribution => (self.from, self.mid),
            Round::Staging => (self.mid, self.to),
        };
        (a.max(self.hi), b.max(self.hi))
    }

    pub(crate) fn report(&mut self, at: f32, msg: String) {
        self.hi = self.hi.max(at);
        (self.progress)(self.hi, msg);
    }
}

/// Ask the analyzer to stage one window, running each step as a G and handing
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_part(
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
