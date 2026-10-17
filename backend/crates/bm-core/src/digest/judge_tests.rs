use super::judge::*;
use super::*;
use crate::voices;

/// The engine's own tag and gloss, read rather than repeated, so a change to the
/// declaration does not silently change what these tests assert.
fn throat() -> (&'static str, &'static str) {
    let n = voices::nonverbals("vieneu")
        .iter()
        .find(|n| n.key == "throat")
        .expect("vieneu declares a throat-clear");
    (n.tag, n.gloss)
}

fn script(segments: Value) -> Value {
    json!({ "segments": segments })
}

#[test]
fn a_declaration_is_the_line_both_ways() {
    let (tag, gloss) = throat();
    let s = script(json!([
        { "speaker": "Lâm", "text": "Khụ khụ khụ, đành đợi vậy.",
          "nonverbal": { "key": "throat", "replaces": "Khụ khụ khụ" } },
        // Narration: read as written, never asked about.
        { "speaker": "Narrator", "text": "Hắn ho một tiếng." },
    ]));
    let d = declarations(&s, "vieneu");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].raw, "Khụ khụ khụ, đành đợi vậy.");
    assert_eq!(d[0].tagged, format!("{tag}, đành đợi vậy."));
    assert_eq!(d[0].replaces, "Khụ khụ khụ");
    assert_eq!(d[0].sound, gloss);
    // The question is about one sound: the tag is written where the words were.
    assert!(!d[0].tagged.contains("Khụ"));
}

#[test]
fn a_declaration_the_code_cannot_place_is_not_asked_about() {
    // The gate refuses all three; the judge must not ask about what the gate
    // already rejected, or it would score a question nobody can answer.
    let s = script(json!([
        { "text": "Cười.", "nonverbal": { "key": "chuckle", "replaces": "Cười" } },
        { "text": "Cười.", "nonverbal": { "key": "throat", "replaces": "Khụ" } },
        { "text": "Cười.", "nonverbal": { "key": "throat", "replaces": "" } },
    ]));
    assert!(declarations(&s, "vieneu").is_empty());
}

#[test]
fn off_asks_nothing_and_costs_nothing() {
    let j = crate::config::JudgeSettings {
        mode: "off".into(),
        ..Default::default()
    };
    let s = script(json!([
        { "text": "Cười.", "nonverbal": { "key": "laugh", "replaces": "Cười" } },
    ]));
    let out = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(check(&s, "vieneu", &j));
    assert_eq!(out.asked, 0);
    assert!(out.doubts.is_empty());
    assert!(out.complaint.is_none());
    // Not even "skipped": off is not an absence, it is a decision.
    assert!(out.skipped.is_none());
}

#[test]
fn the_dial_decides_how_much_doubt_acts() {
    // The judge's answers overlap end to end, so the dial is a price, not a
    // separator. These are the two ends of the documented table.
    assert!(is_doubt(0.09, 0.10));
    assert!(!is_doubt(0.10, 0.10));
    assert!(!is_doubt(0.57, 0.10), "a loud yes is never doubted");
    assert!(is_doubt(0.99, 1.0), "nothing survives a dial of 1.0");
}

#[test]
fn the_complaint_says_which_sound_and_what_to_do() {
    let (_, gloss) = throat();
    let d = Declaration {
        raw: "Khụ khụ khụ, đành đợi vậy.".into(),
        tagged: "[hắng giọng], đành đợi vậy.".into(),
        replaces: "Khụ khụ khụ".into(),
        sound: gloss.into(),
    };
    let c = complaint(&d, 0.03);
    assert!(c.contains(gloss), "{c}");
    assert!(c.contains("0.03"), "{c}");
    assert!(c.contains("drop it"), "{c}");
}
