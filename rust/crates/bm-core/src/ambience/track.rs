use super::mix::slot_effect;
use super::*;

pub(crate) fn ffmpeg(args: &[String]) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(args)
        .output()
        .context("spawning ffmpeg")?;
    if !out.status.success() {
        anyhow::bail!(
            "ffmpeg failed: {}",
            crate::util::head_chars(&String::from_utf8_lossy(&out.stderr), 300)
        );
    }
    Ok(())
}

pub(crate) fn s(v: impl ToString) -> String {
    v.to_string()
}

/// The edge fade every spoken slot gets: a line must not begin or end on a hard
/// sample. A tenth of a second is a click guard, not an attack.
pub const FADE_S: f64 = 0.1;

/// `sox`, the second audio engine the merge shells out to. A voice treatment
/// that names a `sox` chain needs it, exactly as the beds need ffmpeg.
fn sox(args: &[String]) -> Result<()> {
    let out = Command::new("sox")
        .args(args)
        .output()
        .context("spawning sox")?;
    if !out.status.success() {
        anyhow::bail!(
            "sox failed: {}",
            crate::util::head_chars(&String::from_utf8_lossy(&out.stderr), 300)
        );
    }
    Ok(())
}

/// The longest decay any span in this chapter asks for. Zero when nothing
/// reserves a tail, which is when the mix is exactly as long as the voice.
pub(crate) fn voice_reserve(spans: &[Span], presets: &BTreeMap<String, VoiceFx>) -> f64 {
    spans
        .iter()
        .filter_map(|s| s.reverb.as_ref())
        .filter_map(|r| presets.get(r))
        .map(VoiceFx::tail_s)
        .fold(0.0_f64, f64::max)
}

/// Pad `raw` out to `span_len` and put a [`FADE_S`] fade at each edge. Used for
/// a slot with no treatment, which still must not start or end on a click.
fn fade_edges(raw: &Path, out: &Path, span_len: f64) -> Result<()> {
    let af = format!(
        "apad=whole_dur={span_len:.3},atrim=0:{span_len:.3},\
         afade=t=in:st=0:d={FADE_S:.3},afade=t=out:st={:.3}:d={FADE_S:.3}",
        (span_len - FADE_S).max(0.0)
    );
    ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        s(raw.display()),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "1".into(),
        "-af".into(),
        af,
        "-c:a".into(),
        "pcm_s16le".into(),
        s(out.display()),
    ])
}

/// The whole voice track: every slot's piece treated and placed at its own
/// offset, then summed.
///
/// **Placement, not concatenation.** A concat grew the track by every reserved
/// tail and slid the speech against the beds; placing each piece where the
/// script put it keeps the turn fixed, with the decay ringing under the next
/// line.
pub(crate) fn build_voice_track(
    voice_wav: &Path,
    slots: &[Slot],
    spans: &[Span],
    presets: &BTreeMap<String, VoiceFx>,
    total: f64,
    work: &Path,
) -> Result<PathBuf> {
    let mut pieces: Vec<(PathBuf, f64)> = Vec::new();
    for (n, slot) in slots.iter().enumerate() {
        let len = (slot.end - slot.start).max(0.0);
        if len <= 0.0 {
            continue;
        }
        // Seek BEFORE the input: `-ss` as an input option seeks (PCM is
        // sample-accurate for this), so each piece decodes only its own span.
        let raw = work.join(format!("v{n}.raw.wav"));
        ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", slot.start),
            "-t".into(),
            format!("{len:.3}"),
            "-i".into(),
            s(voice_wav.display()),
            "-ar".into(),
            "48000".into(),
            "-ac".into(),
            "1".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            s(raw.display()),
        ])?;
        let fx = slot_effect(slot, spans, presets);
        let tail = fx.map(|(f, _)| f.tail_s()).unwrap_or(0.0);
        let span_len = len + tail;
        let p = work.join(format!("v{n}.wav"));
        match fx {
            Some((f, narrator)) => {
                let depth = if narrator { NARRATOR_DEPTH } else { 1.0 };
                apply_voice_fx(f, depth, &raw, &p, span_len, work)?;
            }
            None => fade_edges(&raw, &p, span_len)?,
        }
        pieces.push((p, slot.start));
    }
    let voice_fx = work.join("voice_fx.wav");
    place_voice(&pieces, &voice_fx, total)?;
    Ok(voice_fx)
}

/// Run one slot's treatment: the effect, the reserved tail, the edge fades, and
/// (for the Narrator) a tenth of the depth by blending back toward dry.
///
/// `depth` is 1.0 for a character and [`NARRATOR_DEPTH`] for the Narrator: a
/// blend against the dry piece, because "in the room but not standing in it"
/// is a mix of two signals rather than a knob the effect has.
fn apply_voice_fx(
    fx: &VoiceFx,
    depth: f64,
    raw: &Path,
    out: &Path,
    span_len: f64,
    work: &Path,
) -> Result<()> {
    if depth <= 0.0 {
        return fade_edges(raw, out, span_len);
    }
    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("v")
        .to_string();
    let (engine, chain) = fx.engine_and_chain();
    let chain = chain.trim();
    let processed = work.join(format!("{stem}.wet.wav"));
    match engine {
        FxEngine::Ffmpeg => {
            let mut af: Vec<String> = Vec::new();
            if !chain.is_empty() {
                af.push(chain.to_string());
            }
            af.push(format!("apad=whole_dur={span_len:.3}"));
            af.push(format!("atrim=0:{span_len:.3}"));
            af.push(format!("afade=t=in:st=0:d={FADE_S:.3}"));
            af.push(format!(
                "afade=t=out:st={:.3}:d={FADE_S:.3}",
                (span_len - FADE_S).max(0.0)
            ));
            ffmpeg(&[
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(raw.display()),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "1".into(),
                "-af".into(),
                af.join(","),
                "-c:a".into(),
                "pcm_s16le".into(),
                s(processed.display()),
            ])?;
        }
        FxEngine::Sox => {
            // SoX's `reverb` never extends its own output, so the room has to
            // ring into silence that already exists: pad the reserved tail on
            // *first*, run the chain, then land the edges on the result.
            let padded = work.join(format!("{stem}.pad.wav"));
            ffmpeg(&[
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(raw.display()),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "1".into(),
                "-af".into(),
                format!("apad=whole_dur={span_len:.3}"),
                "-c:a".into(),
                "pcm_s16le".into(),
                s(padded.display()),
            ])?;
            let wet = work.join(format!("{stem}.sox.wav"));
            if chain.is_empty() {
                std::fs::copy(&padded, &wet)?;
            } else {
                let mut args = vec!["-q".to_string(), s(padded.display()), s(wet.display())];
                args.extend(chain.split_whitespace().map(str::to_string));
                sox(&args)?;
            }
            let af = format!(
                "atrim=0:{span_len:.3},afade=t=in:st=0:d={FADE_S:.3},\
                 afade=t=out:st={:.3}:d={FADE_S:.3}",
                (span_len - FADE_S).max(0.0)
            );
            ffmpeg(&[
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(wet.display()),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "1".into(),
                "-af".into(),
                af,
                "-c:a".into(),
                "pcm_s16le".into(),
                s(processed.display()),
            ])?;
        }
    }
    if depth >= 1.0 {
        std::fs::rename(&processed, out)?;
        return Ok(());
    }
    // The dry leg is the un-treated piece: it is mixed back in for the
    // Narrator, so it needs the same edge fades the wet piece got, or the
    // blend would put the click back at full amplitude over a faded tail.
    let dry_len = (span_len - fx.tail_s()).max(0.0);
    let dry_fade_out = (dry_len - FADE_S).max(0.0);
    ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        s(raw.display()),
        "-i".into(),
        s(processed.display()),
        "-filter_complex".into(),
        format!(
            "[0:a]afade=t=in:st=0:d={FADE_S:.3},\
             afade=t=out:st={dry_fade_out:.3}:d={FADE_S:.3},\
             volume={:.4}[d];[1:a]volume={depth:.4}[w];\
             [d][w]amix=inputs=2:normalize=0,atrim=0:{span_len:.3}[m]",
            1.0 - depth
        ),
        "-map".into(),
        "[m]".into(),
        "-ar".into(),
        "48000".into(),
        "-ac".into(),
        "1".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        s(out.display()),
    ])
}

/// Sum the slot pieces at their absolute offsets, then pad the whole track out
/// to `total` (the voice plus the reserved tail).
fn place_voice(pieces: &[(PathBuf, f64)], out: &Path, total: f64) -> Result<()> {
    if pieces.is_empty() {
        return ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "anullsrc=r=48000:cl=mono".into(),
            "-t".into(),
            format!("{total:.3}"),
            "-c:a".into(),
            "pcm_s16le".into(),
            s(out.display()),
        ]);
    }
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    for (p, _) in pieces {
        args.push("-i".into());
        args.push(s(p.display()));
    }
    let mut graph = String::new();
    for (i, (_, start)) in pieces.iter().enumerate() {
        let ms = (start * 1000.0).round().max(0.0) as i64;
        graph.push_str(&format!("[{i}:a]adelay={ms}[p{i}];"));
    }
    let labels: String = (0..pieces.len()).map(|i| format!("[p{i}]")).collect();
    if pieces.len() == 1 {
        graph.push_str(&format!(
            "{labels}apad=whole_dur={total:.3},atrim=0:{total:.3}[v]"
        ));
    } else {
        graph.push_str(&format!(
            "{labels}amix=inputs={}:normalize=0,apad=whole_dur={total:.3},atrim=0:{total:.3}[v]",
            pieces.len()
        ));
    }
    args.push("-filter_complex".into());
    args.push(graph);
    args.push("-map".into());
    args.push("[v]".into());
    args.push("-ar".into());
    args.push("48000".into());
    args.push("-ac".into());
    args.push("1".into());
    args.push("-c:a".into());
    args.push("pcm_s16le".into());
    args.push(s(out.display()));
    ffmpeg(&args)
}
