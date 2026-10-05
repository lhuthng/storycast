use super::*;

#[test]
fn a_ramp_never_reaches_full_or_empty() {
    // The ramps stop short on purpose: a frame at zero alpha between two
    // captions is the one-frame flash the design avoids.
    let (n_in, n_out, ups, downs) = ramps(30, 9);
    assert_eq!((n_in, n_out), (9, 9));
    assert_eq!(ups.len(), 9);
    assert!((ups[0] - 1.0 / 10.0).abs() < 1e-6);
    assert!((ups[8] - 9.0 / 10.0).abs() < 1e-6);
    assert_eq!(downs.len(), 9);
}

#[test]
fn a_very_short_span_gets_one_up_frame_and_no_down() {
    let (n_in, n_out, ups, downs) = ramps(2, 9);
    assert_eq!((n_in, n_out), (1, 0));
    assert_eq!(ups, vec![0.5]);
    assert!(downs.is_empty());
}

#[test]
fn a_plan_survives_the_trip_to_a_worker_exactly() {
    // A worker hashes these numbers into its chunk keys, so a float that loses
    // a bit on the way is a box whose whole half is re-rendered at home.
    let p = Plan {
        chunk_frames: 120,
        total_frames: 12434,
        total: 414.466,
        chapters: vec![ChapterClock { dur: 414.466, start: 0.0 }],
    };
    let back: Plan = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert_eq!(back.total.to_bits(), p.total.to_bits());
    assert_eq!(back.chapters[0].dur.to_bits(), p.chapters[0].dur.to_bits());
    assert_eq!(back.total_frames, 12434);
}

#[test]
fn a_noisy_envelope_does_not_re_render_a_chunk() {
    // Two ffmpeg builds report a level differing in the last digits. The key
    // may only notice what the pixels can show, or a box's chunks come home
    // and are thrown away as stale.
    let key = |a: f32| {
        let mut h = Sha256::new();
        h.update(amount_level(a).to_le_bytes());
        h.finalize()
    };
    assert_eq!(key(0.5), key(0.5 + 1e-6));
    assert_eq!(key(0.5), key(0.5 - 1e-6));
    // A level the pixels can show is still a different chunk.
    assert_ne!(key(0.5), key(0.5 + 1.0 / AMOUNT_LEVELS));
}

#[test]
fn the_quantiser_clamps_what_the_key_hashes() {
    assert_eq!(quant(0.0), 0);
    assert_eq!(quant(1.0), 1024);
    assert_eq!(quant(8.0), 8192);
    assert_eq!(quant(99.0), 8192);
}
