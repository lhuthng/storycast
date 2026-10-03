use super::chunks::{
    classify_gap, merge_short_chunks, pack_sentences_into_chunks, split_sentences, Chunks,
};
use super::*;
use anyhow::{Context, Result};
use sea_g2p_rs::g2p::G2PEngine;
use sea_g2p_rs::lang::vi::Normalizer;
use sea_g2p_rs::punc::apply_punc_norm;

/// Normalize and phonemize one string, the way `SEAPipeline.run` does: `punc_norm`
pub struct FrontEnd {
    normalizer: Normalizer,
    g2p: G2PEngine,
}

impl FrontEnd {
    /// `dict_path` is the 60 MB `sea_g2p.bin`. The normalizer is deliberately
    pub fn new(dict_path: &str) -> Result<FrontEnd> {
        Ok(FrontEnd {
            normalizer: Normalizer::new("vi", None),
            g2p: G2PEngine::new(dict_path).context("loading the sea-g2p dictionary")?,
        })
    }

    /// Normalize then phonemize. `punc_norm` is applied at the normalizer only.
    pub fn phonemize(&self, text: &str, punc_norm: bool) -> String {
        let normalized = self.normalizer.normalize(text, punc_norm);
        self.g2p.phonemize(&normalized)
    }

    /// Phonemize while keeping inline non-verbal cues as emotion tokens.
    pub fn phonemize_with_emotions(&self, text: &str) -> String {
        if !text.contains('[') && !text.contains("<|emotion_") {
            return self.phonemize(text, true);
        }
        let parts = split_emotions(text);
        let mut out = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i % 2 == 1 {
                let token = emotion_token_k(part).map(|t| t.to_string()).or_else(|| {
                    if part.starts_with("<|emotion_") && is_emotion_span(part) {
                        Some(part.trim().to_string())
                    } else {
                        None
                    }
                });
                if let Some(t) = token {
                    if out.is_empty() {
                        out = t;
                    } else {
                        out.push(' ');
                        out.push_str(&t);
                    }
                    continue;
                }
            }
            let ph = if part.trim().is_empty() {
                String::new()
            } else {
                self.phonemize(part, false)
            };
            if ph.is_empty() {
                continue;
            }
            if out.is_empty() {
                out = ph;
            } else if ph
                .chars()
                .next()
                .is_some_and(|c| ATTACHING_PUNCT.contains(&c))
            {
                out.push_str(&ph);
            } else {
                out.push(' ');
                out.push_str(&ph);
            }
        }
        apply_punc_norm(&out)
    }

    /// The chunker: `(chunks, gaps)`, where `gaps[i]` describes the boundary
    pub fn chunks(&self, text: &str, max_chars: usize, min_chunk_chars: usize) -> Chunks {
        if text.is_empty() {
            return Chunks::default();
        }
        let keep_cues = text.contains('[') || text.contains("<|emotion_");
        let mut chunks: Vec<String> = Vec::new();
        let mut gaps: Vec<String> = Vec::new();

        for sentences in self.normalized_sentences_by_para(text, keep_cues) {
            let para_chunks = pack_sentences_into_chunks(&sentences, max_chars);
            if para_chunks.is_empty() {
                continue;
            }
            if !chunks.is_empty() {
                gaps.push("para".into());
            }
            for (j, ch) in para_chunks.into_iter().enumerate() {
                if j > 0 {
                    gaps.push("sentence".into());
                }
                chunks.push(ch);
            }
        }

        chunks = chunks.iter().map(|c| apply_punc_norm(c)).collect();
        // A `para` boundary stays a paragraph; the rest are re-read from the
        gaps = gaps
            .iter()
            .enumerate()
            .map(|(i, g)| {
                if g == "para" {
                    "para".to_string()
                } else {
                    classify_gap(&chunks[i]).to_string()
                }
            })
            .collect();

        merge_short_chunks(chunks, gaps, min_chunk_chars)
    }

    /// One synthesis chunk per normalized sentence.
    pub fn chunks_sentence_level(&self, text: &str) -> Chunks {
        if text.is_empty() {
            return Chunks::default();
        }
        let keep_cues = text.contains('[') || text.contains("<|emotion_");
        let paragraphs = self.normalized_sentences_by_para(text, keep_cues);
        let (mut chunks, mut gaps) = sentence_chunks(paragraphs);
        chunks = chunks.iter().map(|c| apply_punc_norm(c)).collect();
        gaps = gaps
            .iter()
            .enumerate()
            .map(|(i, gap)| {
                if gap == "para" {
                    "para".to_string()
                } else {
                    classify_gap(&chunks[i]).to_string()
                }
            })
            .collect();
        Chunks { chunks, gaps }
    }

    /// Raw text to paragraphs of normalized sentences.
    fn normalized_sentences_by_para(&self, text: &str, keep_cues: bool) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for para in text.split(['\r', '\n']).filter(|p| !p.trim().is_empty()) {
            let sentences = split_sentences(para);
            if sentences.is_empty() {
                continue;
            }
            if keep_cues {
                out.push(
                    sentences
                        .iter()
                        .map(|s| self.normalize_sentence_keep_cues(s))
                        .collect(),
                );
            } else {
                out.push(
                    sentences
                        .iter()
                        .map(|s| self.normalizer.normalize(s, false))
                        .collect(),
                );
            }
        }
        out
    }

    /// Normalize one sentence, keeping inline cues as tokens.
    fn normalize_sentence_keep_cues(&self, sentence: &str) -> String {
        if !sentence.contains('[') && !sentence.contains("<|emotion_") {
            return self.normalizer.normalize(sentence, false);
        }
        let parts = split_emotions(sentence);
        let mut kept: Vec<String> = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if i % 2 == 1 {
                let tok = emotion_token_k(part).map(|t| t.to_string()).or_else(|| {
                    if part.starts_with("<|emotion_") {
                        Some(part.trim().to_string())
                    } else {
                        None
                    }
                });
                kept.push(tok.unwrap_or_else(|| part.clone()));
            } else if !part.trim().is_empty() {
                kept.push(self.normalizer.normalize(part, false));
            }
        }
        kept.into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}
