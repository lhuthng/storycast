use super::sidecar::op_segment;
use super::*;

#[test]
fn segment_serves_a_rendered_wav_and_its_sentence() {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    layout.ensure().unwrap();
    // Two speakers; the wav covers segment 1 in Adam's voice.
    std::fs::write(
        layout.script(1),
        serde_json::json!({"segments": [
            {"speaker": "Narrator", "text": "Mở đầu."},
            {"speaker": "Kiên", "text": "Kiên lên tiếng."},
        ]})
        .to_string(),
    )
    .unwrap();
    let seg = layout.seg_dir("vieneu", 1);
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0001_Adam.wav"), b"RIFF-fake").unwrap();

    let res = op_segment(&layout, "vieneu", "Kiên", "Adam", None);
    assert!(res.ok, "{}", res.message);
    assert_eq!(res.line_speaker.as_deref(), Some("Kiên"));
    assert_eq!(res.line_text.as_deref(), Some("Kiên lên tiếng."));
    assert!(res.audio_b64.is_some(), "bytes, not a path");
    assert!(
        res.message.contains("rendered"),
        "say what it was: {}",
        res.message
    );

    // The held line never rendered in Adam's voice, but one of Kiên's
    // did (the fresh-swap state: rendered chapter by chapter). Play
    // hers, still zero synthesis, and hold it, so T compares on the
    // same sentence instead of another random pick.
    let fallback = op_segment(
        &layout,
        "vieneu",
        "Kiên",
        "Adam",
        Some("a line from chapter 99"),
    );
    assert!(fallback.ok, "{}", fallback.message);
    assert_eq!(fallback.line_text.as_deref(), Some("Kiên lên tiếng."));
    assert_eq!(fallback.line_speaker.as_deref(), Some("Kiên"));
    assert!(fallback.audio_b64.is_some(), "bytes, not synthesis");

    // A voice with nothing rendered fails honestly, never synthesizes.
    let miss = op_segment(&layout, "vieneu", "Vũ", "Nobody", None);
    assert!(!miss.ok);
    assert!(
        miss.message.contains("elsewhere or not yet"),
        "{}",
        miss.message
    );
    assert!(miss.audio_b64.is_none());
}

#[test]
fn segment_matches_keys_names_and_folds() {
    // Wavs carry whatever the cast held at render time, often a
    // lowercase key (`adam`) while the operator asks the display name
    // (`Adam`), or an ASCII slug (`pham-tuyen`) for `Phạm Tuyên`.
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    layout.ensure().unwrap();
    std::fs::write(
        layout.script(3),
        serde_json::json!({"segments": [
            {"speaker": "Vũ", "text": "Vũ nói."},
            {"speaker": "Kiên", "text": "Kiên đáp."},
        ]})
        .to_string(),
    )
    .unwrap();
    let seg = layout.seg_dir("vieneu", 3);
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0000_adam.wav"), b"RIFF-a").unwrap();
    std::fs::write(seg.join("0001_pham-tuyen.wav"), b"RIFF-p").unwrap();

    let res = op_segment(&layout, "vieneu", "Kiên", "Adam", None);
    assert!(res.ok, "{}", res.message);
    assert_eq!(res.line_speaker.as_deref(), Some("Vũ"));

    let res = op_segment(&layout, "vieneu", "Nobody", "Phạm Tuyên", None);
    assert!(res.ok, "{}", res.message);
    assert_eq!(res.line_text.as_deref(), Some("Kiên đáp."));
}

#[test]
fn segment_miss_names_where_the_renders_are() {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    layout.ensure().unwrap();
    std::fs::write(
        layout.script(4),
        serde_json::json!({"segments": [{"speaker": "Kiên", "text": "Kiên đáp."}]}).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(layout.seg_dir("vieneu", 4)).unwrap();

    // Lines here, no wavs: those chapters rendered on another box.
    let miss = op_segment(&layout, "vieneu", "Kiên", "Nobody", None);
    assert!(!miss.ok);
    assert!(miss.message.contains("another box"), "{}", miss.message);
    // No lines either: the chapters themselves live elsewhere.
    let miss = op_segment(&layout, "vieneu", "Ghost", "Nobody", None);
    assert!(!miss.ok);
    assert!(
        miss.message.contains("elsewhere or not yet"),
        "{}",
        miss.message
    );
}

#[test]
fn segment_prefers_the_characters_own_lines() {
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    layout.ensure().unwrap();
    std::fs::write(
        layout.script(2),
        serde_json::json!({"segments": [
            {"speaker": "Vũ", "text": "Vũ nói."},
            {"speaker": "Kiên", "text": "Kiên đáp."},
        ]})
        .to_string(),
    )
    .unwrap();
    let seg = layout.seg_dir("vieneu", 2);
    std::fs::create_dir_all(&seg).unwrap();
    // Filenames carry whatever the cast held at render time, a key here.
    std::fs::write(seg.join("0000_adam.wav"), b"RIFF-0").unwrap();
    std::fs::write(seg.join("0001_adam.wav"), b"RIFF-1").unwrap();

    // key_for_name("vieneu", "Adam") may or may not know this fixture
    // voice; either way the raw string still matches.
    let res = op_segment(&layout, "vieneu", "Kiên", "adam", None);
    assert!(res.ok, "{}", res.message);
    assert_eq!(
        res.line_speaker.as_deref(),
        Some("Kiên"),
        "own lines win over Vũ's"
    );
    assert_eq!(res.line_text.as_deref(), Some("Kiên đáp."));
}

#[test]
fn a_shipped_take_is_stored_and_a_bad_one_is_skipped() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let layout = bm_core::Layout::new(dir.path());
    let plan = bm_core::assemble::RenderPlan {
        chapter: 1,
        engine: "vieneu".into(),
        plan_version: bm_core::assemble::PLAN_VERSION,
        generated: 0,
        cast_hash: String::new(),
        takes: vec![bm_core::assemble::Take {
            pos: 0,
            tag: "title".into(),
            speaker: "Narrator".into(),
            voice: "Narrator".into(),
            voice_key: String::new(),
            text: "x".into(),
            temperature: 0.7,
            silence_p: 0.1,
            take_key: "k".into(),
            file: "t-k.wav".into(),
            legacy: None,
            adopted: false,
        }],
    };
    plan.save(&layout.plan(1)).unwrap();

    let enc = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let wav = vec![7u8; 1500];
    store_shipments(
        &layout,
        "vieneu",
        "render:1:0",
        &[
            bm_proto::UnitFile {
                name: "t-k.wav".into(),
                b64: enc(&wav),
            },
            // Not in the plan: a worker may not name its own path.
            bm_proto::UnitFile {
                name: "evil.wav".into(),
                b64: enc(&wav),
            },
            // Half-write floor: 10 bytes is not a take.
            bm_proto::UnitFile {
                name: "t-k.wav".into(),
                b64: enc(&[1u8; 10]),
            },
            // Undecodable payload: skipped, not fatal.
            bm_proto::UnitFile {
                name: "t-k.wav".into(),
                b64: "!!!".into(),
            },
        ],
    );

    let stored = layout.seg_dir("vieneu", 1).join("t-k.wav");
    assert_eq!(std::fs::read(&stored).unwrap(), wav, "the take is home");
    assert!(
        !layout.seg_dir("vieneu", 1).join("evil.wav").exists(),
        "unexpected names never land"
    );
    // task_ids that name no chapter store nothing and do not panic.
    store_shipments(
        &layout,
        "vieneu",
        "render",
        &[bm_proto::UnitFile {
            name: "t-k.wav".into(),
            b64: enc(&wav),
        }],
    );
}
