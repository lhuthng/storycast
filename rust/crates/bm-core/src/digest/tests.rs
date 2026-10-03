use super::attribution::attribution_view;
use super::attribution::build_attribution_prompt;
use super::attribution::replace_or_miss;
use super::attribution::replace_prompt_section;
use super::attribution::Continuity;
use super::excerpt::content_language;
use super::excerpt::excerpt_rule;
use super::excerpt::previous_excerpts;
use super::gate::corrected_source;
use super::gate::source_text_matches;
use super::gate::source_without_written_sound;
use super::gate::validate_source_alignment;
use super::json::json_failure;
use super::json::parse_json_repaired;
use super::json::strip_fences;
use super::manual::vocabulary;
use super::parse::attach_fixed_speakers;
use super::parse::collapse_redundant_sounds;
use super::parse::lift_thought_stingers;
use super::parse::merge_rounds;
use super::parse::not_speech_ids;
use super::parse::parse_attribution;
use super::parse::parse_staged_script;
use super::parse::ANONYMOUS_SPEAKER;
use super::parse::EXCERPT_CHARS;
use super::parts::merge_contexts;
use super::parts::merge_scripts;
use super::parts::sound_gap;
use super::parts::Part;
use super::parts::Parts;
use super::prompts::build_staging_prompt;
use super::run::assemble_outcome;
use super::run::route;
use super::run::GCalls;
use super::run::Route;
use super::sound_fields::carry_forward_fields;
use super::sound_fields::expand_sound_fields;
use super::sound_fields::gap_block_p;
use super::sound_fields::sound_design_gap;
use super::validate::canonicalize_aliases;
use super::validate::complete_roster;
use super::validate::validate_attributions;
use super::validate::validate_digest_identity;
use super::*;

/// A bible that knows one character, for the tests that need a named
fn bible_with_phong() -> Value {
    json!({"characters": [
        {"name": "Dịch Phong", "personality": "wry", "voice_hint": "young male",
         "tags": ["male"]}
    ]})
}

/// The source gate with nothing retracted, which is what every test that
fn validate_source_alignment_no_retractions(
    data: &Value,
    prepared: &PreparedChapter,
) -> Result<()> {
    validate_source_alignment(data, prepared, &HashSet::new())
}

/// The chain the whole program rests on, as one test: **a crawler's output
/// `"`, `“`, `「`. So a crawler that returns a container with no quote marks
/// in it, or that picks a site which marks speech some other way, hands the
/// digest one long run of narration. From there *nothing complains*: the
/// attribution answer is complete, the source gate passes, the chapter
/// renders, every ledger row is green, and the book is read in one voice.
///
/// That is why the split is printed. A validator can only catch a model
/// disagreeing with the text it was given; it cannot catch text that never
/// offered a speaker to disagree with.
#[test]
fn the_digest_reports_a_chapter_whose_crawler_kept_no_quote_marks() {
    // The clean shape, for contrast: a real quote mark is found, and the
    // chapter genuinely has two people in it.
    let clean = "Chương 1: Gặp gỡ\n\nHắn đứng đợi. \"Ừm?\" hắn hỏi.";
    let p = prepare_chapter(clean);
    assert!(p.dialogue_count() > 0, "a real quote mark must be seen");
    let s = p.split_summary();
    assert!(s.contains("1 dialogue"), "{s}");
    assert!(!s.contains("no dialogue found"), "a false alarm: {s}");

    // The broken shape: the same prose with the quote marks gone, which is
    let stripped = "Chương 1: Gặp gỡ\n\nHắn đứng đợi. Ừm? hắn hỏi.";
    let p = prepare_chapter(stripped);
    assert_eq!(
        p.dialogue_count(),
        0,
        "the whole point: with no quote mark there is no dialogue to find"
    );
    let s = p.split_summary();
    assert!(s.contains("0 dialogue"), "{s}");
    // …and the line points at the thing to check, because a count on its
    assert!(s.contains("crawler's container selector"), "{s}");
}

/// The same line, for a chapter that is legitimately all narration, must
#[test]
fn the_split_report_does_not_blame_the_crawler_on_a_quiet_chapter() {
    let p = prepare_chapter("Chương 2: Một cảnh\n\nHắn lật trang sách.");
    assert_eq!(p.dialogue_count(), 0);
    let s = p.split_summary();
    assert!(s.contains("0 dialogue"), "{s}");
    // Conditional, because a single-voice chapter is a real thing: the line
    assert!(
        s.contains("correct if the chapter really is narration"),
        "the caveat must come before the suggestion: {s}"
    );
    assert!(!s.contains("left the crawler"), "no accusation, ever: {s}");
}

/// An empty chapter says so rather than claiming zero dialogue of a chapter
#[test]
fn the_split_report_says_so_when_there_is_nothing_to_split() {
    let p = prepare_chapter("");
    assert!(
        p.split_summary().contains("nothing to attribute"),
        "{}",
        p.split_summary()
    );
}

/// The line as it actually reaches an operator: first entry in the log of
#[test]
fn the_split_is_the_first_thing_the_digest_log_says() {
    let text = "Chương 1: Gặp gỡ\n\nHắn đứng đợi. \"Ừm?\" hắn hỏi.";
    let bible = serde_json::json!({});
    let context = serde_json::json!({"speakers": {}, "roster": []});
    let script = serde_json::json!({
        "segments": [{
            "source_id": "e0001", "speaker": "Narrator",
            "text": "Hắn đứng đợi.", "mood": "calm", "scene": "room"
        }]
    });
    let out = assemble_outcome(&bible, &context, &script, text).expect("assembles");
    assert!(
        out.log[0].contains("event(s):") && out.log[0].contains("dialogue"),
        "the split must be the first thing said, got {:?}",
        out.log[0]
    );
}

#[test]
fn fences_are_stripped() {
    assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(strip_fences("\u{feff}{\"a\":1}"), "{\"a\":1}");
    assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
}

#[test]
fn json_repair_escapes_literal_quotes_inside_values() {
    let value =
        parse_json_repaired(r#"{"text":"trên biển khắc một chữ "Võ", rồi chữ đó biến mất."}"#)
            .unwrap();
    assert_eq!(
        value["text"],
        json!("trên biển khắc một chữ \"Võ\", rồi chữ đó biến mất.")
    );

    let quoted = parse_json_repaired(r#"{"text":"he said "hello", then left"}"#).unwrap();
    assert_eq!(quoted["text"], json!("he said \"hello\", then left"));
}

#[test]
fn json_repair_handles_common_model_syntax_mistakes() {
    let value = parse_json_repaired(
        "{\n  \"text\": \"first line\nsecond line\\tand a tab\",\n  \"items\": [1, 2,],\n}",
    )
    .unwrap();
    assert_eq!(value["text"], json!("first line\nsecond line\tand a tab"));
    assert_eq!(value["items"], json!([1, 2]));

    // An apostrophe is ordinary text, not an invitation to rewrite quoting.
    let apostrophe = parse_json_repaired(r#"{"text":"Dịch Phong's shop"}"#).unwrap();
    assert_eq!(apostrophe["text"], json!("Dịch Phong's shop"));
}

/// ch79's staging answer: a `text` value carrying both an unescaped `"` and
/// a raw newline.
///
/// The control-character escaper believes it is outside a string from the
/// stray quote onward, so it emits the newline raw; the quote repair cannot
/// fix a control-character complaint without corrupting an unrelated key.
/// Quotes first, then control characters, is the only order that gets this
/// through, and it failed on this input and on its own repair before, which
/// is how a chapter was lost to `control character found while parsing a
/// string` twice over.
#[test]
fn json_repair_settles_quotes_before_control_characters() {
    let broken = "{\n  \"segments\": [\n    {\"text\": \"Trên bia khắc một chữ \"Võ\", rồi chữ đó biến mất.\nTiếng gõ phía sau vang lên.\"}\n  ]\n}";
    assert!(
        serde_json::from_str::<Value>(broken).is_err(),
        "the shape this fixes must be a parse error to begin with"
    );
    let value = parse_json_repaired(broken).unwrap();
    assert_eq!(
        value["segments"][0]["text"],
        json!("Trên bia khắc một chữ \"Võ\", rồi chữ đó biến mất.\nTiếng gõ phía sau vang lên.")
    );
}

/// The complaint the repair prompt quotes has to say what to write, not
#[test]
fn a_json_complaint_carries_its_own_remedy() {
    let err = json_failure(&serde_json::from_str::<Value>("{\"a\":").unwrap_err());
    let msg = err.to_string();
    assert!(msg.contains("EOF") || msg.contains("end of file"), "{msg}");
    assert!(
        msg.contains("whole object"),
        "and says what a model should do instead: {msg}"
    );
}

/// A literal newline inside a string is ch79's failure in miniature, and it
#[test]
fn a_raw_control_character_is_repaired_rather_than_reported() {
    let value = parse_json_repaired("{\"a\": \"one\ntwo\"}").unwrap();
    assert_eq!(value["a"], json!("one\ntwo"));
}

#[test]
fn json_repair_does_not_hide_genuinely_invalid_json() {
    assert!(parse_json_repaired("{\"a\":1").is_err());
    assert!(parse_json_repaired("not JSON").is_err());
}

#[test]
fn bible_context_is_identity_only() {
    let bible = json!({"characters": [{
        "name": "A", "personality": "p", "voice_hint": "adult male",
        "proper_aliases": ["B"], "first_seen": "01", "chapters_seen": ["01"]
    }]});
    let ctx = bible_context(&bible);
    assert!(ctx.contains("\"name\":\"A\""));
    assert!(
        !ctx.contains("chapters_seen"),
        "context leaked chapter baggage: {ctx}"
    );
}

/// The staging shape asks for `mood`/`scene`/`music`/`text` only where they
#[test]
fn omitted_staging_fields_carry_forward_from_the_previous_segment() {
    let prepared = prepare_chapter("\"Một.\"\n\nNàng gật đầu.");
    let narration = prepared
        .events
        .iter()
        .find(|e| e.kind == "narration")
        .expect("the prose event")
        .text
        .clone();
    let mut data = json!({
        "segments": [
            {"source_id": "e0001", "text": "Một.", "mood": "calm",
             "scene": "room", "music": "quiet"},
            {"source_id": "e0002"}
        ]
    });
    carry_forward_fields(&mut data, &prepared);
    let segs = data["segments"].as_array().unwrap();
    assert_eq!(segs[1]["mood"], json!("calm"));
    assert_eq!(segs[1]["scene"], json!("room"));
    assert_eq!(segs[1]["music"], json!("quiet"));
    // `text` is filled from the prepared event, so the model never re-types
    assert_eq!(segs[1]["text"], json!(narration));

    // A segment that states its own value never inherits the previous one —
    let mut split = json!({
        "segments": [
            {"source_id": "e0001", "text": "Một.", "mood": "calm"},
            {"source_id": "e0001", "text": "Hai.", "mood": "urgent"},
            {"source_id": "e0001"}
        ]
    });
    carry_forward_fields(&mut split, &prepared);
    let segs = split["segments"].as_array().unwrap();
    assert_eq!(segs[1]["mood"], json!("urgent"));
    assert_eq!(segs[2]["mood"], json!("urgent"));
    assert_eq!(segs[1]["text"], json!("Hai."));
}

/// The head of a chapter whose first bed arrives mid-way.
#[test]
fn the_lines_before_the_first_music_declaration_are_none() {
    let prepared = prepare_chapter("Một.\n\nHai.\n\nBa.");
    let mut data = json!({
        "segments": [
            {"source_id": "e0001", "text": "Một."},
            {"source_id": "e0002", "text": "Hai.", "music": "battle"},
            {"source_id": "e0003", "text": "Ba."}
        ]
    });
    carry_forward_fields(&mut data, &prepared);
    let segs = data["segments"].as_array().unwrap();
    assert_eq!(segs[0]["music"], json!("none"), "the silent head is `none`");
    assert_eq!(segs[1]["music"], json!("battle"));
    assert_eq!(segs[2]["music"], json!("battle"), "and it still carries on");

    // A chapter with no `music` anywhere is left alone: it predates the
    let mut legacy = json!({"segments": [
        {"source_id": "e0001", "text": "Một."},
        {"source_id": "e0002", "text": "Hai."}
    ]});
    carry_forward_fields(&mut legacy, &prepared);
    for seg in legacy["segments"].as_array().unwrap() {
        assert!(
            seg.get("music").is_none(),
            "nothing to say about a legacy script"
        );
    }
}

/// A reworded template must surface as a miss, not vanish. This is the
#[test]
fn a_missing_section_marker_is_reported_not_silently_skipped() {
    let mut body = String::from("1. something else entirely\n2. new_characters\n");
    assert!(!replace_prompt_section(
        &mut body,
        "1. mentions records",
        "2. new_characters",
        "NEW"
    ));
    assert_eq!(body, "1. something else entirely\n2. new_characters\n");

    let mut body = String::from("1. mentions records\n2. new_characters\n");
    assert!(replace_prompt_section(
        &mut body,
        "1. mentions records",
        "2. new_characters",
        "NEW"
    ));
    assert!(body.starts_with("NEW"), "{body}");

    let mut miss = Vec::new();
    let mut body = String::from("no marker here");
    replace_or_miss(&mut body, "{chapter_text}", "text", &mut miss);
    assert_eq!(miss.len(), 1);
}

#[test]
fn load_bible_defaults_when_missing_or_corrupt() {
    let missing = load_bible(Path::new("/nonexistent/bible.json"));
    assert_eq!(missing, json!({"characters": []}));
}

/// The two prompts and the two renderers must agree.
#[test]
fn the_two_prompts_render_their_own_placeholders() {
    let dir = std::env::temp_dir().join("bm-prompt-tags");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("prompts")).unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(
        dir.join("prompts/analyze.txt"),
        "{bible_json}|{chapter_text}",
    )
    .unwrap();
    std::fs::write(
        dir.join("prompts/script.txt"),
        "{music_palette}|{scene_words}|{effect_tags}|{inject_sounds}|{cast_json}|{bible_json}|{chapter_text}",
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/scene-map.json"),
        // A rule whose match word is deliberately NOT a bed tag, because
        r#"{"rules": [{"match": ["palace", "jade pavilion"], "effect": [], "level": 0.0}],
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
        // `looped` stated, like every shipped entry: the struct's default
        r#"{"coin": {"tags": ["coin", "metal"], "files": ["injects/coin-1.mp3"], "looped": false, "dur_s": 0.6}}"#,
    )
    .unwrap();
    let layout = Layout::new(&dir);
    let bible = json!({"characters": []});
    let context = json!({"roster": ["Narrator", "Dịch Phong"], "mentions": {"hắn": "Dịch Phong"}});

    // The cast prompt: bible and chapter, and nothing else.
    let cast = build_prompt(&layout, &bible, "text").unwrap();
    for ph in ["{bible_json}", "{chapter_text}"] {
        assert!(!cast.contains(ph), "placeholder leaked: {ph}");
    }
    assert!(
        !cast.contains("{music_palette}") && !cast.contains("{inject_sounds}"),
        "the cast prompt must not carry sound vocabulary: {cast}"
    );

    // The automatic contracts keep dialogue identity separate from staging.
    let prepared = prepare_chapter("Chương 1: Một chuyến gặp\n\n\"Ừm!\"");
    let attribution = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
    assert!(attribution.contains("---ATTRIBUTION OUTPUT CONTRACT---"));
    assert!(attribution.contains("Dialogue must NEVER map to Narrator"));
    // The contract's language is the adapter's: the vi fixture's checkout
    assert!(
        attribution.contains("in the chapter's own language"),
        "{attribution}"
    );
    assert!(
        !attribution.contains("{content_language}"),
        "an unrendered placeholder"
    );
    assert!(attribution.contains("Anonymous"));
    // The answerable list is dialogue with nearby source context; narration
    assert!(attribution.contains("narration_ids"), "{attribution}");
    assert!(attribution.contains("`dialogue_events`"), "{attribution}");
    assert!(attribution.contains("following_context"), "{attribution}");
    // Folded, because these are content assertions and the contract is
    let flat = attribution.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("explicit named dialogue tag"),
        "{attribution}"
    );
    assert!(flat.contains("scenario-dependent"), "{attribution}");
    let fixed = json!({
        "roster": ["Narrator", "anonymous:anon-1"],
        "mentions": {},
        "speakers": {"e0001": "anonymous:anon-1"}
    });
    let staging = build_staging_prompt(&layout, "vieneu", &bible, &fixed, &prepared, None).unwrap();
    assert!(staging.contains("---STAGING OUTPUT CONTRACT---"));
    assert!(staging.contains("fixed_speakers"));
    assert!(staging.contains("Do not return `speaker`"));
    // The staging path shares its template with the script path, so a
    assert!(
        !staging.contains("{scene_words}") && !staging.contains("{effect_tags}"),
        "placeholder leaked into the staging prompt: {staging}"
    );

    // The script prompt: the four vocabularies and the resolved cast.
    let p = build_script_prompt(&layout, "vieneu", &bible, &context, "text").unwrap();
    assert!(p.contains("quiet (soft; low)"), "{p}");
    assert!(p.contains("night"), "{p}");
    // The PLACE vocabulary reaches the prompt, and `palace` is in no pool,
    assert!(p.contains("jade pavilion, palace"), "{p}");
    // the inject vocabulary renders the clip's own mode first
    assert!(p.contains("coin (hit; coin, metal; 0.6s)"), "{p}");
    assert!(p.contains("\"roster\""), "{p}");
    assert!(p.contains("Dịch Phong"), "{p}");
    assert!(p.contains("hắn"), "{p}");
    for ph in [
        "{effect_tags}",
        "{scene_words}",
        "{music_palette}",
        "{inject_sounds}",
        "{cast_json}",
        "{bible_json}",
        "{chapter_text}",
    ] {
        assert!(!p.contains(ph), "placeholder leaked: {ph}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attribution_is_complete_deterministic_and_rejects_narrator_dialogue() {
    let prepared = prepare_chapter(
        "Chương 1: Một chuyến gặp\n\nDịch Phong đứng yên.\n\n\"Ừm!\"\n\nHắn gật đầu.",
    );
    assert_eq!(
        prepared
            .events
            .iter()
            .map(|e| (e.id.as_str(), e.kind.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("e0001", "narration"),
            ("e0002", "dialogue"),
            ("e0003", "narration"),
        ]
    );
    let mut data = json!({
        "roster": ["Narrator", "Dịch Phong"],
        "speakers": {
            "e0001": "Narrator",
            "e0002": "Dịch Phong",
            "e0003": "Narrator"
        }
    });
    let bible = json!({"characters": [{"name": "Dịch Phong"}]});
    validate_attributions(&data, &bible, &prepared).unwrap();

    data["speakers"]["e0002"] = json!("Narrator");
    let err = validate_attributions(&data, &bible, &prepared).unwrap_err();
    assert!(err.to_string().contains("dialogue"), "{err}");
    // The message names the answer the repair round must reach for.
    assert!(err.to_string().contains("Anonymous"), "{err}");
}

#[test]
fn attribution_normalizes_optional_cast_metadata_without_touching_speakers() {
    let prepared = prepare_chapter(
        "Chương 1: Làm cái một đời tông sư\n\nĐồ nhi Dịch Phong đứng trước Huyền Vũ tông.\n\n\"Ừm!\"\n\n\"Ký chủ: Dịch Phong.\"\n\n\"Tỷ tỷ, muội muốn mua sách không?\"\n\n\"Mở cửa!\"",
    );
    let raw = json!({
        "title": "Võ Quán Phàm Nhân",
        "atmosphere": "A quiet martial shop at dawn.",
        "roster": [
            "Narrator", "Dịch Phong", "Hệ thống", "Lạc Lan Tuyết",
            "anonymous:anon-1", "Anonymous"
        ],
        "mentions": {
            "Dịch Phong": "Dịch Phong",
            "Huyền Vũ tông": "Huyền Vũ tông",
            "Đồ nhi": "Dịch Phong",
            "not in source": "Dịch Phong"
        },
        "new_characters": [
            {
                "name": "Dịch Phong",
                "personality": "calm",
                "voice_hint": "young adult male: reserved",
                "aliases": ["Dịch Phong"]
            },
            {
                "name": "Hệ thống",
                "personality": "mechanical",
                "voice_hint": "neutral non-binary middle-aged, mechanical",
                "aliases": ["Hệ thống"]
            },
            {"personality": "nameless junk", "voice_hint": "adult male", "tags": []}
        ],
        "new_aliases": {},
        "speakers": {
            "e0001": "Narrator",
            "e0002": "Dịch Phong",
            "e0003": "Hệ thống",
            "e0004": "Lạc Lan Tuyết",
            "e0005": "anonymous:anon-1"
        }
    })
    .to_string();

    let data = parse_attribution(&raw, &json!({"characters": []}), &prepared, false).unwrap();
    let names = data["new_characters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Dịch Phong", "Hệ thống", "Lạc Lan Tuyết"]);
    assert!(!names.contains(&"nameless junk"));
    assert_eq!(
        data["new_characters"][0]["voice_hint"],
        json!("adult male: young adult male: reserved")
    );
    assert!(data["new_characters"][0]["tags"].is_array());
    assert!(data["new_characters"][1]["proper_aliases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "Hệ thống"));
    assert_eq!(data["mentions"], json!({"Dịch Phong": "Dịch Phong"}));
    assert!(
        !data["roster"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "Anonymous"),
        "an unused anonymous placeholder is metadata, not a cast decision"
    );
    assert_eq!(data["speakers"]["e0003"], json!("Hệ thống"));
}

/// The excerpt is **soft**: whitespace and length are cleaned, never
#[test]
fn the_excerpt_is_soft_on_parse_and_last_part_wins_on_merge() {
    let prepared = prepare_chapter(
        "Chương 1: Làm cái một đời tông sư\n\nĐồ nhi Dịch Phong đứng trước Huyền Vũ tông.\n\n\"Ừm!\"",
    );
    let mut raw = json!({
        "title": "Võ Quán Phàm Nhân",
        "atmosphere": "A quiet martial shop at dawn.",
        "excerpt": "  The  chapter  ends   with the stranger\n\n still unnamed, traveling with the party. ",
        "roster": ["Narrator", "Dịch Phong"],
        "speakers": {"e0001": "Narrator", "e0002": "Dịch Phong"}
    });
    let data = parse_attribution(
        &raw.to_string(),
        &json!({"characters": []}),
        &prepared,
        false,
    )
    .unwrap();
    let excerpt = data["excerpt"].as_str().unwrap();
    assert!(excerpt.starts_with("The chapter ends"), "{excerpt}");
    assert!(!excerpt.contains('\n'), "squeezed: {excerpt}");
    assert!(excerpt.ends_with("party."), "{excerpt}");

    // A runaway answer is capped, not chattered at.
    raw["excerpt"] = json!("dạ ".repeat(EXCERPT_CHARS));
    let data = parse_attribution(
        &raw.to_string(),
        &json!({"characters": []}),
        &prepared,
        false,
    )
    .unwrap();
    assert_eq!(
        data["excerpt"].as_str().unwrap().chars().count(),
        EXCERPT_CHARS
    );

    // Absent is as good as blank: the field simply comes back empty.
    raw.as_object_mut().unwrap().remove("excerpt");
    let data = parse_attribution(
        &raw.to_string(),
        &json!({"characters": []}),
        &prepared,
        false,
    )
    .unwrap();
    assert_eq!(data["excerpt"], json!(""));

    // Last non-empty wins, and an empty tail part does not erase it.
    let base = json!({
        "title": "Tiếng Hỏi Trong Sân",
        "atmosphere": "An empty courtyard at dusk.",
        "roster": [],
        "mentions": {},
        "new_characters": [],
        "new_aliases": {},
        "speakers": {}
    });
    let mut first = base.clone();
    first["excerpt"] = json!("part one ends quietly");
    let mut second = base.clone();
    second["excerpt"] = json!("part two: the reveal lands");
    let third = base;
    let parts = vec![
        staged_part(0, 4, first, json!([])),
        staged_part(4, 8, second, json!([])),
        staged_part(8, 12, third, json!([])),
    ];
    let (merged, _) = merge_contexts(&parts);
    assert_eq!(merged["excerpt"], json!("part two: the reveal lands"));
}

#[test]
fn anonymous_dialogue_uses_reusable_slots_and_never_becomes_a_character() {
    let prepared = prepare_chapter("Chương 1: Tiếng gọi\n\n\"Mở cửa!\"");
    let data = json!({
        "roster": ["Narrator", "anonymous:anon-1"],
        "speakers": {"e0001": "anonymous:anon-1"}
    });
    validate_attributions(&data, &json!({"characters": []}), &prepared).unwrap();

    let reserved_name = json!({
        "roster": ["Narrator", "anonymous:anon-1"],
        "speakers": {"e0001": "anonymous:anon-1"},
        "new_characters": [{
            "name": "anonymous:anon-1",
            "personality": "stranger",
            "voice_hint": "adult male",
            "tags": ["male"]
        }]
    });
    let err = validate_digest_identity(&reserved_name, &json!({"characters": []})).unwrap_err();
    assert!(err.to_string().contains("reserved anonymous"), "{err}");

    assert!(!is_anonymous_speaker("anonymous:anon-0"));
    assert!(!is_anonymous_speaker("anonymous:anon-01"));
    // Legacy scripts keep their numbered ids, and the current name is the
    assert!(is_anonymous_speaker("anonymous:anon-12"));
    assert!(is_anonymous_speaker(ANONYMOUS_SPEAKER));
    assert!(!is_anonymous_speaker("anonymous"));
    assert!(!is_anonymous_speaker("Người lạ"));
}

/// ch6's opening: prose, then a street hailing the same phrase twice on two
#[test]
fn narration_is_attached_by_code_and_the_map_holds_only_dialogue() {
    let prepared = prepare_chapter(
        "Chương 6: Xem như chó hoang\n\nDịch Phong bước ra khỏi cửa.\n\n\"Dịch sư phụ.\"\n\n\"Dịch sư phụ.\"",
    );
    assert_eq!(
        prepared
            .events
            .iter()
            .map(|e| (e.id.as_str(), e.kind.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("e0001", "narration"),
            ("e0002", "dialogue"),
            ("e0003", "dialogue"),
        ]
    );

    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    assert_eq!(view["narration_ids"], json!(["e0001"]));
    assert_eq!(view["dialogue_events"][0]["id"], json!("e0002"));
    assert_eq!(view["dialogue_events"][1]["id"], json!("e0003"));
    assert_eq!(view["dialogue_events"][1]["text"], json!("Dịch sư phụ."));
    assert!(view["dialogue_events"][0]["previous_context"]["text"]
        .as_str()
        .unwrap()
        .contains("Dịch Phong"));
    assert!(view["dialogue_events"][0]["following_context"].is_null());

    let bible = json!({"characters": []});
    let mut data = json!({
        "roster": ["Narrator", "anonymous:anon-1"],
        "speakers": {"e0002": "anonymous:anon-1", "e0003": "anonymous:anon-1"}
    });
    let speakers = validate_attributions(&data, &bible, &prepared).unwrap();
    assert_eq!(speakers["e0001"], "Narrator");
    assert_eq!(speakers["e0002"], "anonymous:anon-1");

    // A model that answers a narration id anyway cannot change who speaks
    data["speakers"]["e0001"] = json!("anonymous:anon-1");
    let speakers = validate_attributions(&data, &bible, &prepared).unwrap();
    assert_eq!(speakers["e0001"], "Narrator");
}

#[test]
fn a_named_tag_after_a_quote_is_attribution_evidence() {
    let prepared = prepare_chapter(
        "Chương 6: Gặp lại\n\nDịch Phong bước ra khỏi cửa.\n\n\"Sư tôn, chính là nơi này.\" Lạc Lan Tuyết vẻ mặt trịnh trọng nói.",
    );
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    let dialogue = &view["dialogue_events"][0];
    assert_eq!(dialogue["id"], json!("e0002"));
    assert_eq!(dialogue["text"], json!("Sư tôn, chính là nơi này."));
    assert_eq!(dialogue["previous_context"]["id"], json!("e0001"));
    assert_eq!(dialogue["following_context"]["id"], json!("e0003"));
    assert_eq!(
        dialogue["following_context"]["text"],
        json!("Lạc Lan Tuyết vẻ mặt trịnh trọng nói.")
    );
}

/// ch51, the shape that put a sect elder's line in the mouth of the
#[test]
fn the_quote_that_a_handoff_tag_belongs_to_says_so_itself() {
    let prepared = prepare_chapter(
        "Chương 51: Còn muốn đuổi tận giết tuyệt\n\n\"Cái gì?\" Vừa nghe xong, \
         Ninh Huyền Vũ quả nhiên nổi trận lôi đình, nhìn chằm chằm Yêu Linh Nhi từng \
         chữ từng câu hỏi: \"Ngươi nói Chấn Thiên Thạch của ta, chí bảo của Huyền Vũ tông \
         ta, bị hắn lấy ra lấp bậc thang ư?\"\n\nNhìn vẻ nổi giận của sư tôn mình, Yêu Linh Nhi thấy khó chịu trong lòng, nàng đành kiên \
         trì gật đầu nói: \"Đúng như lời sư tôn nói...\"",
    );
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    let dialogue = view["dialogue_events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["text"].as_str().unwrap().contains("Chấn Thiên Thạch"))
        .expect("the elder's question is a dialogue event");
    // The narration before it ends in `hỏi:` and hands the floor to THIS
    assert_eq!(
        dialogue["decided_by"],
        json!("previous_context"),
        "{dialogue}"
    );
    // The narration after it ends in `nói:` too, and would read as the same
    let next = view["dialogue_events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| {
            d["text"]
                .as_str()
                .unwrap()
                .contains("Đúng như lời sư tôn nói")
        })
        .expect("her reply follows");
    assert_eq!(next["decided_by"], json!("previous_context"), "{next}");
    assert_eq!(
        next["previous_context"]["id"], dialogue["following_context"]["id"],
        "the narration that did not tag the elder's line tags hers"
    );

    // And the contract has to say what to do with the field, or it is a
    let root = crate::paths::Layout::find_root().unwrap();
    let layout = crate::paths::Layout::resolve(root).unwrap();
    let prompt = build_attribution_prompt(&layout, &json!({}), &prepared, None, None).unwrap();
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("decided_by"), "{prompt}");
    assert!(
        flat.contains("Huyền Vũ tông ta"),
        "the worked example is the whole point and must survive: {prompt}"
    );
    assert!(
        flat.contains("reads the addressee as the speaker"),
        "the example has to say what the wrong answer got wrong: {prompt}"
    );
}

/// A narration that reacts to the quote before it is that quote's tag, and
#[test]
fn a_tag_after_the_quote_is_reported_as_that_quotes_following_tag() {
    let prepared = prepare_chapter(
        "Chương 6: Gặp lại\n\nDịch Phong bước ra khỏi cửa.\n\n\"Sư tôn, chính là nơi này.\" \
         Lạc Lan Tuyết vẻ mặt trịnh trọng nói.",
    );
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    let dialogue = &view["dialogue_events"][0];
    assert_eq!(
        dialogue["decided_by"],
        json!("following_context"),
        "`Lạc Lan Tuyết … nói.` reacts to the quote before it: {dialogue}"
    );
    // The prose before the quote is not a tag at all — no speech verb — so
    assert_eq!(dialogue["previous_context"]["id"], json!("e0001"));
}

/// A quote with no tag on either side says so, rather than leaving the
#[test]
fn a_quote_with_no_tag_on_either_side_reports_null() {
    let prepared = prepare_chapter(
        "Chương 2: Đi đường\n\nDịch Phong bước ra khỏi cửa.\n\n\"Đi thôi.\"\n\nTrời tối dần.",
    );
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    let dialogue = &view["dialogue_events"][0];
    assert_eq!(dialogue["decided_by"], Value::Null, "{dialogue}");
}

/// The measurement this field's name carries: ch51's elder line was
#[test]
fn the_contract_comes_before_the_chapter_it_governs() {
    let root = crate::paths::Layout::find_root().unwrap();
    let layout = crate::paths::Layout::resolve(root).unwrap();
    let prepared = prepare_chapter(
        "Chương 51: Còn muốn đuổi tận giết tuyệt\n\n\"Cái gì?\" Vừa nghe xong, \
         Ninh Huyền Vũ quả nhiên nổi trận lôi đình, từng chữ từng câu hỏi: \"Ngươi nói \
         Chấn Thiên Thạch của ta.\"",
    );
    let prompt = build_attribution_prompt(&layout, &json!({}), &prepared, None, None).unwrap();
    let contract = prompt
        .find("---ATTRIBUTION OUTPUT CONTRACT---")
        .expect("the contract is rendered");
    let chapter = prompt.find("---CHAPTER---").expect("the view is rendered");
    assert!(
        contract < chapter,
        "the rules must be reached before the data they govern"
    );
    // And the view is still whole after it: the contract is inserted, not
    let rest = &prompt[chapter + "---CHAPTER---\n".len()..];
    let end = rest.find("\n---").unwrap_or(rest.len());
    let view: Value = serde_json::from_str(rest[..end].trim())
        .unwrap_or_else(|e| panic!("the view survives the move: {e}"));
    assert!(view.get("dialogue_events").is_some(), "{view}");
    // The bible is still above both, so the contract can name it.
    assert!(prompt.find("---BIBLE---").unwrap() < contract);
}

#[test]
fn the_decided_by_field_is_the_first_thing_in_every_event() {
    let prepared = prepare_chapter(
        "Chương 51: Còn muốn đuổi tận giết tuyệt\n\n\"Cái gì?\" Vừa nghe xong, \
         Ninh Huyền Vũ quả nhiên nổi trận lôi đình, nhìn chằm chằm Yêu Linh Nhi từng \
         chữ từng câu hỏi: \"Ngươi nói Chấn Thiên Thạch của ta.\"",
    );
    let raw = attribution_view(&prepared);
    let view: Value = serde_json::from_str(&raw).unwrap();
    let entries = view["dialogue_events"].as_array().unwrap();
    assert!(!entries.is_empty());
    for entry in entries {
        let keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.first().map(|k| k.as_str()),
            Some("decided_by"),
            "the deciding field has to sort first, or the answer degrades: {keys:?}"
        );
    }
    // And alphabetically it really is first, which is what the serializer
    let at = raw.find("\"decided_by\"").expect("the field is rendered");
    let event = raw.find("\"dialogue_events\"").unwrap();
    assert!(at > event, "{raw}");
    assert!(
        raw[event..at].matches("\"id\"").count() == 0,
        "no event key may be rendered before it: {raw}"
    );
}

#[test]
fn staging_cannot_change_the_fixed_speaker() {
    let speakers = BTreeMap::from([
        ("e0001".to_string(), "Narrator".to_string()),
        ("e0002".to_string(), "anonymous:anon-1".to_string()),
    ]);
    let mut staging = json!({"segments": [
        {"source_id": "e0001", "speaker": "Dịch Phong", "text": "Trời sáng.", "kind": "thought"},
        {"source_id": "e0002", "speaker": "Narrator", "text": "Ai đó?"}
    ]});
    attach_fixed_speakers(
        &mut staging,
        &speakers,
        &HashSet::from(["e0002".to_string()]),
    )
    .unwrap();
    assert_eq!(staging["segments"][0]["speaker"], json!("Narrator"));
    assert_eq!(staging["segments"][1]["speaker"], json!("anonymous:anon-1"));
    // Identity belongs to pass one: the marker the staging model invented
    assert!(staging["segments"][0].get("kind").is_none());
    assert_eq!(staging["segments"][1]["kind"], json!("thought"));
}

/// The thought stinger the pack declares is lifted into a sibling sound
#[test]
fn a_declared_thought_sound_lifts_to_the_thoughts_seam() {
    let mut data = json!({"segments": [
        {"source_id": "e0001", "speaker": "Narrator", "text": "She looked up."},
        {"source_id": "e0002", "speaker": "Maomao", "text": "I need to get this done,", "kind": "thought"},
        {"source_id": "e0002", "speaker": "Maomao", "text": " and quickly.", "kind": "thought"},
        {"source_id": "e0003", "speaker": "Maomao", "text": "I really do.", "kind": "thought"},
        {"sound": "page-turn"},
        {"source_id": "e0004", "speaker": "Narrator", "text": "Done."}
    ]});
    lift_thought_stingers(&mut data, Some("thought-chime"));
    let segs = data["segments"].as_array().unwrap();
    assert_eq!(segs[2]["text"], json!(" and quickly."));
    assert_eq!(
        segs[3]["sound"],
        json!("thought-chime"),
        "after the last half"
    );
    assert_eq!(segs[4]["text"], json!("I really do."));
    assert_eq!(
        segs[5]["sound"],
        json!("page-turn"),
        "the answer's own sting is not doubled"
    );
    assert_eq!(segs[6]["text"], json!("Done."));
    assert_eq!(segs.len(), 7);
}

/// Nothing declared, nothing lifted: every pack that has not asked for a
#[test]
fn thoughts_lift_nothing_without_a_declared_sound() {
    let before = json!({"segments": [
        {"source_id": "e0001", "speaker": "Maomao", "text": "I need to get this done.", "kind": "thought"}
    ]});
    for sound in [None, Some(""), Some("   ")] {
        let mut data = before.clone();
        lift_thought_stingers(&mut data, sound);
        assert_eq!(data, before, "{sound:?}");
    }
}

/// A carved thought is its thinker's and never the Narrator's, and the
#[test]
fn a_carved_thought_takes_a_thinker_and_can_be_retracted() {
    let prepared = prepare_chapter(
        "Maomao looked up. I need to just get this job done. She picked up the basket.",
    );
    assert_eq!(
        prepared
            .events
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["narration", "thought", "narration"],
        "{:?}",
        prepared.events
    );
    let thought = prepared.events[1].id.clone();
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    assert_eq!(view["thought_events"][0]["id"], json!(thought));
    assert!(
        view["dialogue_events"].as_array().unwrap().is_empty(),
        "a thought is not a spoken line: {view}"
    );

    let bible = json!({"characters": [{"name": "Maomao"}]});
    let good = json!({
        "roster": ["Narrator", "Maomao"],
        "speakers": {"e0001": "Narrator", "e0002": "Maomao", "e0003": "Narrator"}
    });
    let speakers = validate_attributions(&good, &bible, &prepared).unwrap();
    assert_eq!(speakers[&thought], "Maomao");

    // A thought belongs to its thinker, so Narrator is refused outright.
    let wrong = json!({
        "roster": ["Narrator"],
        "speakers": {"e0001": "Narrator", "e0002": "Narrator", "e0003": "Narrator"}
    });
    let err = validate_attributions(&wrong, &json!({"characters": []}), &prepared).unwrap_err();
    assert!(err.to_string().contains("thought"), "{err}");

    // A `I`-voiced thought cannot be retracted at all — the retraction is
    let retracted = json!({
        "roster": ["Narrator"],
        "not_speech": [thought.clone()],
        "speakers": {"e0001": "Narrator", "e0002": "Narrator", "e0003": "Narrator"}
    });
    let err = validate_attributions(&retracted, &json!({"characters": []}), &prepared).unwrap_err();
    assert!(err.to_string().contains("first-person"), "{err}");
}

/// A first-person thought cannot be retracted into narration. The live ch1
#[test]
fn a_first_person_thought_cannot_be_retracted_as_narration() {
    let prepared = prepare_chapter(
        "Maomao looked up. Hope my old man's eating properly. She picked up the basket.",
    );
    assert_eq!(prepared.events[1].kind, "thought", "{:?}", prepared.events);
    let thought = prepared.events[1].id.clone();
    let retracted = json!({
        "roster": ["Narrator"],
        "not_speech": [thought.clone()],
        "speakers": {"e0001": "Narrator", "e0002": "Narrator", "e0003": "Narrator"}
    });
    let err = validate_attributions(&retracted, &json!({"characters": []}), &prepared).unwrap_err();
    assert!(err.to_string().contains("first-person"), "{err}");

    // The narrator aside the hatch exists for is `we`-voiced, and retracts.
    let aside = prepare_chapter(
        "They were after women for the palace; let us call them Villagers One, Two, and Three.",
    );
    let aside_event = aside
        .events
        .iter()
        .find(|e| e.text.contains("let us call them"))
        .expect("the aside is a prepared event");
    let id = aside_event.id.clone();
    assert_eq!(aside_event.kind, "thought", "{:?}", aside.events);
    // One sentence, so one event: the `;` is not a sentence boundary, and
    assert_eq!(aside.events.len(), 1, "{:?}", aside.events);
    let retracted = json!({
        "roster": ["Narrator"],
        "not_speech": [id.clone()],
        "speakers": {id.clone(): "Narrator"}
    });
    let speakers = validate_attributions(&retracted, &json!({"characters": []}), &aside).unwrap();
    assert_eq!(speakers[&id], "Narrator");
}

/// The thought marker is code-attached, so the source gate is where a hand
#[test]
fn the_source_gate_requires_the_thought_marker_to_agree() {
    let prepared = prepare_chapter("Maomao looked up. I need to just get this job done.");
    let thought = prepared.events[1].id.clone();
    let line = |kind: Option<&str>| {
        let mut line = json!({
            "source_id": thought,
            "speaker": "Maomao",
            "text": "I need to just get this job done."
        });
        if let Some(kind) = kind {
            line["kind"] = json!(kind);
        }
        line
    };
    let narrated =
        json!({"source_id": "e0001", "speaker": "Narrator", "text": "Maomao looked up."});

    let good = json!({"segments": [narrated.clone(), line(Some("thought"))], "fixes": []});
    validate_source_alignment_no_retractions(&good, &prepared).unwrap();

    let bare = json!({"segments": [narrated.clone(), line(None)], "fixes": []});
    let err = validate_source_alignment_no_retractions(&bare, &prepared).unwrap_err();
    assert!(err.to_string().contains("kind"), "{err}");

    let mut marked_narration = narrated.clone();
    marked_narration["kind"] = json!("thought");
    let on_prose = json!({"segments": [marked_narration, line(Some("thought"))], "fixes": []});
    let err = validate_source_alignment_no_retractions(&on_prose, &prepared).unwrap_err();
    assert!(err.to_string().contains("only a thought"), "{err}");

    let mut narrated_thought = line(Some("thought"));
    narrated_thought["speaker"] = json!("Narrator");
    let wrong_voice = json!({"segments": [narrated.clone(), narrated_thought], "fixes": []});
    let err = validate_source_alignment_no_retractions(&wrong_voice, &prepared).unwrap_err();
    assert!(err.to_string().contains("Narrator"), "{err}");
}

/// A pack that names a thought sound the inject pool does not have is
#[test]
fn a_declared_thought_sound_must_exist_in_the_inject_pool() {
    let dir = std::env::temp_dir().join(format!(
        "bm-thought-stinger-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("assets")).unwrap();
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
    let map = |sound: &str| {
        format!(
            r#"{{"music_palette": {{"quiet": {{"tags": ["soft"], "note": "low"}}}}, "thought": {{"sound": "{sound}"}}}}"#
        )
    };
    std::fs::write(dir.join("assets/scene-map.json"), map("coin")).unwrap();
    let layout = Layout::new(&dir);
    assert_eq!(
        vocabulary(&layout).unwrap().thought_stinger.as_deref(),
        Some("coin")
    );

    std::fs::write(dir.join("assets/scene-map.json"), map("thought-chime")).unwrap();
    let err = match vocabulary(&layout) {
        Ok(_) => panic!("a thought sound with no pooled clip must be refused"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("thought.sound"), "{err}");
    assert!(err.to_string().contains("inject-pool.json"), "{err}");

    // No rule at all is the shipped shape: nothing is lifted.
    std::fs::write(
        dir.join("assets/scene-map.json"),
        r#"{"music_palette": {"quiet": {"tags": ["soft"], "note": "low"}}}"#,
    )
    .unwrap();
    assert_eq!(vocabulary(&layout).unwrap().thought_stinger, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole thought lane, end to end: the carver prepares an unquoted
#[test]
fn a_thought_lane_marks_the_segment_and_lifts_the_pack_sound() {
    let (_dir, layout, _) = long_layout("thought-lane", 3);
    // The pack declares `coin` as its thought sound; the fixture inject pool
    let scene = layout.assets().join("scene-map.json");
    let mut map: Value = crate::read_json(&scene).expect("the fixture scene map");
    map["thought"] = json!({"sound": "coin"});
    std::fs::write(&scene, serde_json::to_string(&map).unwrap()).unwrap();

    let prepared = prepare_chapter(
        "Maomao looked up at the sky. I need to just get this job done. She picked up the basket.",
    );
    assert_eq!(prepared.events[1].kind, "thought", "{:?}", prepared.events);
    let thought = prepared.events[1].id.clone();

    let bible = json!({"characters": []});
    let cast = parse_attribution(
        &json!({
            "title": "Một Ngày Trong Cung",
            "atmosphere": "A grey morning.",
            "roster": ["Narrator", "Anonymous"],
            "mentions": {},
            "new_characters": [],
            "new_aliases": {},
            "not_speech": [],
            "speakers": {thought.clone(): "Anonymous"},
            "summary": "A serving girl starts her day."
        })
        .to_string(),
        &bible,
        &prepared,
        false,
    )
    .expect("the cast is valid");

    let vocab = vocabulary(&layout).expect("the fixture vocabulary");
    assert_eq!(vocab.thought_stinger.as_deref(), Some("coin"));
    let script = parse_staged_script(&script_answer(&prepared), &bible, &cast, &prepared, &vocab)
        .expect("the staging answer is aligned");

    let segments = script["segments"].as_array().unwrap();
    let line = segments
        .iter()
        .position(|s| s.get("source_id").and_then(Value::as_str) == Some(thought.as_str()))
        .expect("the thought segment");
    assert_eq!(segments[line]["kind"], json!("thought"));
    assert_eq!(segments[line + 1]["sound"], json!("coin"), "{segments:?}");
    // The narration on either side carries no marker and no sting.
    assert!(segments[0].get("kind").is_none(), "{:?}", segments[0]);
    assert!(
        segments[line + 2].get("kind").is_none(),
        "{:?}",
        segments[line + 2]
    );
}

#[test]
fn source_gate_requires_complete_ordered_attribution() {
    let prepared = prepare_chapter(
        "Chương 1: Một chuyến gặp\n\nDịch Phong nói với Bành Anh.\n\n\"Anh nhi, xong chưa?\" Vũ Kiệt hỏi.",
    );
    assert_eq!(prepared.events.len(), 3);
    assert_eq!(prepared.events[0].kind, "narration");
    assert_eq!(prepared.events[1].kind, "dialogue");
    let line = |id: &str, speaker: &str, text: &str| json!({"source_id": id, "speaker": speaker, "text": text});
    let good = json!({"segments": [
        line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
        line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa?"),
        line("e0003", "Narrator", "Vũ Kiệt hỏi."),
    ], "fixes": []});
    validate_source_alignment_no_retractions(&good, &prepared).unwrap();

    let dropped = json!({"segments": [
        line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
    ], "fixes": []});
    let err = validate_source_alignment_no_retractions(&dropped, &prepared).unwrap_err();
    assert!(err.to_string().contains("dropped"), "{err}");

    let wrong_owner = json!({"segments": [
        line("e0001", "Bành Anh", "Dịch Phong nói với Bành Anh."),
        line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa?"),
        line("e0003", "Narrator", "Vũ Kiệt hỏi."),
    ], "fixes": []});
    let err = validate_source_alignment_no_retractions(&wrong_owner, &prepared).unwrap_err();
    assert!(err.to_string().contains("narration"), "{err}");

    let merged = json!({"segments": [
        line("e0001", "Narrator", "Dịch Phong nói với Bành Anh."),
        line("e0002", "Vũ Kiệt", "Anh nhi, xong chưa? Vũ Kiệt hỏi."),
    ], "fixes": []});
    let err = validate_source_alignment_no_retractions(&merged, &prepared).unwrap_err();
    assert!(err.to_string().contains("changed"), "{err}");
}

/// Chapter 99's rotated script, as the regression case: three consecutive
#[test]
fn source_gate_refuses_a_rotated_speaker_row() {
    let prepared = prepare_chapter(
        "\"Người đâu, mang ba tên hỗn xược kia lên đây cho ta!\" Diệp Bắc khoát tay nói.\n\nRất nhanh, ba tên Võ Linh kia liền bị dẫn lên, vừa nhìn thấy Diệp Bắc liền lớn tiếng kêu: \"Bang chủ, ngươi làm vậy là có ý gì?\"",
    );
    assert_eq!(prepared.events.len(), 4, "{:?}", prepared.events);
    assert_eq!(prepared.events[0].kind, "dialogue");
    assert_eq!(prepared.events[1].kind, "narration");
    assert_eq!(prepared.events[2].kind, "narration");
    assert_eq!(prepared.events[3].kind, "dialogue");
    let line = |id: &str, speaker: &str, text: &str| json!({"source_id": id, "speaker": speaker, "text": text});
    // The rotation, verbatim in shape: dialogue on Narrator, narration on
    let rotated = json!({"segments": [
        line("e0001", "Narrator", "Người đâu, mang ba tên hỗn xược kia lên đây cho ta!"),
        line("e0002", "Narrator", "Diệp Bắc khoát tay nói."),
        line("e0003", "Diệp Bắc", "Rất nhanh, ba tên Võ Linh kia liền bị dẫn lên, vừa nhìn thấy Diệp Bắc liền lớn tiếng kêu:"),
        line("e0004", "Narrator", "Bang chủ, ngươi làm vậy là có ý gì?"),
    ], "fixes": []});
    let err = validate_source_alignment_no_retractions(&rotated, &prepared).unwrap_err();
    assert!(
        err.to_string().contains("Narrator"),
        "a rotated row must name the Narrator violation, got: {err}"
    );
}

#[test]
fn quote_separators_that_are_only_punctuation_are_not_prepared_as_speech() {
    let prepared = prepare_chapter("Những câu như:\n\n\"Một câu.\", \"Câu tiếp theo.\"");

    assert_eq!(prepared.events.len(), 3, "{:?}", prepared.events);
    assert_eq!(prepared.events[0].text, "Những câu như:");
    assert_eq!(prepared.events[1].text, "Một câu.");
    assert_eq!(prepared.events[2].text, "Câu tiếp theo.");
    assert_eq!(prepared.events[2].id, "e0003");
    assert!(prepared
        .events
        .iter()
        .all(|event| crate::util::has_speakable_content(&event.text)));

    let aligned = json!({"segments": [
        {"source_id": "e0001", "speaker": "Narrator", "text": "Những câu như:"},
        {"source_id": "e0002", "speaker": "Anonymous", "text": "Một câu."},
        {"source_id": "e0003", "speaker": "Anonymous", "text": "Câu tiếp theo."}
    ], "fixes": []});
    validate_source_alignment_no_retractions(&aligned, &prepared).unwrap();

    let mut corrected_separator = prepare_chapter("\"Một câu.\", \"Câu tiếp theo.\"");
    corrected_separator.events[0].text.push(',');
    let aligned = json!({"segments": [
        {"source_id": "e0001", "speaker": "Anonymous", "text": "Một câu.,"},
        {"source_id": "e0002", "speaker": "Anonymous", "text": "Câu tiếp theo."}
    ], "fixes": []});
    validate_source_alignment_no_retractions(&aligned, &corrected_separator).unwrap();
}

#[test]
fn an_entity_bearing_chapter_prepares_to_the_decoded_text() {
    // ch79 as crawled by the pre-fix crawler: numeric entities raw on
    let prepared = prepare_chapter(
        "Chương 79: Cánh cửa\n\nQuả nhiên, cánh cửa nhỏ &#x27;két&#x27; một tiếng, nhẹ nhàng khẽ mở.",
    );
    assert_eq!(prepared.events.len(), 1);
    assert_eq!(
        prepared.events[0].text,
        "Quả nhiên, cánh cửa nhỏ 'két' một tiếng, nhẹ nhàng khẽ mở."
    );
    // The model's natural, decoded answer now matches the gate.
    let data = json!({"segments": [
        {"source_id": "e0001", "speaker": "Narrator", "text": "Quả nhiên, cánh cửa nhỏ 'két' một tiếng, nhẹ nhàng khẽ mở."}
    ], "fixes": []});
    validate_source_alignment_no_retractions(&data, &prepared).unwrap();
}

#[test]
fn the_digest_does_not_edit_a_chapters_words() {
    // The digest used to strip Storya's furniture here, on the way into the
    let prepared = prepare_chapter(
        "Chương 81: Liền phòng ngự\n\nCài đặt đọc\n\nHắn đã hoàn thành nhiệm vụ.\n\nHệ thống thực thể dưới dạng chiếc đỉnh. Truyện đã hoàn thành",
    );

    // The furniture is present because nobody here was asked to remove it …
    assert!(
        prepared.prompt_json.contains("Cài đặt đọc"),
        "the digest is not the place that knows what a site prints: {}",
        prepared.prompt_json
    );
    // … and it is *owed*, not skipped: every line is an event, and a
    assert_eq!(prepared.events.len(), 3);
    assert_eq!(prepared.events[0].text, "Cài đặt đọc");
    assert_eq!(prepared.events[1].text, "Hắn đã hoàn thành nhiệm vụ.");
}

#[test]
fn a_decoded_quot_becomes_a_dialogue_boundary() {
    // `&quot;` survived the old crawler too, only as raw markup. Decoding
    let prepared =
        prepare_chapter("Chương 1: Gặp gỡ\n\n&quot;Ừm.&quot; hắn đáp, &quot;xong rồi.&quot;");
    assert_eq!(prepared.events.len(), 3);
    assert_eq!(prepared.events[0].kind, "dialogue");
    assert_eq!(prepared.events[0].text, "Ừm.");
    assert_eq!(prepared.events[1].kind, "narration");
    assert_eq!(prepared.events[1].text, "hắn đáp,");
    assert_eq!(prepared.events[2].kind, "dialogue");
    assert_eq!(prepared.events[2].text, "xong rồi.");
    let data = json!({"segments": [
        {"source_id": "e0001", "speaker": "Vũ Kiệt", "text": "Ừm."},
        {"source_id": "e0002", "speaker": "Narrator", "text": "hắn đáp,"},
        {"source_id": "e0003", "speaker": "Vũ Kiệt", "text": "xong rồi."}
    ], "fixes": []});
    validate_source_alignment_no_retractions(&data, &prepared).unwrap();
}

/// ch248's real tail, verbatim. The crawler cut the line mid-speech and
/// left a dangling backslash, so the closing `"` never arrived — and
/// every span after the last matched pair became one dialogue event. The
/// chapter read as a single voice with a green ledger row, which is the
/// mirror of the no-quotes case and the reason this is a warning.
#[test]
fn an_unclosed_quote_is_reported_rather_than_read_as_one_voice() {
    let text = concat!(
        "\"Tiền bối, không thể nói như thế chứ, hắn đi tới Nam Sa chúng ta, ",
        "dù sao cũng phải có chút thể hiện chứ!\"\n\n",
        "Hắn lắc đầu, thở dài một tiếng.\n\n",
        "\"Đúng vậy đúng vậy, cũng không thể phụ lòng nhiệt tình của chúng ta chứ!\\\n",
    );
    let prepared = prepare_chapter(text);
    assert!(
        prepared.unbalanced_at.is_some(),
        "the trailing quote is never closed, so the chapter is unbalanced"
    );
    // Everything after the last matched pair became dialogue, which is the
    assert!(prepared
        .events
        .iter()
        .any(|e| e.kind == "dialogue" && e.text.starts_with("Đúng vậy")));
    let summary = prepared.split_summary();
    assert!(
        summary.contains("still open"),
        "the summary must name the open quote: {summary}"
    );
}

/// A balanced chapter must not be warned about, or the operator learns to
#[test]
fn a_balanced_chapter_says_nothing_about_quotes() {
    let prepared = prepare_chapter("Hắn lật trang sách.\n\n\"Ngươi đọc xong chưa?\" hắn hỏi.");
    assert!(prepared.unbalanced_at.is_none());
    let summary = prepared.split_summary();
    assert!(!summary.contains("still open"), "{summary}");
    assert!(!summary.contains("no narration at all"), "{summary}");
}

/// The other end of the same blind spot: a chapter that is one quoted
#[test]
fn an_all_dialogue_chapter_is_asked_about_not_refused() {
    let prepared = prepare_chapter("\"Ký chủ: Dịch Phong.\"\n\n\"Tuổi tác: 20.\"");
    assert!(prepared.unbalanced_at.is_none());
    assert_eq!(
        prepared
            .events
            .iter()
            .filter(|e| e.kind == "narration")
            .count(),
        0
    );
    assert!(prepared.split_summary().contains("no narration at all"));
}

/// The defect this field exists for. A quoted title inside narration has
#[test]
fn a_quoted_title_in_narration_can_be_retracted_to_the_narrator() {
    let prepared = prepare_chapter(
        "Hắn lật ra cuốn sách \"Khải hoàn ca của vương triều\" bất ngờ với nội dung bên trong.\n\nDịch Phong ngẩng đầu.",
    );
    // The preparer calls it dialogue, and that is the whole problem: the
    let title = prepared
        .events
        .iter()
        .find(|e| e.text == "Khải hoàn ca của vương triều")
        .expect("the title is a prepared event");
    assert_eq!(title.kind, "dialogue");
    assert_eq!(title.id, "e0002");

    // The model retracts it and assigns Narrator: both halves agree, so it
    let data = json!({
        "roster": ["Narrator"],
        "not_speech": ["e0002"],
        "speakers": {"e0002": "Narrator"}
    });
    let speakers = validate_attributions(&data, &bible_with_phong(), &prepared).unwrap();
    assert_eq!(speakers["e0002"], "Narrator");

    let script = json!({"segments": [
        {"source_id": "e0001", "speaker": "Narrator", "text": "Hắn lật ra cuốn sách"},
        {"source_id": "e0002", "speaker": "Narrator", "text": "Khải hoàn ca của vương triều"},
        {"source_id": "e0003", "speaker": "Narrator", "text": "bất ngờ với nội dung bên trong."},
        {"source_id": "e0004", "speaker": "Narrator", "text": "Dịch Phong ngẩng đầu."}
    ], "fixes": []});
    validate_source_alignment(&script, &prepared, &not_speech_ids(&data).unwrap()).unwrap();
}

/// A span too short to be speech never becomes a dialogue event at all:
#[test]
fn a_short_quoted_term_embedded_in_prose_stays_narration() {
    let prepared =
        prepare_chapter("the hougong, the “rear palace”: the residence of the Imperial women.");
    assert_eq!(prepared.events.len(), 1, "{:?}", prepared.events);
    assert_eq!(prepared.events[0].kind, "narration");
    assert_eq!(prepared.dialogue_count(), 0);

    // A longer span in the same position still splits for the model: only
    let long = prepare_chapter(
        "the hougong, the “rear palace of the Imperial women in the capital”: the residence.",
    );
    assert!(
        long.events.iter().any(|e| e.kind == "dialogue"),
        "{:?}",
        long.events
    );
}

/// An unquoted first-person sentence carves out of narration as a `thought`
#[test]
fn a_first_person_sentence_carves_as_a_thought() {
    let prepared = prepare_chapter(
        "She lived in a cage. I need to just get this job done. Maomao picked up the basket.",
    );
    let kinds: Vec<&str> = prepared.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["narration", "thought", "narration"],
        "{:?}",
        prepared.events
    );
    assert_eq!(prepared.events[1].text, "I need to just get this job done.");
}

/// The ch1 sentence behind a live misfile: a marker seated after two
#[test]
fn a_marker_under_two_commas_is_narration_not_a_thought() {
    let prepared = prepare_chapter(
        "But Maomao, who had been making her way just fine as an apothecary, thank you very much, saw it solely as so much trouble.",
    );
    assert_eq!(prepared.events.len(), 1, "{:?}", prepared.events);
    assert_eq!(prepared.events[0].kind, "narration");
    assert_eq!(prepared.thought_count(), 0);
    // One comma of headroom is still a thought: an adverbial opening does
    let opening = prepare_chapter("In that case, I will go.");
    assert_eq!(opening.events[0].kind, "thought", "{:?}", opening.events);
}

/// A first-person novel is voiced `I` throughout: carving it would turn
#[test]
fn a_first_person_chapter_carves_nothing() {
    let chapter = "I woke up. I ate breakfast. I left the house. I saw him. I ran.";
    let prepared = prepare_chapter(chapter);
    assert_eq!(prepared.dialogue_count(), 0, "{:?}", prepared.events);
    assert_eq!(prepared.events.len(), 1, "{:?}", prepared.events);
}

/// Abbreviations over-split and rejoin: `Mr.` is not a sentence, and the
#[test]
fn an_abbreviation_does_not_strand_a_fragment() {
    let prepared = prepare_chapter("He met Mr. Smith. I must go.");
    let kinds: Vec<&str> = prepared.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, vec!["narration", "thought"], "{:?}", prepared.events);
    assert!(prepared.events[0].text.ends_with("Mr. Smith."));
    assert_eq!(prepared.events[1].text, "I must go.");
}

/// The voice guard counts second person too: a you-voiced chapter is a
#[test]
fn second_person_stays_narration() {
    let prepared = prepare_chapter("You walk in. You see him. You run. You hide. You wait.");
    assert_eq!(prepared.dialogue_count(), 0, "{:?}", prepared.events);
    assert_eq!(prepared.events.len(), 1, "{:?}", prepared.events);
}

/// Second-person musing carves like first-person: the thinker is whoever
#[test]
fn second_person_musing_carves_as_a_thought() {
    let prepared = prepare_chapter(
        "The room was empty. You know no one is going to come visit you in your own room, right? Maomao traded the basket.",
    );
    let kinds: Vec<&str> = prepared.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["narration", "thought", "narration"],
        "{:?}",
        prepared.events
    );
    assert!(prepared.events[1].text.starts_with("You know no one"));
}

/// Vietnamese second person stays out: `bạn` is as often "friend", and
#[test]
fn vietnamese_second_person_does_not_carve() {
    for text in [
        "Trong lòng Chu Vân kinh hãi, con ngươi suýt chút nữa lồi ra.",
        "Hắn gặp một người bạn cũ của hắn.",
    ] {
        let prepared = prepare_chapter(text);
        assert_eq!(
            prepared.dialogue_count(),
            0,
            "{text:?}: {:?}",
            prepared.events
        );
    }
}

/// A heading carrying a marker is filtered, not carved: carving runs
#[test]
fn a_heading_with_a_marker_is_still_just_dropped() {
    let prepared = prepare_chapter("Chapter 9: What You Mean\n\nBody here quietly.");
    assert_eq!(prepared.events.len(), 1, "{:?}", prepared.events);
    assert_eq!(prepared.events[0].kind, "narration");
    assert!(!prepared.events[0].text.contains("Chapter"));
}

/// A narrator aside still matches a marker, so it carves — and the model
#[test]
fn a_narrator_aside_carves_for_the_model_to_retract() {
    let prepared = prepare_chapter(
        "They were after women for the palace; let us call them Villagers One, Two, and Three.",
    );
    let aside = prepared
        .events
        .iter()
        .find(|e| e.text.contains("let us call them"))
        .expect("the aside is a prepared event");
    assert_eq!(aside.kind, "thought");
}

/// Vietnamese first person carves like English.
#[test]
fn vietnamese_first_person_carves() {
    let prepared = prepare_chapter("Hắn lật trang sách. Tôi cần phải đi. Hắn gật đầu.");
    let kinds: Vec<&str> = prepared.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["narration", "thought", "narration"],
        "{:?}",
        prepared.events
    );
    assert_eq!(prepared.events[1].text, "Tôi cần phải đi.");
}

/// A translated term stays put while a thought beside it carves: the two
#[test]
fn a_thought_beside_a_quoted_term_carves_only_the_thought() {
    let prepared = prepare_chapter("the hougong, the “rear palace”: the residence. I need to just get this job done. Maomao picked up the basket.");
    let kinds: Vec<&str> = prepared.events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["narration", "thought", "narration"],
        "{:?}",
        prepared.events
    );
    assert_eq!(prepared.events[1].text, "I need to just get this job done.");
}

/// A long thought after normal prose trips no quote gate: the sentence
#[test]
fn a_long_thought_after_prose_trips_no_gate() {
    let thought = "I need to just get this job done as quickly as I possibly can without dropping anything on the flagstones today, she told herself.";
    assert!(
        thought.chars().count() > 120,
        "the fixture must clear the gate's length guard"
    );
    let text = format!("Maomao walked through the vast eastern quarter with her heavy basket. {thought} She hurried on.");
    let prepared = prepare_chapter(&text);
    assert!(
        prepared
            .events
            .iter()
            .any(|e| e.kind == "thought" && e.text == thought),
        "{:?}",
        prepared.events
    );
    assert!(
        quote_findings(&text).is_empty(),
        "{:?}",
        quote_findings(&text)
    );
}

/// A short quote handed over from running prose is still speech: the
#[test]
fn a_short_quote_after_a_handover_mark_stays_dialogue() {
    // Punctuation hands over: question, period, comma, colon.
    for text in [
        "\"What is it called?\" \"Cacao,\" Maomao replied.",
        "She wiped the counter. \"Ugh,\" he said.",
        "Finally he said, \"xiao Mao, then,\" a diminutive form.",
        "Maomao said simply: \"I understand,\" and went back.",
    ] {
        let prepared = prepare_chapter(text);
        assert!(
            prepared.events.iter().any(|e| e.kind == "dialogue"),
            "{text:?} lost its dialogue: {:?}",
            prepared.events
        );
    }
    // Nothing before the opener on the line: a speech opening its paragraph.
    let opening = prepare_chapter("\"Just leave it there.\" Within, a consort sipped.");
    assert_eq!(
        opening
            .events
            .iter()
            .filter(|e| e.kind == "dialogue")
            .count(),
        1,
        "{:?}",
        opening.events
    );
}

/// A headline glued to a quote hands the line over instead of joining it:
#[test]
fn a_headline_glued_to_a_quote_does_not_swallow_the_line() {
    let prepared =
        prepare_chapter("Chapter 25: Wine \"What terrible news,\" Consort Gyokuyou said.");
    let speech = prepared
        .events
        .iter()
        .find(|e| e.kind == "dialogue")
        .expect("the line stays dialogue");
    assert_eq!(speech.text, "What terrible news,");
    assert!(
        !prepared
            .events
            .iter()
            .any(|e| e.text.contains("Chapter 25")),
        "the headline is filtered, not narrated: {:?}",
        prepared.events
    );
}

/// The case a keyword list would get wrong, and the reason retraction is
#[test]
fn the_same_words_spoken_aloud_stay_dialogue() {
    let prepared = prepare_chapter(
        "Hắn lật ra cuốn sách \"Khải hoàn\" bất ngờ với nội dung bên trong.\n\n\"Ngươi đọc 'Yêu Đại Giới' chưa?\" hắn hỏi.",
    );
    let spoken = prepared
        .events
        .iter()
        .find(|e| e.text.contains("Yêu Đại Giới"))
        .expect("the spoken title is a prepared event");
    // The short title in narration is one narration event now, delimiters
    let title = prepared
        .events
        .iter()
        .find(|e| e.text.contains("Khải hoàn"))
        .expect("the narrated title is a prepared event");
    assert_eq!(title.kind, "narration");
    assert_ne!(spoken.id, title.id, "two distinct events");
    assert_eq!(spoken.kind, "dialogue");

    // Absent from `not_speech`, the spoken one is ordinary dialogue and is
    let data = json!({
        "roster": ["Narrator", "Dịch Phong"],
        "not_speech": [],
        "speakers": {
            spoken.id.clone(): "Dịch Phong"
        }
    });
    let not_speech = not_speech_ids(&data).unwrap();
    assert!(
        !not_speech.contains(&spoken.id),
        "the spoken title is not retracted"
    );
    let speakers = validate_attributions(&data, &bible_with_phong(), &prepared).unwrap();
    assert_eq!(speakers[&spoken.id], "Dịch Phong");

    // And retracting *this* one is not a free pass: it would narrate a
    let retracted = json!({
        "roster": ["Narrator", "Dịch Phong"],
        "not_speech": [spoken.id.clone()],
        "speakers": {
            spoken.id.clone(): "Narrator"
        }
    });
    let speakers = validate_attributions(&retracted, &bible_with_phong(), &prepared).unwrap();
    assert_eq!(speakers[&spoken.id], "Narrator");
}

/// A disagreement resolves in favour of the listing, and does not fail the
#[test]
fn a_retraction_wins_over_a_disagreeing_speaker() {
    let prepared =
        prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn ca của vương triều\" bên trong.");
    let contradiction = json!({
        "roster": ["Narrator", "Dịch Phong"],
        "not_speech": ["e0002"],
        "speakers": {"e0002": "Dịch Phong"}
    });
    let speakers = validate_attributions(&contradiction, &bible_with_phong(), &prepared).unwrap();
    assert_eq!(
        speakers["e0002"], "Narrator",
        "the listing is the decision; the speaker it contradicts is overwritten"
    );

    // Listed with no speaker at all is the same case, and used to be
    let orphan = json!({
        "roster": ["Narrator"],
        "not_speech": ["e0002"],
        "speakers": {}
    });
    let speakers = validate_attributions(&orphan, &json!({"characters": []}), &prepared).unwrap();
    assert_eq!(speakers["e0002"], "Narrator");
}

/// Narration still cannot be promoted to dialogue, and an unlisted
#[test]
fn the_retraction_is_one_way_only() {
    let prepared = prepare_chapter("Hắn bước ra cửa.\n\n\"Ngươi đi đâu?\" hắn hỏi.");
    let narration = prepared
        .events
        .iter()
        .find(|e| e.kind == "narration")
        .expect("there is narration");
    let speech = prepared
        .events
        .iter()
        .find(|e| e.kind == "dialogue")
        .expect("there is dialogue");

    // Listing a narration id does not make it answerable as speech. The
    let hoisted = json!({
        "roster": ["Narrator", "Dịch Phong"],
        "not_speech": [narration.id.clone()],
        "speakers": {
            narration.id.clone(): "Dịch Phong",
            speech.id.clone(): "Dịch Phong"
        }
    });
    let speakers = validate_attributions(&hoisted, &bible_with_phong(), &prepared).unwrap();
    assert_eq!(
        speakers[&narration.id], "Narrator",
        "a claimed speaker for prose must be rewritten, not honoured"
    );
    assert_eq!(speakers[&speech.id], "Dịch Phong");

    // And a dialogue id with no retraction and no speaker is still the
    let ignored = json!({"roster": ["Narrator"], "speakers": {}});
    let err = validate_attributions(&ignored, &json!({"characters": []}), &prepared).unwrap_err();
    assert!(err.to_string().contains("dropped source event"), "{err}");
}

/// The complaint is the entire input to the one repair pass, so it has to
#[test]
fn a_dropped_event_complaint_quotes_the_line_it_names() {
    let prepared = prepare_chapter(
        "Gấu đen rụt đầu vào trong khe.\n\n\"Các ngươi không thấy ta đâu.\"\n\n\"A!\"",
    );
    // One event answered, one dropped, so the complaint is about a known id
    let short = prepared
        .events
        .iter()
        .find(|e| e.text == "A!")
        .expect("the exclamation is a prepared dialogue event");
    let other = prepared
        .events
        .iter()
        .find(|e| e.kind == "dialogue" && e.id != short.id)
        .expect("there is a second dialogue event");
    let err = validate_attributions(
        &json!({
            "roster": ["Narrator", "anonymous:anon-1"],
            "speakers": {other.id.clone(): "anonymous:anon-1"}
        }),
        &json!({"characters": []}),
        &prepared,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(&short.id), "{msg}");
    assert!(
        msg.contains("A!"),
        "the complaint must quote the dropped line so one repair can fix it: {msg}"
    );
}

/// Same for the not-in-roster complaint, which ch347 hit on a second id in
#[test]
fn a_roster_complaint_quotes_the_line_too() {
    let prepared = prepare_chapter("Hắn ngồi xuống.\n\n\"Ngươi đi đâu đấy?\" hắn hỏi.");
    let speech = prepared
        .events
        .iter()
        .find(|e| e.kind == "dialogue")
        .expect("there is dialogue");
    let err = validate_attributions(
        &json!({"roster": ["Narrator"], "speakers": {speech.id.clone(): "Ghost"}}),
        &json!({"characters": [{"name": "Ghost", "personality": "x",
                                "voice_hint": "adult male", "tags": ["male"]}]}),
        &prepared,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("Ngươi đi đâu đấy?"),
        "the complaint must quote the line: {err}"
    );
}

/// A malformed retraction is refused, not skipped. Silently ignoring a
#[test]
fn a_malformed_retraction_is_refused_rather_than_ignored() {
    let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
    let err = validate_attributions(
        &json!({"roster": ["Narrator"], "not_speech": "e0002", "speakers": {}}),
        &json!({"characters": []}),
        &prepared,
    )
    .unwrap_err();
    assert!(err.to_string().contains("must be an array"), "{err}");

    let err = validate_attributions(
        &json!({"roster": ["Narrator"], "not_speech": [7], "speakers": {}}),
        &json!({"characters": []}),
        &prepared,
    )
    .unwrap_err();
    assert!(err.to_string().contains("non-string"), "{err}");
}

/// An absent field means the answer agrees with the preparer, which is
#[test]
fn an_absent_retraction_field_means_no_retraction() {
    assert!(not_speech_ids(&json!({})).unwrap().is_empty());
    assert!(not_speech_ids(&json!({"not_speech": null}))
        .unwrap()
        .is_empty());
    assert!(not_speech_ids(&json!({"not_speech": []}))
        .unwrap()
        .is_empty());
}

/// **This is the test that would have caught the field not working.**
/// contract listed the allowed keys and said "Dialogue must NEVER map to
/// Narrator" with no exception, and a model reads the contract, not the
#[test]
fn the_prompt_contract_offers_the_retraction_not_only_the_note() {
    // The contract is appended in code, not only in the profile template,
    let root = crate::paths::Layout::find_root().unwrap();
    let layout = crate::paths::Layout::resolve(root).unwrap();
    let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
    let prompt = build_attribution_prompt(&layout, &json!({}), &prepared, None, None).unwrap();
    assert!(
        prompt.contains("not_speech"),
        "the schema block must show the field, or a model returning the \
         documented shape has no way to reach it"
    );
    assert!(
        prompt.contains("The ONE exception"),
        "the absolute 'dialogue is NEVER Narrator' rule needs its exception \
         next to it, not in a note"
    );
    // The exception must be an exception and not a replacement: a real
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("are real dialogue and stay with a character"),
        "the contract must still forbid Narrator for real speech"
    );
    // Unquoted first-person thoughts are answerable dialogue too, with
    assert!(
        prompt.contains("inner thought"),
        "the contract must tell the model what an unquoted first-person passage is"
    );
    // Two live misattributions from ch1 of the apothecary book: passages
    assert!(
        prompt.contains("saying their own name in third"),
        "the contract must forbid voicing a character's third-person self-naming"
    );
    assert!(
        prompt.contains("viewpoint character"),
        "the contract must give untagged musings to the viewpoint character"
    );
    // And the view's note must not restate the rules, or the two can
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    let note = view["note"].as_str().unwrap();
    assert!(
        !note.contains("NEVER"),
        "rules live in the contract: {note}"
    );
}

/// A chapter whose prose says `Ninh Huyền Vũ` while the bible's canonical
#[test]
fn the_contract_says_which_string_a_speaker_is() {
    let root = crate::paths::Layout::find_root().unwrap();
    let layout = crate::paths::Layout::resolve(root).unwrap();
    let prepared = prepare_chapter("\"Cái gì?\"");
    let prompt = build_attribution_prompt(&layout, &json!({}), &prepared, None, None).unwrap();
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("`name` from INPUT 1, copied exactly"),
        "the canonical name has to be named as a field, or the model copies \
         the chapter's spelling: {prompt}"
    );
    assert!(
        flat.contains("Ninh Huyền Vũ"),
        "the rule needs the exact case that cost the round, or it reads as \
         a general caution nobody applies: {prompt}"
    );
}

/// The prompt has to offer the field, or the model cannot use it. The
#[test]
fn the_attribution_view_offers_the_retraction() {
    let prepared = prepare_chapter("Hắn lật ra cuốn sách \"Khải hoàn\" bên trong.");
    let view: Value = serde_json::from_str(&attribution_view(&prepared)).unwrap();
    let note = view["note"].as_str().unwrap();
    assert!(note.contains("not_speech"), "{note}");
    // And it must keep saying the rest, or the field reads as a licence
    assert!(note.contains("every `dialogue_events`"), "{note}");
    assert!(note.contains("`thought_events` id"), "{note}");
}

#[test]
fn a_written_sound_is_collapsed_rather_than_left_beside_its_tag() {
    let mut data = json!({"segments": [
        {"source_id": "e0001", "speaker": "Narrator", "text": "[hắng giọng] Khụ khụ khụ, ban đầu ta cầm bảo đao."},
        {"source_id": "e0002", "speaker": "Narrator", "text": "Khụ khụ khụ, ban đầu ta cầm bảo đao."},
        {"source_id": "e0003", "speaker": "Narrator", "text": "Hắn lật trang sách."},
    ]});
    collapse_redundant_sounds(&mut data);
    // The tag and the words it stands for are spoken as one cough, and the
    for i in [0, 1] {
        assert_eq!(
            data["segments"][i]["text"], "[hắng giọng] ban đầu ta cầm bảo đao.",
            "segment {i}"
        );
    }
    assert_eq!(data["segments"][2]["text"], "Hắn lật trang sách.");
}

#[test]
fn written_laughter_is_recognized_wherever_it_sits_in_the_line() {
    // The engine counts `haha`, one word, and a laugh in the middle of a
    assert_eq!(
        retag_text("Vài ngày nữa trời lạnh, haha, vừa sạch sẽ."),
        Some("Vài ngày nữa trời lạnh, [cười] vừa sạch sẽ.".into())
    );
    assert_eq!(
        retag_text("Khụ khụ khụ, ban đầu ta cầm bảo đao."),
        Some("[hắng giọng] ban đầu ta cầm bảo đao.".into())
    );
    // Ignoring written sound is symmetric now: the stripper runs on both
    assert_eq!(
        source_without_written_sound("…cho mình, haha, vừa sạch sẽ."),
        source_without_written_sound("…cho mình, [cười] vừa sạch sẽ.")
    );
}

#[test]
fn vietnamese_sigh_quantifiers_normalize_like_the_engine_tag() {
    for (written, tagged) in [
        (
            "Bành Anh thở dài một tiếng, nói:",
            "Bành Anh [thở dài] nói:",
        ),
        (
            "Bành Anh thở dài một hơi rồi mới cất lời:",
            "Bành Anh [thở dài] mới cất lời:",
        ),
    ] {
        let mut data = json!({"segments": [
            {"source_id": "e0015", "speaker": "Bành Anh", "text": written}
        ]});
        collapse_redundant_sounds(&mut data);
        assert_eq!(data["segments"][0]["text"], json!(tagged));
        assert!(
            source_text_matches(written, &[tagged.to_string()]),
            "source gate must accept the engine tag as the written sigh: {written}"
        );
    }

    let mut model_shape = json!({"segments": [
        {"source_id": "e0015", "speaker": "Bành Anh", "text": "Bành Anh [thở dài] một tiếng, nói:"}
    ]});
    collapse_redundant_sounds(&mut model_shape);
    assert_eq!(
        model_shape["segments"][0]["text"],
        json!("Bành Anh [thở dài] nói:")
    );
}

/// The sentence's own period, which went with the sound on the way in.
#[test]
fn a_period_beside_an_engine_tag_is_not_a_source_change() {
    let expected = "Cũng may, cũng may không bị trượt tay, [cười]";
    assert!(
        source_text_matches(
            expected,
            &["Cũng may, cũng may không bị trượt tay, [cười].".to_string()]
        ),
        "the tag's own period must not read as a change"
    );
    assert!(source_text_matches(
        "Hắn gầm lên, [hắng giọng]",
        &["Hắn gầm lên, [hắng giọng]!".to_string()]
    ));
    // The tag *and* the words it stands for is still the error it is: only
    assert!(!source_text_matches(
        expected,
        &["Cũng may, cũng may không bị trượt tay, [cười] ha ha.".to_string()]
    ));
}

#[test]
fn source_gate_names_a_leftover_written_sound() {
    let prepared = prepare_chapter(
        "Chương 1: Một chuyến gặp\n\n\"Khụ khụ khụ, ban đầu ta cầm bảo đao.\" Thanh Sơn lão tổ nói.",
    );
    let dialogue = prepared
        .events
        .iter()
        .find(|e| e.kind == "dialogue")
        .expect("the cough line is prepared");
    let tail = prepared
        .events
        .iter()
        .find(|e| e.kind == "narration")
        .expect("its tag is prepared");
    assert!(corrected_source(dialogue, &[]).starts_with("[hắng giọng]"));

    let answer = |cough: &str| {
        json!({"segments": [
            {"source_id": dialogue.id, "speaker": "Thanh Sơn lão tổ", "text": cough},
            {"source_id": tail.id, "speaker": "Narrator", "text": tail.text},
        ], "fixes": []})
    };

    // Hoisting the tag to the head of the line and keeping the words is
    let hoisted = answer("[hắng giọng] Khụ khụ khụ, ban đầu ta cầm bảo đao.");
    let err = validate_source_alignment_no_retractions(&hoisted, &prepared).unwrap_err();
    assert!(err.to_string().contains("written sound"), "{err}");

    // The same answer passes once it comes through `retag_text`, which is
    let mut collapsed = hoisted.clone();
    collapse_redundant_sounds(&mut collapsed);
    validate_source_alignment_no_retractions(&collapsed, &prepared).unwrap();

    // A genuine change keeps the honest generic message: the hint fires
    let rewritten = answer("Đêm ấy trời trở gió.");
    let err = validate_source_alignment_no_retractions(&rewritten, &prepared).unwrap_err();
    assert!(err.to_string().contains("changed"), "{err}");
    assert!(!err.to_string().contains("written sound"), "{err}");
}

/// The ch51 shape: right person, the chapter's spelling. It used to cost
#[test]
fn an_unambiguous_alias_is_rewritten_before_the_gate_sees_it() {
    let bible = json!({"characters": [
        {"name": "Huyền Vũ lão tổ", "proper_aliases": ["Ninh Huyền Vũ", "Huyền Vũ"]},
        {"name": "Yêu Linh Nhi", "proper_aliases": []},
    ]});
    let mut data = json!({
        "roster": ["Narrator", "Yêu Linh Nhi", "Ninh Huyền Vũ"],
        "mentions": {"Ninh Huyền Vũ": "Ninh Huyền Vũ"},
        "speakers": {"e0001": "Yêu Linh Nhi", "e0010": "Ninh Huyền Vũ"},
        "segments": [{"speaker": "Ninh Huyền Vũ", "text": "Chí bảo của ta."}],
    });
    let fixes = canonicalize_aliases(&mut data, &bible);

    assert_eq!(
        data["roster"],
        json!(["Narrator", "Yêu Linh Nhi", "Huyền Vũ lão tổ"])
    );
    assert_eq!(data["speakers"]["e0010"], json!("Huyền Vũ lão tổ"));
    assert_eq!(
        data["speakers"]["e0001"],
        json!("Yêu Linh Nhi"),
        "a real name is left alone"
    );
    assert_eq!(data["mentions"]["Ninh Huyền Vũ"], json!("Huyền Vũ lão tổ"));
    assert!(
        fixes.iter().any(|f| f.contains("Huyền Vũ lão tổ")),
        "{fixes:?}"
    );

    // What the fix buys: the gate now accepts an answer it used to refuse,
    validate_digest_identity(&data, &bible).unwrap();
}

/// The rewrite must not become a licence. An alias two characters claim is
#[test]
fn an_ambiguous_alias_is_still_refused_and_never_guessed() {
    let bible = json!({"characters": [
        {"name": "Vân Thăng", "proper_aliases": ["Vân gia chủ"]},
        {"name": "Vân Lam", "proper_aliases": ["Vân gia chủ"]},
    ]});
    let mut data = json!({
        "roster": ["Narrator", "Vân gia chủ"],
        "speakers": {"e0001": "Vân gia chủ"},
    });
    let fixes = canonicalize_aliases(&mut data, &bible);
    assert!(fixes.is_empty(), "two owners means no rewrite: {fixes:?}");
    assert_eq!(data["roster"], json!(["Narrator", "Vân gia chủ"]));

    // Unchanged, so the gate says what it always said.
    let err = validate_digest_identity(&data, &bible).unwrap_err();
    assert!(
        err.to_string()
            .contains("not a canonical known/new character name"),
        "{err}"
    );

    // And a name no character claims at all is not an alias either.
    let mut stranger = json!({"roster": ["Narrator", "Kẻ lạ mặt"], "speakers": {}});
    assert!(canonicalize_aliases(&mut stranger, &bible).is_empty());
    assert!(validate_digest_identity(&stranger, &bible).is_err());
}

/// A name listed as somebody's alias *and* standing as its own canonical
#[test]
fn canonical_names_and_reserved_speakers_are_never_rewritten() {
    let bible = json!({"characters": [
        {"name": "Lục Thanh Sơn", "proper_aliases": ["Thanh Sơn lão tổ"]},
        // Somebody else also carries the bare name as an alias — a clash the
        {"name": "Thanh Sơn", "proper_aliases": []},
    ]});
    let mut data = json!({
        "roster": ["Narrator", "anonymous:anon-1", "Thanh Sơn"],
        "speakers": {"e0001": "Thanh Sơn", "e0002": "anonymous:anon-1"},
    });
    let fixes = canonicalize_aliases(&mut data, &bible);
    assert!(fixes.is_empty(), "{fixes:?}");
    assert_eq!(
        data["roster"],
        json!(["Narrator", "anonymous:anon-1", "Thanh Sơn"])
    );
    assert_eq!(data["speakers"]["e0002"], json!("anonymous:anon-1"));
}

/// An answer that spells one character both ways collapses to one roster
#[test]
fn the_rewrite_deduplicates_a_roster_that_named_one_character_twice() {
    let bible = json!({"characters": [
        {"name": "Huyền Vũ lão tổ", "proper_aliases": ["Ninh Huyền Vũ"]},
    ]});
    let mut data = json!({
        "roster": ["Narrator", "Huyền Vũ lão tổ", "Ninh Huyền Vũ"],
        "speakers": {"e0001": "Ninh Huyền Vũ"},
    });
    canonicalize_aliases(&mut data, &bible);
    assert_eq!(data["roster"], json!(["Narrator", "Huyền Vũ lão tổ"]));
    validate_digest_identity(&data, &bible).unwrap();
}

/// ch51's `Được!`: assigned to the reserved crowd speaker, with `Anonymous`
#[test]
fn a_used_speaker_missing_from_the_roster_is_added_not_refused() {
    let bible = json!({"characters": [{"name": "Lan", "proper_aliases": []}]});
    let prepared = prepare_chapter("Trời tối. \"Ngươi đi đi.\" \"Được!\"");
    let mut data = json!({
        "roster": ["Narrator", "Lan"],
        "speakers": {"e0002": "Lan", "e0003": "anonymous:anon-1"},
    });
    let fixes = complete_roster(&mut data, &bible, &prepared);
    assert!(
        fixes.iter().any(|f| f.contains("anonymous:anon-1")),
        "{fixes:?}"
    );
    let roster: Vec<&str> = data["roster"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(roster.contains(&"anonymous:anon-1"), "{roster:?}");
    assert!(
        roster.contains(&"Narrator"),
        "narration needs its speaker listed"
    );
    // Nothing is duplicated when it was already there, and the answer's own
    assert_eq!(roster.iter().filter(|n| **n == "Lan").count(), 1);
    assert_eq!(data["speakers"]["e0003"], json!("anonymous:anon-1"));
}

/// The completion must not become an admission: a name nobody in the bible
#[test]
fn complete_roster_never_admits_a_stranger() {
    let bible = json!({"characters": [{"name": "Lan", "proper_aliases": []}]});
    let prepared = prepare_chapter("Trời tối. \"Ngươi đi đi.\"");
    let mut data = json!({
        "roster": ["Narrator"],
        "speakers": {"e0002": "Kẻ lạ mặt"},
    });
    let fixes = complete_roster(&mut data, &bible, &prepared);
    assert!(
        fixes.is_empty(),
        "a stranger is not a roster entry: {fixes:?}"
    );
    assert_eq!(data["roster"], json!(["Narrator"]));
    // And the gate that would have been satisfied by adding it still
    let err = validate_attributions(&data, &bible, &prepared).unwrap_err();
    assert!(err.to_string().contains("Kẻ lạ mặt"), "{err}");
    assert!(
        err.to_string().contains("not in the chapter roster"),
        "{err}"
    );

    // The contrast that proves the point: the same shape with a name the
    let mut known = json!({
        "roster": ["Narrator"],
        "speakers": {"e0002": "Lan"},
    });
    assert!(!complete_roster(&mut known, &bible, &prepared).is_empty());
    validate_attributions(&known, &bible, &prepared).unwrap();
}

#[test]
fn roster_entry_that_is_a_bible_alias_names_the_canonical_form() {
    let bible = json!({"characters": [
        {"name": "Vũ Kiệt", "proper_aliases": ["Vu Vũ Kiệt"]},
    ]});
    // The alias has no voice to resolve to: `speaker` is matched against
    let aliased = json!({
        "roster": ["Narrator", "Vu Vũ Kiệt"],
        "segments": [{"speaker": "Vu Vũ Kiệt", "text": "Anh nhi, xong chưa?"}],
    });
    let err = validate_digest_identity(&aliased, &bible).unwrap_err();
    assert!(err.to_string().contains("alias"), "{err}");
    assert!(err.to_string().contains("Vũ Kiệt"), "{err}");

    // Canonical name in `roster`, alias in `mentions`: the contract.
    let canonical = json!({
        "roster": ["Narrator", "Vũ Kiệt"],
        "mentions": {"Vu Vũ Kiệt": "Vũ Kiệt"},
        "segments": [{"speaker": "Vũ Kiệt", "text": "Anh nhi, xong chưa?"}],
    });
    validate_digest_identity(&canonical, &bible).unwrap();
}

#[test]
fn a_mention_may_name_a_character_who_does_not_speak_here() {
    let bible = json!({"characters": [
        {"name": "Vũ Kiệt", "proper_aliases": ["Vu Vũ Kiệt"]},
        {"name": "Thanh Sơn lão tổ", "proper_aliases": []},
    ]});
    // `roster` is the speaker list, so a character named only in the
    let data = json!({
        "roster": ["Narrator", "Thanh Sơn lão tổ"],
        "mentions": {"Vu Vũ Kiệt": "Vũ Kiệt"},
        "segments": [{"speaker": "Narrator", "text": "Thanh Sơn lão tổ hiện ra trước Vu Vũ Kiệt."}],
    });
    validate_digest_identity(&data, &bible).unwrap();

    // The owner still has to be the canonical name, never the surface form.
    let self_mapped = json!({
        "roster": ["Narrator", "Thanh Sơn lão tổ"],
        "mentions": {"Vu Vũ Kiệt": "Vu Vũ Kiệt"},
        "segments": [{"speaker": "Narrator", "text": "Thanh Sơn lão tổ hiện ra trước Vu Vũ Kiệt."}],
    });
    let err = validate_digest_identity(&self_mapped, &bible).unwrap_err();
    assert!(err.to_string().contains("owned by"), "{err}");
    assert!(err.to_string().contains("Vũ Kiệt"), "{err}");
}

#[test]
fn source_gate_allows_sound_splits_under_one_id() {
    let prepared = prepare_chapter("Chương 1: Một cảnh\n\nHắn lật trang sách.");
    let data = json!({"segments": [
        {"source_id": "e0001", "speaker": "Narrator", "text": "Hắn lật"},
        {"source_id": "e0001", "speaker": "Narrator", "text": "trang sách."},
    ], "fixes": []});
    validate_source_alignment_no_retractions(&data, &prepared).unwrap();
}

#[test]
fn source_gate_rejects_a_split_that_changes_speaker() {
    let prepared = prepare_chapter("Chương 1: Một cảnh\n\n\"Anh nhi, xong chưa?\"");
    let data = json!({"segments": [
        {"source_id": "e0001", "speaker": "Vũ Kiệt", "text": "Anh nhi,"},
        {"source_id": "e0001", "speaker": "Bành Anh", "text": "xong chưa?"},
    ], "fixes": []});
    let err = validate_source_alignment_no_retractions(&data, &prepared).unwrap_err();
    assert!(err.to_string().contains("split across speakers"), "{err}");
}

#[test]
fn source_gate_ignores_repeated_standalone_headlines() {
    let prepared = prepare_chapter(
        "Chương 1: Một cảnh\n\nHắn bước đi.\n\nChương 1: Một cảnh\n\nHắn dừng lại.",
    );
    assert_eq!(prepared.events.len(), 2);
    assert_eq!(prepared.events[0].text, "Hắn bước đi.");
    assert_eq!(prepared.events[1].text, "Hắn dừng lại.");
}

/// The sound fields come off the lines and become items at their seams.
#[test]
fn expand_sound_fields_lifts_the_seam_out_of_the_line() {
    let line = |text: &str| json!({"speaker": "Narrator", "text": text});
    let mut a = line("Nàng lau mồ hôi trên trán, siết chặt cuốn võ thư trong tay");
    a["sound_after"] = json!("page-turn");
    a["stop_after"] = json!("none");
    let b = line(", như nhặt được báu vật.");
    let got = expand_sound_fields(&[a, b.clone()]).unwrap();
    assert_eq!(got.len(), 3, "{got:?}");
    // The half keeps its text, word for word, and loses both fields.
    assert_eq!(
        got[0]["text"],
        json!("Nàng lau mồ hôi trên trán, siết chặt cuốn võ thư trong tay")
    );
    assert!(
        got[0].get("sound_after").is_none() && got[0].get("stop_after").is_none(),
        "{:?}",
        got[0]
    );
    // The sound lands between the halves, not after the whole line.
    assert_eq!(got[1], json!({"sound": "page-turn"}));
    assert_eq!(got[2], b);

    // A bed and its stop, both marked, in that order: start then stop, so a
    let mut open = line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp.");
    open["sound_after"] = json!("food-prep");
    let mut close = line("Thanh Sơn lão tổ gật đầu lia lịa như gà mổ thóc.");
    close["stop_after"] = json!("food-prep");
    let got = expand_sound_fields(&[open, close]).unwrap();
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[1], json!({"sound": "food-prep"}));
    assert_eq!(got[3], json!({"stop": "food-prep"}));

    // `none` is a decision and adds nothing; the field still comes off.
    let mut quiet = line("x");
    quiet["sound_after"] = json!("none");
    quiet["stop_after"] = json!("NONE");
    let got = expand_sound_fields(&[quiet, line("y")]).unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got[0].get("sound_after").is_none() && got[0].get("stop_after").is_none());

    // A blank is refused, by name, and the message says what to write.
    for key in ["sound_after", "stop_after"] {
        let mut blank = line("z");
        blank[key] = json!("  ");
        let err = expand_sound_fields(&[blank]).unwrap_err();
        assert!(err.to_string().contains("none"), "{key}: {err}");
    }

    // A name outside the vocabulary is lifted anyway, so the validator gets
    let mut bogus = line("z");
    bogus["sound_after"] = json!("thunder");
    let got = expand_sound_fields(&[bogus]).unwrap();
    assert_eq!(got[1], json!({"sound": "thunder"}));
}

/// Both halves of the sound-design gate, on the two answers ch9 actually
#[test]
fn sound_design_gap_catches_an_empty_and_an_unclosed_design() {
    use crate::audio_pool::{ClipPool, Sound};
    let mk = |looped: bool| Sound {
        tags: vec![],
        files: vec!["injects/x.mp3".into()],
        looped,
        dur_s: Some(25.2),
        mode: Some("overlap".into()),
        hold: None,
        level: None,
    };
    let pool: ClipPool = [("food-prep", mk(true)), ("coin", mk(false))]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let line = |t: &str| json!({"speaker": "Narrator", "text": t});
    let chapter = "Sau một hồi cảm khái, hai người liền đi đến phòng bếp. Thanh Sơn lão tổ tìm thấy chiếc dao phay.";

    // 1. Nothing placed at all, in a chapter that stages plenty.
    let silent = json!({"segments": [
        line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
        line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
    ]});
    let gap = sound_design_gap(&silent, chapter, &pool).expect("a staged chapter with none");
    assert!(gap.contains("phòng bếp"), "{gap}");

    // ch15 names a cleaver while asking for one to be forged later; an
    let chapter15 = "Dịch Phong vươn tay lấy ra con dao phay nói: nhớ lần trước bá mẫu nói qua, nhờ ta rèn một con dao phay lúc rảnh rỗi, giúp ta mang cho họ nhé!";
    let mention_only = json!({"segments": [line(chapter15)]});
    assert!(
        sound_design_gap(&mention_only, chapter15, &pool).is_none(),
        "a mentioned cleaver must not require a chopping sound"
    );

    // 2. The bed opened and never closed, ch9's exact answer.
    let unclosed = json!({"segments": [
        line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
        {"sound": "food-prep"},
        line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
    ]});
    let gap = sound_design_gap(&unclosed, chapter, &pool).expect("an unclosed bed");
    assert!(
        gap.contains("food-prep") && gap.contains("stops dead"),
        "{gap}"
    );

    // 3. Closed: both halves satisfied.
    let closed = json!({"segments": [
        line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
        {"sound": "food-prep"},
        line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
        {"stop": "food-prep"},
    ]});
    assert!(
        sound_design_gap(&closed, chapter, &pool).is_none(),
        "closed must pass"
    );

    // 4. A one-shot needs no stop, only a `looped` sound does.
    let oneshot = json!({"segments": [
        line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
        {"sound": "coin"},
        line("Thanh Sơn lão tổ tìm thấy chiếc dao phay."),
    ]});
    assert!(sound_design_gap(&oneshot, chapter, &pool).is_none());

    // 5. A chapter that stages nothing is allowed to place nothing.
    assert!(sound_design_gap(&silent, "Trời hôm nay đẹp.", &pool).is_none());
}

/// The soft-release curve: three blocks, then accept. Pinned because the
#[test]
fn gap_block_p_releases_on_the_fourth_failure() {
    assert!((gap_block_p(0) - 0.9).abs() < 1e-9);
    assert!(gap_block_p(1) >= 0.5);
    assert!(gap_block_p(2) >= 0.5);
    assert!(gap_block_p(3) < 0.5);
}

/// ch262's false positive, pinned: bare `chém` is an idiom
#[test]
fn bare_chem_is_no_sound_cue() {
    use crate::audio_pool::ClipPool;
    let pool = ClipPool::new();
    let line = |t: &str| json!({"speaker": "Narrator", "text": t});
    let chapter = "Muốn chém muốn giết, người cứ nói thẳng ra một lời.";
    let silent = json!({"segments": [line(chapter)]});
    assert!(sound_design_gap(&silent, chapter, &pool).is_none());
    let chapter2 = "Hắn rút kiếm chém xuống.";
    let silent2 = json!({"segments": [line(chapter2)]});
    assert!(sound_design_gap(&silent2, chapter2, &pool).is_some());
}

/// The two rounds are stitched by key, and each key has one owner.
#[test]
fn merge_rounds_gives_each_key_to_its_owner() {
    let context = json!({
        "title": "Bí Ẩn Dao Phay",
        "atmosphere": "A kitchen at dusk.",
        "excerpt": "Lan is wounded; the stranger stays unnamed.",
        "roster": ["Narrator"],
        "mentions": {"hắn": "Dịch Phong"},
        "new_characters": [],
        "new_aliases": {},
        "speakers": {"e0001": "Narrator"},
        // Not the cast pass's business, and it does not win.
        "segments": [{"speaker": "Ghost", "text": "from the cast pass"}],
    });
    let script = json!({
        // Not the script pass's business, and it does not win.
        "title": "Tê! Thật là khủng khiếp dao phay",
        "roster": ["Nobody"],
        "segments": [
            {"speaker": "Narrator", "text": "Trời sáng."},
            {"sound": "food-prep"},
            {"speaker": "Narrator", "text": "Rồi nấu."},
            {"stop": "food-prep"},
        ],
        "fixes": [],
    });
    let merged = merge_rounds(&context, &script);
    assert_eq!(merged["title"], json!("Bí Ẩn Dao Phay"));
    assert_eq!(
        merged["excerpt"],
        json!("Lan is wounded; the stranger stays unnamed."),
        "the cast pass's excerpt must survive the merge: the script writer \
         reads this key and falls back to an empty string when it is absent, \
         which is how every chapter kept a blank excerpt"
    );
    assert_eq!(merged["roster"], json!(["Narrator"]));
    assert_eq!(merged["mentions"], json!({"hắn": "Dịch Phong"}));
    assert_eq!(merged["speakers"], json!({"e0001": "Narrator"}));
    assert_eq!(merged["segments"].as_array().unwrap().len(), 4);
    assert_eq!(merged["segments"][0]["text"], json!("Trời sáng."));
    assert_eq!(merged["fixes"], json!([]));
    // Every key the pipeline reads is present, from whichever round owns it.
    for key in [
        "title",
        "atmosphere",
        "excerpt",
        "roster",
        "mentions",
        "new_characters",
        "new_aliases",
        "speakers",
        "segments",
        "fixes",
    ] {
        assert!(merged.get(key).is_some(), "missing {key} after the merge");
    }
}

/// The shipped pair, against the fixture pools.
#[test]
fn shipped_prompts_render_every_placeholder() {
    let dir = std::env::temp_dir().join("bm-prompt-fixture");
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    let bible: Value = json!({"characters": []});
    std::fs::create_dir_all(layout.chapters()).unwrap();
    std::fs::write(
        layout.chapter_txt(51),
        "Chương 51: Fixture\n\nBody text here.\n",
    )
    .unwrap();
    let text = std::fs::read_to_string(layout.chapter_txt(51)).unwrap();

    let cast = build_prompt(&layout, &bible, &text).unwrap();
    for ph in ["{bible_json}", "{chapter_text}"] {
        assert!(!cast.contains(ph), "placeholder leaked: {ph}");
    }

    let context = json!({"roster": ["Narrator"], "mentions": {}});
    let p = build_script_prompt(&layout, "vieneu", &bible, &context, &text).unwrap();
    for ph in [
        "{music_palette}",
        "{effect_tags}",
        "{inject_sounds}",
        "{cast_json}",
        "{bible_json}",
        "{chapter_text}",
        "{voice_tags}",
        "{tag_laugh}",
        "{tag_sigh}",
        "{tag_throat}",
    ] {
        assert!(!p.contains(ph), "placeholder leaked: {ph}");
    }
    assert!(p.contains("quiet (soft, calm;"), "{p}");
    assert!(p.contains("battle, birds, calm"), "{p}");
    assert!(p.contains("blood-spatter (hit; blood"), "{p}");

    // The non-verbal vocabulary is VieNeu's, so its tags render — and an
    assert!(p.contains("[cười] [thở dài] [hắng giọng]"), "{p}");
    let none = build_script_prompt(&layout, "gemini", &bible, &context, &text).unwrap();
    assert!(
        !none.contains("NON-VERBAL"),
        "another engine must not be taught the rule at all: {none}"
    );
    assert!(!none.contains("[cười]"), "{none}");
    assert!(!none.contains("{tag_"), "placeholder leaked: {none}");
}

/// The manual path's contract, against the fixture.
/// automatic path enforces, or "the same digest with a person standing in for
/// the model" is not true of it. So this pins the *identity*: round 1 is
#[test]
fn the_manual_rounds_ask_for_the_right_prompt_and_refuse_a_paste_out_of_order() {
    let dir = std::env::temp_dir().join("bm-manual-fixture");
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.chapters()).unwrap();
    std::fs::write(
        layout.chapter_txt(51),
        "Chương 51: Fixture\n\nHắn gật đầu.\n\n\"Ừm!\"\n",
    )
    .unwrap();

    // Round 1 is the attribution pass, and the fixture's template is a stub
    let first = manual_prompt(&layout, "vieneu", 51, None).unwrap();
    assert_eq!(first.round, Round::Attribution);
    assert!(
        first.text.contains("Fixture dramatization prompt"),
        "the attribution template, as the fixture ships it: {}",
        head_chars(&first.text, 120)
    );
    // The chapter arrives as prepared events: the quote is answerable, the
    assert!(first.text.contains("dialogue_events"), "{}", first.text);
    assert!(first.text.contains("Ừm!"), "the line it must attribute");
    assert!(
        first.text.contains("---ATTRIBUTION OUTPUT CONTRACT---"),
        "the worker's own contract: {}",
        head_chars(&first.text, 200)
    );
    assert!(
        !first.text.contains("{chapter_text}"),
        "no placeholder leaked"
    );

    // A paste for round 2 with no cast is refused by name, rather than
    let err =
        manual_accept(&layout, 51, Round::Staging, "{}", None).expect_err("round 2 needs round 1");
    assert!(err.to_string().contains("round 1's cast"), "{err}");

    // A garbage paste fails the *worker's* validator, the same one, and
    let err = manual_accept(&layout, 51, Round::Attribution, "not json at all", None)
        .expect_err("not JSON");
    assert!(err.to_string().contains("not valid JSON"), "{err:#}");

    // With a cast in hand, round 2 renders the *staging* prompt against it.
    let cast = json!({
        "roster": ["Narrator", "Anonymous"],
        "mentions": {},
        "speakers": {"e0002": "Anonymous"}
    });
    let second = manual_prompt(&layout, "vieneu", 51, Some(&cast)).unwrap();
    assert_eq!(second.round, Round::Staging);
    assert_ne!(second.text, first.text, "a different pass, not a repeat");
    assert!(second.text.contains("---STAGING OUTPUT CONTRACT---"));
    assert!(
        second.text.contains("fixed_speakers"),
        "round 2 is rendered against the map round 1 fixed"
    );
    for ph in ["{cast_json}", "{music_palette}", "{inject_sounds}"] {
        assert!(!second.text.contains(ph), "placeholder leaked: {ph}");
    }

    // A chapter with no text fails by path, so the operator knows which file
    let err = manual_prompt(&layout, "vieneu", 999, None).expect_err("no chapter text");
    assert!(err.to_string().contains("ch999"), "{err:#}");
}

/// The happy path, end to end, without a model.
#[test]
fn a_valid_pair_of_pastes_finishes_the_chapter() {
    let dir = std::env::temp_dir().join("bm-manual-happy");
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.chapters()).unwrap();
    std::fs::write(
        layout.chapter_txt(51),
        "Chương 51: Fixture\n\nHắn gật đầu.\n\n\"Ừm!\"\n",
    )
    .unwrap();

    // Round 1: the attribution answer, one entry per *dialogue* event, in
    let cast = manual_accept(
        &layout,
        51,
        Round::Attribution,
        r#"{"title": "Dao Phay Trong Bếp", "atmosphere": "A quiet kitchen at dusk.",
            "roster": ["Narrator", "Anonymous"], "mentions": {},
            "new_characters": [], "new_aliases": {},
            "speakers": {"e0002": "Anonymous"}}"#,
        None,
    )
    .expect("a well-formed attribution answer");
    let cast = cast.cast.expect("round 1 yields the cast");
    assert!(cast.get("outcome").is_none(), "and nothing finished");
    assert_eq!(cast["speakers"]["e0002"], json!("Anonymous"));
    // Narration is attached by code, so it is in the map the *next* round
    assert_eq!(cast["speakers"]["e0001"], json!("Narrator"));

    // Round 2: staging, carrying no speaker, checked against that map.
    let done = manual_accept(
        &layout,
        51,
        Round::Staging,
        r#"{"segments": [
            {"source_id": "e0001", "text": "Hắn gật đầu.", "music": "calm"},
            {"source_id": "e0002", "text": "Ừm!", "music": "calm"}],
            "fixes": []}"#,
        Some(&cast),
    )
    .expect("a well-formed staging answer against the map it was given");
    let outcome = done.outcome.expect("round 2 finishes the chapter");

    assert_eq!(outcome.segments, 2, "two spoken segments");
    assert_eq!(outcome.script["title"], json!("Dao Phay Trong Bếp"));
    let segments = outcome.script["segments"].as_array().unwrap();
    assert_eq!(segments[0]["speaker"], json!("Narrator"));
    assert_eq!(segments[1]["speaker"], json!("Anonymous"));
    assert_eq!(segments[0]["music"], json!("quiet"));
    // The delta is what the inductor merges into the bible, a manual digest
    assert!(outcome.delta.get("roster").is_some(), "{:?}", outcome.delta);
    assert!(
        outcome.log.iter().any(|l| l.contains("segments=2")),
        "{:?}",
        outcome.log
    );

    // **And the hand-off is real, not decorative.** A staging answer that
    let err = manual_accept(
        &layout,
        51,
        Round::Staging,
        r#"{"segments": [{"source_id": "e0001", "text": "Hắn gật đầu."}], "fixes": []}"#,
        Some(&cast),
    )
    .expect_err("an event the source gate never saw cannot land");
    assert!(
        err.to_string().contains("dropped") || err.to_string().contains("source"),
        "and says which source contract broke: {err:#}"
    );
}

/// Manual gate, not CI: runs a real digest of ch51 through the analyzer
#[tokio::test]
#[ignore]
async fn live_digest_ch51_prints_script() {
    if std::env::var("BM_LIVE_DIGEST").is_err() {
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let layout = Layout::new(&root);
    let bible = load_bible(&layout.bible());
    let settings = crate::config::Settings::load(&layout.settings());
    let mut progress = |_f: f32, _s: String| {};
    let out = digest_chapter(&layout, 51, &bible, &settings, "gemini", &mut progress)
        .await
        .unwrap();
    println!("{}", serde_json::to_string_pretty(&out.script).unwrap());
}

// -----------------------------------------------------------------------

/// A chapter long enough to need parts, **built rather than committed**.
fn long_chapter(paragraphs: usize) -> String {
    let mut text = String::new();
    for i in 0..paragraphs {
        text.push_str(&format!(
            "Đoạn {i} kể rằng buổi chiều hôm ấy trời trở gió, và người trong sân vẫn đứng im \
             như tượng đá trước hiên nhà, chẳng ai dám lên tiếng trước.\n"
        ));
        if i % 3 == 1 {
            text.push_str("\"Ngươi có nghe thấy tiếng gì không?\" người ấy hỏi.\n");
        }
    }
    text
}

/// A fixture workspace with one long chapter in it.
fn long_layout(tag: &str, paragraphs: usize) -> (std::path::PathBuf, Layout, String) {
    let dir = std::env::temp_dir().join(format!("bm-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.chapters()).unwrap();
    let text = long_chapter(paragraphs);
    std::fs::write(layout.chapter_txt(51), &text).unwrap();
    (dir, layout, text)
}

/// A part's attribution answer as a model would write it: one speaker per
fn cast_answer(slice: &PreparedChapter) -> String {
    let speakers: serde_json::Map<String, Value> = slice
        .events
        .iter()
        .filter(|e| e.kind == "dialogue")
        .map(|e| (e.id.clone(), json!("Anonymous")))
        .collect();
    let roster = if speakers.is_empty() {
        json!(["Narrator"])
    } else {
        json!(["Narrator", "Anonymous"])
    };
    json!({
        "title": "Tiếng Hỏi Trong Sân",
        "atmosphere": "An empty courtyard at dusk.",
        "roster": roster,
        "mentions": {},
        "new_characters": [],
        "new_aliases": {},
        "not_speech": [],
        "speakers": speakers,
        "summary": "The courtyard falls quiet and someone asks a question.",
    })
    .to_string()
}

/// A part's staging answer: every event of the part, once, in source order,
fn script_answer(slice: &PreparedChapter) -> String {
    let segments: Vec<Value> = slice
        .events
        .iter()
        .map(|e| json!({"source_id": e.id, "text": e.text}))
        .collect();
    json!({"segments": segments, "fixes": []}).to_string()
}

fn staged_part(from: usize, to: usize, context: Value, segments: Value) -> Part {
    Part {
        from,
        to,
        summary: format!("part {from}..{to}"),
        context,
        script: json!({"segments": segments, "fixes": []}),
    }
}

/// **The parity promise, in bytes.** A chapter that fits one call is asked
#[test]
fn a_chapter_that_fits_one_call_is_asked_the_prompt_it_always_was() {
    let (_dir, layout, text) = long_layout("one-call-prompt", 3);
    let prepared = prepare_chapter(&text);
    let bible = json!({"characters": []});
    let one = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
    for absent in ["PART 1 OF", "PLOT SO FAR", "\"summary\""] {
        assert!(!one.contains(absent), "{absent} reached a one-call prompt");
    }

    let first = Continuity {
        index: 0,
        total: 2,
        plot: &[],
    };
    let part_one =
        build_attribution_prompt(&layout, &bible, &prepared, Some(&first), None).unwrap();
    assert!(
        part_one.contains("PART 1 OF 2"),
        "{}",
        head_chars(&part_one, 40)
    );
    assert!(
        part_one.contains("\"summary\""),
        "the field a part must return"
    );
    assert!(
        !part_one.contains("PLOT SO FAR"),
        "the first part has nothing behind it"
    );

    let plot = vec!["They reach the courtyard and nobody speaks.".to_string()];
    let second = Continuity {
        index: 1,
        total: 2,
        plot: &plot,
    };
    let part_two =
        build_attribution_prompt(&layout, &bible, &prepared, Some(&second), None).unwrap();
    assert!(part_two.contains("PART 2 OF 2"));
    assert!(
        part_two.contains("They reach the courtyard"),
        "part 2 is handed part 1's own words"
    );

    // Staging has its own note, and the rule that matters for it is the bed
    let cast = json!({"roster": ["Narrator"], "speakers": {"e0001": "Narrator"}});
    let quiet = build_staging_prompt(&layout, "vieneu", &bible, &cast, &prepared, None).unwrap();
    assert!(
        !quiet.contains("PART 1 OF"),
        "no part note when there is one part"
    );
    let split =
        build_staging_prompt(&layout, "vieneu", &bible, &cast, &prepared, Some(&second)).unwrap();
    assert!(split.contains("PART 2 OF 2"));
    assert!(
        split.contains("`loop`ed bed may run past the end of your part"),
        "the bed rule, told to the round that places beds"
    );
    assert!(split.contains("write no ending"), "and no rounding off");
}

/// The one cross-chapter memory: a stored predecessor's excerpt rides
#[test]
fn the_previous_excerpt_rides_as_a_previously_block() {
    let (_dir, layout, text) = long_layout("excerpt-prompt", 3);
    let prepared = prepare_chapter(&text);
    let bible = json!({"characters": []});

    let bare = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
    assert!(
        !bare.contains("PREVIOUSLY"),
        "no block without a predecessor: {}",
        head_chars(&bare, 40)
    );

    let with = build_attribution_prompt(
        &layout,
        &bible,
        &prepared,
        None,
        Some("CH 41: The white-robed swordswoman is still unnamed; she left with the party."),
    )
    .unwrap();
    assert!(
        with.contains("---PREVIOUSLY---"),
        "{}",
        head_chars(&with, 40)
    );
    assert!(with.contains("still unnamed"), "the memory itself");
    assert!(
        with.contains("identity context only"),
        "the block names what it is for: resolve, not answer"
    );
}

/// The chain reads the stored scripts: an excerpt is picked up from
#[test]
fn previous_excerpts_read_stored_scripts_and_skip_gaps() {
    let (_dir, layout, _text) = long_layout("excerpt-store", 3);
    std::fs::create_dir_all(layout.script(41).parent().unwrap()).unwrap();
    std::fs::write(
        layout.script(41),
        r#"{"excerpt": "The stranger is still unnamed."}"#,
    )
    .unwrap();

    let got = previous_excerpts(&layout, 42).unwrap();
    assert_eq!(got, "CH 41: The stranger is still unnamed.");

    // The default window is 1: chapter 43 asks for script(42), which was
    assert!(previous_excerpts(&layout, 43).is_none());
    assert!(
        previous_excerpts(&layout, 41).is_none(),
        "the first chapter has no predecessor by definition"
    );
}

/// The excerpt-only prompt asks the digest's own question: the same rule
#[test]
fn the_excerpt_only_prompt_asks_the_digests_own_excerpt_rule() {
    let (_dir, layout, text) = long_layout("excerpt-only-prompt", 3);

    let bare = build_excerpt_prompt(&layout, 51, &text).unwrap();
    assert!(
        bare.contains("written for the NEXT chapter's analyzer"),
        "the shared rule text: {}",
        head_chars(&bare, 40)
    );
    assert!(
        bare.contains("Đoạn 0 kể rằng"),
        "the chapter itself is the input"
    );
    assert!(
        !bare.contains("PREVIOUSLY"),
        "no predecessor in the fixture, so no memory to feed"
    );

    // Store a predecessor excerpt and the chain appears, named as context.
    std::fs::create_dir_all(layout.script(50).parent().unwrap()).unwrap();
    std::fs::write(layout.script(50), r#"{"excerpt": "She is still unnamed."}"#).unwrap();
    let fed = build_excerpt_prompt(&layout, 51, &text).unwrap();
    assert!(fed.contains("---PREVIOUSLY---"), "{}", head_chars(&fed, 40));
    assert!(fed.contains("She is still unnamed."), "the memory itself");
    assert!(
        fed.contains("write only this chapter's excerpt"),
        "the block says it is for resolving, not for answering"
    );
}

/// One rule, two askers: the attribution contract and the excerpt prompt
#[test]
fn the_attribution_contract_and_the_excerpt_prompt_share_one_rule() {
    let (_dir, layout, text) = long_layout("excerpt-rule-parity", 3);
    let prepared = prepare_chapter(&text);
    let bible = json!({"characters": []});
    let attribution = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
    let excerpt_only = build_excerpt_prompt(&layout, 51, &text).unwrap();
    let rule = excerpt_rule(&content_language(&layout));
    assert!(rule.contains("State, not plot."));
    assert!(
        attribution.contains(&rule),
        "the contract carries the shared rule"
    );
    assert!(
        excerpt_only.contains(&rule),
        "the excerpt prompt carries the same shared rule"
    );
}

/// The answer is read softly: the strict object, the prose a model returns
#[test]
fn an_excerpt_answer_is_read_softly() {
    assert_eq!(
        parse_excerpt(r#"{"excerpt": "  She  leaves  unnamed. "}"#).as_deref(),
        Some("She leaves unnamed.")
    );
    assert_eq!(
        parse_excerpt("```\nShe leaves unnamed.\n```").as_deref(),
        Some("She leaves unnamed.")
    );
    assert_eq!(parse_excerpt("   \n").as_deref(), None);
    assert_eq!(parse_excerpt(r#"{"excerpt": ""}"#).as_deref(), None);
}

/// Recovering an excerpt must not disturb the script it lands in: the
#[test]
fn write_excerpt_touches_only_the_excerpt() {
    let (_dir, layout, _text) = long_layout("excerpt-write", 3);
    std::fs::create_dir_all(layout.script(7).parent().unwrap()).unwrap();
    std::fs::write(
        layout.script(7),
        r#"{"segments": [{"id": "s1", "text": "quiet"}], "cast": {"n": 1}, "excerpt": ""}"#,
    )
    .unwrap();
    write_excerpt(&layout, 7, "The hall empties; the stranger stays unnamed.").unwrap();
    let stored: Value = crate::read_json::<Value>(&layout.script(7)).unwrap();
    assert_eq!(
        stored["excerpt"],
        json!("The hall empties; the stranger stays unnamed.")
    );
    assert_eq!(stored["segments"][0]["id"], json!("s1"));
    assert_eq!(stored["cast"]["n"], json!(1));
}

/// A part has to say what happened in it, because that summary is the whole
#[test]
fn a_part_of_a_chapter_has_to_say_what_happened_in_it() {
    let prepared = prepare_chapter("Hắn gật đầu.\n\n\"Ừm!\"\n");
    let bible = json!({"characters": []});
    let answer = |extra: &str| {
        format!(
            "{{\"title\": \"Tiếng Hỏi Trong Sân\", \"atmosphere\": \"Quiet.\", \"roster\": \
             [\"Narrator\", \"Anonymous\"], \"mentions\": {{}}, \"new_characters\": [], \
             \"new_aliases\": {{}}, \"speakers\": {{\"e0002\": \"Anonymous\"}}{extra}}}"
        )
    };
    parse_attribution(&answer(""), &bible, &prepared, false)
        .expect("a one-call chapter is asked for no summary");
    let err = parse_attribution(&answer(""), &bible, &prepared, true)
        .expect_err("a part without a summary is a part the rest continues blind");
    assert!(err.to_string().contains("summary"), "{err:#}");
    let ok = parse_attribution(
        &answer(", \"summary\": \"Hắn đồng ý.\""),
        &bible,
        &prepared,
        true,
    )
    .unwrap();
    assert_eq!(ok["summary"], json!("Hắn đồng ý."));
}

/// The merge: source order, one identity per person, and every speaker from
#[test]
fn merging_parts_keeps_the_source_order_and_every_identity() {
    let a = staged_part(
        0,
        2,
        json!({
            "title": "Tiếng Hỏi Trong Sân",
            "atmosphere": "A courtyard at dusk.",
            "roster": ["Narrator"],
            "mentions": {"hắn": "Dịch Phong"},
            "new_characters": [{"name": "Dịch Phong", "personality": "wry"}],
            "new_aliases": {"Lão Phong": "Dịch Phong"},
            "speakers": {"e0001": "Narrator"},
        }),
        json!([{"source_id": "e0001", "text": "Hắn gật đầu."}]),
    );
    let b = staged_part(
        2,
        3,
        json!({
            "title": "Something Else",
            "atmosphere": "A hall at noon.",
            "roster": ["Narrator", "Anonymous"],
            "mentions": {"hắn": "Dịch Phong"},
            "new_characters": [{"name": "Dịch Phong", "voice_hint": "low"}],
            "new_aliases": {},
            "speakers": {"e0003": "Anonymous"},
        }),
        json!([{"source_id": "e0003", "text": "Ừm!"}]),
    );
    let script = merge_scripts([&a.script, &b.script]);
    let (merged, conflicts) = merge_contexts(&[a, b]);
    assert!(conflicts.is_empty(), "{conflicts:?}");

    // The script is the parts in order, which is source order.
    let segments = script["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0]["source_id"], json!("e0001"));
    assert_eq!(segments[1]["source_id"], json!("e0003"));

    // The opening part names the chapter and sets its mood; the rest is the
    assert_eq!(merged["title"], json!("Tiếng Hỏi Trong Sân"));
    assert_eq!(merged["atmosphere"], json!("A courtyard at dusk."));
    assert_eq!(merged["roster"], json!(["Narrator", "Anonymous"]));
    assert_eq!(merged["speakers"]["e0001"], json!("Narrator"));
    assert_eq!(merged["speakers"]["e0003"], json!("Anonymous"));
    assert_eq!(merged["mentions"]["hắn"], json!("Dịch Phong"));
    assert_eq!(merged["new_aliases"]["Lão Phong"], json!("Dịch Phong"));

    // One character declared twice is one character, and the later part fills
    let characters = merged["new_characters"].as_array().unwrap();
    assert_eq!(characters.len(), 1, "{characters:?}");
    assert_eq!(characters[0]["personality"], json!("wry"));
    assert_eq!(characters[0]["voice_hint"], json!("low"));
}

/// Two parts owning one surface form differently is the one thing a union
#[test]
fn two_parts_owning_one_form_differently_is_reported() {
    let part = |owner: &str| {
        staged_part(
            0,
            1,
            json!({"mentions": {"hắn": owner}}),
            json!([{"source_id": "e0001", "text": "x"}]),
        )
    };
    let (merged, conflicts) = merge_contexts(&[part("Dịch Phong"), part("Vũ Kiệt")]);
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert!(conflicts[0].contains("Dịch Phong") && conflicts[0].contains("Vũ Kiệt"));
    assert_eq!(
        merged["mentions"]["hắn"],
        json!("Dịch Phong"),
        "the earlier part wins"
    );
    // Agreement is not a conflict, however many parts say the same thing.
    let (_, conflicts) = merge_contexts(&[part("Dịch Phong"), part("Dịch Phong")]);
    assert!(conflicts.is_empty(), "{conflicts:?}");
}

/// The checkpoint is what makes a sixteen-call chapter survivable: a part is
#[test]
fn the_parts_checkpoint_resumes_what_matches_and_forgets_what_does_not() {
    let (_dir, layout, text) = long_layout("parts-checkpoint", 300);
    let settings = Settings::load(&layout.settings());
    let bible = json!({"characters": []});
    let prepared = prepare_chapter(&text);
    let windows = plan_windows(&prepared, &settings.digest);
    assert!(windows.len() > 1, "{}", windows.len());

    let mut parts = Parts::open(&layout, 51, &text, &bible, &windows, &settings);
    assert_eq!(parts.len(), 0, "nothing is staged before the first call");
    let first = staged_part(
        windows[0].from,
        windows[0].to,
        json!({"title": "T", "summary": "câu chuyện mở ra"}),
        json!([{"source_id": prepared.events[windows[0].from].id, "text": "x"}]),
    );
    parts.push(first.clone()).unwrap();
    assert!(!parts.path.exists() || parts.path.metadata().is_ok());

    let reopened = Parts::open(&layout, 51, &text, &bible, &windows, &settings);
    assert_eq!(
        reopened.len(),
        1,
        "the finished part is resumed, not re-asked"
    );
    assert_eq!(reopened.summaries().len(), 1);

    // A different plan is a different chapter's work: the stored parts are
    let chunkier = crate::config::DigestSettings {
        chunk_sentences: 4,
        chunk_chars: 0,
        answer_tokens: 0,
    };
    let other = plan_windows(&prepared, &chunkier);
    assert_ne!(other.len(), windows.len());
    assert_eq!(
        Parts::open(&layout, 51, &text, &bible, &other, &settings).len(),
        0
    );
    // As is an edited chapter, even under the same plan.
    assert_eq!(
        Parts::open(
            &layout,
            51,
            &format!("{text} x"),
            &bible,
            &windows,
            &settings
        )
        .len(),
        0
    );
    // As is another bible, which is what makes later parts' casts safe.
    assert_eq!(
        Parts::open(
            &layout,
            51,
            &text,
            &json!({"characters": [{"name": "Dịch Phong"}]}),
            &windows,
            &settings,
        )
        .len(),
        0
    );
    // And the plan moving under a stored part drops it, rather than staging
    let shifted: Vec<Window> = windows
        .iter()
        .skip(1)
        .map(|w| Window {
            from: w.from - windows[0].events,
            to: w.to - windows[0].events,
            ..*w
        })
        .collect();
    assert_eq!(
        Parts::open(&layout, 51, &text, &bible, &shifted, &settings).len(),
        0
    );
}

/// The gates name the part that owes an answer: the one that opened the
#[test]
fn the_gate_names_the_part_that_owes_an_answer() {
    use crate::audio_pool::{ClipPool, Sound};
    let mk = |looped: bool| Sound {
        tags: vec![],
        files: vec!["injects/x.mp3".into()],
        looped,
        dur_s: Some(25.2),
        mode: Some("overlap".into()),
        hold: None,
        level: None,
    };
    let pool: ClipPool = [("food-prep", mk(true)), ("coin", mk(false))]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let line = |t: &str| json!({"speaker": "Narrator", "text": t});

    // Rule 1 across a part boundary: opened in part 2, never closed. The
    let quiet = json!({"segments": [line("Trời tối."), {"sound": "coin"}]});
    let opens = json!({"segments": [line("Hai người đến phòng bếp."), {"sound": "food-prep"}, line("Rồi đi ra.")]});
    let scripts = [&quiet, &opens];
    let (gap, owner) =
        sound_gap(&scripts, &["Trời tối.", "Hai người."], &pool, "part").expect("an open bed");
    assert_eq!(owner, 1, "the part that opened it");
    assert!(
        gap.contains("food-prep") && gap.contains("stops dead"),
        "{gap}"
    );

    // Rule 2 is per part, and this is why: part 1's prose stages a kitchen
    let silent = json!({"segments": [line("Hắn đi đến phòng bếp.")]});
    let placed = json!({"segments": [line("Trời tối."), {"sound": "coin"}]});
    let scripts = [&silent, &placed];
    let (gap, owner) = sound_gap(
        &scripts,
        &["Hắn đi đến phòng bếp.", "Trời tối."],
        &pool,
        "part",
    )
    .expect("a part that staged a cue and placed none");
    assert_eq!(owner, 0);
    assert!(gap.contains("phòng bếp"), "{gap}");
    assert!(gap.contains("part"), "the message names what it is about");

    // A chapter that did not split goes through `sound_design_gap` itself, so
    let scripts = [&silent];
    let gap = sound_gap(&scripts, &["Hắn đi đến phòng bếp."], &pool, "chapter")
        .expect("a staged chapter with none");
    assert!(gap.0.contains("chapter"), "{}", gap.0);
}

/// **The end-to-end proof, without a model.** A 40 KB chapter — three times
#[test]
fn a_long_chapter_is_staged_in_parts_and_merges_into_one_script() {
    let (_dir, layout, text) = long_layout("manual-parts", 220);
    let prepared = prepare_chapter(&text);
    let settings = Settings::load(&layout.settings());
    let windows = plan_windows(&prepared, &settings.digest);
    assert!(
        windows.len() > 1,
        "a {}-char chapter has to need parts: {} windows",
        text.chars().count(),
        windows.len()
    );

    // (1) Why parts exist, in the numbers the backends impose: the answer to
    let whole_chars = weight(&prepared.events);
    assert!(
        tokens(whole_chars) > 16_384,
        "the fixture must be over the hard cap in one call: {} tokens",
        tokens(whole_chars)
    );
    for (i, w) in windows.iter().enumerate() {
        assert!(
            tokens(w.chars) < settings.digest.answer_tokens as usize,
            "part {} does not fit the budget either: {} tokens",
            i + 1,
            tokens(w.chars)
        );
    }

    // (2) The prompt shrinks with the part, which is the truncation being
    let bible = load_bible(&layout.bible());
    let whole_prompt = build_attribution_prompt(&layout, &bible, &prepared, None, None).unwrap();
    let checkpoint = layout.data().join(".digest-parts-ch51.json");

    // (3) The flow, part by part, exactly as the TUI and the backup runner
    let mut cast: Option<Value> = None;
    let mut outcome = None;
    let mut prompts: Vec<ManualPrompt> = Vec::new();
    let mut part_prompts: Vec<String> = Vec::new();
    for i in 0..windows.len() {
        let slice = windows[i].prepared(&prepared);

        let step = manual_prompt(&layout, "vieneu", 51, cast.as_ref()).unwrap();
        assert_eq!(
            step.round,
            Round::Attribution,
            "every part opens on its cast"
        );
        let part = ManualPart {
            index: i + 1,
            total: windows.len(),
        };
        assert_eq!(step.part, Some(part), "the round knows which part it is");
        part_prompts.push(step.text.clone());
        prompts.push(step);

        let accepted =
            manual_accept(&layout, 51, Round::Attribution, &cast_answer(&slice), None).unwrap();
        assert!(
            accepted.prompt.is_none(),
            "round 2 is asked for by the caller"
        );
        cast = accepted.cast;
        assert!(cast.is_some(), "a part's cast is validated and handed back");

        let second = manual_prompt(&layout, "vieneu", 51, cast.as_ref()).unwrap();
        assert_eq!(second.round, Round::Staging);
        assert_eq!(second.part, Some(part));
        part_prompts.push(second.text.clone());
        if i > 0 {
            assert!(
                second.text.contains("PLOT SO FAR"),
                "part {} is told what the parts before it established",
                i + 1
            );
        }

        let accepted = manual_accept(
            &layout,
            51,
            Round::Staging,
            &script_answer(&slice),
            cast.as_ref(),
        )
        .unwrap();
        cast = None;
        if i + 1 < windows.len() {
            assert!(
                checkpoint.exists(),
                "part {} is on disk the moment it is accepted",
                i + 1
            );
            let next = accepted.prompt.expect("the next part's cast prompt");
            assert_eq!(next.round, Round::Attribution);
            assert_eq!(
                next.part,
                Some(ManualPart {
                    index: i + 2,
                    total: windows.len()
                })
            );
            assert!(
                accepted.outcome.is_none(),
                "the chapter is not finished yet"
            );
        } else {
            assert!(accepted.prompt.is_none());
            outcome = accepted.outcome;
        }
    }
    let outcome = outcome.expect("the last part finishes the chapter");

    // Every part's prompt is a fraction of the one the whole chapter would
    for (i, prompt) in part_prompts.iter().enumerate() {
        assert!(
            prompt.len() < whole_prompt.len(),
            "part prompt {i} is not smaller than the whole chapter's"
        );
    }

    // (4) The chapter is written, so the parts it was built from are gone.
    assert!(!checkpoint.exists(), "the checkpoint is cleared at the end");

    // (3) The merged script: one segment per source event, in source order,
    let segments = outcome.script["segments"].as_array().unwrap();
    assert_eq!(segments.len(), prepared.events.len());
    for (segment, event) in segments.iter().zip(&prepared.events) {
        assert_eq!(segment["source_id"], json!(event.id));
        assert_eq!(segment["text"], json!(event.text));
    }
    assert_eq!(outcome.segments, prepared.events.len());
    assert_eq!(
        outcome.delta["segments"].as_array().unwrap().len(),
        prepared.events.len(),
        "the delta the inductor merges carries the whole chapter"
    );
    assert_eq!(outcome.script["roster"], json!(["Narrator", "Anonymous"]));

    // The ledger line says what the chapter cost, part by part — the only
    assert!(
        outcome.log.iter().any(|l| l.contains("staged in")),
        "{:?}",
        outcome.log
    );
    assert_eq!(
        outcome
            .log
            .iter()
            .filter(|l| l.contains("   part "))
            .count(),
        windows.len()
    );
}

/// A part that leaves a looping bed open is refused **before** the checkpoint
#[test]
fn a_part_that_leaves_a_bed_open_is_refused_without_moving_on() {
    let (_dir, layout, _text) = long_layout("manual-gate", 300);
    let settings = Settings::load(&layout.settings());
    let text = std::fs::read_to_string(layout.chapter_txt(51)).unwrap();
    let prepared = prepare_chapter(&text);
    let windows = plan_windows(&prepared, &settings.digest);
    assert!(windows.len() > 1);
    for (i, w) in windows.iter().enumerate() {
        let slice = w.prepared(&prepared);
        let accepted =
            manual_accept(&layout, 51, Round::Attribution, &cast_answer(&slice), None).unwrap();
        let cast = accepted.cast.unwrap();
        // The last part's staging answer opens a bed and never closes it.
        let mut answer: Value = serde_json::from_str(&script_answer(&slice)).unwrap();
        if i == windows.len() - 1 {
            answer["segments"][0]["sound_after"] = json!("food-prep");
        }
        let result = manual_accept(
            &layout,
            51,
            Round::Staging,
            &answer.to_string(),
            Some(&cast),
        );
        if i == windows.len() - 1 {
            let err = result.expect_err("an unclosed bed is refused").to_string();
            assert!(err.contains("food-prep"), "{err}");
            assert!(
                err.starts_with(&format!("part {}/{}", i + 1, windows.len())),
                "the complaint names the part to fix: {err}"
            );
        } else {
            result.expect("the earlier parts are fine");
        }
    }
}

/// **The severity rule, every cell.** The ladder is only as honest as this
#[test]
fn a_failure_is_retried_in_place_or_handed_back_by_whoever_owns_it() {
    // Its own fault: one more ask in place, then the chapter is over.
    assert_eq!(route(Round::Staging, Round::Staging, 0), Route::Retry);
    assert_eq!(route(Round::Staging, Round::Staging, 1), Route::Die);
    // Someone else's fault: never a retry, and **not on the last attempt
    assert_eq!(route(Round::Attribution, Round::Staging, 0), Route::Back);
    assert_eq!(route(Round::Attribution, Round::Staging, 1), Route::Back);
    // Total, so a blame naming a *later* step is still a hand-back rather
    assert_eq!(route(Round::Staging, Round::Attribution, 0), Route::Back);
}

#[test]
fn a_gate_says_whose_fault_it_is() {
    let (_dir, layout, text) = long_layout("blame", 3);
    let prepared = prepare_chapter(&text);
    let bible = json!({"characters": []});
    let vocab = vocabulary(&layout).expect("the fixture vocabulary");
    let context = parse_attribution(&cast_answer(&prepared), &bible, &prepared, false)
        .expect("the fixture cast is valid");

    // A staging answer handed an event the cast never attributed. Only the
    let orphan = script_answer(&prepared);
    let mut orphan: Value = serde_json::from_str(&orphan).unwrap();
    let mut segments = orphan["segments"].as_array().unwrap().clone();
    segments.push(json!({"source_id": "e9999", "text": "Một câu không ai đọng."}));
    orphan["segments"] = json!(segments);
    let complaint = parse_staged_script(&orphan.to_string(), &bible, &context, &prepared, &vocab)
        .expect_err("an unattributed event is refused");
    assert_eq!(
        complaint.blame,
        Round::Attribution,
        "the missing speaker is the cast's to fix: {}",
        complaint.why
    );

    // A staging answer that dropped an event the cast did attribute. That is
    let dropped = script_answer(&prepared);
    let mut dropped: Value = serde_json::from_str(&dropped).unwrap();
    let mut segments = dropped["segments"].as_array().unwrap().clone();
    segments.pop();
    dropped["segments"] = json!(segments);
    let complaint = parse_staged_script(&dropped.to_string(), &bible, &context, &prepared, &vocab)
        .expect_err("a dropped event is refused");
    assert_eq!(
        complaint.blame,
        Round::Staging,
        "dropping an event is staging's own mistake: {}",
        complaint.why
    );

    // And the attribution gate blames itself for everything, because
    let complaint = parse_attribution("not json at all", &bible, &prepared, false)
        .expect_err("garbage is refused");
    assert_eq!(complaint.blame, Round::Attribution, "{}", complaint.why);
}

#[test]
fn the_chapter_budget_refuses_rather_than_spending_forever() {
    let mut calls = GCalls::new(7);
    // The phrase pass gets its own three asks; each part then gets four,
    assert_eq!(calls.left, 3, "the phrase pass alone");
    calls.allow_parts(2);
    assert_eq!(calls.left, 11);
    let allowed = calls.left;
    for i in 0..allowed {
        calls.spend(Round::Staging.as_str()).expect("within budget");
        assert_eq!(calls.spent, i + 1);
    }
    // Routing can hand work backwards for ever if nothing stops it. The cap
    let err = calls
        .spend(Round::Attribution.as_str())
        .expect_err("the cap holds");
    let why = err.to_string();
    assert!(why.contains("ch7"), "{why}");
    assert!(why.contains("budget"), "{why}");
    assert_eq!(calls.spent, allowed, "the refused call is not counted");
}
