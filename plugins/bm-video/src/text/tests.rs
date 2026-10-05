use super::*;

#[test]
fn timestamps_are_srt_or_vtt() {
    assert_eq!(ts(3661.5, true), "01:01:01,500");
    assert_eq!(ts(3661.5, false), "01:01:01.500");
    assert_eq!(ts(0.0, true), "00:00:00,000");
    // A negative clock never reaches the sidecar.
    assert_eq!(ts(-1.0, false), "00:00:00.000");
}

#[test]
fn only_the_narrator_goes_unquoted() {
    assert_eq!(speaker_label("Narrator"), "");
    assert_eq!(speaker_label("người dẫn chuyện"), "");
    assert_eq!(speaker_label("  "), "");
    assert_eq!(speaker_label("Chu Vân"), "\"");
}

#[test]
fn a_caption_never_keeps_a_dash() {
    // A dash is a pause the narrator takes, not a word.
    assert!(!caption_text("A — b").contains('—'));
    assert!(!caption_text("A–b").contains('–'));
}
