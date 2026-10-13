use super::attribution::replace_or_miss;
use super::attribution::warn_missing_sections;
use super::json::parse_json_repaired;
use super::prepare::prepare_chapter;
use super::run::GCalls;
use super::*;
/// One structural problem the quote scan found, in coordinates an operator and
///   for spans longer than a quoted term ever gets, so `Tràng "cuồng phong bạo
///   vũ"` mid-sentence stays legal while a swallowed sentence does not.
#[derive(Debug, Clone, PartialEq, Eq)]

pub struct QuoteFinding {
    /// 1-based paragraph number, in the order the chapter reads.
    pub paragraph: usize,
    /// The paragraph (or, for a swallowed one, the head of the whole span).
    pub text: String,
    /// Which structural rule fired, as above.
    pub kind: &'static str,
}

/// The findings that say speech and narration are mis-split — the two a
/// proofread is asked for. They are hints for the prompt, never a verdict.
pub(crate) fn structural(finding: &QuoteFinding) -> bool {
    finding.kind == "swallowed paragraph" || finding.kind == "welded prose"
}

/// Scan a chapter for unbalanced quotation structure. Empty means clean.
pub fn quote_findings(text: &str) -> Vec<QuoteFinding> {
    let prepared = prepare_chapter(text);
    // `prepare_chapter` sanitizes internally and sanitation is idempotent, so
    let clean = crate::crawl::sanitize_chapter_text(text);
    let paragraph_of = |at: usize| {
        let before = &clean[..at.min(clean.len())];
        let mut n = before.lines().filter(|l| !l.trim().is_empty()).count();
        // The partial line the cursor sits in is not a completed paragraph —
        if !before.is_empty() && !before.ends_with('\n') {
            n -= 1;
        }
        n + 1
    };
    let mut findings = Vec::new();

    // The net check first: a delimiter still open at the end. Every event after
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
        // mark that opens **on the same line as the word before it**; a quote
        // that opens a paragraph stands on its own, whatever the paragraph
        // above it ended with.
        if event.at > 0 && event.text.chars().count() > 120 {
            let line_start = clean[..event.at].rfind('\n').map(|at| at + 1).unwrap_or(0);
            let before = clean[line_start..event.at].trim_end();
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

/// The text a chapter's digest reads: the chapter its proofread returned, when
/// there is one and it was written for this crawl.
pub(crate) fn effective_text(layout: &Layout, n: u32, original: &str) -> String {
    proofread(layout, n, original).unwrap_or_else(|| original.to_string())
}

/// Whether `n` still needs its proofread. The pass is a chapter's first prompt,
/// asked once, and the worker and the operator's own paste both go through this
/// one rule.
pub(crate) fn needs_proofread(layout: &Layout, n: u32, original: &str) -> bool {
    proofread(layout, n, original).is_none()
}

/// The accepted proofread for `n`, if one is on disk for **this** chapter text:
/// a re-crawled chapter is a new chapter, and an older proofread of it is not
/// read back over it.
fn proofread(layout: &Layout, n: u32, original: &str) -> Option<String> {
    let text = std::fs::read_to_string(repaired_txt(layout, n)).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(repaired_record(layout, n)).ok()?).ok()?;
    let hash = record.get("source").and_then(Value::as_str)?;
    (hash == text_hash(original)).then_some(text)
}

/// Where a proofread chapter is kept: beside the chapter, never over it.
pub(crate) fn repaired_txt(layout: &Layout, n: u32) -> PathBuf {
    layout.data().join(format!("ch{n:02}-repaired.txt"))
}

/// Where that proofread's own change list is kept, with the hash of the chapter
/// it was written against.
pub(crate) fn repaired_record(layout: &Layout, n: u32) -> PathBuf {
    layout.data().join(format!("ch{n:02}-repaired.json"))
}

/// The chapter text a proofread record is keyed by: the sanitized form, which is
/// what the prompt carries, so a chapter written back with different whitespace
/// still matches its own proofread.
fn text_hash(text: &str) -> String {
    let clean = crate::crawl::sanitize_chapter_text(text);
    let mut h = Sha256::new();
    h.update(b"bm-proofread-v1");
    h.update([0]);
    h.update(clean.as_bytes());
    format!("{:x}", h.finalize())
}

/// The proofread prompt for one chapter's text, rendered from the adapter's
/// template. The chapter goes in **sanitized**, because that is the text the
/// findings' paragraph numbers count and the text every later pass reads.
pub(crate) fn build_repair_prompt(layout: &Layout, text: &str, complaint: &str) -> Result<String> {
    let path = layout.repair_prompt();
    let template = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt template {}", path.display()))?;
    let mut missed: Vec<String> = Vec::new();
    let mut body = template;
    let chapter = crate::crawl::sanitize_chapter_text(text);
    replace_or_miss(&mut body, "{fault_line}", complaint, &mut missed);
    replace_or_miss(&mut body, "{chapter_text}", &chapter, &mut missed);
    warn_missing_sections("repair prompt", &missed);
    // Appended, never in the file: this is the shape code reads back, and the
    // words and the marks in it are the model's to decide.
    body.push_str(
        "\n---REPAIR OUTPUT CONTRACT---\nReturn ONE strict JSON object, never markdown or \
         commentary:\n{\"text\": \"the full corrected chapter text\", \"changes\": [\"one short \
         line per change\"]}\n\n`text` is the whole chapter and nothing but the chapter — no note to \
         the reader, no summary of what you did. `changes` names what you actually changed in \
         `text`; leave it empty when the chapter needed nothing. Nothing else is checked by \
         code: which marks and which words the chapter needs is your reading of it.\n",
    );
    Ok(body)
}

/// The gated proofread ladder, run once per chapter, only when the scan finds
/// What one proofread answer is worth. The ladder and the manual round judge by
/// the same rules, so the two cannot drift apart.
pub(crate) enum RepairVerdict {
    /// Accepted text plus the model's own change list, for the sidecar.
    Fixed(String, Value),
    /// The answer was not a chapter: no JSON, no `text`, or nothing in it.
    Reject(String),
}

/// The one thing code may say about a proofread: whether a chapter came back.
/// Which words and which marks belong where is semantic work, and a rail that
/// guessed at it would only ever refuse a correct answer: the prompt and the
/// model own the proofread, and the operator owns the review.
pub(crate) fn judge_repair(raw: &str) -> RepairVerdict {
    let Some(parsed) = parse_json_repaired(raw).ok() else {
        return RepairVerdict::Reject("no usable JSON".to_string());
    };
    let Some(fixed) = parsed
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return RepairVerdict::Reject("no text field".to_string());
    };
    if fixed.trim().is_empty() {
        return RepairVerdict::Reject("an empty chapter".to_string());
    }
    let changes = parsed.get("changes").cloned().unwrap_or(Value::Null);
    RepairVerdict::Fixed(fixed, changes)
}

/// The complaint the repair prompt carries: the scan's findings, or a note that
/// the scanner is satisfied and the pass must read for what it cannot see.
pub(crate) fn repair_complaint(findings: &[QuoteFinding]) -> String {
    if findings.is_empty() {
        return "the scanner found no mis-split; check the pairing yourself and return the \
                chapter unchanged if it is right"
            .to_string();
    }
    findings
        .iter()
        .map(|f| {
            format!(
                "  - {}: paragraph {}: {}",
                symptom(f.kind),
                f.paragraph,
                head_chars(&f.text, 120)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One finding as the model has to read it: the scan's name for the fault, and
/// the fault in the words of the rule it broke. A chapter came back untouched
/// from a complaint that named a symptom the model had no repair for.
fn symptom(kind: &str) -> &str {
    match kind {
        "swallowed paragraph" => {
            "swallowed paragraph — a speech runs over a paragraph break, and every paragraph of a \
             speech needs its own pair of marks, or the staging cannot tell the speaker from the \
             narrator"
        }
        "welded prose" => {
            "welded prose — narration runs straight into a speech with no mark between them"
        }
        "unclosed quote" => {
            "unclosed quote — a quotation mark is still open where the chapter ends"
        }
        other => other,
    }
}

/// Record an accepted proofread: the chapter that will be spoken, and beside it
/// the changes the model declared. The file is also how the rest of the pipeline
/// knows this chapter has had its pass, so it is written even when the model
/// returned the chapter unchanged.
///
/// `shown` is the chapter the prompt carried, whose sanitized form keys the
/// record: the same text the next call sees on disk.
pub(crate) fn keep_repaired(
    layout: &Layout,
    n: u32,
    shown: &str,
    fixed: &str,
    changes: &Value,
) -> String {
    let _ = atomic_write(&repaired_txt(layout, n), fixed);
    let record = json!({"source": text_hash(shown), "changes": changes});
    let _ = atomic_write(
        &repaired_record(layout, n),
        &serde_json::to_string_pretty(&record).unwrap_or_default(),
    );
    fixed.to_string()
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn repair_quotes(
    layout: &Layout,
    n: u32,
    text: &str,
    findings: &[QuoteFinding],
    analyzer: &str,
    settings: &Settings,
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<String> {
    let ask = |complaint: String| {
        build_repair_prompt(layout, text, &complaint).map_err(|e| {
            eprintln!("ch{n} repair pass unavailable: {e}");
            e
        })
    };
    // Spent from the chapter's budget like any other G, so the phrase pass's own
    // asks are the whole of it and a rate limit does not buy another one.
    async fn call(
        layout: &Layout,
        n: u32,
        analyzer: &str,
        settings: &Settings,
        prompt: &str,
    ) -> Result<String> {
        dump_raw(layout, "digest-repair", prompt);
        match generate(prompt, analyzer, settings).await {
            Ok((raw, _)) => Ok(raw),
            Err(GenError::RateLimited(m)) => {
                anyhow::bail!("ch{n} the analyzer is rate-limited ({m})")
            }
            Err(GenError::Fatal(e)) => anyhow::bail!("ch{n} the analyzer failed ({e})"),
        }
    }

    // The chapter with every finding named, then "that was not a chapter" twice
    // more. A proofread's quality is the model's call: the only answer worth
    // re-asking for is one that is not a chapter at all.
    let mut unusable = String::new();
    for attempt in 0..3 {
        let prompt = ask(if attempt == 0 {
            format!(
                "Proofread this chapter's speech quotation marks. What the scan found:\n{}",
                repair_complaint(findings)
            )
        } else {
            format!(
                "Your last output could not be used ({unusable}). Return the full corrected \
                 chapter again, as one strict JSON object."
            )
        })?;
        progress(
            0.02 + 0.01 * attempt as f32,
            format!("ch{n} proofreading the chapter's quotation structure"),
        );
        calls.spend("phrase")?;
        let raw = call(layout, n, analyzer, settings, &prompt).await?;
        match judge_repair(&raw) {
            RepairVerdict::Fixed(fixed, changes) => {
                let fixed = keep_repaired(layout, n, text, &fixed, &changes);
                progress(
                    0.03 + 0.01 * attempt as f32,
                    format!("ch{n} proofread accepted"),
                );
                return Ok(fixed);
            }
            RepairVerdict::Reject(why) => {
                eprintln!("ch{n} proofread answer unusable ({why}); re-asking");
                unusable = why;
            }
        }
    }
    anyhow::bail!("the model returned no usable chapter in 3 asks (last: {unusable})")
}

/// Whether a narration event ends by handing the floor to the speech that
pub(crate) fn hands_off_to_quote(text: &str) -> bool {
    text.trim_end().ends_with(':')
}

/// Whether a narration attributes a quote to somebody at all — a speech verb
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

pub(crate) fn attributes_speech(text: &str) -> bool {
    let folded: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let folded = format!(" {folded}");
    SPEECH_VERBS.iter().any(|verb| folded.contains(verb))
}
