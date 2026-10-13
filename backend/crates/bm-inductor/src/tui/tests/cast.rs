use super::*;

// --- cast overview ------------------------------------------------------

#[test]
fn cast_rows_put_narrator_first_and_keep_unassigned_speakers() {
    let rows = cast_rows(&roster_fixture());
    assert_eq!(
        rows[0].character, "Narrator",
        "Narrator is the fallback voice"
    );
    assert_eq!(rows.len(), 6, "every speaker appears exactly once");
    let moi = rows.iter().find(|r| r.character == "Mới").unwrap();
    assert!(moi.unassigned());
    assert_eq!(moi.verdict(), Verdict::Unassigned);
}

#[test]
fn cast_rows_flag_shared_voices_from_both_sides() {
    let rows = cast_rows(&roster_fixture());
    let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().clone();
    assert_eq!(by("Kiên").shared_with, vec!["Vũ".to_string()]);
    assert_eq!(by("Vũ").shared_with, vec!["Kiên".to_string()]);
    assert!(by("Kiên").shared());
    assert!(
        !by("Narrator").shared(),
        "a sole user of a voice is not flagged"
    );
}

#[test]
fn cast_rows_accept_every_voice_the_roster_lists_and_flag_unknown_ones() {
    let rows = cast_rows(&roster_fixture());
    let by = |n: &str| rows.iter().find(|r| r.character == n).unwrap().verdict();
    assert_eq!(by("Lâm"), Verdict::Ok, "listed, so assignable");
    assert_eq!(
        by("Hà"),
        Verdict::Unknown,
        "the roster has never heard of it"
    );
    assert_eq!(by("Kiên"), Verdict::Ok, "enrolled clones are assignable");
    assert_eq!(by("Narrator"), Verdict::Ok);
}

#[test]
fn unassigned_speakers_do_not_count_as_sharing_the_empty_voice() {
    let mut r = roster_fixture();
    r.cast.retain(|k, _| k == "Kiên");
    let rows = cast_rows(&r);
    let unassigned: Vec<&CastRow> = rows.iter().filter(|x| x.unassigned()).collect();
    assert!(
        unassigned.len() > 1,
        "the fixture must have several unassigned speakers"
    );
    assert!(unassigned.iter().all(|x| x.shared_with.is_empty()));
}

#[test]
fn cast_rows_filter_by_speaker_voice_or_style_ignoring_diacritics() {
    let rows = cast_rows(&roster_fixture());
    assert_eq!(
        filtered_cast_rows(&rows, "duc tri").len(),
        1,
        "matches the voice"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "adam").len(),
        2,
        "both speakers on Adam"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "kien").len(),
        1,
        "matches the speaker"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "tin tuc").len(),
        4,
        "the style is searchable too, and without diacritics"
    );
    assert_eq!(
        filtered_cast_rows(&rows, "   ").len(),
        rows.len(),
        "a blank filter keeps all"
    );
    assert!(filtered_cast_rows(&rows, "nobody").is_empty());
}

#[test]
fn cast_filter_preserves_row_order_and_never_invents_rows() {
    let rows = cast_rows(&roster_fixture());
    let filtered = filtered_cast_rows(&rows, "adam");
    let order: Vec<&String> = filtered.iter().map(|r| &r.character).collect();
    assert_eq!(order, vec!["Kiên", "Vũ"]);
}

#[test]
fn cast_rows_carry_the_voice_metadata_through() {
    let rows = cast_rows(&roster_fixture());
    let narrator = rows.iter().find(|r| r.character == "Narrator").unwrap();
    assert_eq!(narrator.gender, "male");
    assert_eq!(narrator.accent, "South");
    assert!(narrator.in_roster);
    assert!(!narrator.enrolled);
    // A voice the roster does not list carries no metadata at all, so
    let ha = rows.iter().find(|r| r.character == "Hà").unwrap();
    assert!(!ha.in_roster);
    assert!(ha.accent.is_empty() && ha.gender.is_empty());
}
