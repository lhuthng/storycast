//! Post-process: per-scene ambience beds + room reverb under the voice mix.
//!
//! Ported from `ambience.py`. Offline, deterministic, no API. A missing bed
//! file degrades gracefully to dry voice for that span — never an error.

use crate::assemble::{read_wav, Run};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SceneRule {
    #[serde(default)]
    pub bed: Option<String>,
    #[serde(default)]
    pub level: f64,
    #[serde(default)]
    pub reverb: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    #[serde(rename = "match", default)]
    pub matches: Vec<String>,
    #[serde(default)]
    pub bed: Option<String>,
    #[serde(default)]
    pub level: f64,
    #[serde(default)]
    pub reverb: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Duck {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_ratio")]
    pub ratio: f64,
    #[serde(default = "default_attack")]
    pub attack: u32,
    #[serde(default = "default_release")]
    pub release: u32,
}

fn default_threshold() -> f64 {
    0.02
}
fn default_ratio() -> f64 {
    6.0
}
fn default_attack() -> u32 {
    20
}
fn default_release() -> u32 {
    400
}

impl Default for Duck {
    fn default() -> Self {
        Duck {
            threshold: default_threshold(),
            ratio: default_ratio(),
            attack: default_attack(),
            release: default_release(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SceneMap {
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub default: SceneRule,
    #[serde(default)]
    pub reverb_presets: BTreeMap<String, String>,
    #[serde(default)]
    pub duck: Duck,
}

pub fn load_map(path: &Path) -> Result<SceneMap> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading scene map {}", path.display()))?;
    let map: SceneMap = serde_json::from_str(&text)
        .with_context(|| format!("parsing scene map {}", path.display()))?;
    Ok(map)
}

/// First matching rule wins (ordered specific -> general).
pub fn match_scene(scene: &str, cfg: &SceneMap) -> SceneRule {
    let s = scene.to_lowercase();
    for rule in &cfg.rules {
        if rule.matches.iter().any(|k| s.contains(&k.to_lowercase())) {
            return SceneRule {
                bed: rule.bed.clone(),
                level: rule.level,
                reverb: rule.reverb.clone(),
            };
        }
    }
    cfg.default.clone()
}

/// Majority scene tag per run (runs are consecutive same-speaker lines).
pub fn run_scenes(segments: &[Value], runs: &[Run]) -> Vec<String> {
    runs.iter()
        .map(|run| {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for i in &run.idx {
                let tag = segments
                    .get(*i)
                    .and_then(|s| s.get("scene"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if tag.is_empty() {
                    continue;
                }
                match counts.iter_mut().find(|(t, _)| *t == tag) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((tag, 1)),
                }
            }
            // ties resolve to the first tag seen, like Python's Counter
            counts
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .map(|(t, _)| t)
                .unwrap_or_default()
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub bed: Option<String>,
    pub level: f64,
    pub reverb: Option<String>,
    pub scene: String,
    pub start: f64,
    pub end: f64,
}

/// Tile the mix timeline into spans of identical (bed, level, reverb).
pub fn build_spans(wavs: &[PathBuf], scenes: &[String], gap_ms: u32, cfg: &SceneMap) -> Result<Vec<Span>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut t = 0.0f64;
    for (wav, scene) in wavs.iter().zip(scenes.iter()) {
        let dur = read_wav(wav)?.seconds();
        let rule = match_scene(scene, cfg);
        match spans.last_mut() {
            Some(last)
                if last.bed == rule.bed && last.level == rule.level && last.reverb == rule.reverb =>
            {
                last.end = t + dur;
            }
            _ => spans.push(Span {
                bed: rule.bed,
                level: rule.level,
                reverb: rule.reverb,
                scene: scene.clone(),
                start: t,
                end: t + dur,
            }),
        }
        t += dur + gap_ms as f64 / 1000.0;
    }
    Ok(spans)
}

fn ffmpeg(args: &[String]) -> Result<()> {
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

fn s(v: impl ToString) -> String {
    v.to_string()
}

fn concat_files(parts: &[PathBuf], out: &Path) -> Result<()> {
    let list = out.with_file_name("parts.txt");
    let mut body = String::new();
    for p in parts {
        // absolute paths: the concat demuxer resolves relative ones against the playlist dir
        let abs = p.canonicalize().unwrap_or_else(|_| p.clone());
        body.push_str(&format!("file '{}'\n", abs.display()));
    }
    std::fs::write(&list, body)?;
    let r = ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        s(list.display()),
        "-c:a".into(),
        "pcm_s16le".into(),
        s(out.display()),
    ]);
    let _ = std::fs::remove_file(&list);
    r
}

/// Place bed slices at exact scene offsets (adelay + amix, no drift) with tiny
/// edge fades to avoid clicks.
fn join_beds(slices: &[(PathBuf, Span)], out: &Path, total: f64) -> Result<()> {
    if slices.is_empty() {
        return ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("anullsrc=r=48000:cl=mono:d={total:.3}"),
            s(out.display()),
        ]);
    }
    let mut args: Vec<String> = vec!["-y".into(), "-loglevel".into(), "error".into()];
    let mut filters: Vec<String> = Vec::new();
    for (n, (p, span)) in slices.iter().enumerate() {
        args.push("-i".into());
        args.push(s(p.display()));
        let dur = span.end - span.start;
        let ms = (span.start * 1000.0) as i64;
        filters.push(format!(
            "[{n}:a]afade=t=in:st=0:d=0.3,afade=t=out:st={:.3}:d=0.3,adelay={ms}|{ms}[s{n}]",
            (dur - 0.3).max(0.0)
        ));
    }
    let labels: String = (0..slices.len()).map(|n| format!("[s{n}]")).collect();
    filters.push(format!(
        "{labels}amix=inputs={}:normalize=0,apad=whole_dur={total:.3},\
         aformat=sample_rates=48000:channel_layouts=mono[out]",
        slices.len()
    ));
    args.push("-filter_complex".into());
    args.push(filters.join(";"));
    args.push("-map".into());
    args.push("[out]".into());
    args.push("-t".into());
    args.push(format!("{total:.3}"));
    args.push(s(out.display()));
    ffmpeg(&args)
}

/// Mix per-scene beds + room reverb under the voice track.
/// `scenes` aligns with `wavs`.
pub fn apply_ambience(
    voice_wav: &Path,
    scenes: &[String],
    wavs: &[PathBuf],
    gap_ms: u32,
    out: &Path,
    assets: &Path,
) -> Result<PathBuf> {
    let cfg = load_map(&assets.join("scene-map.json"))?;
    let mut spans = build_spans(wavs, scenes, gap_ms, &cfg)?;

    let beds_dir = assets.join("ambience");
    let mut missing: Vec<String> = Vec::new();
    for span in &spans {
        if let Some(bed) = &span.bed {
            if !beds_dir.join(bed).exists() && !missing.iter().any(|m| m == bed) {
                missing.push(bed.clone());
            }
        }
    }
    for bed in &missing {
        eprintln!("ambience: bed missing ({bed}) -> dry voice for those spans");
    }
    for span in spans.iter_mut() {
        if span.bed.as_ref().map(|b| missing.contains(b)).unwrap_or(false) {
            span.bed = None;
            span.level = 0.0;
        }
    }

    if spans.iter().all(|s| s.bed.is_none() && s.reverb.is_none()) {
        eprintln!("ambience: everything dry, skipped");
        return Ok(voice_wav.to_path_buf());
    }

    let work = out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".amb_tmp");
    std::fs::create_dir_all(&work)?;

    // 1. voice track with per-scene reverb
    let mut voice_fx = voice_wav.to_path_buf();
    if spans.iter().any(|s| s.reverb.is_some()) {
        let mut parts = Vec::new();
        for (n, span) in spans.iter().enumerate() {
            let p = work.join(format!("v{n}.wav"));
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                s(voice_wav.display()),
                "-ss".into(),
                format!("{:.3}", span.start),
                "-to".into(),
                format!("{:.3}", span.end),
            ];
            if let Some(fx) = span.reverb.as_ref().and_then(|r| cfg.reverb_presets.get(r)) {
                args.push("-af".into());
                args.push(fx.clone());
            }
            args.push(s(p.display()));
            ffmpeg(&args)?;
            parts.push(p);
        }
        voice_fx = work.join("voice_fx.wav");
        concat_files(&parts, &voice_fx)?;
    }

    // 2. bed track, looped per span
    let mut bed_slices: Vec<(PathBuf, Span)> = Vec::new();
    for (n, span) in spans.iter().enumerate() {
        let Some(bed) = &span.bed else { continue };
        let p = work.join(format!("b{n}.wav"));
        ffmpeg(&[
            "-y".into(),
            "-loglevel".into(),
            "error".into(),
            "-stream_loop".into(),
            "-1".into(),
            "-i".into(),
            s(beds_dir.join(bed).display()),
            "-t".into(),
            format!("{:.3}", span.end - span.start),
            "-af".into(),
            format!(
                "volume={},aformat=sample_rates=48000:channel_layouts=mono",
                span.level
            ),
            s(p.display()),
        ])?;
        bed_slices.push((p, span.clone()));
    }
    let bed_mix = work.join("bed.wav");
    let total = read_wav(&voice_fx)?.seconds();
    join_beds(&bed_slices, &bed_mix, total)?;

    // 3. duck the bed under the voice, then mix
    let duck = &cfg.duck;
    ffmpeg(&[
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        s(voice_fx.display()),
        "-i".into(),
        s(bed_mix.display()),
        "-filter_complex".into(),
        format!(
            "[1:a][0:a]sidechaincompress=threshold={}:ratio={}:attack={}:release={}[duck];\
             [0:a][duck]amix=inputs=2:normalize=0[a]",
            duck.threshold, duck.ratio, duck.attack, duck.release
        ),
        "-map".into(),
        "[a]".into(),
        s(out.display()),
    ])?;

    for span in &spans {
        let tag = span.bed.clone().unwrap_or_else(|| "dry".into());
        eprintln!(
            "ambience [{:.0}-{:.0}s] {} -> {}{}",
            span.start,
            span.end,
            if span.scene.is_empty() { "?" } else { &span.scene },
            tag,
            span.reverb
                .as_ref()
                .map(|r| format!(" + {r}"))
                .unwrap_or_default()
        );
    }

    if let Ok(entries) = std::fs::read_dir(&work) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    Ok(out.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::silent_wav;
    use serde_json::json;

    fn scene_map() -> SceneMap {
        serde_json::from_str(
            r#"{
              "rules": [
                {"match": ["storm","thunder"], "bed": "rain-storm.mp3", "level": 0.22, "reverb": null},
                {"match": ["rain"], "bed": "rain-light.mp3", "level": 0.18, "reverb": null},
                {"match": ["night","evening"], "bed": "night-crickets.mp3", "level": 0.15, "reverb": null},
                {"match": ["market","street"], "bed": "market-crowd.mp3", "level": 0.16, "reverb": null},
                {"match": ["cave"], "bed": "cave-drip.mp3", "level": 0.18, "reverb": "cave"},
                {"match": ["hall","sect"], "bed": null, "level": 0.0, "reverb": "hall"}
              ],
              "default": {"bed": null, "level": 0.0, "reverb": null},
              "reverb_presets": {"hall": "aecho=0.8:0.65:40|60:0.35|0.25"},
              "duck": {"threshold": 0.02, "ratio": 6.0, "attack": 20, "release": 400}
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn first_matching_rule_wins_specific_before_general() {
        let cfg = scene_map();
        assert_eq!(
            match_scene("street-day-book-discovery", &cfg).bed.as_deref(),
            Some("market-crowd.mp3")
        );
        assert_eq!(
            match_scene("courtyard-rain-day", &cfg).bed.as_deref(),
            Some("rain-light.mp3")
        );
        assert_eq!(match_scene("great-hall-day", &cfg).reverb.as_deref(), Some("hall"));
        assert_eq!(match_scene("something-unknown-xyz", &cfg).bed, None);
    }

    #[test]
    fn run_scenes_takes_the_majority_tag() {
        let segments = vec![
            json!({"scene": "market-morning"}),
            json!({"scene": "market-morning"}),
            json!({"scene": "courtyard-evening"}),
        ];
        let runs = crate::assemble::runs(&[
            json!({"speaker": "A"}),
            json!({"speaker": "A"}),
            json!({"speaker": "A"}),
        ]);
        let scenes = run_scenes(&segments, &runs);
        assert_eq!(scenes, vec!["market-morning"]);
    }

    #[test]
    fn run_scenes_ignores_blank_tags() {
        let segments = vec![json!({"scene": ""}), json!({"scene": "  "})];
        let runs = crate::assemble::runs(&[json!({"speaker": "A"}), json!({"speaker": "A"})]);
        assert_eq!(run_scenes(&segments, &runs), vec![""]);
    }

    #[test]
    fn spans_merge_adjacent_identical_scenes_and_respect_the_gap() {
        let d = std::env::temp_dir().join("bm-amb-spans");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let a = d.join("a.wav");
        let b = d.join("b.wav");
        silent_wav(&a, 1.0, 48_000).unwrap();
        silent_wav(&b, 1.0, 48_000).unwrap();
        let cfg = scene_map();

        let spans = build_spans(
            &[a.clone(), b.clone()],
            &["street-day".into(), "night-x".into()],
            300,
            &cfg,
        )
        .unwrap();
        assert_eq!(spans.len(), 2);
        assert!((spans[1].start - 1.3).abs() < 0.01, "{spans:?}");

        let merged = build_spans(
            &[a, b],
            &["street-day".into(), "street-day".into()],
            0,
            &cfg,
        )
        .unwrap();
        assert_eq!(merged.len(), 1);
        assert!((merged[0].end - 2.0).abs() < 0.01);
    }
}
