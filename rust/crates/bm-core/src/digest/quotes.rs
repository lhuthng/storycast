use super::attribution::replace_or_miss;
use super::attribution::warn_missing_sections;
use super::json::parse_json_repaired;
use super::prepare::prepare_chapter;
use super::run::GCalls;
use super::*;
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
pub(crate) fn effective_text(layout: &Layout, n: u32, original: &str) -> String {
    let sidecar = repaired_txt(layout, n);
    std::fs::read_to_string(&sidecar)
        .ok()
        .filter(|fixed| quote_findings(fixed).is_empty())
        .unwrap_or_else(|| original.to_string())
}

/// Where a repaired chapter is kept: beside the chapter, never over it.
pub(crate) fn repaired_txt(layout: &Layout, n: u32) -> PathBuf {
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
pub(crate) fn build_repair_prompt(layout: &Layout, text: &str, complaint: &str) -> Result<String> {
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
pub(crate) async fn repair_quotes(
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
pub(crate) fn strip_punctuation(text: &str) -> String {
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
pub(crate) fn hands_off_to_quote(text: &str) -> bool {
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

pub(crate) fn attributes_speech(text: &str) -> bool {
    let folded: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let folded = format!(" {folded}");
    SPEECH_VERBS.iter().any(|verb| folded.contains(verb))
}
