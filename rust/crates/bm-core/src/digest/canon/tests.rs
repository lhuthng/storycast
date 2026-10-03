use super::*;

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

#[test]
fn merge_bible_keeps_normalised_tags() {
    let mut bible = json!({"characters": []});
    let data = json!({
        "new_characters": [{"name": "Lão Trần", "personality": "gruff",
            "voice_hint": "elderly male", "tags": ["Old", "MALE", "old"],
            "proper_aliases": []}],
        "roster": ["Lão Trần"],
        "segments": []
    });
    merge_bible(&mut bible, &data, "07");
    assert_eq!(bible["characters"][0]["tags"], json!(["old", "male"]));
}

#[test]
fn merge_bible_adds_characters_and_refuses_duplicates() {
    let mut bible = json!({"characters": []});
    let data = json!({
        "new_characters": [{"name": "Lão Trần", "personality": "gruff", "voice_hint": "elderly male", "proper_aliases": ["Trần lão"]}],
        "roster": ["Lão Trần"],
        "segments": [{"speaker": "Lão Trần"}]
    });
    let log = merge_bible(&mut bible, &data, "07");
    assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
    assert_eq!(bible["characters"][0]["first_seen"], "07");
    assert_eq!(bible["characters"][0]["chapters_seen"], json!(["07"]));
    assert!(log.iter().any(|l| l.contains("bible +Lão Trần")));

    // second time: no duplicate
    merge_bible(&mut bible, &data, "08");
    assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
}

#[test]
fn merge_bible_never_promotes_pronouns() {
    let mut bible = json!({"characters": []});
    let data = json!({
        "new_characters": [{"name": "Hắn", "voice_hint": "adult male", "proper_aliases": ["y", "phàm nhân"]}],
        "roster": [],
        "segments": []
    });
    merge_bible(&mut bible, &data, "01");
    let aliases = bible["characters"][0]["proper_aliases"].as_array().unwrap();
    assert_eq!(aliases.len(), 1, "only the name itself: {aliases:?}");
    assert_eq!(aliases[0], "Hắn");
}

#[test]
fn merge_bible_rejects_an_alias_owned_by_another_character() {
    let mut bible = bible_with("A", &["Tuyết"]);
    let data = json!({
        "new_characters": [{"name": "B", "voice_hint": "adult female", "proper_aliases": ["Tuyết"]}],
        "roster": [],
        "segments": []
    });
    let log = merge_bible(&mut bible, &data, "02");
    assert!(log.iter().any(|l| l.contains("reject alias")), "{log:?}");
    let b = bible["characters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "B")
        .unwrap();
    assert_eq!(b["proper_aliases"], json!(["B"]));
}

#[test]
fn canon_key_strips_titles_descriptions_and_case() {
    assert_eq!(canon_key("Huyền Vũ lão tổ"), "huyền vũ");
    assert_eq!(canon_key("Mao Ý (thanh niên mặc hoa phục)"), "mao ý");
    assert_eq!(canon_key("Sở Cuồng Sư"), canon_key("Sở Cuồng sư"));
    assert_eq!(canon_key("  Dịch   Phong  "), "dịch phong");
    assert_eq!(
        canon_key("Lão Tổ"),
        "lão tổ",
        "a bare title is a name, not stripped"
    );
    assert_eq!(canon_key("Huyền Vũ tiền bối"), "huyền vũ");
}

#[test]
fn merge_bible_folds_a_suffixed_new_character_into_its_owner() {
    let mut bible = bible_with("Huyền Vũ", &["Huyền Vũ"]);
    let data = json!({
        "new_characters": [{
            "name": "Huyền Vũ lão tổ", "voice_hint": "adult male",
            "personality": "x", "tags": ["male"],
            "proper_aliases": []
        }],
        "roster": ["Huyền Vũ lão tổ"],
        "segments": [{"speaker": "Huyền Vũ lão tổ", "text": "Ừ."}]
    });
    let log = merge_bible(&mut bible, &data, "25");
    let chars = bible["characters"].as_array().unwrap();
    assert_eq!(chars.len(), 1, "no fork: {chars:?}");
    let aliases = chars[0]["proper_aliases"].as_array().unwrap();
    assert!(
        aliases.iter().any(|a| a == "Huyền Vũ lão tổ"),
        "{aliases:?}"
    );
    assert!(log.iter().any(|l| l.contains("=fold")), "{log:?}");
    assert_eq!(
        chars[0]["chapters_seen"],
        json!(["25"]),
        "variant speaker marks seen"
    );
}

#[test]
fn merge_bible_folds_a_case_variant_without_a_second_entry() {
    let mut bible = bible_with("Sở Cuồng Sư", &["Sở Cuồng Sư"]);
    let data = json!({
        "new_characters": [{
            "name": "Sở Cuồng sư", "voice_hint": "adult male",
            "personality": "x", "tags": ["male"], "proper_aliases": []
        }],
        "roster": [],
        "segments": []
    });
    merge_bible(&mut bible, &data, "03");
    assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
}

#[test]
fn merge_bible_resolves_a_variant_owner_key() {
    let mut bible = bible_with("Mao Ý", &["Mao Ý"]);
    let data = json!({
        "new_characters": [],
        "new_aliases": {"Mao Ý (thanh niên mặc hoa phục)": ["Mao Ý hoa phục"]},
        "roster": [],
        "segments": []
    });
    merge_bible(&mut bible, &data, "04");
    let aliases = bible["characters"][0]["proper_aliases"].as_array().unwrap();
    assert!(aliases.iter().any(|a| a == "Mao Ý hoa phục"), "{aliases:?}");
}

#[test]
fn resolve_speaker_prefers_exact_then_canon_then_passthrough() {
    let bible = bible_with("Sở Cuồng Sư", &["Sở Cuồng Sư"]);
    assert_eq!(resolve_speaker(&bible, "Sở Cuồng Sư"), "Sở Cuồng Sư");
    assert_eq!(resolve_speaker(&bible, "Sở Cuồng sư"), "Sở Cuồng Sư");
    assert_eq!(
        resolve_speaker(&bible, "Người Lạ"),
        "Người Lạ",
        "unknown passes through"
    );
}

#[test]
fn a_characters_own_name_beats_another_characters_alias_for_it() {
    // The bible contradicts itself routinely: a digest lists an epithet as
    // one character's alias, then a later chapter introduces it as a
    // character in its own right. Real shape — `Vân bá` is a character and
    // also sits in `Lão giả`'s aliases, earlier in the file. With the
    // passes interleaved, position decided the answer, the cast was keyed
    // under `Lão giả`, and the chapter would not plan.
    let bible = json!({"characters": [
        {"name": "Lão giả", "proper_aliases": ["Lão giả", "Kim lão", "Vân bá"]},
        {"name": "Vân bá", "proper_aliases": ["Ngao Vân"]}
    ]});
    assert_eq!(
        resolve_speaker(&bible, "Vân bá"),
        "Vân bá",
        "an exact name is an identity, never an alias"
    );
    // The alias still works for a form that is nobody's own name.
    assert_eq!(resolve_speaker(&bible, "Kim lão"), "Lão giả");
    // And the owner is stable whichever way round the file lists them.
    let flipped = json!({"characters": [
        {"name": "Vân bá", "proper_aliases": ["Ngao Vân"]},
        {"name": "Lão giả", "proper_aliases": ["Lão giả", "Kim lão", "Vân bá"]}
    ]});
    assert_eq!(resolve_speaker(&flipped, "Vân bá"), "Vân bá");
}

#[test]
fn canonicalize_script_rewrites_roster_and_speakers() {
    let bible = bible_with("Mao Ý", &["Mao Ý", "Mao Ý (thanh niên mặc hoa phục)"]);
    let mut data = json!({
        "roster": ["Mao Ý (thanh niên mặc hoa phục)", "Narrator"],
        "segments": [
            {"speaker": "Mao Ý (thanh niên mặc hoa phục)", "text": "Hừ."},
            {"speaker": "Narrator", "text": "Gió thổi."}
        ]
    });
    assert_eq!(canonicalize_script(&mut data, &bible), 2);
    assert_eq!(data["roster"], json!(["Mao Ý", "Narrator"]));
    assert_eq!(data["segments"][0]["speaker"], json!("Mao Ý"));
    assert_eq!(data["segments"][1]["speaker"], json!("Narrator"));
}

#[test]
fn apply_merges_unions_aliases_chapters_and_keeps_the_canonical_voice_hint() {
    let mut bible = json!({"characters": [
        {"name": "Huyền Vũ", "personality": "cold", "voice_hint": "adult male",
         "proper_aliases": ["Huyền Vũ"], "first_seen": "10", "chapters_seen": ["10"]},
        {"name": "Huyền Vũ lão tổ", "personality": "", "voice_hint": "",
         "proper_aliases": ["Huyền Vũ lão tổ"], "first_seen": "25", "chapters_seen": ["25", "26"]}
    ]});
    let (applied, _) = apply_merges(
        &mut bible,
        &[("Huyền Vũ".into(), vec!["Huyền Vũ lão tổ".into()])],
    );
    assert_eq!(applied.len(), 1);
    let chars = bible["characters"].as_array().unwrap();
    assert_eq!(chars.len(), 1);
    assert_eq!(chars[0]["name"], json!("Huyền Vũ"));
    let aliases = chars[0]["proper_aliases"].as_array().unwrap();
    assert!(
        aliases.iter().any(|a| a == "Huyền Vũ lão tổ"),
        "{aliases:?}"
    );
    assert_eq!(chars[0]["chapters_seen"], json!(["10", "25", "26"]));
    assert_eq!(
        chars[0]["voice_hint"],
        json!("adult male"),
        "canonical hint survives"
    );
}

#[test]
fn apply_merges_skips_unknown_names_quietly() {
    let mut bible = bible_with("A", &["A"]);
    let (applied, _) = apply_merges(
        &mut bible,
        &[
            ("A".into(), vec!["Ghost".into()]),
            ("Ghost".into(), vec!["A".into()]),
        ],
    );
    assert!(applied.is_empty());
    assert_eq!(bible["characters"].as_array().unwrap().len(), 1);
}

#[test]
fn merge_bible_refuses_bare_generics_as_new_aliases() {
    // ch112's hijack, pinned at the gate: "nữ tử" must never attach to
    // anyone again, while a name-bearing form still does.
    let mut bible = bible_with("Lạc Lan Tuyết", &["Lạc Lan Tuyết"]);
    let data = json!({
        "new_characters": [],
        "new_aliases": {"Lạc Lan Tuyết": ["nữ tử", "Nữ tử", "cô gái", "tiền bối", "vị kia", "Lý cô nương"]},
        "roster": [],
        "segments": []
    });
    merge_bible(&mut bible, &data, "200");
    let aliases: Vec<&str> = bible["characters"][0]["proper_aliases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a.as_str())
        .collect();
    assert!(
        aliases.contains(&"Lý cô nương"),
        "a name-bearing form still attaches: {aliases:?}"
    );
    for generic in ["nữ tử", "Nữ tử", "cô gái", "tiền bối", "vị kia"] {
        assert!(
            !aliases.iter().any(|a| a.to_lowercase() == generic),
            "{generic:?} must be refused: {aliases:?}"
        );
    }
}

#[test]
fn scrub_drops_legacy_generics_but_keeps_names_and_specifics() {
    let mut bible = json!({"characters": [
        {"name": "Lạc Lan Tuyết", "proper_aliases":
            ["Lạc Lan Tuyết", "Nữ tử", "nữ tử", "cô gái", "nữ tử áo trắng", "Lý cô nương"]},
        {"name": "Dịch Phong", "proper_aliases":
            ["Dịch Phong", "công tử", "tiền bối", "vị kia", "Dịch sư phụ"]},
        {"name": "Quản gia", "proper_aliases": ["Quản gia"]},
    ]});
    let log = scrub_ambiguous_aliases(&mut bible);
    assert_eq!(log.len(), 2, "{log:?}");
    let aliases = |n: &str| -> Vec<String> {
        bible["characters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap()["proper_aliases"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect()
    };
    assert_eq!(
        aliases("Lạc Lan Tuyết"),
        vec!["Lạc Lan Tuyết", "nữ tử áo trắng", "Lý cô nương"],
        "bare generics go, name-bearing forms stay"
    );
    assert_eq!(aliases("Dịch Phong"), vec!["Dịch Phong", "Dịch sư phụ"]);
    assert_eq!(
        aliases("Quản gia"),
        vec!["Quản gia"],
        "a character keeps its own name even when generic"
    );
    // Second run is a no-op: the scrub is idempotent.
    assert!(scrub_ambiguous_aliases(&mut bible).is_empty());
}

#[test]
fn a_fold_never_re_imports_a_generic_alias() {
    // The hole a scrub could never win through: `apply_merges` copied the
    // absorbed entry's aliases wholesale, so "Sư phụ" came back onto the
    // character every reconcile had just removed it from — and
    // `validate_digest_identity` then read it as exclusive ownership.
    let mut bible = json!({"characters": [
        {"name": "Thanh Sơn lão tổ", "proper_aliases":
            ["Thanh Sơn lão tổ", "Sư phụ", "sư tôn"]},
        {"name": "Thanh Sơn", "proper_aliases": ["lão đầu", "Lục Thanh Sơn"]},
    ]});
    let merges = vec![(
        "Thanh Sơn lão tổ".to_string(),
        vec!["Thanh Sơn".to_string()],
    )];
    let (applied, _) = apply_merges(&mut bible, &merges);
    assert_eq!(applied.len(), 1, "the fold applied");
    let aliases = alias_list(&bible, "Thanh Sơn lão tổ");
    for generic in ["Sư phụ", "sư tôn", "lão đầu"] {
        assert!(
            !aliases.iter().any(|a| a == generic),
            "{generic:?} must not survive the fold: {aliases:?}"
        );
    }
    assert!(
        aliases.contains(&"Lục Thanh Sơn".to_string()),
        "a proper name still joins: {aliases:?}"
    );
}

#[test]
fn merge_bible_heals_an_already_polluted_alias_list() {
    // Nobody presses reconcile on a machine nobody drives, so the legacy
    // pollution has to be cleaned by the writer that already runs: the
    // merge on every digest completion.
    let mut bible = json!({"characters": [{
        "name": "Thanh Sơn lão tổ", "personality": "p", "voice_hint": "elderly male",
        "tags": [], "proper_aliases":
            ["Thanh Sơn lão tổ", "Sư phụ", "tiểu thư", "Lục Thanh Sơn"],
        "first_seen": "01", "chapters_seen": []
    }]});
    let data = json!({
        "new_characters": [], "new_aliases": {},
        "roster": ["Thanh Sơn lão tổ"], "segments": []
    });
    let log = merge_bible(&mut bible, &data, "01");
    assert_eq!(
        alias_list(&bible, "Thanh Sơn lão tổ"),
        vec!["Thanh Sơn lão tổ", "Lục Thanh Sơn"],
        "the generic goes, the name stays"
    );
    assert!(
        log.iter().any(|l| l.contains("scrub")),
        "the repair is said out loud: {log:?}"
    );
}

/// The alias list of the named character, as owned strings.
fn alias_list(bible: &Value, name: &str) -> Vec<String> {
    bible["characters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap()["proper_aliases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a.as_str().map(String::from))
        .collect()
}
