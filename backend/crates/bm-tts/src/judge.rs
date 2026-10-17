//! The declared-sound judge: Laya, an ONNX decision model, asked whether one
//! declared non-verbal sound is the right one for a line.
//!
//! It is a claim scorer, not a classifier: a statement and two options go in,
//! and the logits at the option markers come back. So the question is posed as
//! the claim "the words <replaces> are <sound>", with both spellings of the line
//! — the source's own words and the line with the tag applied — in front of it.
//!
//! Nothing here knows a vocabulary. The caller supplies the sound's name.

use anyhow::{anyhow, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;
use tokenizers::Tokenizer;

/// The checkpoint's own budget (`rl_agent_config.json`: `max_len` 1024).
const MAX_LEN: usize = 1024;
/// `build_head` caps one option's description at 48 tokens.
const OPTION_TOKENS: usize = 48;
/// The model's `noul` question type: a statement, held or not.
const Q_NOUL: i64 = 2;
/// The model's `choice` question type, for the card's neutral-key workaround.
const Q_CHOICE: i64 = 0;
/// What the checkpoint's `build_sequence` prefixes a question with. Part of the
/// training format, not decoration: the same question without it scores
/// differently, so it stays exactly as measured.
const QUESTION_PREFIX: &str = "choice question: ";

/// The fixed option pair a `noul` question is scored against.
const NOUL_OPTIONS: [&str; 2] = [
    "false: no, the statement does not hold",
    "true: yes, the statement holds",
];

/// A loaded judge. One session; `p_yes` takes `&mut self` because ORT's `run`
/// does, so callers hold it behind one lock.
pub struct Judge {
    session: Session,
    tok: Tokenizer,
}

impl Judge {
    /// Load the graph and its tokenizer from `dir`, or `None` when the model is
    /// not installed — a box without it runs without a judge, by design.
    pub fn from_dir(dir: &Path) -> Result<Option<Judge>> {
        let mut graphs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "onnx"))
                    .collect()
            })
            .unwrap_or_default();
        graphs.sort();
        // Two graphs and no way to tell them apart: picking one would load
        // whichever `read_dir` happened to return, up to a gigabyte of it.
        if graphs.len() > 1 {
            return Err(anyhow!(
                "{} holds {} graphs ({}); the judge's directory must hold one",
                dir.display(),
                graphs.len(),
                graphs
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let Some(onnx) = graphs.pop() else {
            return Ok(None);
        };
        let tokenizer = dir.join("tokenizer/tokenizer.json");
        if !tokenizer.is_file() {
            return Ok(None);
        }
        let tok = Tokenizer::from_file(&tokenizer)
            .map_err(|e| anyhow!("loading {}: {e}", tokenizer.display()))?;
        // ort's builder errors are not `Send`, so anyhow cannot take them with `?`.
        let session = Session::builder()
            .map_err(|e| anyhow!("{e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(&onnx)
            .with_context(|| format!("loading {}", onnx.display()))?;
        Ok(Some(Judge { session, tok }))
    }

    /// `P(yes)`: the sound is the right one for those words. The shipped
    /// question — the claim is scored directly, which is the framing that
    /// carries signal (see `docs/` and the probe's `gate` mode for the numbers).
    pub fn p_yes(
        &mut self,
        raw: &str,
        tagged: &str,
        replaces: &str,
        sound: &str,
    ) -> Result<f32> {
        let instructions = claim(raw, tagged, replaces, sound);
        let p = self.score(&instructions, tagged, &NOUL_OPTIONS, Q_NOUL)?;
        Ok(p[1])
    }

    /// The vendor card's workaround for a weak `noul`: a neutral-key A/B choice
    /// with the yes/no moved into the descriptions. Kept because it is the
    /// framing to compare against — it answers "yes" to both options of a pair
    /// (see the probe's `gate` mode).
    pub fn p_yes_choice(
        &mut self,
        raw: &str,
        tagged: &str,
        replaces: &str,
        sound: &str,
    ) -> Result<f32> {
        let instructions = question(raw, tagged, replaces, sound);
        let options = [
            format!("A: yes, {sound} is the sound of those words"),
            format!("B: no, those words are not {sound}"),
        ];
        let p = self.score(&instructions, tagged, &options, Q_CHOICE)?;
        Ok(p[0])
    }

    /// The per-option probabilities for one question.
    fn score(
        &mut self,
        instructions: &str,
        state: &str,
        options: &[impl AsRef<str>],
        qcode: i64,
    ) -> Result<Vec<f32>> {
        let scrub = |s: &str| s.replace("<mask>", " ");
        let cls = self.special("<bos>")?;
        let sep = self.special("<eos>")?;
        let mask = self.special("<mask>")?;
        let encoded = self
            .tok
            .encode(format!("{QUESTION_PREFIX}{instructions}"), false)
            .map_err(|e| anyhow!("tokenizing the question: {e}"))?;
        let mut ids: Vec<i64> = vec![cls];
        ids.extend(encoded.get_ids().iter().map(|&t| i64::from(t)));
        ids.push(sep);
        let mut markers: Vec<i64> = Vec::with_capacity(options.len());
        for opt in options {
            let mut span = vec![mask];
            let mut toks = self.encode(&scrub(&format!(" {}", opt.as_ref())))?;
            toks.truncate(OPTION_TOKENS);
            span.extend(toks);
            markers.push(ids.len() as i64);
            ids.extend(span);
        }
        ids.push(sep);
        let state_ids = self.encode(&scrub(state))?;
        let room = MAX_LEN.saturating_sub(ids.len() + 1);
        let kept = state_ids.len().min(room);
        ids.extend(state_ids[..kept].iter().copied());
        ids.push(sep);

        let n = ids.len();
        let m = markers.len();
        let ids_t = Tensor::from_array((vec![1i64, n as i64], ids))?;
        let attn_t = Tensor::from_array((vec![1i64, n as i64], vec![1i64; n]))?;
        let pos_t = Tensor::from_array((vec![1i64, m as i64], markers))?;
        let mask_t = Tensor::from_array((vec![1i64, m as i64], vec![true; m]))?;
        let qtype_t = Tensor::from_array((vec![1i64], vec![qcode]))?;
        let outs = self
            .session
            .run(ort::inputs![
                "input_ids" => ids_t,
                "attention_mask" => attn_t,
                "marker_pos" => pos_t,
                "marker_mask" => mask_t,
                "qtype" => qtype_t,
            ])
            .context("running the judge")?;
        let (_, logits) = outs["logits"]
            .try_extract_tensor::<f32>()
            .context("the judge's logits are not f32")?;
        Ok(softmax(logits))
    }

    fn encode(&self, text: &str) -> Result<Vec<i64>> {
        Ok(self
            .tok
            .encode(text, false)
            .map_err(|e| anyhow!("tokenizing {text:?}: {e}"))?
            .get_ids()
            .iter()
            .map(|&t| i64::from(t))
            .collect())
    }

    fn special(&self, token: &str) -> Result<i64> {
        self.tok
            .token_to_id(token)
            .map(i64::from)
            .ok_or_else(|| anyhow!("{token} is not in the vocabulary"))
    }
}

/// The statement the shipped question scores: is this the sound of those words?
pub fn claim(raw: &str, tagged: &str, replaces: &str, sound: &str) -> String {
    format!(
        "In the Vietnamese line: {raw}  —  written instead as: {tagged}  —  \
         the words \"{replaces}\" are {sound}"
    )
}

/// The card's A/B phrasing of the same question.
pub fn question(raw: &str, tagged: &str, replaces: &str, sound: &str) -> String {
    format!(
        "In the Vietnamese line: {raw}  —  written instead as: {tagged}  —  \
         is the sound of {sound} correct for the words \"{replaces}\"?"
    )
}

fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.iter().map(|e| e / sum).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_carries_both_spellings_and_exactly_one_sound() {
        let c = claim(
            "Khụ khụ khụ, đành đợi vậy.",
            "[hắng giọng], đành đợi vậy.",
            "Khụ khụ khụ",
            "clearing of the throat",
        );
        assert!(c.contains("Khụ khụ khụ, đành đợi vậy."), "{c}");
        assert!(c.contains("[hắng giọng], đành đợi vậy."), "{c}");
        assert!(c.contains("are clearing of the throat"), "{c}");
        // A claim, not a question: `noul` scores a statement.
        assert!(!c.contains('?'), "{c}");
        assert!(question("a", "b", "a", "laughter").contains('?'));
    }
}
