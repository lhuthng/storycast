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
    enum Verdict {
        /// Accepted text plus the model's own change list, for the sidecar.
        Fixed(String, Value),
        /// The answer was unusable (no JSON) or a rewrite: stop, fall back.
        Reject(String),
        /// Words untouched, but a structural gate still fires: the complaint
        Gated(String),
    }
    // ...and the one place every answer is judged, so the ladder cannot drift
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
pub(crate) fn strip_punctuation(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
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
