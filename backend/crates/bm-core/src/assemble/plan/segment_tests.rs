use super::*;

#[test]
fn pick_prefers_own_lines_and_picks_something() {
    let seg = |speaker: &str| RenderedSegment {
        speaker: speaker.into(),
        text: "x".into(),
        path: PathBuf::from("x.wav"),
        chapter: 1,
    };
    assert!(pick_rendered(&[], "A").is_none(), "empty pool, no pick");
    let one = vec![seg("A")];
    assert_eq!(pick_rendered(&one, "Z").unwrap().speaker, "A");
    // Deterministic whenever the preferred set has exactly one member.
    let two = vec![seg("A"), seg("B")];
    assert_eq!(pick_rendered(&two, "B").unwrap().speaker, "B");
}

#[test]
fn norm_voice_meets_keys_names_and_wav_tags() {
    // The three surface forms of one clone voice.
    assert_eq!(norm_voice("pham-tuyen"), norm_voice("Phạm Tuyên"));
    assert_eq!(norm_voice("adam"), norm_voice("Adam"));
    assert_eq!(norm_voice("young-female-1"), norm_voice("young-female-1"));
    assert_ne!(norm_voice("minh-duc"), norm_voice("minh-triet"));
}
