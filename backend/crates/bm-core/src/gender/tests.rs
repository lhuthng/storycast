use super::*;
use crate::voices::vieneu_policy;
use serde_json::json;

fn pool_with(entries: &[(&str, &[&str])]) -> Pool {
    entries
        .iter()
        .map(|(name, tags)| {
            (
                (*name).to_string(),
                pool::PoolEntry {
                    file: format!("refs/{name}.mp3"),
                    tags: tags.iter().map(|t| (*t).to_string()).collect(),
                    label: String::new(),
                },
            )
        })
        .collect()
}

fn vi() -> GenderLexicon {
    GenderLexicon {
        female: vec![
            "mẫu".into(),
            "mẹ".into(),
            "má".into(),
            "cô".into(),
            "bà".into(),
            "cô nương".into(),
        ],
        male: vec!["lão".into(), "bá".into()],
    }
}

#[test]
fn a_head_states_a_gender_and_unknown_states_none() {
    assert_eq!(hint_gender("adult female, cold"), Some(FEMALE));
    assert_eq!(hint_gender("elderly male"), Some(MALE));
    assert_eq!(hint_gender("girl"), Some(FEMALE));
    assert_eq!(hint_gender("boy"), Some(MALE));
    assert_eq!(hint_gender("unknown"), None);
    assert_eq!(hint_gender(""), None);
}

#[test]
fn a_voice_states_its_gender_by_policy_or_pool_tag() {
    let p = vieneu_policy();
    let pool = pool_with(&[
        ("young-male-1", &["young", "male"]),
        ("young-female-1", &["young", "female"]),
        ("mystery", &["young"]),
    ]);
    assert_eq!(voice_gender("Trúc Ly", &p, &pool), Some(FEMALE));
    assert_eq!(voice_gender("young-male-1", &p, &pool), Some(MALE));
    assert_eq!(voice_gender("young-female-1", &p, &pool), Some(FEMALE));
    assert_eq!(voice_gender("mystery", &p, &pool), None);
}

#[test]
fn the_lexicon_matches_whole_words_and_keeps_their_diacritics() {
    let lex = vi();
    assert_eq!(lex.gender_of("Vân mẫu", &[]), Some(FEMALE));
    assert_eq!(lex.gender_of("Mẹ Vân", &[]), Some(FEMALE));
    assert_eq!(lex.gender_of("Cô Nương X", &[]), Some(FEMALE));
    assert_eq!(lex.gender_of("Lão Trần", &[]), Some(MALE));
    assert_eq!(lex.gender_of("Vân Vũ", &[]), None);
    // Folding dropped, `Ma` would be `má`, `Cơ` would be `cô`, `bá` would be
    // `bà` — a demon lord, a mountain, and an uncle all misgendered.
    assert_eq!(lex.gender_of("Hổ Ma Tôn", &[]), None);
    assert_eq!(lex.gender_of("Trúc Cơ Sơn", &[]), None);
    assert_eq!(lex.gender_of("Lâm bá", &[]), Some(MALE));
}

#[test]
fn a_stated_bible_gender_outranks_a_contradicting_voice() {
    let p = vieneu_policy();
    let pool = pool_with(&[
        ("young-male-1", &["young", "male"]),
        ("young-female-1", &["young", "female"]),
    ]);
    let bible = json!({"characters": [
        {"name": "Vân Vũ", "voice_hint": "adult female", "tags": ["youth"], "personality": "Lively."}
    ]});
    let cast: BTreeMap<String, String> = [("Vân Vũ".into(), "young-male-1".into())].into();
    let fixes = plan(&bible, &cast, &p, &pool, &GenderLexicon::default());
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0].gender, FEMALE);
    assert_eq!(fixes[0].new_voice.as_deref(), Some("young-female-1"));
    assert_eq!(fixes[0].new_hint, None);
}

#[test]
fn a_placeholder_is_silence_and_the_lexicon_speaks() {
    let p = vieneu_policy();
    let pool = pool_with(&[
        ("young-male-1", &["young", "male"]),
        ("young-female-1", &["young", "female"]),
    ]);
    let bible = json!({"characters": [
        {"name": "Vân mẫu", "voice_hint": "adult male", "tags": ["male"], "personality": ""}
    ]});
    let cast: BTreeMap<String, String> = [("Vân mẫu".into(), "young-male-1".into())].into();
    let fixes = plan(&bible, &cast, &p, &pool, &vi());
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0].gender, FEMALE);
    assert_eq!(fixes[0].new_voice.as_deref(), Some("young-female-1"));
    assert_eq!(fixes[0].new_hint.as_deref(), Some("adult female"));
}

#[test]
fn silence_is_not_repaired() {
    // A placeholder with no lexicon word: nothing states a gender, so a voice
    // that already disagrees is left for a real reading, not guessed here.
    let p = vieneu_policy();
    let pool = pool_with(&[("young-female-1", &["young", "female"])]);
    let bible = json!({"characters": [
        {"name": "Thủy hành", "voice_hint": "adult male", "tags": ["male"], "personality": ""}
    ]});
    let cast: BTreeMap<String, String> = [("Thủy hành".into(), "young-female-1".into())].into();
    assert!(plan(&bible, &cast, &p, &pool, &GenderLexicon::default()).is_empty());
}

#[test]
fn a_cast_only_name_the_lexicon_knows_is_repaired() {
    let p = vieneu_policy();
    let pool = pool_with(&[
        ("young-male-1", &["young", "male"]),
        ("young-female-1", &["young", "female"]),
    ]);
    let cast: BTreeMap<String, String> = [("Mẹ Vân".into(), "young-male-1".into())].into();
    let fixes = plan(
        &json!({"characters": []}),
        &cast,
        &p,
        &pool,
        &GenderLexicon {
            female: vec!["mẹ".into()],
            male: vec![],
        },
    );
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0].new_voice.as_deref(), Some("young-female-1"));
}

