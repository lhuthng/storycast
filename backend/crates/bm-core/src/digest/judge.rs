//! The declared-sound judge: the one check the code cannot make.
//!
//! The gate proves a declaration is *well-formed* — the key exists, the words
//! are in the line, the line is not narration. Whether the sound is the **right**
//! one for those words is a judgement about meaning, and the code does not know
//! meanings. So a model on the box is asked (the sidecar's `/judge`, see
//! `bm-tts::judge`) with both spellings of the line, and its answer is a dial:
//! `advise` records the doubt, `refuse` makes it a complaint the model must
//! answer.
//!
//! The dial exists because the model's answers overlap end to end — see the
//! threshold's own note in [`crate::config::JudgeSettings`]. Nothing here can
//! separate right from wrong; it can only decide how much doubt to act on.

use super::*;
use crate::config::JudgeSettings;
use crate::voices;
use std::time::Duration;

/// How long one verdict may take. The model is small and local, so a slow answer
/// is a broken box, not a slow one.
const JUDGE_TIMEOUT: Duration = Duration::from_secs(20);

/// One declaration, as the question puts it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declaration {
    /// The line as the source wrote it.
    pub(crate) raw: String,
    /// The same line with the tag applied where the sound stands.
    pub(crate) tagged: String,
    /// The exact words the tag stands for.
    pub(crate) replaces: String,
    /// What the engine calls the sound, e.g. `clearing of the throat`.
    pub(crate) sound: String,
}

/// What came of asking.
#[derive(Debug, Default)]
pub(crate) struct Judged {
    /// How many declarations the box answered about.
    pub(crate) asked: usize,
    /// One line per doubtful declaration, for the run's log.
    pub(crate) doubts: Vec<String>,
    /// Set only in `refuse` mode, for the first doubt below the threshold.
    pub(crate) complaint: Option<String>,
    /// Why nothing was asked, when nothing was: no judge on the box, or no
    /// answer. Never fatal — the digest proceeds.
    pub(crate) skipped: Option<String>,
}

/// Every declaration in a staged script, with the line it stands in. The words
/// and the tag are the engine's; the caller never invents either.
pub(crate) fn declarations(script: &Value, engine: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    let Some(segs) = script.get("segments").and_then(Value::as_array) else {
        return out;
    };
    for seg in segs {
        let text = seg.get("text").and_then(Value::as_str).unwrap_or_default();
        let Some(nv) = seg.get("nonverbal") else {
            continue;
        };
        let (Some(key), Some(replaces)) = (
            nv.get("key").and_then(Value::as_str),
            nv.get("replaces").and_then(Value::as_str),
        ) else {
            continue;
        };
        let Some(decl) = voices::nonverbals(engine).iter().find(|n| n.key == key) else {
            continue;
        };
        if replaces.is_empty() || !text.contains(replaces) {
            continue;
        }
        out.push(Declaration {
            raw: text.to_string(),
            tagged: text.replacen(replaces, decl.tag, 1),
            replaces: replaces.to_string(),
            sound: decl.gloss.to_string(),
        });
    }
    out
}

/// Ask the box about every declaration in a staged script.
///
/// A judge that is absent or unreachable answers nothing and the chapter goes
/// on: the check it adds is one the code could not make anyway, so losing it
/// must not cost a chapter.
pub(crate) async fn check(script: &Value, engine: &str, settings: &JudgeSettings) -> Judged {
    let mut out = Judged::default();
    if settings.mode == "off" {
        return out;
    }
    let decls = declarations(script, engine);
    if decls.is_empty() {
        return out;
    }
    let client = match reqwest::Client::builder().timeout(JUDGE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            out.skipped = Some(format!("could not build an HTTP client: {e}"));
            return out;
        }
    };
    for d in decls {
        let p_yes = match ask(&client, &settings.url, &d).await {
            Ok(p) => p,
            // One transport failure ends the asking, not the chapter. A verdict
            // already in hand stands: it came from a real answer.
            Err(why) => {
                out.skipped = Some(why);
                break;
            }
        };
        out.asked += 1;
        if !is_doubt(p_yes, settings.threshold) {
            continue;
        }
        let doubt = format!(
            "judge: P(yes) {p_yes:.2} for {} over {:?}",
            d.sound, d.replaces
        );
        out.doubts.push(doubt);
        if settings.mode == "refuse" && out.complaint.is_none() {
            out.complaint = Some(complaint(&d, p_yes));
        }
    }
    out
}

/// A verdict below the dial. The dial is not a separator — the judge's own
/// answers overlap end to end — so this answers "how much doubt acts", not
/// "is this wrong".
pub(crate) fn is_doubt(p_yes: f32, threshold: f32) -> bool {
    p_yes < threshold
}

/// What the model is told when its declaration is refused: the sound, the
/// score, and what to do about it.
pub(crate) fn complaint(d: &Declaration, p_yes: f32) -> String {
    format!(
        "the declared sound is doubtful: the judge scores whether that line \
         really sounds like {} and puts it at {p_yes:.2}. The code cannot \
         check this. Keep the declaration only if the words really are that \
         sound; otherwise drop it and let the line read as written.",
        d.sound
    )
}

/// One verdict: `P(yes)`, or why the box did not give one.
async fn ask(
    client: &reqwest::Client,
    url: &str,
    d: &Declaration,
) -> std::result::Result<f32, String> {
    let resp = client
        .post(url)
        .json(&json!({
            "raw": d.raw,
            "tagged": d.tagged,
            "replaces": d.replaces,
            "sound": d.sound,
        }))
        .send()
        .await
        .map_err(|e| format!("the judge is not answering at {url} ({e})"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("no judge on this box ({url})"));
    }
    if !status.is_success() {
        return Err(format!(
            "the judge answered {status}: {}",
            head_chars(&body, 200)
        ));
    }
    let doc: Value = serde_json::from_str(&body)
        .map_err(|e| format!("the judge's answer is not JSON: {e}"))?;
    doc.get("p_yes")
        .and_then(Value::as_f64)
        .map(|p| p as f32)
        .ok_or_else(|| format!("the judge's answer carries no p_yes: {}", head_chars(&body, 200)))
}
