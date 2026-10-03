use super::json::parse_json_repaired;
use super::json::strip_fences;
use super::parse::EXCERPT_CHARS;
use super::*;
/// The previous chapters' excerpts chapter `n` is fed, newest first, each
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
/// ("an adapter has one language, and it is both the source's and the
/// target's"), so that is what the wording follows; an adapter that claims
pub(crate) fn content_language(layout: &Layout) -> String {
    crate::adapter::in_force(layout)
        .ok()
        .flatten()
        .map(|m| m.language.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "the chapter's own language".into())
}

/// The excerpt instruction, verbatim.
pub(crate) fn excerpt_rule(content_language: &str) -> String {
    format!(
        "2-4 sentences in {content_language} on the state this chapter ENDS in: who is \
         present, identity reveals (X is Y), disguises, deaths, and any stranger the prose \
         still has not named — written for the NEXT chapter's analyzer, who has not seen \
         this chapter and resolves its cast against it. State, not plot."
    )
}

/// The **excerpt-only** prompt: the attribution pass's excerpt, asked for on its
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
pub fn parse_excerpt(raw: &str) -> Option<String> {
    let cleaned = strip_fences(raw);
    // `strip_fences` knows ```` ```json ```` and a trailing fence; a bare
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
