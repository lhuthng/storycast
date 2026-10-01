//! End-to-end check of the voice-treatment pass, with the real `ffmpeg` and
//! `sox` binaries and a synthetic voice.
//!
//! Two claims, both of which a unit test that never runs the tools cannot make:
//!
//! 1. **A tail is reserved.** Two slots in a cave, whose preset reserves
//!    2.8 s, produce a mix of exactly `voice + 2.8` — the *longest* preset's
//!    tail once, not one tail per slot: the decay rings under the next line and
//!    only the chapter's end is extended.
//! 2. **The speech does not drift.** Each slot's piece is *placed* at its
//!    script offset, not concatenated, so the second line's beep still lands at
//!    its scripted second even though the first line reserved a tail. The old
//!    concat appended the tail and slid every later line onto the beds.
//!
//! It skips (returns) when either tool is missing, so the suite stays green on
//! a box that cannot merge anyway; run it by hand with `--ignored` where the
//! tools live.

use bm_core::ambience::{apply_layers, timeline, LayerSwitch, Turn};
use bm_core::assemble::{read_wav, write_wav, Wav};
use std::collections::BTreeMap;
use std::path::Path;

fn tool(bin: &str, probe: &str) -> bool {
    std::process::Command::new(bin)
        .arg(probe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A mono 48 kHz WAV with a 150 ms 880 Hz burst at each `beeps_at` second,
/// silence elsewhere.
fn beeped_wav(path: &Path, seconds: f64, beeps_at: &[f64]) {
    let rate = 48_000usize;
    let n = (seconds * rate as f64) as usize;
    let mut data = vec![0u8; n * 2];
    for &at in beeps_at {
        let start = (at * rate as f64) as usize;
        let len = (0.15 * rate as f64) as usize;
        for i in 0..len {
            let idx = start + i;
            if idx >= n {
                break;
            }
            let t = i as f64 / rate as f64;
            let v = ((t * 660.0 * std::f64::consts::TAU).sin() * 0.8 * i16::MAX as f64) as i16;
            data[idx * 2..idx * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
    write_wav(
        path,
        &Wav {
            channels: 1,
            sample_rate: rate as u32,
            bits: 16,
            data,
        },
    )
    .expect("write synthetic wav");
}

/// RMS over a 60 ms window starting at `at`, as a fraction of full scale.
fn rms_at(w: &Wav, at: f64) -> f64 {
    let rate = w.sample_rate as f64;
    let start = (at * rate) as usize;
    let len = (0.06 * rate) as usize;
    let mut sum = 0.0;
    let mut count = 0usize;
    for i in start..(start + len).min(w.frames()) {
        let s = i16::from_le_bytes([w.data[i * 2], w.data[i * 2 + 1]]) as f64 / 32768.0;
        sum += s * s;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

/// The four-second peak of the windowed RMS, so a beep can be located without
/// trusting a fixed time. Coarse (10 ms) — this is a drift check, not a clock.
fn peak_near(w: &Wav, around: f64) -> f64 {
    let mut best = (f64::MIN, around);
    let mut t = (around - 0.4).max(0.0);
    while t < around + 0.4 {
        let r = rms_at(w, t);
        if r > best.0 {
            best = (r, t);
        }
        t += 0.01;
    }
    best.1
}

#[test]
#[ignore = "runs ffmpeg + sox; run with --ignored where both are installed"]
fn a_cave_tail_is_reserved_and_the_speech_does_not_drift() {
    if !tool("ffmpeg", "-version") || !tool("sox", "--version") {
        eprintln!("skipping: ffmpeg and/or sox is not on PATH");
        return;
    }

    let dir = std::env::temp_dir().join(format!("bm-voice-fx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();

    // One place, one room. Empty pools: nothing but the voice track is produced,
    // so the mix IS the treated voice and can be measured directly.
    std::fs::write(
        assets.join("scene-map.json"),
        r#"{
          "rules": [
            {"match": ["cave"], "effect": [], "level": 0.0, "reverb": "cave"}
          ],
          "reverb_presets": {
            "cave": {"sox": "bass 2 reverb 78 35 95", "tail_s": 2.8, "narrator": 0.3}
          }
        }"#,
    )
    .unwrap();
    std::fs::write(assets.join("effect-pool.json"), "{}").unwrap();
    std::fs::write(assets.join("music-pool.json"), "{}").unwrap();

    // Two three-second turns, each beeping at its own onset. `timeline` probes
    // them for the slot clock; the concat of their samples is the voice wav.
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    beeped_wav(&a, 3.0, &[0.3]);
    beeped_wav(&b, 3.0, &[0.3]);
    let turns = vec![
        Turn {
            wav: a.clone(),
            scene: "cave-mouth".into(),
            music: String::new(),
            speaker: "Lỗ Đạt Sênh".into(),
            injects: Vec::new(),
        },
        Turn {
            wav: b.clone(),
            scene: "cave-depth".into(),
            music: String::new(),
            speaker: "Lỗ Đạt Sênh".into(),
            injects: Vec::new(),
        },
    ];
    let slots = timeline(&turns, 0, &BTreeMap::new()).unwrap();
    assert_eq!(slots.len(), 2);

    let voice = dir.join("voice.wav");
    let mut data = read_wav(&a).unwrap().data;
    data.extend_from_slice(&read_wav(&b).unwrap().data);
    write_wav(
        &voice,
        &Wav {
            channels: 1,
            sample_rate: 48_000,
            bits: 16,
            data,
        },
    )
    .unwrap();

    let out = dir.join("mix.wav");
    let work = dir.join("work");
    let merged = apply_layers(
        &voice,
        &slots,
        1,
        LayerSwitch::new(true, false, 1.0, 0.0, 0.0),
        &out,
        &work,
        &assets,
        &BTreeMap::new(),
    )
    .unwrap();

    let w = read_wav(&merged).unwrap();
    // 6 s of voice + one 2.8 s reserve, not two: the first line's tail rings
    // under the second, and only the chapter's end is extended.
    assert!(
        (w.seconds() - 8.8).abs() < 0.05,
        "expected 8.8 s (6.0 voice + one 2.8 reserve), got {:.3}",
        w.seconds()
    );

    // The second line's beep is still at its scripted 3.3 s, not slid right by
    // the first line's reserved tail (which would land it near 6.1 s).
    let first = peak_near(&w, 0.3);
    let second = peak_near(&w, 3.3);
    assert!(
        (first - 0.3).abs() < 0.1,
        "the first line should speak at 0.3, peaked at {first:.2}"
    );
    assert!(
        (second - 3.3).abs() < 0.1,
        "the second line should speak at 3.3 (no drift), peaked at {second:.2}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
