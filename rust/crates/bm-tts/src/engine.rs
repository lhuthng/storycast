//! The generator: prompt build, prefill, and the per-frame decode loop.

use crate::framecap::max_expected_frames;
use crate::npz::{read_npz, Array};
use crate::sample::{Rng, Sampling, DEFAULT_REP_WINDOW};
use anyhow::{anyhow, bail, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::{DynValue, Tensor};
use serde::Deserialize;
use std::path::Path;
use tokenizers::Tokenizer;

/// `config.json`, narrowed to what the generator reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub n_vq: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    #[serde(default = "one")]
    pub local_num_hidden_layers: usize,
    #[serde(default = "eight")]
    pub local_num_attention_heads: usize,
    pub audio_pad_token_id: i64,
    pub text_prompt_start_token_id: i64,
    pub text_prompt_end_token_id: i64,
    pub speech_generation_start_token_id: i64,
    pub speech_generation_end_token_id: i64,
    pub audio_ref_slot_token_id: i64,
    pub text_vocab_size: usize,
    #[serde(default = "sixteen")]
    pub default_style_token_id: i64,
    #[serde(default)]
    pub use_speaker_embedding: bool,
    #[serde(default = "one_ninety_two")]
    pub speaker_embedding_dim: usize,
}

fn one() -> usize {
    1
}
fn eight() -> usize {
    8
}
fn sixteen() -> i64 {
    16
}
fn one_ninety_two() -> usize {
    192
}

/// The speaker projection: `Linear(192 -> H)` then `LayerNorm(H)`.
struct Xvec {
    w: Array, // (H, 192)
    b: Array, // (H,)
    ln_w: Array,
    ln_b: Array,
    ln_eps: f32,
}

/// What to generate. Borrowed rather than owned so a caller can keep a voice
pub struct Request<'a> {
    pub phonemes: &'a str,
    pub sampling: Sampling,
    pub max_new_frames: usize,
    /// Cap the frame count by what the phoneme string could plausibly need. The
    pub frame_cap: bool,
    pub speaker_emb: Option<&'a [f32]>,
    /// Use this anchor verbatim instead of projecting `speaker_emb`.
    pub anchor_override: Option<&'a [f32]>,
    /// Reference codes from enrollment, `(frames, n_vq)`. `None` for a preset
    pub ref_codes: Option<&'a [Vec<i64>]>,
}

impl<'a> Request<'a> {
    pub fn new(phonemes: &'a str) -> Self {
        Request {
            phonemes,
            sampling: Sampling::default(),
            max_new_frames: 300,
            frame_cap: true,
            speaker_emb: None,
            anchor_override: None,
            ref_codes: None,
        }
    }
}

/// The generated codes, `frames x n_vq`.
pub struct Frames {
    pub codes: Vec<Vec<i64>>,
    /// True when the loop stopped because it hit the ceiling rather than an
    pub hit_cap: bool,
    pub cap: usize,
}

pub struct Engine {
    pub cfg: Config,
    /// The intra-op thread count the sessions were opened with.
    intra: usize,
    hidden: usize,
    audio_vocab: usize,
    text_emb: Array,
    audio_emb: Array,
    xvec: Option<Xvec>,
    tokenizer: Tokenizer,
    sess_pre: Session,
    sess_dec: Session,
    sess_ac: Session,
}

impl Engine {
    /// Load the backbone from one directory: `config.json`, `tokenizer.json`,
    pub fn load(dir: &Path, threads: usize) -> Result<Engine> {
        let cfg: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .with_context(|| format!("reading {}/config.json", dir.display()))?,
        )
        .context("parsing config.json")?;

        let heads = read_npz(&dir.join("vieneu_v3_heads.npz"))?;
        let text_emb = heads
            .get("text_emb")
            .context("vieneu_v3_heads.npz has no text_emb")?
            .clone();
        let audio_emb = heads
            .get("audio_emb")
            .context("vieneu_v3_heads.npz has no audio_emb")?
            .clone();
        if text_emb.shape.len() != 2 || text_emb.shape[0] != cfg.text_vocab_size {
            bail!(
                "text_emb is {:?}, expected ({}, {})",
                text_emb.shape,
                cfg.text_vocab_size,
                cfg.hidden_size
            );
        }
        if audio_emb.shape != vec![cfg.n_vq, audio_emb.shape[1], cfg.hidden_size] {
            bail!(
                "audio_emb is {:?}, expected ({}, _, {})",
                audio_emb.shape,
                cfg.n_vq,
                cfg.hidden_size
            );
        }
        let audio_vocab = audio_emb.shape[1];

        let xvec = if cfg.use_speaker_embedding {
            match (heads.get("xvec_w"), heads.get("xvec_b")) {
                (Some(w), Some(b)) => Some(Xvec {
                    w: w.clone(),
                    b: b.clone(),
                    ln_w: heads
                        .get("xvec_ln_w")
                        .context("heads.npz has xvec_w but no xvec_ln_w")?
                        .clone(),
                    ln_b: heads
                        .get("xvec_ln_b")
                        .context("heads.npz has xvec_w but no xvec_ln_b")?
                        .clone(),
                    ln_eps: heads
                        .get("xvec_ln_eps")
                        .context("heads.npz has xvec_w but no xvec_ln_eps")?
                        .scalar(),
                }),
                _ => bail!(
                    "this model conditions on a speaker but heads.npz has no xvec_proj weights — \
                     re-export with the update model"
                ),
            }
        } else {
            None
        };

        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("loading tokenizer.json: {e}"))?;

        let intra = if threads > 0 {
            threads
        } else {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8);
            (cores / 2).clamp(1, 8)
        };
        let open = |name: &str| -> Result<Session> { open_session(&dir.join(name), intra) };

        Ok(Engine {
            intra,
            hidden: cfg.hidden_size,
            audio_vocab,
            text_emb,
            audio_emb,
            xvec,
            tokenizer,
            sess_pre: open("vieneu_prefill.onnx")?,
            sess_dec: open("vieneu_decode_step.onnx")?,
            sess_ac: open("vieneu_acoustic_cached.onnx")?,
            cfg,
        })
    }

    /// The intra-op thread count the sessions were actually opened with.
    pub fn intra_threads(&self) -> usize {
        self.intra
    }

    /// `192-d` x-vector to an `(H,)` anchor.
    pub fn speaker_anchor(&self, emb: Option<&[f32]>) -> Result<Option<Vec<f32>>> {
        if !self.cfg.use_speaker_embedding {
            return Ok(None);
        }
        let e = emb.context(
            "this model conditions on a speaker: pass a speaker embedding from the reference wav",
        )?;
        let x = self
            .xvec
            .as_ref()
            .context("heads.npz has no xvec_proj weights — re-export with the update model")?;
        if e.len() != x.w.shape[1] {
            bail!(
                "speaker embedding is {} values, expected {}",
                e.len(),
                x.w.shape[1]
            );
        }
        if !e.iter().any(|v| *v != 0.0) {
            bail!("speaker embedding is all-zero — not a valid anchor");
        }
        let h = self.hidden;
        let mut v = vec![0f32; h];
        for (i, slot) in v.iter_mut().enumerate() {
            let row = &x.w.data[i * e.len()..(i + 1) * e.len()];
            let mut acc = 0f32;
            for k in 0..e.len() {
                acc += row[k] * e[k];
            }
            *slot = acc + x.b.data[i];
        }
        // LayerNorm. Accumulated in f32, as NumPy does for a float32 array.
        let mean = v.iter().sum::<f32>() / h as f32;
        let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / h as f32;
        let inv = 1.0 / (var + x.ln_eps).sqrt();
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = (*slot - mean) * inv * x.ln_w.data[i] + x.ln_b.data[i];
        }
        Ok(Some(v))
    }

    /// `(T, n_vq+1)` rows to `(T, H)` embeddings.
    pub fn embed_rows(&self, rows: &[i64], t: usize, anchor: Option<&[f32]>) -> Vec<f32> {
        let n = self.cfg.n_vq + 1;
        let h = self.hidden;
        let mut emb = vec![0f32; t * h];
        for r in 0..t {
            let id = rows[r * n] as usize;
            emb[r * h..(r + 1) * h].copy_from_slice(&self.text_emb.data[id * h..(id + 1) * h]);
        }
        for ch in 0..self.cfg.n_vq {
            let base = ch * self.audio_vocab * h;
            for r in 0..t {
                let id = rows[r * n + ch + 1];
                if id == self.cfg.audio_pad_token_id {
                    continue;
                }
                let src = base + id as usize * h;
                for k in 0..h {
                    emb[r * h + k] += self.audio_emb.data[src + k];
                }
            }
        }
        if let Some(a) = anchor {
            for r in 0..t {
                for k in 0..h {
                    emb[r * h + k] += a[k];
                }
            }
        }
        emb
    }

    /// The prompt rows: `[style, <tps>, …phones…, <tpe>]` then the reference
    pub fn build_rows(
        &self,
        phonemes: &str,
        ref_codes: Option<&[Vec<i64>]>,
    ) -> Result<(Vec<i64>, usize)> {
        let enc = self
            .tokenizer
            .encode(phonemes, false)
            .map_err(|e| anyhow!("tokenizing phonemes: {e}"))?;
        let mut text_ids: Vec<i64> = Vec::with_capacity(enc.len() + 3);
        text_ids.push(self.cfg.default_style_token_id);
        text_ids.push(self.cfg.text_prompt_start_token_id);
        text_ids.extend(enc.get_ids().iter().map(|i| *i as i64));
        text_ids.push(self.cfg.text_prompt_end_token_id);

        let n = self.cfg.n_vq + 1;
        let t0 = text_ids.len();
        let extra = ref_codes.map(|r| r.len()).unwrap_or(0);
        let mut rows = vec![self.cfg.audio_pad_token_id; (t0 + extra) * n];
        for (i, id) in text_ids.iter().enumerate() {
            rows[i * n] = *id;
        }
        if let Some(rc) = ref_codes {
            for (j, codes) in rc.iter().enumerate() {
                if codes.len() != self.cfg.n_vq {
                    bail!(
                        "reference frame {j} has {} codes, expected {}",
                        codes.len(),
                        self.cfg.n_vq
                    );
                }
                let r = t0 + j;
                rows[r * n] = self.cfg.audio_ref_slot_token_id;
                rows[r * n + 1..r * n + 1 + self.cfg.n_vq].copy_from_slice(codes);
            }
        }
        Ok((rows, t0))
    }

    /// Generate one chunk's codes.
    pub fn generate(&mut self, req: &Request, rng: &mut Rng) -> Result<Frames> {
        let anchor = match req.anchor_override {
            Some(a) => Some(a.to_vec()),
            None => self.speaker_anchor(req.speaker_emb)?,
        };
        // The text-only length is not what the loop positions against; see below.
        let (rows, _text_len) = self.build_rows(req.phonemes, req.ref_codes)?;
        // `Tprompt` in the reference is `prompt_embeds.shape[1]` — the whole
        let tprompt = rows.len() / (self.cfg.n_vq + 1);
        let prompt = self.embed_rows(&rows, tprompt, anchor.as_deref());

        let cap = if req.frame_cap {
            req.max_new_frames.min(max_expected_frames(req.phonemes))
        } else {
            req.max_new_frames
        };
        if cap == 0 {
            bail!("frame ceiling is zero; the phoneme string is empty or the cap is misconfigured");
        }

        let h = self.hidden;
        let l = self.cfg.num_hidden_layers;
        let l_loc = self.cfg.local_num_hidden_layers;
        let n_vq = self.cfg.n_vq;
        let pad = self.cfg.audio_pad_token_id;

        // ── prefill ──────────────────────────────────────────────────────────
        let (hidden, mut past_k, mut past_v) = {
            let t = Tensor::from_array((vec![1i64, tprompt as i64, h as i64], prompt))
                .context("building inputs_embeds")?;
            let t_run = std::time::Instant::now();
            let mut outs = self
                .sess_pre
                .run(ort::inputs!["inputs_embeds" => t])
                .context("prefill")?;
            crate::stats::add(crate::stats::Kind::Prefill, t_run.elapsed());
            let hidden = outs.remove("hidden").context("prefill: no hidden")?;
            let mut pk = Vec::with_capacity(l);
            let mut pv = Vec::with_capacity(l);
            for i in 0..l {
                pk.push(
                    outs.remove(format!("present_k_{i}"))
                        .with_context(|| format!("prefill: no present_k_{i}"))?,
                );
                pv.push(
                    outs.remove(format!("present_v_{i}"))
                        .with_context(|| format!("prefill: no present_v_{i}"))?,
                );
            }
            (hidden, pk, pv)
        };
        // `h` for the first frame is the last prompt position — the last
        let mut cond = last_row(&hidden, tprompt)?;

        let mut hist = if (req.sampling.repetition_penalty - 1.0).abs() > 1e-9 {
            Some(crate::sample::RepetitionHistory::new(
                n_vq,
                DEFAULT_REP_WINDOW,
            ))
        } else {
            None
        };
        let mut codes: Vec<Vec<i64>> = Vec::with_capacity(cap);
        let mut hit_cap = true;
        for step in 0..cap {
            let frame = self.acoustic_frame(&cond, &req.sampling, hist.as_mut(), rng, l_loc)?;
            codes.push(frame.codes.clone());
            if frame.eos {
                hit_cap = false;
                break;
            }

            // Feed this frame back to advance the backbone one position.
            let mut slot = vec![pad; n_vq + 1];
            slot[0] = self.cfg.speech_generation_start_token_id;
            slot[1..].copy_from_slice(&frame.codes);
            let se = self.embed_rows(&slot, 1, anchor.as_deref());
            let (next_hidden, pk, pv) = {
                // `into_dyn` so every entry of the feed is the same type; the
                let emb = Tensor::from_array((vec![1i64, 1, h as i64], se))?.into_dyn();
                let pos =
                    Tensor::from_array((vec![1i64, 1], vec![(tprompt + step) as i64]))?.into_dyn();
                let mut feed: Vec<(String, &DynValue)> = vec![
                    ("inputs_embeds".into(), &emb),
                    ("position_ids".into(), &pos),
                ];
                for i in 0..l {
                    feed.push((format!("past_k_{i}"), &past_k[i]));
                    feed.push((format!("past_v_{i}"), &past_v[i]));
                }
                let t_run = std::time::Instant::now();
                let mut outs = self
                    .sess_dec
                    .run(feed)
                    .with_context(|| format!("decode step {step}"))?;
                crate::stats::add(crate::stats::Kind::Decode, t_run.elapsed());
                let hidden = outs.remove("hidden").context("decode: no hidden")?;
                let mut pk = Vec::with_capacity(l);
                let mut pv = Vec::with_capacity(l);
                for i in 0..l {
                    pk.push(
                        outs.remove(format!("present_k_{i}"))
                            .with_context(|| format!("decode: no present_k_{i}"))?,
                    );
                    pv.push(
                        outs.remove(format!("present_v_{i}"))
                            .with_context(|| format!("decode: no present_v_{i}"))?,
                    );
                }
                (hidden, pk, pv)
            };
            past_k = pk;
            past_v = pv;
            cond = first_row(&next_hidden)?;
        }

        Ok(Frames {
            codes,
            hit_cap,
            cap,
        })
    }

    /// One acoustic frame: `n_vq` codes, and whether the end-of-speech token won
    fn acoustic_frame(
        &mut self,
        cond: &[f32],
        s: &Sampling,
        hist: Option<&mut crate::sample::RepetitionHistory>,
        rng: &mut Rng,
        l_loc: usize,
    ) -> Result<Frame> {
        let h = self.hidden;
        let n_vq = self.cfg.n_vq;
        let mut hist = hist;

        // The two-token opening: the backbone's last hidden state, then the
        let mut tok = Vec::with_capacity(2 * h);
        tok.extend_from_slice(cond);
        tok.extend_from_slice(
            &self.text_emb.data[self.cfg.speech_generation_start_token_id as usize * h..][..h],
        );

        let mut codes: Vec<i64> = Vec::with_capacity(n_vq);
        let (hidden, mut pk, mut pv) = {
            let t = Tensor::from_array((vec![1i64, 2, h as i64], tok))?.into_dyn();
            let pos = Tensor::from_array((vec![1i64, 2], vec![0i64, 1]))?.into_dyn();
            let (empty_k, empty_v) = empty_past(
                l_loc,
                self.cfg.local_num_attention_heads,
                h / self.cfg.local_num_attention_heads,
            )?;
            let mut feed: Vec<(String, &DynValue)> =
                vec![("token_emb".into(), &t), ("position_ids".into(), &pos)];
            for i in 0..l_loc {
                feed.push((format!("past_k_{i}"), &empty_k[i]));
                feed.push((format!("past_v_{i}"), &empty_v[i]));
            }
            let t_run = std::time::Instant::now();
            let mut outs = self.sess_ac.run(feed).context("acoustic opening")?;
            crate::stats::add(crate::stats::Kind::Acoustic, t_run.elapsed());
            let hidden = outs.remove("hidden").context("acoustic: no hidden")?;
            let mut a = Vec::with_capacity(l_loc);
            let mut b = Vec::with_capacity(l_loc);
            for i in 0..l_loc {
                a.push(
                    outs.remove(format!("present_k_{i}"))
                        .with_context(|| format!("acoustic: no present_k_{i}"))?,
                );
                b.push(
                    outs.remove(format!("present_v_{i}"))
                        .with_context(|| format!("acoustic: no present_v_{i}"))?,
                );
            }
            (hidden, a, b)
        };

        // Channel 0 reads the *second* position; the remaining channels read the
        let slot0 = row(&hidden, 0)?;
        let mut vec0 = row(&hidden, 1)?;
        codes.push(sample_channel(
            &mut vec0,
            &self.audio_emb,
            0,
            self.audio_vocab,
            h,
            s,
            hist.as_deref_mut(),
            rng,
        ));

        for ch in 1..n_vq {
            let prev = codes[ch - 1] as usize;
            let base = (ch - 1) * self.audio_vocab * h + prev * h;
            let emb: Vec<f32> = self.audio_emb.data[base..base + h].to_vec();
            let (hidden2, a, b) = {
                let t = Tensor::from_array((vec![1i64, 1, h as i64], emb))?.into_dyn();
                let pos = Tensor::from_array((vec![1i64, 1], vec![(ch + 1) as i64]))?.into_dyn();
                let mut feed: Vec<(String, &DynValue)> =
                    vec![("token_emb".into(), &t), ("position_ids".into(), &pos)];
                for i in 0..l_loc {
                    feed.push((format!("past_k_{i}"), &pk[i]));
                    feed.push((format!("past_v_{i}"), &pv[i]));
                }
                let t_run = std::time::Instant::now();
                let mut outs = self
                    .sess_ac
                    .run(feed)
                    .with_context(|| format!("acoustic channel {ch}"))?;
                crate::stats::add(crate::stats::Kind::Acoustic, t_run.elapsed());
                let hidden = outs.remove("hidden").context("acoustic: no hidden")?;
                let mut a = Vec::with_capacity(l_loc);
                let mut b = Vec::with_capacity(l_loc);
                for i in 0..l_loc {
                    a.push(
                        outs.remove(format!("present_k_{i}"))
                            .with_context(|| format!("acoustic: no present_k_{i}"))?,
                    );
                    b.push(
                        outs.remove(format!("present_v_{i}"))
                            .with_context(|| format!("acoustic: no present_v_{i}"))?,
                    );
                }
                (hidden, a, b)
            };
            pk = a;
            pv = b;
            let mut v = row(&hidden2, 0)?;
            codes.push(sample_channel(
                &mut v,
                &self.audio_emb,
                ch,
                self.audio_vocab,
                h,
                s,
                hist.as_deref_mut(),
                rng,
            ));
        }

        // End of speech is decided by the *text* head at the opening position.
        let eos = crate::sample::argmax(&matvec(
            &slot0,
            &self.text_emb.data,
            self.cfg.text_vocab_size,
            h,
        )) == self.cfg.speech_generation_end_token_id as usize;

        Ok(Frame { codes, eos })
    }
}

struct Frame {
    codes: Vec<i64>,
    eos: bool,
}

/// Open one graph with the settings the reference uses.
pub(crate) fn open_session(path: &Path, intra: usize) -> Result<Session> {
    fn msg<E: std::fmt::Display>(e: E) -> anyhow::Error {
        anyhow!("{e}")
    }
    let b = Session::builder().map_err(msg)?;
    let b = b
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(msg)?;
    let b = b.with_intra_threads(intra).map_err(msg)?;
    let b = b.with_inter_threads(1).map_err(msg)?;
    // Matches the reference's `intra_op.allow_spinning = 0`: a server rendering
    let mut b = b
        .with_config_entry("session.intra_op.allow_spinning", "0")
        .map_err(msg)?;
    b.commit_from_file(path)
        .with_context(|| format!("loading {}", path.display()))
}

/// The empty KV cache the acoustic graph starts from: `[1, heads, 0, dim]`.
fn empty_past(layers: usize, heads: usize, dim: usize) -> Result<(Vec<DynValue>, Vec<DynValue>)> {
    let mut k = Vec::with_capacity(layers);
    let mut v = Vec::with_capacity(layers);
    for _ in 0..layers {
        k.push(
            Tensor::from_array((vec![1i64, heads as i64, 0i64, dim as i64], vec![0f32; 0]))
                .map_err(|e| anyhow!("empty past: {e}"))?
                .into_dyn(),
        );
        v.push(
            Tensor::from_array((vec![1i64, heads as i64, 0i64, dim as i64], vec![0f32; 0]))
                .map_err(|e| anyhow!("empty past: {e}"))?
                .into_dyn(),
        );
    }
    Ok((k, v))
}

/// Sample one channel: `hidden @ audio_emb[ch].T`, penalised and filtered.
#[allow(clippy::too_many_arguments)]
fn sample_channel(
    hidden: &mut [f32],
    audio_emb: &Array,
    ch: usize,
    audio_vocab: usize,
    h: usize,
    s: &Sampling,
    hist: Option<&mut crate::sample::RepetitionHistory>,
    rng: &mut Rng,
) -> i64 {
    let base = ch * audio_vocab * h;
    let mut logits = matvec(
        hidden,
        &audio_emb.data[base..base + audio_vocab * h],
        audio_vocab,
        h,
    );
    let window = hist.as_ref().map(|hh| &hh.channels[ch]);
    let code = crate::sample::sample(&mut logits, s, window, rng) as i64;
    if let Some(hh) = hist {
        hh.channels[ch].add(code);
    }
    code
}

/// `x @ table.T` — a row per table entry.
fn matvec(x: &[f32], table: &[f32], rows: usize, width: usize) -> Vec<f32> {
    let t0 = std::time::Instant::now();
    let out = matvec_inner(x, table, rows, width);
    crate::stats::add(crate::stats::Kind::Matvec, t0.elapsed());
    out
}

fn matvec_inner(x: &[f32], table: &[f32], rows: usize, width: usize) -> Vec<f32> {
    // Establish the bound once: `x` arrives as a slice of unknown length, and
    let x = &x[..width];
    let mut out = vec![0f32; rows];
    for (r, slot) in out.iter_mut().enumerate() {
        *slot = crate::simd::dot(&table[r * width..(r + 1) * width], x);
    }
    out
}

/// Row `i` of a `[1, T, H]` tensor.
fn row(t: &DynValue, i: usize) -> Result<Vec<f32>> {
    let (shape, data) = t.try_extract_tensor::<f32>().context("output is not f32")?;
    let dims: Vec<i64> = shape.as_ref().to_vec();
    if dims.len() != 3 {
        bail!("expected a 3-D output, got {dims:?}");
    }
    let (tt, h) = (dims[1] as usize, dims[2] as usize);
    if i >= tt {
        bail!("row {i} is out of range for {dims:?}");
    }
    Ok(data[i * h..(i + 1) * h].to_vec())
}

fn last_row(t: &DynValue, t_len: usize) -> Result<Vec<f32>> {
    row(t, t_len - 1)
}

fn first_row(t: &DynValue) -> Result<Vec<f32>> {
    row(t, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matvec_is_a_dot_per_row() {
        // 2 rows of width 3
        let table = vec![1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];
        assert_eq!(matvec(&[2.0, 3.0, 9.0], &table, 2, 3), vec![2.0, 3.0]);
    }

    #[test]
    fn config_defaults_match_the_reference_get_calls() {
        let c: Config = serde_json::from_str(
            r#"{"n_vq":16,"hidden_size":768,"num_hidden_layers":12,
                "audio_pad_token_id":1024,"text_prompt_start_token_id":3,
                "text_prompt_end_token_id":4,"speech_generation_start_token_id":5,
                "speech_generation_end_token_id":6,"audio_ref_slot_token_id":7,
                "text_vocab_size":419}"#,
        )
        .unwrap();
        assert_eq!(c.local_num_hidden_layers, 1);
        assert_eq!(c.local_num_attention_heads, 8);
        assert_eq!(c.default_style_token_id, 16);
        assert_eq!(c.speaker_embedding_dim, 192);
        assert!(!c.use_speaker_embedding);
    }

    #[test]
    fn empty_past_is_zero_length_not_empty_shape() {
        let (k, v) = empty_past(1, 8, 64).unwrap();
        assert_eq!(k.len(), 1);
        assert_eq!(v.len(), 1);
        let (shape, _) = k[0].try_extract_tensor::<f32>().unwrap();
        assert_eq!(shape.as_ref(), &[1, 8, 0, 64]);
    }
}
