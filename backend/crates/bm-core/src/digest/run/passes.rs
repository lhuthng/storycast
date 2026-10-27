//! The chapter's two whole-chapter passes: the proofread every later step
//! reads, and the sound design that runs once over every staged line.

use super::*;

/// How many tries the sound gate gets inside one pass: one answer plus repairs.
/// Past that, a refusal that has not moved gets settled without the model.
pub(crate) const SOUND_TRIES: usize = 5;

/// The chapter's sound pass: one G over every staged line, then repairs while a
/// gate refuses the answer. Sound design is chapter-wide, so this runs once for
/// the whole chapter rather than per part.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_sound(
    layout: &Layout,
    n: u32,
    analyzer: &str,
    settings: &Settings,
    context: &Value,
    staged: &Value,
    chapter_text: &str,
    vocab: &Vocabulary,
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<Vec<Value>> {
    let mut segments = staged
        .get("segments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The sound pass owns these now: drop whatever a staging answer left on a
    // line, so a stray field cannot survive the pass that replaces it.
    for line in segments.iter_mut() {
        if let Some(object) = line.as_object_mut() {
            for key in ["scene", "music", "sound_after", "stop_after"] {
                object.remove(key);
            }
        }
    }
    let mut complaint: Option<String> = None;
    let mut drift = 0usize;
    for attempt in 0..SOUND_TRIES {
        let prompt = build_sound_prompt(layout, context, &segments)?;
        // A G like any other: it spends from the same chapter budget, so a
        // chapter cannot buy its way past a gate with extra calls.
        calls.spend(Round::Sound.as_str())?;
        let raw = if attempt == 0 {
            generate_retrying(&prompt, analyzer, settings, progress, 0.88, 0.96).await?
        } else {
            repair_once(
                &prompt,
                &anyhow::anyhow!(complaint.clone().unwrap_or_default()),
                analyzer,
                settings,
            )
            .await?
        };
        let dump = if attempt == 0 {
            "digest-sound"
        } else {
            "digest-sound-retry"
        };
        dump_raw(layout, dump, &raw);
        match parse_sound_script(&raw, &segments, chapter_text, vocab) {
            Ok(lines) => return Ok(lines),
            Err(why) => {
                // The same refusal twice in a row is a seam drifting, not a new
                // opinion. The count only moves when the refusal moves.
                drift = if complaint.as_deref() == Some(why.as_str()) {
                    drift + 1
                } else {
                    1
                };
                complaint = Some(why);
                if attempt + 1 < SOUND_TRIES {
                    progress(
                        0.96,
                        format!(
                            "digest ch{n} sound gate said {c}, asking for one repair",
                            c = complaint.as_deref().unwrap_or_default()
                        ),
                    );
                    continue;
                }
                // The same drift through every try means the model cannot see
                // what its own seam opened: drop that bed (the opening of one
                // still open, or one that keeps running past the cap, whole)
                // and let the next gates judge the remainder, instead of
                // failing a chapter the gate can no longer teach anything.
                if drift == SOUND_TRIES {
                    match trim_open_beds(
                        &raw,
                        &segments,
                        chapter_text,
                        vocab,
                        complaint.as_deref().unwrap_or_default(),
                    ) {
                        Ok(lines) => {
                            progress(
                                0.96,
                                format!(
                                    "digest ch{n} sound gate said the same thing \
                                     {SOUND_TRIES} times; settled the drifting bed(s) and \
                                     judged the remaining gates"
                                ),
                            );
                            return Ok(lines);
                        }
                        // The next gate refused the trimmed answer, and that
                        // refusal is the one worth reporting.
                        Err(trim_why) => complaint = Some(trim_why),
                    }
                }
                let dump = layout.data().join(".last-analyze-raw.json");
                let _ = atomic_write(&dump, &raw);
                anyhow::bail!(
                    "digest sound invalid ({}); raw saved to {}",
                    complaint.as_deref().unwrap_or_default(),
                    dump.display()
                );
            }
        }
    }
    unreachable!("run_sound leaves on an accepted answer or a refusal")
}

/// The chapter's proofread: the text every later pass reads. Asked once at the
/// start of a digest; a retry after a later step failed keeps it, and no step
/// may ask for it again.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn proofread_text(
    layout: &Layout,
    n: u32,
    original: &str,
    analyzer: &str,
    settings: &Settings,
    calls: &mut GCalls,
    progress: &mut (dyn FnMut(f32, String) + Send),
) -> Result<String> {
    // The proofread is the chapter's first prompt and it runs on every new
    // digest, faulted or not: a satisfied scan is not a clean text, and every
    // step after this one reads what this pass returns. A retry after a later
    // step failed keeps this text (see `resume`); a new digest does not.
    let text = original.to_string();
    let findings = quote_findings(&text);
    let faulted = findings.iter().any(structural);
    // One semantic verdict per repair attempt can follow the phrase asks.
    if settings.digest.repair_judge.mode != "off" {
        calls.left += 3;
    }
    match repair_quotes(
        layout, n, &text, &findings, analyzer, settings, calls, progress,
    )
    .await
    {
        Ok(fixed) => Ok(fixed),
        // Nothing the scan can fault, so nothing to stop the chapter for: a
        // model that answers in prose instead of JSON had nothing to answer.
        Err(e) if !faulted => {
            eprintln!("ch{n} proofread returned nothing usable ({e:#}); the text stands");
            progress(0.02, format!("ch{n} proofread: nothing to change"));
            Ok(text)
        }
        Err(e) => Err(e.context(format!(
            "ch{n} the proofread could not put the quotation structure right ({} \
             finding(s)); the chapter is not digested",
            findings.len()
        ))),
    }
}
