use super::*;
use crate::audio_pool::{ClipPool, Sound};
use crate::digest::TagAliases;

/// A pooled sound, with only the fields the sound pass reads set.
fn sound(looped: bool) -> Sound {
    Sound {
        tags: vec![],
        files: vec!["injects/x.mp3".into()],
        looped,
        dur_s: Some(0.5),
        mode: Some("overlap".into()),
        hold: None,
        level: None,
    }
}

fn vocab() -> Vocabulary {
    let injects: ClipPool = [
        ("page-turn", sound(false)),
        ("food-prep", sound(true)),
        ("cooking", sound(true)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    Vocabulary {
        palette: vec!["quiet".into(), "battle".into(), "none".into()],
        effects: vec!["paper".into()],
        injects,
        aliases: TagAliases::default(),
        thought_stinger: None,
        sound_cues: Vec::new(),
        engine: "vieneu".into(),
    }
}

fn staged(id: &str, text: &str) -> Value {
    json!({"source_id": id, "speaker": "Narrator", "text": text})
}

const CHAPTER: &str =
    "Nàng lau mồ hôi trên trán, siết chặt cuốn võ thư trong tay, như nhặt được báu vật.";

#[test]
fn a_sound_answer_lands_its_fields_and_lifts_the_seam() {
    let segments = vec![
        staged(
            "e0001",
            "Nàng lau mồ hôi trên trán, siết chặt cuốn võ thư trong tay",
        ),
        staged("e0002", ", như nhặt được báu vật."),
    ];
    let answer = json!({"segments": [
        {"id": 1, "scene": "study-room-day", "music": "quiet", "sound_after": "page-turn", "stop_after": "none"},
        {"id": 2, "sound_after": "none", "stop_after": "none"}
    ]})
    .to_string();
    let got = parse_sound_script(&answer, &segments, CHAPTER, &vocab()).expect("a valid answer");
    // The seam the answer marked became its own sound item, between the lines.
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0]["scene"], json!("study-room-day"));
    assert_eq!(got[0]["music"], json!("quiet"));
    assert_eq!(got[1], json!({"sound": "page-turn"}));
    assert_eq!(got[2]["music"], json!("quiet"), "carried to the next line");
}

#[test]
fn music_and_scene_carry_until_they_change() {
    let segments = vec![
        staged("e0001", "Một."),
        staged("e0002", "Hai."),
        staged("e0003", "Ba."),
    ];
    let answer = json!({"segments": [
        {"id": 1, "scene": "hall-night", "music": "quiet", "sound_after": "none", "stop_after": "none"},
        {"id": 2, "sound_after": "none", "stop_after": "none"},
        {"id": 3, "music": "battle", "sound_after": "none", "stop_after": "none"}
    ]})
    .to_string();
    let got = parse_sound_script(&answer, &segments, "Một. Hai. Ba.", &vocab()).expect("valid");
    assert_eq!(got[0]["music"], json!("quiet"));
    assert_eq!(got[1]["music"], json!("quiet"), "omitted music carries");
    assert_eq!(
        got[1]["scene"],
        json!("hall-night"),
        "omitted scene carries"
    );
    assert_eq!(got[2]["music"], json!("battle"), "stated music replaces");
}

#[test]
fn a_split_places_a_sound_inside_the_line() {
    let segments = vec![staged("e0001", "Ta rút kiếm, chém thẳng.")];
    let answer = json!({"segments": [
        {"id": 1, "text": "Ta rút kiếm,", "music": "battle", "sound_after": "page-turn", "stop_after": "none"},
        {"id": 1, "text": "chém thẳng.", "sound_after": "none", "stop_after": "none"}
    ]})
    .to_string();
    let got = parse_sound_script(&answer, &segments, CHAPTER, &vocab()).expect("a valid split");
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0]["text"], json!("Ta rút kiếm,"));
    assert_eq!(got[1], json!({"sound": "page-turn"}));
    assert_eq!(got[2]["text"], json!("chém thẳng."));
    assert_eq!(
        got[2]["source_id"],
        json!("e0001"),
        "a split keeps the source"
    );
}

#[test]
fn a_music_token_outside_the_palette_is_refused() {
    let segments = vec![staged("e0001", "Một.")];
    let answer = json!({"segments": [
        {"id": 1, "music": "epic", "sound_after": "none", "stop_after": "none"}
    ]})
    .to_string();
    let err = parse_sound_script(&answer, &segments, "Một.", &vocab()).unwrap_err();
    assert!(err.contains("not in the palette"), "{err}");
}

#[test]
fn a_bed_opened_and_never_stopped_is_refused() {
    let segments = vec![staged("e0001", "Hai người đi đến phòng bếp.")];
    let answer = json!({"segments": [
        {"id": 1, "sound_after": "food-prep", "stop_after": "none"}
    ]})
    .to_string();
    let err = parse_sound_script(&answer, &segments, CHAPTER, &vocab()).unwrap_err();
    assert!(
        err.contains("food-prep") && err.contains("stops dead"),
        "{err}"
    );
}

#[test]
fn a_missing_line_is_refused_by_name() {
    let segments = vec![staged("e0001", "Một."), staged("e0002", "Hai.")];
    let answer = json!({"segments": [
        {"id": 1, "sound_after": "none", "stop_after": "none"}
    ]})
    .to_string();
    let err = parse_sound_script(&answer, &segments, "Một. Hai.", &vocab()).unwrap_err();
    assert!(err.contains("line 2") && err.contains("missing"), "{err}");
}

#[test]
fn the_sound_prompt_renders_the_vocabularies_and_the_lines() {
    let dir = std::env::temp_dir().join("bm-sound-prompt");
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    let context = json!({"atmosphere": "A grey morning."});
    let segments = vec![staged("e0001", "Hắn đứng đợi.")];
    let prompt = build_sound_prompt(&layout, &context, &segments).unwrap();
    assert!(prompt.contains("---SOUND OUTPUT CONTRACT---"));
    assert!(prompt.contains("A grey morning."));
    assert!(prompt.contains("Hắn đứng đợi."));
    assert!(prompt.contains("quiet"), "the music palette reaches it");
    // The rules are the adapter's own: the fixture is the vi spelling, so its
    assert!(
        prompt.contains("máu văng"),
        "the adapter's own sweep reaches the sound prompt"
    );
}

/// The rules live in `prompts/sound.txt`, not in Rust: a template that says so
#[test]
fn the_sound_rules_come_from_the_adapter_template() {
    let dir = std::env::temp_dir().join("bm-sound-template");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("prompts/sound.txt"),
        "MARKER-RULE {atmosphere}|{music_palette}|{scene_words}|{effect_tags}|{inject_sounds}|{staged_lines}",
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/scene-map.json"),
        r#"{"rules": [{"match": ["palace"], "effect": [], "level": 0.0}],
            "music_palette": {"quiet": {"tags": ["soft"], "note": "low"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/effect-pool.json"),
        r#"{"night": {"tags": ["night"], "files": ["effects/night-1.mp3"]}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/inject-pool.json"),
        r#"{"coin": {"tags": ["coin"], "files": ["injects/coin-1.mp3"], "looped": false, "dur_s": 0.6}}"#,
    )
    .unwrap();
    let layout = Layout::new(&dir);
    let segments = vec![staged("e0001", "Hắn đứng đợi.")];
    let prompt = build_sound_prompt(&layout, &json!({}), &segments).unwrap();
    assert!(prompt.contains("MARKER-RULE"), "{prompt}");
    assert!(
        prompt.contains("(not stated)"),
        "an empty atmosphere is named, not left blank: {prompt}"
    );
    for ph in [
        "{atmosphere}",
        "{music_palette}",
        "{scene_words}",
        "{effect_tags}",
        "{inject_sounds}",
        "{staged_lines}",
    ] {
        assert!(!prompt.contains(ph), "placeholder leaked: {ph}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
