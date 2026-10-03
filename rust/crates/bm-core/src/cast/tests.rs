use super::*;
use crate::voices::vieneu_policy;
use serde_json::json;

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bm-cast-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn assigns_a_voice_for_every_speaker() {
    let d = tmpdir("assign");
    let script = d.join("script-01.json");
    std::fs::write(
        &script,
        serde_json::to_string(&json!({
            "roster": ["Narrator", "Dịch Phong", "New Guy"],
            "segments": [{"speaker": "Narrator", "text": "x"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let cast_path = d.join("cast-vieneu.json");
    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        true,
    )
    .unwrap();
    assert!(cast.contains_key("Narrator"));
    assert!(cast.contains_key("New Guy"));
    assert!(cast_path.exists(), "save=true must persist");
}

#[test]
fn anonymous_speakers_borrow_the_narrator_voice_without_bible_entries() {
    let d = tmpdir("anonymous-slot");
    let bible = d.join("bible.json");
    std::fs::write(&bible, r#"{"characters":[]}"#).unwrap();
    let cast_path = d.join("cast-vieneu.json");
    let first = d.join("script-01.json");
    std::fs::write(
        &first,
        r#"{"roster":["Narrator","Anonymous"],
            "segments":[{"speaker":"Anonymous","text":"Mở cửa!"}]}"#,
    )
    .unwrap();

    let cast = load_cast(&first, &cast_path, &bible, &vieneu_policy(), None, true).unwrap();
    assert_eq!(
        cast.get("Anonymous"),
        cast.get("Narrator"),
        "the crowd speaks in the Narrator's voice"
    );
    let stored: Value = crate::read_json(&bible).unwrap();
    assert_eq!(stored, json!({"characters": []}), "not a Bible character");

    // A legacy script's numbered slot resolves to the same voice, and a cast
    // file that already held a clone of its own is corrected on load.
    let second = d.join("script-02.json");
    std::fs::write(&second, r#"{"roster":["anonymous:anon-1"],"segments":[]}"#).unwrap();
    let created = Cast::from_iter([("anonymous:anon-1".to_string(), "Bảo An".to_string())]);
    write_cast("vieneu", &cast_path, &created).unwrap();
    let again = load_cast(&second, &cast_path, &bible, &vieneu_policy(), None, true).unwrap();
    assert_eq!(again.get("anonymous:anon-1"), again.get("Narrator"));
    assert_ne!(again.get("anonymous:anon-1"), Some(&"Bảo An".to_string()));
}

#[test]
fn never_overwrites_an_existing_assignment() {
    let d = tmpdir("stable");
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Narrator"],"segments":[]}"#).unwrap();
    let cast_path = d.join("cast-vieneu.json");
    std::fs::write(&cast_path, r#"{"Narrator":"Adam"}"#).unwrap();
    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(cast.get("Narrator").unwrap(), "Adam");
}

#[test]
fn save_false_leaves_the_file_untouched() {
    let d = tmpdir("readonly");
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Brand New"],"segments":[]}"#).unwrap();
    let cast_path = d.join("cast-vieneu.json");
    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    assert!(cast.contains_key("Brand New"));
    assert!(
        !cast_path.exists(),
        "read-only mode must not create the file"
    );
}

#[test]
fn a_broad_character_tag_does_not_force_a_default_preset() {
    let d = tmpdir("clone-before-preset");
    std::fs::write(
        d.join("voice-pool.json"),
        r#"{"young-female-1":{"file":"refs/young-female-1.mp3","tags":["young","female"]},
            "young-male-1":{"file":"refs/young-male-1.mp3","tags":["young","male"]}}"#,
    )
    .unwrap();
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Hệ thống"],"segments":[]}"#).unwrap();
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Hệ thống","voice_hint":"adult male",
            "tags":["system"],"proper_aliases":[]}]}"#,
    )
    .unwrap();

    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(cast.get("Hệ thống").unwrap(), "young-male-1");
}

#[test]
fn bible_hints_drive_gender_pool() {
    let d = tmpdir("bible");
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Cô Bé"],"segments":[]}"#).unwrap();
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Cô Bé","voice_hint":"girl","proper_aliases":[]}]}"#,
    )
    .unwrap();
    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    let p = vieneu_policy();
    assert!(
        p.female.contains(cast.get("Cô Bé").unwrap()),
        "expected a female preset, got {:?}",
        cast.get("Cô Bé")
    );
}

// --- stage 2: the cast file stores keys, the reader accepts both --------

fn on_disk(path: &Path) -> BTreeMap<String, String> {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_name_based_cast_reads_and_is_rewritten_as_keys() {
    // The un-migrated form. Reading must resolve names, and the next write
    // must key the file — otherwise the rename-fragility never goes away and
    // the migration is something an operator has to remember forever.
    let d = tmpdir("migrate-on-write");
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Narrator"],"segments":[]}"#).unwrap();
    let cast_path = d.join("cast-vieneu.json");
    std::fs::write(&cast_path, r#"{"Narrator":"Đức Trí"}"#).unwrap();

    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(
        cast.get("Narrator").unwrap(),
        "Đức Trí",
        "in memory the pipeline still speaks names"
    );
    assert_eq!(
        on_disk(&cast_path).get("Narrator").unwrap(),
        "duc-tri",
        "on disk it is a key, so a rename cannot orphan the assignment"
    );
}

#[test]
fn a_key_based_cast_reads_back_as_names_and_does_not_churn() {
    let d = tmpdir("from-keys");
    let script = d.join("script-01.json");
    std::fs::write(
        &script,
        r#"{"roster":["Narrator","Dịch Phong"],"segments":[]}"#,
    )
    .unwrap();
    let cast_path = d.join("cast-vieneu.json");
    std::fs::write(
        &cast_path,
        r#"{"Narrator":"duc-tri","Dịch Phong":"thai-son"}"#,
    )
    .unwrap();

    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(cast.get("Narrator").unwrap(), "Đức Trí");
    assert_eq!(cast.get("Dịch Phong").unwrap(), "Thái Sơn");
    // Keys in, keys out: re-writing a migrated file is a no-op in shape.
    let disk = on_disk(&cast_path);
    assert_eq!(disk.get("Narrator").unwrap(), "duc-tri");
    assert_eq!(disk.get("Dịch Phong").unwrap(), "thai-son");
}

#[test]
fn a_clone_without_a_key_keeps_its_name_on_disk() {
    // Clones have no catalogue key until stage 3, so the assignment is
    // written as a name — and must still resolve on the way back in.
    let d = tmpdir("clone-name");
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Suneo"],"segments":[]}"#).unwrap();
    let cast_path = d.join("cast-vieneu.json");
    std::fs::write(&cast_path, r#"{"Suneo":"Suneo"}"#).unwrap();

    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(cast.get("Suneo").unwrap(), "Suneo");
    assert_eq!(on_disk(&cast_path).get("Suneo").unwrap(), "Suneo");
}

#[test]
fn a_half_migrated_cast_works() {
    // The property the whole design rests on: a file with one keyed entry and
    // one named entry renders, so the migration can be interrupted.
    let d = tmpdir("half-migrated");
    let script = d.join("script-01.json");
    std::fs::write(
        &script,
        r#"{"roster":["Narrator","Dịch Phong"],"segments":[]}"#,
    )
    .unwrap();
    let cast_path = d.join("cast-vieneu.json");
    std::fs::write(
        &cast_path,
        r#"{"Narrator":"duc-tri","Dịch Phong":"Thái Sơn"}"#,
    )
    .unwrap();

    let cast = load_cast(
        &script,
        &cast_path,
        &d.join("bible.json"),
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(cast.get("Narrator").unwrap(), "Đức Trí");
    assert_eq!(cast.get("Dịch Phong").unwrap(), "Thái Sơn");
}

#[test]
fn without_an_overlay_there_is_nothing_to_exclude() {
    // No machine-local roster: the catalogue is unrestricted, so an old
    // man with no pool rolls the first male preset.
    let d = tmpdir("policy-assign");
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Ông Già"],"segments":[]}"#).unwrap();
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Ông Già","voice_hint":"elderly male, stern","tags":["old","male"],"proper_aliases":[]}]}"#,
    )
    .unwrap();

    let policy = policy_for_bible("vieneu", &crate::Layout::new(&d));
    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &policy,
        None,
        false,
    )
    .unwrap();
    let got = cast.get("Ông Già").unwrap();
    assert!(
        policy.male.contains(got),
        "an old man rolls a male preset: {got:?}"
    );
}

#[test]
fn the_policy_is_the_catalogue_with_no_overlay() {
    // No machine-local roster exists any more: even a stray .bm/voices.json
    // is ignored, and the policy is the shipped catalogue.
    let d = tmpdir("policy-catalogue");
    std::fs::create_dir_all(d.join(".bm")).unwrap();
    std::fs::write(d.join(".bm/voices.json"), "{ nope").unwrap();
    let policy = policy_for_bible("vieneu", &crate::Layout::new(&d));
    assert_eq!(policy.engine, "vieneu");
    assert_eq!(policy.male, vieneu_policy().male);
    assert_eq!(policy.female, vieneu_policy().female);
}

/// A cast may only name voices the engine's own store holds.
///
/// The bug this is for, as it actually happened: `voices.default.json`
/// declares twenty-five pocket presets, the installed tree ships nine, and
/// the roll drew `anna` and `bill-boerst` from the catalogue. Both renders
/// came back `unknown voice "…" on this box`, failed three times and
/// shelved. Nothing was wrong with the engine, the book or the voice files
/// — only with trusting the catalogue over the store.
#[test]
fn a_cast_only_ever_names_voices_the_engine_store_holds() {
    let d = tmpdir("installed-only");
    let layout = crate::Layout::new(&d);
    std::fs::create_dir_all(d.join("data")).unwrap();
    let cast_path = layout.cast("vieneu");
    // An engine tree holding three of the female presets the catalogue
    // declares five of.
    let models = d.join("engines/vieneu/models");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::write(
        models.join("voices.json"),
        r#"{"presets":{"alba":{},"cosette":{},"eponine":{}}}"#,
    )
    .unwrap();

    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[
            {"name":"A","voice_hint":"adult female","tags":["female"]},
            {"name":"B","voice_hint":"adult female","tags":["female"]},
            {"name":"C","voice_hint":"adult female","tags":["female"]}
        ]}"#,
    )
    .unwrap();
    let script = d.join("script-01.json");
    std::fs::write(
        &script,
        r#"{"roster":["Narrator","A","B","C"],"segments":[{"speaker":"A","text":"x"}]}"#,
    )
    .unwrap();

    let policy = policy_for_bible("vieneu", &layout);
    let installed = crate::pool::installed_voices(&layout).expect("the store is readable");
    assert_eq!(
        installed.len(),
        3,
        "the store is the authority, not the catalogue"
    );
    for pool in [&policy.female, &policy.male, &policy.neutral] {
        for voice in pool {
            assert!(
                installed.contains(voice),
                "{voice} is in the catalogue but not in the store it would be spoken by"
            );
        }
    }

    let cast = load_cast(&script, &cast_path, &bible, &policy, Some(&installed), true).unwrap();
    for (who, voice) in &cast {
        assert!(
            installed.contains(voice),
            "{who} was given {voice}, which this engine cannot speak"
        );
    }

    // And a cast already on disk naming a voice the store lacks is re-rolled
    // rather than kept: it is a promise the sidecar will refuse.
    std::fs::write(&cast_path, r#"{"A":"marius","B":"alba"}"#).unwrap();
    let healed = load_cast(&script, &cast_path, &bible, &policy, Some(&installed), true).unwrap();
    assert!(
        !healed.values().any(|v| v == "marius"),
        "a voice the store cannot speak must not survive into the cast"
    );
    assert_eq!(
        healed.get("B"),
        Some(&"alba".to_string()),
        "an installed voice that is already assigned is left alone"
    );
}

#[test]
fn an_unknown_voice_is_preserved_rather_than_silently_reassigned() {
    // The cast overview has to be able to flag this; substituting a valid
    // voice would hide a real problem behind a plausible render.
    let d = tmpdir("unknown");
    let cast_path = d.join("cast-vieneu.json");
    std::fs::write(&cast_path, r#"{"Narrator":"Đã Biến Mất"}"#).unwrap();
    let cast = read_cast("vieneu", &cast_path);
    assert_eq!(cast.get("Narrator").unwrap(), "Đã Biến Mất");
}

// --- the sample pool rolls first -----------------------------------------

#[test]
fn a_variant_speaker_name_resolves_to_the_assigned_voice() {
    // The map is keyed canonically — `load_cast` folds before assigning —
    // but the planner is handed the raw script string. A speaker written
    // as an alias, a case variant or a title-suffixed form must therefore
    // still find its voice, or the chapter is assigned a voice under one
    // name and looked up under another.
    let d = tmpdir("variant-lookup");
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Quản Vân Bằng","voice_hint":"old male",
            "proper_aliases":["Quản Vân Bằng","nam tử bị thương"]}]}"#,
    )
    .unwrap();
    let script = d.join("script-01.json");
    std::fs::write(
        &script,
        r#"{"roster":["Narrator","Nam tử bị thương"],
            "segments":[{"speaker":"Nam tử bị thương","text":"Cứu ta."}]}"#,
    )
    .unwrap();

    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    let voice = cast.get("Quản Vân Bằng").expect("assigned under the name");
    assert_eq!(
        cast.get("Nam tử bị thương"),
        Some(voice),
        "the script's own spelling must resolve to the same voice"
    );
    // And the planner — which only ever sees that spelling — plans.
    let segs = vec![json!({"speaker": "Nam tử bị thương", "text": "Cứu ta."})];
    let units = crate::assemble::plan_render(
        &crate::assemble::Planned::plan(&segs),
        &cast,
        Path::new("segs"),
        true,
        None,
    )
    .expect("a variant speaker must plan");
    assert_eq!(units.len(), 1);
    assert_eq!(&units[0].voice, voice);
}

#[test]
fn a_cast_read_off_disk_looks_up_exactly() {
    // The picker and the migration read the file directly and expect a
    // plain map: no bible, no folding, no surprise substitution.
    let d = tmpdir("plain-map");
    let path = d.join("cast-vieneu.json");
    std::fs::write(&path, r#"{"A":"Đức Trí"}"#).unwrap();
    let cast = read_cast("vieneu", &path);
    assert_eq!(cast.get("A").unwrap(), "Đức Trí");
    assert!(cast.get("a").is_none(), "no folding without a bible");
    assert_eq!(cast.into_map().len(), 1);
}

fn pool_fixture(d: &Path) {
    std::fs::write(
        d.join("voice-pool.json"),
        r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]},
            "old-male-1": {"file": "refs/old-male-1.mp3", "tags": ["old", "male"]}}"#,
    )
    .unwrap();
}

#[test]
fn a_tagged_newcomer_rolls_from_the_pool() {
    let d = tmpdir("pool-roll");
    pool_fixture(&d);
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Cô Bé"],"segments":[]}"#).unwrap();
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Cô Bé","voice_hint":"girl, bright","tags":["young","female"],"proper_aliases":[]}]}"#,
    )
    .unwrap();
    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(cast.get("Cô Bé").unwrap(), "young-female-1");
}

#[test]
fn a_clashing_character_falls_back_to_presets() {
    // young+male shares `young` with the pool's only sample, but `male`
    // clashes with its `female` — so no pool voice may speak him.
    let d = tmpdir("pool-clash");
    std::fs::write(
        d.join("voice-pool.json"),
        r#"{"young-female-1": {"file": "refs/young-female-1.mp3", "tags": ["young", "female"]}}"#,
    )
    .unwrap();
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Cậu Bé"],"segments":[]}"#).unwrap();
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Cậu Bé","voice_hint":"boy, polite","tags":["young","male"],"proper_aliases":[]}]}"#,
    )
    .unwrap();
    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    let got = cast.get("Cậu Bé").unwrap();
    assert_ne!(
        got, "young-female-1",
        "a clashing sample must never voice him"
    );
    assert!(
        vieneu_policy().male.contains(got),
        "falls back to the male presets: {got:?}"
    );
}

#[test]
fn the_pool_is_the_workspaces_own_and_not_the_checkouts() {
    // Real layout: the pool in the workspace, the bible under
    // `<workspace>/data/`. The lookup climbs from `data/` to the
    // workspace — and **stops there**. The checkout's pool belongs to
    // whatever book owns the checkout; a second book reading it is exactly
    // how the wrong roster got cast.
    let d = tmpdir("pool-walkup");
    let book = d.join("workspaces").join("book");
    let data = book.join("data");
    std::fs::create_dir_all(&data).unwrap();
    // Both a checkout pool and the workspace's own: the workspace's wins.
    pool_fixture(&d);
    pool_fixture(&book);

    let script = data.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Cô Bé"],"segments":[]}"#).unwrap();
    let bible = data.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Cô Bé","voice_hint":"girl, bright","tags":["young","female"],"proper_aliases":[]}]}"#,
    )
    .unwrap();
    let cast = load_cast(
        &script,
        &data.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(cast.get("Cô Bé").unwrap(), "young-female-1");

    // With no pool of its own, the checkout's does not answer for it: the
    // walk stops at the workspace.
    std::fs::remove_file(book.join("voice-pool.json")).unwrap();
    assert!(
        pool_for_bible(&bible).is_empty(),
        "a book with no pool of its own must not cast from the checkout's"
    );
}

#[test]
fn hint_tags_backfill_a_bible_that_predates_tags() {
    // No `tags` key at all: the voice_hint still routes to the pool.
    let d = tmpdir("pool-hint");
    pool_fixture(&d);
    let script = d.join("script-01.json");
    std::fs::write(&script, r#"{"roster":["Lão Ông"],"segments":[]}"#).unwrap();
    let bible = d.join("bible.json");
    std::fs::write(
        &bible,
        r#"{"characters":[{"name":"Lão Ông","voice_hint":"elderly male, stern","proper_aliases":[]}]}"#,
    )
    .unwrap();
    let cast = load_cast(
        &script,
        &d.join("cast-vieneu.json"),
        &bible,
        &vieneu_policy(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(cast.get("Lão Ông").unwrap(), "old-male-1");
}
