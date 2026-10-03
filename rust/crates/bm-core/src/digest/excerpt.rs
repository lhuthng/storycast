use super::json::parse_json_repaired;
use super::json::strip_fences;
use super::parse::EXCERPT_CHARS;
use super::*;
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
pub(crate) fn previous_excerpts(layout: &Layout, n: u32) -> Option<String> {
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
pub(crate) fn content_language(layout: &Layout) -> String {
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
pub(crate) fn excerpt_rule(content_language: &str) -> String {
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
