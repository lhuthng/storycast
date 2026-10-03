use super::validate::inline_tags;
use super::*;
use serde_json::json;

/// A stand-in palette: the shipped map's values, which is what the digest
fn pal() -> Vec<String> {
    ["quiet", "warm", "busy", "battle", "grand", "none"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `kind` is code-attached, and `thought` is the only value code writes:
#[test]
fn only_a_thought_may_carry_a_kind() {
    let bible = json!({"characters": []});
    let context = json!({"roster": ["Narrator"]});
    let line = |kind: Option<&str>| {
        let mut line = json!({"speaker": "Narrator", "text": "She looked up."});
        if let Some(kind) = kind {
            line["kind"] = json!(kind);
        }
        line
    };
    validate_script(
        &json!({"segments": [line(Some("thought"))]}),
        &bible,
        &context,
        &pal(),
    )
    .unwrap();
    validate_script(&json!({"segments": [line(None)]}), &bible, &context, &pal()).unwrap();

    for bogus in ["dialogue", "narration", "spoken"] {
        let data = json!({"segments": [line(Some(bogus))]});
        let err = validate_script(&data, &bible, &context, &pal()).unwrap_err();
        assert!(err.to_string().contains("kind"), "{bogus}: {err}");
    }
}

#[test]
fn unknown_effect_tags_are_dropped_after_alias_resolution() {
    let mut data = json!({
        "segments": [{
            "speaker": "Narrator",
            "text": "Một nhát chém xuống.",
            "effect": ["battle", "stone", 7]
        }]
    });
    discard_unknown_effect_tags(&mut data, &["battle".into(), "sword".into()]);
    assert_eq!(data["segments"][0]["effect"], json!(["battle"]));
}

#[test]
fn tag_alias_targets_must_be_real_canonical_names() {
    let aliases: TagAliases = serde_json::from_value(json!({
        "music": {"calm": "quiet"},
        "effect": {"people": "crowd"},
        "sound": {"footsteps": "footstep-stone"}
    }))
    .unwrap();
    aliases
        .validate(
            &pal(),
            &["street", "crowd", "market"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            ["footstep-stone", "footstep-wood"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();

    let bad: TagAliases =
        serde_json::from_value(json!({"music": {"calm": "not-a-palette-name"}})).unwrap();
    let err = bad
        .validate(&pal(), &[], std::iter::empty())
        .expect_err("an unknown target must be caught before a digest starts");
    assert!(err.to_string().contains("not-a-palette-name"), "{err}");
}

#[test]
fn tag_aliases_canonicalize_every_closed_sound_vocabulary() {
    let aliases: TagAliases = serde_json::from_value(json!({
        "music": {"calm": "quiet"},
        "effect": {"people": "crowd"},
        "sound": {"footsteps": "footstep-stone"}
    }))
    .unwrap();
    let mut script = json!({
        "segments": [
            {"music": " calm ", "effect": ["street", "people"]},
            {"sound": "footsteps"},
            {"stop": "footsteps"}
        ]
    });

    apply_tag_aliases(&mut script, &aliases);

    assert_eq!(script["segments"][0]["music"], json!("quiet"));
    assert_eq!(script["segments"][0]["effect"], json!(["street", "crowd"]));
    assert_eq!(script["segments"][1]["sound"], json!("footstep-stone"));
    assert_eq!(script["segments"][2]["stop"], json!("footstep-stone"));
}

fn bible_with(name: &str, aliases: &[&str]) -> Value {
    json!({"characters": [{
        "name": name,
        "personality": "x",
        "voice_hint": "adult male",
        "proper_aliases": aliases,
        "first_seen": "01",
        "chapters_seen": []
    }]})
}

/// The cast pass's output for a script under test: the roster it declares.
fn ctx_of(script: &Value) -> Value {
    json!({"roster": script.get("roster").cloned().unwrap_or(json!([]))})
}

#[test]
fn inline_tags_accept_the_engine_three_and_nothing_else() {
    assert_eq!(inline_tags("Hắn [cười] lớn."), vec!["cười"]);
    assert_eq!(inline_tags("[thở dài] Rồi đi."), vec!["thở dài"]);
    assert!(inline_tags("Không có gì.").is_empty());
    assert_eq!(inline_tags("a [b] c [d]"), vec!["b", "d"]);

    let tagged = |text: &str| {
        json!({
            "segments": [{"speaker": "Narrator", "text": text, "direction": "Say calm in Vietnamese: x"}],
            "roster": ["Narrator"]
        })
    };
    let ok = |t: &str| {
        let d = tagged(t);
        validate_script(&d, &json!({"characters": []}), &ctx_of(&d), &pal())
    };
    ok("Hắn [cười].").unwrap();
    ok("Nàng [CƯỜI].").unwrap();
    ok("Hắn [sigh].").unwrap();
    let err = ok("Dừng [pause] lại.").unwrap_err();
    assert!(err.to_string().contains("[pause]"), "{err}");
    // Invented tags are read aloud downstream — that is why they fail here.
    let err = ok("Hắn [khóc].").unwrap_err();
    assert!(err.to_string().contains("voice tag"), "{err}");
}

#[test]
fn validate_allows_a_punctuation_line_when_the_script_has_speech() {
    let script = json!({
        "segments": [
            {"speaker": "Narrator", "text": ","},
            {"speaker": "Narrator", "text": "A spoken line."}
        ],
        "roster": ["Narrator"]
    });
    validate_script(
        &script,
        &json!({"characters": []}),
        &ctx_of(&script),
        &pal(),
    )
    .unwrap();

    let only_punctuation = json!({
        "segments": [{"speaker": "Narrator", "text": ","}],
        "roster": ["Narrator"]
    });
    let err = validate_script(
        &only_punctuation,
        &json!({"characters": []}),
        &ctx_of(&only_punctuation),
        &pal(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("no speakable content"), "{err}");

    let cue = json!({
        "segments": [{"speaker": "Narrator", "text": "[cười]"}],
        "roster": ["Narrator"]
    });
    validate_script(&cue, &json!({"characters": []}), &ctx_of(&cue), &pal()).unwrap();
}

#[test]
fn retag_text_converts_laughs_sighs_and_coughs() {
    // Real shapes from the corpus.
    assert_eq!(
        retag_text("Ha ha ha!"),
        Some("[cười]".into()),
        "a bare laugh becomes a bare tag"
    );
    assert_eq!(
        retag_text("\"Ha ha, khách sáo quá.\""),
        Some("\"[cười] khách sáo quá.\"".into())
    );
    assert_eq!(
        retag_text("Thật à, ta đúng là kỳ tài ngút trời ha ha..."),
        Some("Thật à, ta đúng là kỳ tài ngút trời [cười]".into())
    );
    assert_eq!(
        retag_text("\"A ha ha, đã lâu không gặp.\""),
        Some("\"[cười] đã lâu không gặp.\"".into())
    );
    assert_eq!(
        retag_text("\"Hắc hắc, tới đây!\""),
        Some("\"[cười] tới đây!\"".into())
    );
    assert_eq!(retag_text("Hô hô hô hô."), Some("[cười]".into()));
    assert_eq!(
        retag_text("Haizz, đứa trẻ này..."),
        Some("[thở dài] đứa trẻ này...".into())
    );
    assert_eq!(
        retag_text("Bành Anh thở dài một tiếng, nói:"),
        Some("Bành Anh [thở dài] nói:".into()),
        "the Vietnamese written form carries a quantifier that the tag replaces"
    );
    assert_eq!(
        retag_text("Bành Anh thở dài một hơi rồi mới cất lời:"),
        Some("Bành Anh [thở dài] mới cất lời:".into()),
        "the connective belongs to the written sigh, not the spoken words"
    );
    assert_eq!(
        retag_text("Bành Anh [thở dài] một tiếng, nói:"),
        Some("Bành Anh [thở dài] nói:".into()),
        "a quantifier after the tag is still part of the written sound"
    );
    assert_eq!(
        retag_text("\"Khụ khụ, thôi không được đâu.\""),
        Some("\"[hắng giọng] thôi không được đâu.\"".into())
    );
    // Tag plus literal collapses to the tag.
    assert_eq!(
        retag_text("[cười] Ha ha ha. Cứ kêu đi!"),
        Some("[cười] Cứ kêu đi!".into())
    );
    assert_eq!(
        retag_text("[cười] \"Ha ha, khách sáo quá.\""),
        Some("[cười] \"khách sáo quá.\"".into())
    );
    // A tag already present blocks a second one: no change at all.
    assert_eq!(retag_text("[cười] Haizz..."), None);
}

#[test]
fn retag_text_leaves_everything_else_alone() {
    // Contempt, acknowledgments, exclamations, clicks, verbs: no tag fits.
    for t in [
        "Hừ!",
        "Thanh Sơn lão tổ hừ lạnh một tiếng.",
        "Ừm!",
        "\"Ừm...\" Mậu Mậu gãi đầu.",
        "Ồ, đến rồi!",
        "Trời ơi!",
        "\"Hả?\"",
        "Dịch Phong khẽ tặc lưỡi.",
        "Lạc Lan Tuyết hít sâu một hơi, nghiêm túc nói:",
        "như trút được gánh nặng, thở phào nhẹ nhõm",
        "Không có gì đặc biệt.",
    ] {
        assert_eq!(retag_text(t), None, "{t:?} must not change");
    }
    // Word-boundary discipline: `hai` (two) and `aha` (eureka) are words.
    assert_eq!(retag_text("mang thêm hai cái ghế ra đây."), None);
    assert_eq!(retag_text("Aha, ra vậy!"), None);
    // A lone `hô` is the verb "to shout", not laughter — only repetition counts.
    assert_eq!(retag_text("Mọi người hô to."), None);
    assert_eq!(retag_text("cách xưng hô của ngươi."), None);
    assert_eq!(retag_text("Hô hô hô hô."), Some("[cười]".into()));
}

#[test]
fn validate_rejects_a_speaker_outside_the_roster() {
    let data = json!({
        "segments": [{"speaker": "Ghost", "text": "hi", "direction": "Say calm in Vietnamese: hi"}],
        "roster": ["Narrator"]
    });
    let err =
        validate_script(&data, &json!({"characters": []}), &ctx_of(&data), &pal()).unwrap_err();
    assert!(err.to_string().contains("unknown speaker"), "{err}");
    // ...and the same script passes when the cast pass listed the speaker.
    let listed = json!({"roster": ["Narrator", "Ghost"]});
    validate_script(&data, &json!({"characters": []}), &listed, &pal()).unwrap();
}

#[test]
fn validate_ignores_direction_and_rejects_a_bad_voice_hint() {
    // `direction` used to be required ("Say ..."); nothing consumes it, so
    let no_dir = json!({
        "segments": [{"speaker": "Narrator", "text": "hi"}],
        "roster": ["Narrator"]
    });
    validate_script(
        &no_dir,
        &json!({"characters": []}),
        &ctx_of(&no_dir),
        &pal(),
    )
    .unwrap();

    let bad_hint = json!({
        "segments": [{"speaker": "Narrator", "text": "hi"}],
        "roster": ["Narrator"],
        "new_characters": [{"name": "X", "voice_hint": "mysterious"}]
    });
    let err = validate_context(&bad_hint, &json!({"characters": []})).unwrap_err();
    assert!(err.to_string().contains("gender/age"), "{err}");
}

#[test]
fn validate_accepts_a_well_formed_digest() {
    let data = json!({
        "atmosphere": "A market at dawn.",
        "roster": ["Narrator", "Dịch Phong"],
        "mentions": {"hắn": "Dịch Phong"},
        "new_characters": [{"name": "Lão Trần", "voice_hint": "elderly male, gruff", "tags": ["old", "male"]}],
        "segments": [{"speaker": "Narrator", "text": "Trời sáng.", "direction": "Say calm in Vietnamese: Trời sáng."}]
    });
    // The cast pass owns identity and the script pass owns the speech, so a
    validate_context(&data, &json!({"characters": []})).unwrap();
    validate_script(&data, &json!({"characters": []}), &ctx_of(&data), &pal()).unwrap();
}

#[test]
fn validate_rejects_a_missing_or_sloppy_tags_array() {
    let base = || {
        json!({
            "segments": [{"speaker": "Narrator", "text": "hi", "direction": "Say calm in Vietnamese: hi"}],
            "roster": ["Narrator"],
        })
    };
    // Missing key entirely.
    let mut no_tags = base();
    no_tags["new_characters"] = json!([{"name": "X", "voice_hint": "adult male, gruff"}]);
    assert!(validate_context(&no_tags, &json!({"characters": []})).is_err());

    // A sentence is not a tag.
    let mut sloppy = base();
    sloppy["new_characters"] =
        json!([{"name": "X", "voice_hint": "adult male, gruff", "tags": ["old man"]}]);
    let err = validate_context(&sloppy, &json!({"characters": []})).unwrap_err();
    assert!(err.to_string().contains("single tokens"), "{err}");

    // `[]` is the honest answer for the ageless — and it validates.
    let mut ageless = base();
    ageless["new_characters"] =
        json!([{"name": "X", "voice_hint": "elderly male, flat", "tags": []}]);
    validate_context(&ageless, &json!({"characters": []})).unwrap();
}

#[test]
fn validate_closes_the_music_vocabulary_and_keeps_old_scripts_mergeable() {
    let one = |music: &str| {
        json!({
            "segments": [{"speaker": "Narrator", "text": "x", "music": music}],
            "roster": ["Narrator"]
        })
    };
    let bible = json!({"characters": []});
    let check = |music: &str| {
        let d = one(music);
        validate_script(&d, &bible, &ctx_of(&d), &pal())
    };

    check("quiet").unwrap();
    // `none` is a value, not an absence.
    check("none").unwrap();

    // Out of the palette: rejected, and the message names it so the repair
    let err = check("melancholy").unwrap_err();
    assert!(err.to_string().contains("palette"), "{err}");
    assert!(err.to_string().contains("quiet"), "{err}");

    // Half-declared is rejected: the field is a statement about every
    let mixed = json!({
        "segments": [
            {"speaker": "Narrator", "text": "x", "music": "quiet"},
            {"speaker": "Narrator", "text": "y"}
        ],
        "roster": ["Narrator"]
    });
    let err = validate_script(&mixed, &bible, &ctx_of(&mixed), &pal()).unwrap_err();
    assert!(err.to_string().contains("missing `music`"), "{err}");

    // No value anywhere: a script from before the field existed. It still
    let legacy = json!({
        "segments": [{"speaker": "Narrator", "text": "x", "scene": "street-day"}],
        "roster": ["Narrator"]
    });
    validate_script(&legacy, &bible, &ctx_of(&legacy), &pal()).unwrap();

    // A map with no palette cannot judge a value, so it does not try.
    let d = one("melancholy");
    validate_script(&d, &bible, &ctx_of(&d), &[]).unwrap();
}

/// ch6's street: two consecutive lines both hail `"Dịch sư phụ."`. They are
#[test]
fn the_duplicate_line_rule_reads_source_ids_not_adjacency() {
    let bible = json!({"characters": []});
    let crowd = json!({
        "segments": [
            {"source_id": "e0002", "speaker": "Anonymous", "text": "Dịch sư phụ."},
            {"source_id": "e0003", "speaker": "Anonymous", "text": "Dịch sư phụ."}
        ],
        "roster": ["Anonymous"]
    });
    validate_script(&crowd, &bible, &ctx_of(&crowd), &pal()).unwrap();

    // One event, both halves the whole line: that is the split the rule is
    let split = json!({
        "segments": [
            {"source_id": "e0002", "speaker": "Anonymous", "text": "Dịch sư phụ."},
            {"source_id": "e0002", "speaker": "Anonymous", "text": "Dịch sư phụ."}
        ],
        "roster": ["Anonymous"]
    });
    let err = validate_script(&split, &bible, &ctx_of(&split), &pal()).unwrap_err();
    assert!(err.to_string().contains("e0002"), "{err}");
    assert!(err.to_string().contains("partitions"), "{err}");

    // No ids at all — a script from the manual prompt, where nothing else
    let legacy = json!({
        "segments": [
            {"speaker": "Narrator", "text": "Hắn gật đầu."},
            {"speaker": "Narrator", "text": "Hắn gật đầu."}
        ],
        "roster": ["Narrator"]
    });
    let err = validate_script(&legacy, &bible, &ctx_of(&legacy), &pal()).unwrap_err();
    assert!(err.to_string().contains("same line twice"), "{err}");
}

#[test]
fn validate_effect_tags_accepts_pool_tags_and_old_digests() {
    let fx = ["rain".to_string(), "night".to_string()];
    let seg = |effect: Value| {
        json!({
            "segments": [{"speaker": "Narrator", "text": "x", "effect": effect}],
            "roster": ["Narrator"]
        })
    };
    validate_effect_tags(&seg(json!(["rain", "night"])), &fx).unwrap();
    validate_effect_tags(&seg(json!([])), &fx).unwrap();
    // No `effect` anywhere: a digest from before the field existed.
    validate_effect_tags(
        &json!({
            "segments": [{"speaker": "Narrator", "text": "x"}],
            "roster": ["Narrator"]
        }),
        &fx,
    )
    .unwrap();
    let err = validate_effect_tags(&seg(json!(["rain", "thunderstorm"])), &fx).unwrap_err();
    assert!(err.to_string().contains("thunderstorm"), "{err}");
    let err = validate_effect_tags(&seg(json!("rain")), &fx).unwrap_err();
    assert!(err.to_string().contains("must be an array"), "{err}");
}

#[test]
fn validate_injects_accepts_sound_items_and_refuses_bad_names_and_long_hits() {
    use crate::audio_pool::{ClipPool, Sound};
    let mk = |dur: f64, mode: &str| Sound {
        tags: vec![],
        files: vec!["injects/x.mp3".into()],
        looped: false,
        dur_s: Some(dur),
        mode: Some(mode.into()),
        hold: None,
        level: None,
    };
    let pool: ClipPool = [("blood", mk(1.1, "hit")), ("boil", mk(51.0, "overlap"))]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    // A line, then the sound items that follow it. Every sound below sits
    let doc = |sounds: Value| {
        let mut items = vec![json!({"speaker": "Narrator", "text": "Hắn vung kiếm."})];
        items.extend(sounds.as_array().cloned().unwrap_or_default());
        items.push(json!({"speaker": "Narrator", "text": "Rồi hắn gục xuống."}));
        json!({"segments": items, "roster": ["Narrator"]})
    };
    // A sound is a name and nothing else; a stop is a name and nothing else.
    validate_injects(
        &doc(json!([{"sound": "boil"}, {"sound": "blood"}, {"stop": "boil"}])),
        &pool,
    )
    .unwrap();
    validate_injects(&doc(json!([])), &pool).unwrap();
    validate_injects(
        &json!({"segments": [{"speaker": "Narrator", "text": "x"}]}),
        &pool,
    )
    .unwrap();
    // Unknown sounds, in either shape.
    let err = validate_injects(&doc(json!([{"sound": "thunder"}])), &pool).unwrap_err();
    assert!(err.to_string().contains("thunder"), "{err}");
    let err = validate_injects(&doc(json!([{"stop": "thunder"}])), &pool).unwrap_err();
    assert!(err.to_string().contains("thunder"), "{err}");
    // Behaviour is not the script's to set. All three keys, by name — each
    for key in ["mode", "hold", "level"] {
        let mut item = json!({"sound": "blood"});
        item[key] = if key == "mode" {
            json!("trail")
        } else {
            json!(2.0)
        };
        let err = validate_injects(&doc(json!([item])), &pool).unwrap_err();
        assert!(
            err.to_string().contains("not the script's to set"),
            "{key}: {err}"
        );
    }
    // A pool entry that says `hit` on a 51 s clip is a pool bug, and the
    let bad: ClipPool = [("boil", mk(51.0, "hit"))]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let err = validate_injects(&doc(json!([{"sound": "boil"}])), &bad).unwrap_err();
    assert!(err.to_string().contains("inject-pool.json"), "{err}");
    // An item has to be one thing or the other.
    let err = validate_injects(
        &json!({"segments": [
            {"speaker": "Narrator", "text": "x"},
            {"mood": "angry"},
        ]}),
        &pool,
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a line"), "{err}");
    // A sound with no line before it has no seam to fire at.
    let err = validate_injects(
        &json!({"segments": [
            {"sound": "blood"},
            {"speaker": "Narrator", "text": "x"},
        ]}),
        &pool,
    )
    .unwrap_err();
    assert!(err.to_string().contains("seam"), "{err}");
    // The rejected shape, refused by name: a sound field on a line is read
    let err = validate_injects(
        &json!({"segments": [{"speaker": "Narrator", "text": "x", "sound": "blood"}]}),
        &pool,
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a field on a line"), "{err}");
    // ...and so is the top-level array it used to live in.
    let err = validate_injects(
        &json!({
            "segments": [{"speaker": "Narrator", "text": "x"}],
            "injects": [{"sound": "blood"}],
        }),
        &pool,
    )
    .unwrap_err();
    assert!(err.to_string().contains("not a top-level array"), "{err}");
}

/// The prompt-side sound fields must not survive onto a written script.
#[test]
fn validate_injects_refuses_a_prompt_side_sound_field() {
    use crate::audio_pool::{ClipPool, Sound};
    let pool: ClipPool = [(
        "page-turn",
        Sound {
            tags: vec![],
            files: vec!["injects/page-turn-1.mp3".into()],
            looped: false,
            dur_s: Some(0.6),
            mode: Some("overlap".into()),
            hold: None,
            level: None,
        },
    )]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for key in ["sound_after", "stop_after"] {
        let mut line = json!({"speaker": "Narrator", "text": "x"});
        line[key] = json!("page-turn");
        let err = validate_injects(&json!({"segments": [line], "roster": ["Narrator"]}), &pool)
            .unwrap_err();
        assert!(
            err.to_string().contains("prompt-side field"),
            "{key}: {err}"
        );
    }
}

/// A `stop` for a sound nothing started is silence with extra steps.
#[test]
fn validate_injects_refuses_a_stop_with_nothing_to_stop() {
    use crate::audio_pool::{ClipPool, Sound};
    let mk = |dur: f64, mode: &str| Sound {
        tags: vec![],
        files: vec!["injects/x.mp3".into()],
        looped: true,
        dur_s: Some(dur),
        mode: Some(mode.into()),
        hold: None,
        level: None,
    };
    let pool: ClipPool = [("cooking", mk(22.0, "overlap"))]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let doc = |items: Value| json!({"segments": items, "roster": ["Narrator"]});
    let line = |t: &str| json!({"speaker": "Narrator", "text": t});

    // The exact failure: a stop, and no start anywhere before it.
    let err = validate_injects(
        &doc(json!([
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"stop": "cooking"},
            line("Đồ nhi, tâm cảnh của tiền bối thật đáng để chúng ta học hỏi!"),
        ])),
        &pool,
    )
    .unwrap_err();
    assert!(err.to_string().contains("nothing is running"), "{err}");

    // The pair, and it passes.
    validate_injects(
        &doc(json!([
            line("Sau một hồi cảm khái, hai người liền đi đến phòng bếp."),
            {"sound": "cooking"},
            line("Đồ nhi, tâm cảnh của tiền bối thật đáng để chúng ta học hỏi!"),
            {"stop": "cooking"},
        ])),
        &pool,
    )
    .unwrap();

    // Order matters: a stop written *before* its start is the same silence.
    let err = validate_injects(
        &doc(json!([
            line("Một."),
            {"stop": "cooking"},
            {"sound": "cooking"},
            line("Hai."),
        ])),
        &pool,
    )
    .unwrap_err();
    assert!(err.to_string().contains("nothing is running"), "{err}");
}

#[test]
fn validate_title_takes_a_name_and_refuses_the_crawled_headline() {
    let doc = |t: &str| {
        json!({
            "title": t,
            "segments": [{"speaker": "Narrator", "text": "Chương 9: Tê! Thật là khủng khiếp dao phay"}],
        })
    };
    validate_title(&doc("Thần Binh Dao Phay")).unwrap();
    validate_title(&doc("Thanh Sơn Kinh Hồn")).unwrap();
    // The two real headlines, refused. Both are word-for-word MT of the
    let err = validate_title(&doc("Tê! Thật là khủng khiếp dao phay")).unwrap_err();
    assert!(err.to_string().contains("machine-translated"), "{err}");
    let err =
        validate_title(&doc("Tiền bối đối với dao phay yêu cầu đều cao như vậy?")).unwrap_err();
    assert!(err.to_string().contains("machine-translated"), "{err}");
    // Copied through unchanged is still copied through, even when the
    let clean = |t: &str| {
        json!({
            "title": t,
            "segments": [{"speaker": "Narrator", "text": "Chương 9: Thần binh dao phay"}],
        })
    };
    let err = validate_title(&clean("Thần binh dao phay")).unwrap_err();
    assert!(err.to_string().contains("copied through"), "{err}");
    validate_title(&clean("Thần Binh Dao Phay")).unwrap();
    // Absent, empty, numbered, a single word, and a sentence.
    let err = validate_title(&json!({"segments": []})).unwrap_err();
    assert!(err.to_string().contains("no `title`"), "{err}");
    let err = validate_title(&doc("   ")).unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
    let err = validate_title(&doc("Chương 9 Thần Binh")).unwrap_err();
    assert!(err.to_string().contains("chapter number"), "{err}");
    let err = validate_title(&doc("Dao")).unwrap_err();
    assert!(err.to_string().contains("one word"), "{err}");
    let err =
        validate_title(&doc("một hai ba bốn năm sáu bảy tám chín mười mười một")).unwrap_err();
    assert!(err.to_string().contains("not a sentence"), "{err}");
}

#[test]
fn vietnamese_leak_detection_ignores_known_names() {
    let bible = bible_with("Lạc Lan Tuyết", &["Tuyết"]);
    let data = json!({
        "atmosphere": "A cold morning in the courtyard.",
        "new_characters": [{"name": "Lạc Lan Tuyết", "personality": "lạnh lùng", "voice_hint": "adult female"}]
    });
    let warns = warn_vietnamese(&data, &bible);
    assert!(
        warns.iter().any(|w| w.contains("personality")),
        "expected a personality warning: {warns:?}"
    );
    // the name itself must not trip the detector
    assert!(
        !warns.iter().any(|w| w.contains("atmosphere")),
        "English atmosphere flagged: {warns:?}"
    );
}

#[test]
fn adjacent_duplicate_lines_are_refused_but_distant_repeats_pass() {
    // ch112's shape: every quoted line emitted twice in a row, once as
    let doubled = json!({
        "segments": [
            {"speaker": "Narrator", "text": "A, đây có một cái đầm nước."},
            {"speaker": "Dịch Phong", "text": "A, đây có một cái đầm nước."},
        ],
        "roster": ["Narrator", "Dịch Phong"]
    });
    let err = validate_script(
        &doubled,
        &json!({"characters": []}),
        &ctx_of(&doubled),
        &pal(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("twice"), "{err}");

    // A sound item between the halves does not launder the duplication.
    let with_sound = json!({
        "segments": [
            {"speaker": "Narrator", "text": "Đi thôi."},
            {"sound": "coin"},
            {"speaker": "Dịch Phong", "text": "Đi thôi."},
        ],
        "roster": ["Narrator", "Dịch Phong"]
    });
    validate_script(
        &with_sound,
        &json!({"characters": []}),
        &ctx_of(&with_sound),
        &pal(),
    )
    .unwrap_err();

    // The same cry pages apart is the chapter's business, not a failed split.
    let distant = json!({
        "segments": [
            {"speaker": "Dịch Phong", "text": "Lạc Ly!"},
            {"speaker": "Narrator", "text": "Gió thổi qua mặt hồ."},
            {"speaker": "Dịch Phong", "text": "Lạc Ly!"},
        ],
        "roster": ["Narrator", "Dịch Phong"]
    });
    validate_script(
        &distant,
        &json!({"characters": []}),
        &ctx_of(&distant),
        &pal(),
    )
    .unwrap();
}
