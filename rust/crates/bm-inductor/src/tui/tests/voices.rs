use super::*;

// --- the picker's voice list -------------------------------------------

#[test]
fn the_voice_list_groups_pool_samples_by_tag_least_used_first_and_the_rest_by_name() {
    let app = voice_app(pooled_roster());
    let order: Vec<String> = listed(&app)
        .into_iter()
        .map(|(_, _, name)| name.unwrap_or_else(|| "—".into()))
        .collect();
    assert_eq!(
        order,
        vec![
            // Group 1, `female + old` before `male + young` — the groups are
            "—",
            "old-female-2",
            "—",
            // …and inside a group the emptiest voice leads, so the pool is spent
            "young-male-3",
            "young-male-10",
            // Group 2: unique voices, by name, diacritics folded.
            "—",
            "Bắc Kỳ",
            "Võ Tắc Thiên",
        ],
        "tag groups alphabetical, least used first, then unique by name"
    );
    let kinds: Vec<VoiceKind> = filtered_voices(&app, "")
        .iter()
        .filter_map(|r| match r {
            VoiceRow::Group { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            VoiceKind::AutoAssign,
            VoiceKind::AutoAssign,
            VoiceKind::Unique
        ],
        "two pooled groups, then the unique ones — in that order"
    );
}

#[test]
fn a_voice_the_filter_drops_takes_its_whole_group_with_it() {
    let app = voice_app(pooled_roster());
    let text = listed(&app).len();
    let filtered = filtered_voices(&app, "young-male");
    assert_eq!(filtered.len(), 3, "one heading and the two voices under it");
    assert!(text > 2, "the unfiltered list is longer");
    assert!(matches!(filtered[0], VoiceRow::Group { .. }));
    // A filter that matches nothing leaves no heading behind: a heading for a
    assert!(filtered_voices(&app, "zzz").is_empty());
}

#[test]
fn the_used_by_cell_names_one_character_and_counts_the_rest() {
    let users = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    assert_eq!(used_by(&[], "Narrator"), "", "a free voice says nothing");
    assert_eq!(
        used_by(&users(&["Lâm"]), "Narrator"),
        "Lâm",
        "one user is named, not counted"
    );
    assert_eq!(
        used_by(&users(&["Kiên", "Vũ", "Hà"]), "Narrator"),
        "Kiên, +2",
        "the first user, and how many others are on it"
    );
    assert_eq!(
        used_by(&users(&["Adam", "Kiên", "Vũ"]), "Vũ"),
        "Vũ, +2",
        "the character being swapped leads: that is the incumbent"
    );
}

#[test]
fn a_pool_samples_gender_comes_from_its_tag_when_the_roster_does_not_know() {
    let v = |gender: &str, tags: &[&str]| VoiceInfo {
        key: String::new(),
        name: "x".into(),
        gender: gender.into(),
        accent: String::new(),
        language: "vi-VN".into(),
        style: String::new(),
        pool_tags: tags.iter().map(|t| t.to_string()).collect(),
        enrolled: true,
    };
    assert_eq!(
        gender_of(&v("female", &["young"])),
        "female",
        "the roster knows"
    );
    assert_eq!(
        gender_of(&v("unknown", &["young", "female"])),
        "female",
        "the tag says it, so the row does not print a dash"
    );
    assert_eq!(gender_of(&v("unknown", &["male", "young"])), "male");
    assert_eq!(
        gender_of(&v("unknown", &["strong"])),
        "—",
        "no tag claims it"
    );
}

#[test]
fn the_picker_step_two_keeps_its_columns_aligned_however_long_a_voice_is() {
    // The row used to be `format!`-padded to guessed widths with no truncation,
    let mut roster = pooled_roster();
    roster.voices.push(VoiceInfo {
        key: String::new(),
        name: "a-considerably-longer-pooled-sample-name".into(),
        gender: "female".into(),
        accent: "Northern".into(),
        language: "vi-VN".into(),
        style: "pool: female, young".into(),
        pool_tags: vec!["female".into(), "young".into()],
        enrolled: true,
    });
    for c in ["Một", "Hai", "Ba"] {
        roster.cast.insert(c.into(), "Võ Tắc Thiên".into());
    }
    let mut app = voice_app(roster);
    let text = render_text(&mut app, 140, 44);
    // The gender column starts at the same offset on every voice row, which is
    let mut offsets = Vec::new();
    for line in text.lines() {
        for name in ["Adam", "Bắc Kỳ", "Võ Tắc Thiên", "young-male-10"] {
            if let Some(i) = line.find(name) {
                offsets.push(i + 2);
            }
        }
    }
    assert!(
        offsets.windows(2).all(|w| w[0] == w[1]),
        "every name column starts at the same offset: {offsets:?}\n{text}"
    );
    assert!(
        text.contains("a-considerably-long…"),
        "a name too long for its cell is cut and says so:\n{text}"
    );
    let busy = text
        .lines()
        .find(|l| l.contains("Võ Tắc Thiên"))
        .expect("the row is on screen");
    assert!(
        busy.contains("Ba, +2"),
        "four characters on one voice read as one name — the first, in cast order — \
         and a count of the rest:\n{busy}"
    );
    // The columns that said nothing are gone, and the clone word with them.
    for gone in ["vi-VN", "Northern", "clone", "accent policy concern"] {
        assert!(
            !text.contains(gone),
            "the old column is gone: {gone}\n{text}"
        );
    }
}

#[test]
fn step_one_shows_the_incumbent_voice_and_its_gender_and_no_column_that_repeats_itself() {
    // The character list answers "who do I swap?", so the only thing worth
    let mut roster = roster_fixture();
    roster.voices.push(VoiceInfo {
        key: String::new(),
        name: "young-female-9".into(),
        gender: "unknown".into(),
        accent: "unknown".into(),
        language: "vi-VN".into(),
        style: "pool: young, female".into(),
        pool_tags: vec!["young".into(), "female".into()],
        enrolled: true,
    });
    roster.cast.insert("Bé Mắt".into(), "young-female-9".into());
    // Step 1 lists the *speakers*, so the name has to be one of those.
    roster.characters.push("Bé Mắt".into());
    let mut app = voice_app(roster);
    if let Screen::Pick(p) = &mut app.screen {
        p.stage = PickStage::Character;
        p.filter.clear();
    }
    let text = render_text(&mut app, 140, 44);
    let row = |voice: &str| {
        text.lines()
            .find(|l| l.contains(voice))
            .unwrap_or_else(|| panic!("{voice} must be on screen:\n{text}"))
            // `render_text` returns the whole row: the overlay's own border
            .trim_end_matches([' ', '│'])
            .to_string()
    };
    assert!(
        row("Đức Trí").ends_with("male"),
        "the row ends at the incumbent's gender:\n{}",
        row("Đức Trí")
    );
    // A pooled sample carries no roster gender — its tag is what it plainly
    assert!(
        row("young-female-9").ends_with("female"),
        "a pooled sample's gender comes from its tag:\n{}",
        row("young-female-9")
    );
    for gone in ["unknown", "vi-VN"] {
        assert!(!text.contains(gone), "no row says {gone} any more:\n{text}");
    }
}

#[tokio::test]
async fn a_group_heading_is_a_label_and_never_the_thing_a_key_acts_on() {
    let http = reqwest::Client::new();
    let (job_tx, mut job_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut app = voice_app(pooled_roster());
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.cursor, 0, "opens on row 0, the first heading"),
        other => panic!("{other:?}"),
    }
    // `T` auditions the voice under the cursor, never a heading.
    handle_key(&mut app, key(KeyCode::Char('T')), &http, &job_tx).await;
    let req = last_op(&mut job_rx).expect("a pointed voice must still audition");
    assert_eq!(
        req.voice.as_deref(),
        Some("old-female-2"),
        "the first voice of the first group, not the heading above it"
    );
    match &app.screen {
        Screen::Pick(p) => assert_eq!(p.cursor, 1, "the cursor settles onto that voice"),
        other => panic!("{other:?}"),
    }
    // And Enter picks it, rather than reporting that nothing is selected.
    handle_key(&mut app, key(KeyCode::Enter), &http, &job_tx).await;
    match &app.screen {
        Screen::Confirm(c) => {
            assert!(
                c.body.iter().any(|l| l.contains("old-female-2")),
                "the confirmation names the voice: {:?}",
                c.body
            )
        }
        other => panic!("{other:?}"),
    }
    // Down steps *over* a heading, and Up steps *back* over one. Up is the
    let mut app = voice_app(pooled_roster());
    if let Screen::Pick(p) = &mut app.screen {
        p.cursor = 3; // the first voice of the second group
    }
    handle_key(&mut app, key(KeyCode::Up), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => assert_eq!(
            filtered_voices(&app, &p.filter)
                .get(p.cursor)
                .and_then(|r| r.voice())
                .map(|v| v.name.as_str()),
            Some("old-female-2"),
            "Up leaves the group behind, landing on the previous group's last voice"
        ),
        other => panic!("{other:?}"),
    }
    // At the very top there is nothing above: Up settles onto the first voice
    handle_key(&mut app, key(KeyCode::Up), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => assert_eq!(
            filtered_voices(&app, &p.filter)
                .get(p.cursor)
                .and_then(|r| r.voice())
                .map(|v| v.name.as_str()),
            Some("old-female-2"),
            "the top of the list is the first voice, not the heading above it"
        ),
        other => panic!("{other:?}"),
    }
    // Down from that same voice crosses its heading into the next group.
    handle_key(&mut app, key(KeyCode::Down), &http, &job_tx).await;
    match &app.screen {
        Screen::Pick(p) => assert_eq!(
            filtered_voices(&app, &p.filter)
                .get(p.cursor)
                .and_then(|r| r.voice())
                .map(|v| v.name.as_str()),
            Some("young-male-3"),
            "Down steps over a heading instead of stopping on one"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_cast_table_names_only_what_it_shows() {
    // `gender` and `accent` read `unknown` on most rows and the language is
    let mut app = App::new("http://127.0.0.1:8901");
    app.roster = Some(roster_fixture());
    app.screen = Screen::Cast(CastView::new());
    let text = render_text(&mut app, 140, 44);
    for gone in ["gender", "accent", "status"] {
        assert!(!text.contains(gone), "the header is gone: {gone}\n{text}");
    }
    for kept in ["speaker", "voice", "shared"] {
        assert!(text.contains(kept), "the header is here: {kept}\n{text}");
    }
    assert!(
        text.contains("1 to fix"),
        "…and the problems are still counted:\n{text}"
    );
}
