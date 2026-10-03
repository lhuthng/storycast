use super::*;
use crate::assemble::wav::silent_wav;
use serde_json::json;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bm-assemble-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A plan with no sound items — what most of these tests want.
fn planned(segs: &[Value]) -> Planned {
    Planned::plan(segs)
}

#[test]
fn audition_finds_takes_whatever_tier_stored_them() {
    // The mp3 tier changed the default take extension; the audition pool
    let d = tmpdir("audition-tiers");
    let l = crate::Layout::new(&d);
    std::fs::create_dir_all(l.script_dir()).unwrap();
    std::fs::create_dir_all(l.render_dir()).unwrap();
    std::fs::create_dir_all(l.seg_dir("vieneu", 7)).unwrap();
    std::fs::write(
        l.script(7),
        json!({"segments": [{"speaker": "A", "text": "hello"}]}).to_string(),
    )
    .unwrap();
    std::fs::write(
        l.plan(7),
        json!({"takes": [
            {"take_key": "aa", "speaker": "A", "voice": "V", "voice_key": "V", "text": "hello"},
            {"take_key": "bb", "speaker": "A", "voice": "V", "voice_key": "V", "text": "hello"},
        ]})
        .to_string(),
    )
    .unwrap();
    std::fs::write(l.seg_dir("vieneu", 7).join("t-aa.mp3"), b"fake-mp3").unwrap();
    std::fs::write(l.seg_dir("vieneu", 7).join("t-bb.wav"), b"fake-wav").unwrap();
    std::fs::write(l.seg_dir("vieneu", 7).join("note.txt"), b"not a take").unwrap();
    let found = rendered_segments(&l, "vieneu", "V");
    assert_eq!(found.len(), 2, "one take per tier, no strays");
}

#[test]
fn runs_group_consecutive_speakers() {
    let segs = vec![
        json!({"speaker": "A", "text": "1"}),
        json!({"speaker": "A", "text": "2"}),
        json!({"speaker": "B", "text": "3"}),
        json!({"speaker": "A", "text": "4"}),
    ];
    let r = planned(&segs).runs();
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].idx, vec![0, 1]);
    assert_eq!(r[1].idx, vec![2]);
    assert_eq!(r[2].idx, vec![3]);
}

#[test]
fn punctuation_only_segments_are_folded_into_the_previous_line() {
    let segs = vec![
        json!({"speaker": "Anonymous", "text": "First line"}),
        json!({"speaker": "Narrator", "text": ","}),
        json!({"speaker": "Anonymous", "text": "Second line."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 2, "no punctuation-only TTS take");
    assert_eq!(p.speech[0]["text"], "First line,");
    assert_eq!(p.speech[1]["text"], "Second line.");
    assert_eq!(p.origin, vec![0, 2], "origins still point into the script");
    assert!(p
        .speech
        .iter()
        .all(|segment| crate::util::has_speakable_content(seg_text(segment))));

    let cast = crate::cast::Cast::from_iter([("Anonymous".into(), "Adam".into())]);
    let local = plan_render(&p, &cast, Path::new("s"), true, None).unwrap();
    assert_eq!(local.len(), 1, "same-speaker lines remain one render run");
    assert_eq!(local[0].text, "First line, Second line.");

    let cloud = plan_render(&p, &cast, Path::new("s"), false, None).unwrap();
    assert_eq!(cloud.len(), 2);
    assert_eq!(cloud[0].text, "First line,");
    assert_eq!(cloud[1].text, "Second line.");

    let leading = planned(&[
        json!({"speaker": "Narrator", "text": ","}),
        json!({"speaker": "Anonymous", "text": "First line."}),
    ]);
    assert_eq!(leading.speech.len(), 1, "a leading comma is not a TTS take");
    assert_eq!(leading.speech[0]["text"], "First line.");

    let already_terminated = planned(&[
        json!({"speaker": "Anonymous", "text": "First line."}),
        json!({"speaker": "Narrator", "text": ","}),
    ]);
    assert_eq!(already_terminated.speech[0]["text"], "First line.");
}

#[test]
fn a_sound_between_two_halves_of_a_sentence_is_not_a_line() {
    // The injection IS the split. `say sưa lật xem` sits three words into
    let segs = vec![
        json!({"speaker": "Narrator", "text": "Doãn Lạc Ly cực kỳ vui vẻ, say sưa lật xem."}),
        json!({"sound": "page-turn", "mode": "overlap"}),
        json!({"speaker": "Narrator", "text": "Rồi nàng cất sách đi."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 2, "{:?}", p.speech);
    assert_eq!(
        seg_text(&p.speech[0]),
        "Doãn Lạc Ly cực kỳ vui vẻ, say sưa lật xem."
    );
    assert_eq!(seg_text(&p.speech[1]), "Rồi nàng cất sách đi.");
    // The sound is nowhere in the speech: the renderer is handed two lines
    assert!(!p.speech.iter().any(crate::util::is_sound_item));
    assert!(p.fires_at(0), "the effect fires at the seam");
    assert!(!p.fires_at(1));
    assert_eq!(p.fires[0].len(), 1);
    assert_eq!(p.fires[0][0]["sound"], "page-turn");
    // The two halves are one place and one mood: the split is a seam, not
    assert_eq!(p.speech[1]["speaker"], p.speech[0]["speaker"]);
    // And it is two TTS calls, so the run breaks there.
    let r = p.runs();
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0].idx, vec![0]);
    assert_eq!(r[1].idx, vec![1]);
}

#[test]
fn a_sound_at_the_end_of_a_line_cuts_nothing_and_fires_at_the_seam() {
    // "phun ra một ngụm máu tươi." already ends the line, so there is no
    let segs = vec![
        json!({"speaker": "Narrator", "text": "Hắn gầm lên. Rồi phun ra một ngụm máu tươi."}),
        json!({"sound": "blood-spatter"}),
        json!({"speaker": "Narrator", "text": "Thân thể hắn đổ xuống."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 2, "no split: the sound closes the line");
    assert!(p.fires_at(0));
    assert!(!p.fires_at(1));
    // The run breaks anyway: the sound fires at that line's end, and
    assert_eq!(p.runs().len(), 2);
}

#[test]
fn two_sounds_in_a_row_both_fire_at_the_same_seam_in_order() {
    let segs = vec![
        json!({"speaker": "A", "text": "Một."}),
        json!({"sound": "sword-slash"}),
        json!({"sound": "metal-hit"}),
        json!({"speaker": "A", "text": "Hai."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 2);
    assert_eq!(p.fires[0].len(), 2);
    assert_eq!(p.fires[0][0]["sound"], "sword-slash");
    assert_eq!(p.fires[0][1]["sound"], "metal-hit");
    assert!(p.fires[1].is_empty());
}

#[test]
fn a_stop_item_is_a_sound_and_carries_no_speech() {
    let segs = vec![
        json!({"speaker": "A", "text": "Nước sôi."}),
        json!({"sound": "boiling-water", "mode": "overlap"}),
        json!({"speaker": "A", "text": "Rồi nàng bước ra ngoài."}),
        json!({"stop": "boiling-water"}),
        json!({"speaker": "A", "text": "Cửa đóng lại."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 3);
    assert_eq!(p.fires[0][0]["sound"], "boiling-water");
    assert_eq!(p.fires[1][0]["stop"], "boiling-water");
    assert!(p.fires[2].is_empty());
}

#[test]
fn a_sound_leading_the_chapter_is_dropped_not_guessed() {
    let segs = vec![
        json!({"sound": "sword-slash"}),
        json!({"speaker": "A", "text": "Một câu."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 1);
    assert!(
        !p.fires_at(0),
        "no seam before the first line, nothing invented"
    );
}

#[test]
fn an_item_with_text_and_a_sound_speaks_the_text_and_drops_the_sound() {
    // Malformed, and the validator refuses it — but the planner must not
    let segs = vec![json!({"speaker": "A", "text": "Một câu.", "sound": "coin"})];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 1);
    assert_eq!(seg_text(&p.speech[0]), "Một câu.");
    assert!(!p.fires_at(0));
}

#[test]
fn the_headline_is_dropped_before_any_sound_is_placed() {
    // `origin` is the post-headline item index — the space `segments` is in
    let segs = vec![
        json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"}),
        json!({"speaker": "A", "text": "Một."}),
        json!({"sound": "coin"}),
        json!({"speaker": "A", "text": "Hai."}),
    ];
    let p = planned(&segs);
    assert_eq!(p.speech.len(), 2, "{:?}", p.speech);
    assert_eq!(seg_text(&p.speech[0]), "Một.");
    assert_eq!(seg_text(&p.speech[1]), "Hai.");
    assert_eq!(p.origin, vec![0, 2], "origins count the sound item");
    assert!(p.fires_at(0));
}

#[test]
fn expected_wavs_names_match_the_run_shape() {
    let segs = vec![
        json!({"speaker": "A", "text": "1"}),
        json!({"speaker": "A", "text": "2"}),
        json!({"speaker": "B", "text": "3"}),
    ];
    let mut cast = Cast::new();
    cast.insert("A".into(), "Đức Trí".into());
    cast.insert("B".into(), "Adam".into());
    let local = expected_wavs(&planned(&segs), &cast, Path::new("segs"), true, None).unwrap();
    assert!(
        local[0].ends_with("0000-0001_Đức Trí.wav"),
        "{:?}",
        local[0]
    );
    assert!(local[1].ends_with("0002_Adam.wav"), "{:?}", local[1]);
    let cloud = expected_wavs(&planned(&segs), &cast, Path::new("segs"), false, None).unwrap();
    assert!(cloud[0].ends_with("0000_Đức Trí.wav"));
    assert_eq!(cloud.len(), 3);
}

#[test]
fn expected_wavs_errors_on_an_uncast_speaker() {
    let segs = vec![json!({"speaker": "Nobody", "text": "1"})];
    let err = expected_wavs(&planned(&segs), &Cast::new(), Path::new("s"), true, None).unwrap_err();
    assert!(err.to_string().contains("no voice for"), "{err}");
}

#[test]
fn plan_render_is_per_run_locally_and_per_line_in_the_cloud() {
    let segs = vec![
        json!({"speaker": "A", "text": "one", "mood": "calm"}),
        json!({"speaker": "A", "text": "two", "mood": "calm"}),
    ];
    let mut cast = Cast::new();
    cast.insert("A".into(), "Đức Trí".into());
    let local = plan_render(&planned(&segs), &cast, Path::new("s"), true, None).unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].tag, "0000-0001");
    assert_eq!(local[0].text, "one two");
    assert_eq!(local[0].temperature, take_for_mood("calm").0);

    let cloud = plan_render(&planned(&segs), &cast, Path::new("s"), false, None).unwrap();
    assert_eq!(cloud.len(), 2);
}

fn titled_layout(tag: &str, headline: &str) -> (PathBuf, crate::Layout) {
    let d = tmpdir(&format!("title-{tag}"));
    let l = crate::Layout::new(&d);
    std::fs::create_dir_all(l.chapters()).unwrap();
    std::fs::write(l.chapter_txt(7), format!("{headline}\n\nbody\n")).unwrap();
    (d, l)
}

#[test]
fn headline_gets_its_own_leading_run_and_cache_file() {
    let (_d, l) = titled_layout("t", "Chương 7: Kiếm khí xung thiên. . .");
    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "Đức Trí".into());
    cast.insert("A".into(), "Adam".into());
    let segs = vec![json!({"speaker": "A", "text": "mở đầu"})];
    let first = "mở đầu";
    let title = title_speech(&l, 7, &cast, first).unwrap();
    assert_eq!(title.text, "Chương 7, Kiếm khí xung thiên");
    assert_eq!(title.voice, "Đức Trí");
    let units = plan_render(&planned(&segs), &cast, Path::new("s"), true, Some(&title)).unwrap();
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].tag, "title");
    assert!(
        units[0].dest.ends_with("title_Đức Trí.wav"),
        "{:?}",
        units[0].dest
    );
    assert_eq!(units[1].tag, "0000");
    let wavs = expected_wavs(&planned(&segs), &cast, Path::new("s"), true, Some(&title)).unwrap();
    assert_eq!(wavs.len(), 2);
    assert!(wavs[0].ends_with("title_Đức Trí.wav"));
}

#[test]
fn headline_skipped_when_the_digest_kept_its_own() {
    // Its own tag: `tmpdir` does `remove_dir_all` first, and tests run in
    let (_d, l) = titled_layout("t4", "Chương 7: Kiếm khí xung thiên");
    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "Đức Trí".into());
    assert!(title_speech(&l, 7, &cast, "Kiếm khí xung thiên vang lên").is_none());
    // A quoted chapter number is dialogue, not a headline: title still spoken.
    assert!(title_speech(&l, 7, &cast, "\"Chương 7\" ai đó nói").is_some());
}

#[test]
fn headline_skipped_without_chapter_text() {
    let d = tmpdir("title-missing");
    let l = crate::Layout::new(&d);
    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "Đức Trí".into());
    assert!(title_speech(&l, 7, &cast, "mở đầu").is_none());
}

#[test]
fn drop_headline_only_cuts_a_leading_chapter_heading() {
    let hl = || json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"});
    let body = || json!({"speaker": "A", "text": "mở đầu"});
    assert_eq!(drop_headline(&[hl(), body()]).len(), 1);
    assert_eq!(drop_headline(&[body(), hl()]).len(), 2); // headline later: kept
    assert_eq!(drop_headline(&[]).len(), 0);
    assert!(is_headline("  Chương 12: x"));
    assert!(!is_headline("Chương pháp này rất hay")); // no digits: content
    assert!(!is_headline("mở đầu"));
}

#[test]
fn under_title_mode_default_the_headline_is_spoken_even_when_the_prose_names_it() {
    // `Chapter 1: Maomao` is a chapter *about* Maomao, so its first line
    let d = tmpdir("title-default");
    std::fs::create_dir_all(d.join("adapters/jnovel-en-US/prompts")).unwrap();
    std::fs::write(
        d.join("adapters/jnovel-en-US/adapter.json"),
        r#"{"pack":"","language":"en-US","engine":""}"#,
    )
    .unwrap();
    let mut l = crate::Layout::new(&d);
    l.adapter = "jnovel-en-US".into();
    std::fs::create_dir_all(l.chapters()).unwrap();
    std::fs::write(l.chapter_txt(1), "Chapter 1: Maomao\n\nbody\n").unwrap();
    std::fs::create_dir_all(l.script(1).parent().unwrap()).unwrap();
    std::fs::write(
        l.script(1),
        r#"{"title":"Maomao Enters The Rear Palace","segments":[]}"#,
    )
    .unwrap();
    let mut s = crate::config::Settings::load(&l.settings());
    s.title_mode = "default".into();
    std::fs::create_dir_all(l.settings().parent().unwrap()).unwrap();
    s.save(&l.settings()).unwrap();

    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "your-narrator".into());
    let title = title_speech(&l, 1, &cast, "Maomao looked up at the overcast sky.").unwrap();
    assert_eq!(title.text, "Chapter 1, Maomao");
}

#[test]
fn a_headline_is_recognized_in_both_languages() {
    assert!(is_headline("Chapter 7: Kiếm khí xung thiên"));
    assert!(is_headline("  Chapter 12 — x"));
    // The word must lead and be followed by a digit: prose that merely
    assert!(!is_headline("Chapter without digits"));
    assert!(!is_headline("The Chapter 7 was long"));
}

#[test]
fn the_spoken_heading_follows_the_adapters_language() {
    let d = tmpdir("title-en");
    std::fs::create_dir_all(d.join("adapters/jnovel-en-US/prompts")).unwrap();
    std::fs::write(
        d.join("adapters/jnovel-en-US/adapter.json"),
        r#"{"pack":"","language":"en-US","engine":""}"#,
    )
    .unwrap();
    let mut l = crate::Layout::new(&d);
    l.adapter = "jnovel-en-US".into();
    std::fs::create_dir_all(l.chapters()).unwrap();
    std::fs::write(l.chapter_txt(7), "Chapter 7: Kiếm khí xung thiên\n\nbody\n").unwrap();
    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "your-narrator".into());
    cast.insert("A".into(), "Adam".into());
    let title = title_speech(&l, 7, &cast, "mở đầu").unwrap();
    assert_eq!(title.text, "Chapter 7, Kiếm khí xung thiên");

    // And the embedded English headline is recognized, so a script that
    let kept = vec![
        json!({"speaker": "Narrator", "text": "Chapter 7: Kiếm khí xung thiên"}),
        json!({"speaker": "A", "text": "mở đầu"}),
    ];
    let body = planned(&kept);
    assert_eq!(body.speech.len(), 1, "the English headline is dropped");
    assert_eq!(seg_text(&body.speech[0]), "mở đầu");
}

#[test]
fn kept_headline_never_speaks_twice() {
    let (_d, l) = titled_layout("t2", "Chương 7: Kiếm khí xung thiên");
    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "Đức Trí".into());
    cast.insert("A".into(), "Adam".into());
    // Digest kept "Chương 7: ..." as its first segment.
    let segs = vec![
        json!({"speaker": "Narrator", "text": "Chương 7: Kiếm khí xung thiên"}),
        json!({"speaker": "A", "text": "mở đầu"}),
    ];
    let body = planned(&segs);
    assert_eq!(body.speech.len(), 1);
    let first = body.speech[0].get("text").and_then(|t| t.as_str()).unwrap();
    let title = title_speech(&l, 7, &cast, first).unwrap();
    let units = plan_render(&body, &cast, Path::new("s"), true, Some(&title)).unwrap();
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].tag, "title");
    assert_eq!(units[0].text, "Chương 7, Kiếm khí xung thiên");
    // The embedded raw headline appears in no unit.
    assert!(!units.iter().any(|u| u.text.contains("Chương 7:")));
    let wavs = expected_wavs(&body, &cast, Path::new("s"), true, Some(&title)).unwrap();
    assert_eq!(wavs.len(), 2);
    assert!(wavs[0].ends_with("title_Đức Trí.wav"));
}

#[test]
fn stripped_and_kept_scripts_plan_the_same_runs() {
    let (_d, l) = titled_layout("t3", "Chương 7: Kiếm khí xung thiên");
    let mut cast = Cast::new();
    cast.insert("Narrator".into(), "Đức Trí".into());
    cast.insert("A".into(), "Adam".into());
    let body = vec![json!({"speaker": "A", "text": "mở đầu"})];
    let first = "mở đầu";
    let title = title_speech(&l, 7, &cast, first).unwrap();
    let a = plan_render(&planned(&body), &cast, Path::new("s"), true, Some(&title)).unwrap();
    let mut kept = vec![json!({"speaker": "Narrator", "text": "Chương 7: x"})];
    kept.extend(body.clone());
    let b = plan_render(&planned(&kept), &cast, Path::new("s"), true, Some(&title)).unwrap();
    assert_eq!(
        a.iter().map(|u| u.tag.clone()).collect::<Vec<_>>(),
        b.iter().map(|u| u.tag.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn segments_complete_is_false_until_every_wav_exists() {
    let d = tmpdir("complete");
    let l = crate::Layout::new(&d);
    let script = d.join("script-01.json");
    std::fs::write(
        &script,
        r#"{"segments":[{"speaker":"A","text":"one"},{"speaker":"A","text":"two"}]}"#,
    )
    .unwrap();
    let cast = d.join("cast-vieneu.json");
    std::fs::write(&cast, r#"{"A":"Đức Trí"}"#).unwrap();
    let segs = d.join("segs");
    std::fs::create_dir_all(&segs).unwrap();
    assert!(!segments_complete(
        &l,
        &script,
        &cast,
        &d.join("bible.json"),
        &segs,
        "vieneu"
    ));
    silent_wav(&segs.join("0000-0001_Đức Trí.wav"), 0.05, 48_000).unwrap();
    assert!(segments_complete(
        &l,
        &script,
        &cast,
        &d.join("bible.json"),
        &segs,
        "vieneu"
    ));
}
