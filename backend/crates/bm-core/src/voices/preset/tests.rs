use super::*;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-voice-preset-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_preset_is_a_label_categories_and_tagged_voices() {
    let p = VoicePreset::parse(
        r#"{
          "_note": "ignored",
          "label": "Adult natural",
          "category": ["VI", "Adult", "vi"],
          "voices": [
            {"voice": "duc-tri", "tags": ["male", "adult", "strong"]},
            {"voice": "truc-ly", "tags": ["female", "young"]}
          ]
        }"#,
    )
    .unwrap();
    assert_eq!(p.version, VERSION);
    assert_eq!(p.category, ["vi", "adult"]);
    assert_eq!(p.voices[0].tags, ["male", "adult", "strong"]);
    assert!(!p.is_empty());
}

#[test]
fn tags_are_canonical_and_voices_are_deduped() {
    let p = VoicePreset::parse(
        r#"{
          "label": "Test",
          "voices": [
            {"voice": " A ", "tags": [" Young ", "young", "Adult"]},
            {"voice": "A", "tags": ["male"]},
            {"voice": "", "tags": []}
          ]
        }"#,
    )
    .unwrap();
    assert_eq!(p.voices.len(), 1);
    assert_eq!(p.voices[0].voice, "A");
    assert_eq!(p.voices[0].tags, ["young", "adult"]);
}

#[test]
fn gender_is_not_a_conflict_but_opposites_are() {
    let both = VoicePreset::parse(
        r#"{"label":"Both","voices":[{"voice":"x","tags":["male","female"]}]}"#,
    )
    .unwrap();
    assert_eq!(both.voices[0].tags, ["male", "female"]);
    let err =
        VoicePreset::parse(r#"{"label":"Bad","voices":[{"voice":"x","tags":["young","old"]}]}"#)
            .unwrap_err();
    assert!(err.to_string().contains("young"), "{err:#}");
}

#[test]
fn a_label_is_required_and_an_empty_draft_is_allowed() {
    assert!(VoicePreset::parse(r#"{"voices":[]}"#).is_err());
    let draft = VoicePreset::parse(r#"{"label":"New preset","voices":[]}"#).unwrap();
    assert!(draft.is_empty());
}

#[test]
fn a_preset_round_trips_through_a_directory() {
    let dir = tmp("roundtrip");
    let p = VoicePreset::parse(
        r#"{"label":"Narrator","category":["vi"],"voices":[{"voice":"duc-tri","tags":["male","adult"]}]}"#,
    )
    .unwrap();
    let file = path(&dir, "narrator").unwrap();
    p.save(&file).unwrap();
    let back = VoicePreset::load(&file).unwrap();
    assert_eq!(back, p);
    let all = read_dir(&dir).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all["narrator"].label, "Narrator");

    std::fs::remove_file(&file).unwrap();
    assert!(read_dir(&dir).unwrap().is_empty());
}

#[test]
fn a_missing_directory_is_an_empty_set() {
    let dir = tmp("missing");
    let gone = dir.join("nope");
    assert!(read_dir(&gone).unwrap().is_empty());
}

#[test]
fn a_preset_name_cannot_escape_its_directory() {
    let dir = tmp("slug");
    for bad in ["", "Upper", "has space", "../x", "a/b", "-dash"] {
        assert!(path(&dir, bad).is_err(), "{bad:?} must be refused");
    }
    assert!(path(&dir, "narrator-1")
        .unwrap()
        .ends_with("narrator-1.json"));
}

#[test]
fn a_broken_file_is_an_error_that_names_it() {
    let dir = tmp("broken");
    std::fs::write(dir.join("broken.json"), "{ not json").unwrap();
    let err = read_dir(&dir).unwrap_err();
    assert!(err.to_string().contains("broken.json"), "{err:#}");
}

#[test]
fn non_preset_files_are_ignored() {
    let dir = tmp("ignore");
    std::fs::write(dir.join("README.md"), "not a preset").unwrap();
    std::fs::write(dir.join("Bad Name.json"), "{}").unwrap();
    assert!(read_dir(&dir).unwrap().is_empty());
}
