use super::*;
use serde_json::json;
use std::path::Path;

mod portraits;
mod sprite;

/// A workspace with an `output/` for the fixtures to publish into.
fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bm-filmcraft-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("output")).unwrap();
    dir
}

/// One published chapter: its mp3 stand-in and a v2 cue sidecar.
fn chapter(dir: &Path, n: u32, title: &str, duration_s: f64, cues: &[(f64, f64, &str)]) {
    let stem = format!("Ch.{n} - {title}");
    std::fs::write(dir.join("output").join(format!("{stem}.mp3")), b"an mp3 stand-in").unwrap();
    let cues: Vec<serde_json::Value> = cues
        .iter()
        .enumerate()
        .map(|(i, (start, end, text))| {
            json!({"i": i + 1, "speaker": "Người kể", "start": start, "end": end, "text": text})
        })
        .collect();
    let doc = json!({"version": 2, "chapter": n, "duration_s": duration_s, "cues": cues});
    std::fs::write(
        dir.join("output").join(format!("{stem}.cues.json")),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();
}

fn two_acts() -> model::Manifest {
    serde_json::from_str(
        r#"{"name":"book","acts":[
             {"act":1,"title":"Lộ diện","chapters":[1,2]},
             {"act":2,"title":"Mượn đao","chapters":[3]}]}"#,
    )
    .unwrap()
}

/// Three published chapters. Each caller names it, because the tests run in
/// parallel and one workspace is one directory.
fn book(name: &str) -> PathBuf {
    let dir = workspace(name);
    chapter(&dir, 1, "One", 10.0, &[(0.0, 2.0, "Chương 1"), (2.0, 9.5, "Lời đầu tiên")]);
    chapter(&dir, 2, "Two", 5.0, &[(0.0, 4.0, "Chương 2")]);
    chapter(&dir, 3, "Three", 6.0, &[(0.0, 5.5, "Chương 3")]);
    dir
}

/// The repo's own render template: the look `bm-video` renders from, and the
/// one FilmCraft now composes.
fn template() -> look::Look {
    look::Look::load(Path::new("../../tools/video-template.json")).expect("the repo's template")
}

/// A caption's line in the rendered file, and the stamp above it.
fn entry(srt: &str, n: usize) -> (String, String, String) {
    let blocks: Vec<&str> = srt.split("\n\n").filter(|b| !b.trim().is_empty()).collect();
    let lines: Vec<&str> = blocks[n - 1].lines().collect();
    (lines[0].to_string(), lines[1].to_string(), lines[2..].join("\n"))
}

#[test]
fn chapters_butt_up_with_the_gap_between_them() {
    let plan = plan::Plan::build(&two_acts(), &book("butt-up"), 0.8, template(), (1920, 1080)).unwrap();
    let starts: Vec<f64> = plan.spans.iter().map(|s| s.start).collect();
    assert_eq!(starts, vec![0.0, 10.8, 16.6], "one gap between chapters, not after the last");
    assert_eq!(plan.total, 22.6, "the last chapter ends the book");
    assert_eq!(plan.acts[0].start, 0.0);
    assert_eq!(plan.acts[1].start, 16.6, "act 2 begins at its first chapter");
    assert_eq!(plan.captions(), 4);
}

#[test]
fn a_chapter_listed_twice_is_refused() {
    let manifest: model::Manifest = serde_json::from_str(
        r#"{"acts":[{"act":1,"title":"A","chapters":[1,2]},{"act":2,"title":"B","chapters":[2]}]}"#,
    )
    .unwrap();
    let err = plan::Plan::build(&manifest, &book("twice"), 0.8, template(), (1920, 1080)).unwrap_err().to_string();
    assert!(err.contains("chapter 2 is listed twice"), "{err}");
}

#[test]
fn an_act_with_no_chapters_is_refused() {
    let manifest: model::Manifest =
        serde_json::from_str(r#"{"acts":[{"act":1,"title":"A","chapters":[]}]}"#).unwrap();
    let err = plan::Plan::build(&manifest, &book("empty-act"), 0.8, template(), (1920, 1080)).unwrap_err().to_string();
    assert!(err.contains("no chapters"), "{err}");
}

#[test]
fn the_span_falls_back_to_the_last_cue() {
    let dir = workspace("span");
    chapter(&dir, 1, "One", 0.0, &[(0.0, 2.0, "a"), (2.0, 7.25, "b")]);
    let manifest: model::Manifest =
        serde_json::from_str(r#"{"acts":[{"act":1,"title":"A","chapters":[1]}]}"#).unwrap();
    let plan = plan::Plan::build(&manifest, &dir, 0.8, template(), (1920, 1080)).unwrap();
    assert_eq!(plan.total, 7.25, "a sidecar without duration_s still has a length");
}

#[test]
fn a_chapter_with_no_cues_is_refused() {
    let dir = workspace("nocues");
    std::fs::write(dir.join("output/Ch.1 - One.mp3"), b"stand-in").unwrap();
    std::fs::write(dir.join("output/Ch.1 - One.cues.json"), "{}").unwrap();
    let manifest: model::Manifest =
        serde_json::from_str(r#"{"acts":[{"act":1,"title":"A","chapters":[1]}]}"#).unwrap();
    let err = plan::Plan::build(&manifest, &dir, 0.8, template(), (1920, 1080)).unwrap_err().to_string();
    assert!(err.contains("chapter 1 has no cues"), "{err}");
}

#[test]
fn a_missing_chapter_names_itself() {
    let manifest: model::Manifest =
        serde_json::from_str(r#"{"acts":[{"act":1,"title":"A","chapters":[9]}]}"#).unwrap();
    let err = plan::Plan::build(&manifest, &book("missing"), 0.8, template(), (1920, 1080)).unwrap_err().to_string();
    assert!(err.contains("no published mp3 for chapter 9"), "{err}");
}

#[test]
fn captions_carry_the_chapter_offset_into_subrip() {
    let dir = workspace("srt");
    chapter(&dir, 1, "One", 10.0, &[(0.5, 2.5, "Chương 1")]);
    chapter(&dir, 2, "Two", 5.0, &[(1.0, 3.0, "Chương 2")]);
    let manifest: model::Manifest = serde_json::from_str(
        r#"{"acts":[{"act":1,"title":"A","chapters":[1]},{"act":2,"title":"B","chapters":[2]}]}"#,
    )
    .unwrap();
    let plan = plan::Plan::build(&manifest, &dir, 0.8, template(), (1920, 1080)).unwrap();
    let srt = plan.srt();
    let (number, stamp, text) = entry(&srt, 1);
    assert_eq!((number.as_str(), stamp.as_str(), text.as_str()), ("1", "00:00:00,500 --> 00:00:02,500", "Chương 1"));
    let (number, stamp, text) = entry(&srt, 2);
    assert_eq!(number, "2", "numbering runs on across chapters");
    assert_eq!(stamp, "00:00:11,800 --> 00:00:13,800", "chapter 2 starts at 10.0 + the gap");
    assert_eq!(text, "Chương 2");
}

#[test]
fn acts_butt_up_on_a_frame_even_when_their_windows_do_not() {
    // A real chapter's length lands between two frames — 7.31 s is 219.3 of
    // them — and FilmCraft rounds a clip's own end up to a whole frame, so a
    // plate on the raw instant is refused: `V11 is not free here`.
    let dir = workspace("frames");
    chapter(&dir, 1, "One", 7.31, &[(0.0, 7.0, "Chương 1")]);
    chapter(&dir, 2, "Two", 4.0, &[(0.0, 3.5, "Chương 2")]);
    let manifest: model::Manifest = serde_json::from_str(
        r#"{"acts":[{"act":1,"title":"A","chapters":[1]},{"act":2,"title":"B","chapters":[2]}]}"#,
    )
    .unwrap();
    let plan = plan::Plan::build(&manifest, &dir, 0.8, template(), (1920, 1080)).unwrap();
    let boundary = plan.acts[1].start * 30.0;
    assert!(
        (boundary - boundary.round()).abs() > 1e-6,
        "the test is pointless if the boundary is already on a frame: {boundary}"
    );
    let steps = plan
        .steps(&[1, 2], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[])
        .unwrap();
    let texts: Vec<&serde_json::Value> = steps
        .iter()
        .filter(|s| s.id == "graphics.newText")
        .map(|s| &s.params)
        .collect();
    assert_eq!(texts.len(), 4, "a label and a title per act");
    // One frame in ticks, and what FilmCraft does with the seconds it is given.
    let frame = 254_016_000_000i64 / 30;
    let len = |t: &serde_json::Value| (t["seconds"].as_f64().unwrap() * 30.0).round() as i64 * frame;
    let end = |t: &serde_json::Value| t["time"].as_i64().unwrap() + len(t);
    for t in &texts {
        assert_eq!(t["time"].as_i64().unwrap() % frame, 0, "the start is on the grid: {t}");
        assert_eq!(len(t) % frame, 0, "and so is the length: {t}");
    }
    assert_eq!(
        texts[2]["time"].as_i64().unwrap(),
        end(texts[0]),
        "act 2's label starts on the frame act 1's ends on"
    );
    assert_eq!(texts[3]["time"].as_i64().unwrap(), end(texts[1]), "and so does its title");
}

#[test]
fn the_stamp_carries_hours_and_milliseconds() {
    let srt = srt::render(&[srt::Entry {
        start: 3725.25,
        end: 3726.001,
        text: "late".into(),
    }]);
    let (_, stamp, _) = entry(&srt, 1);
    assert_eq!(stamp, "01:02:05,250 --> 01:02:06,001");
}

#[test]
fn an_untimed_line_is_not_a_caption() {
    // A non-speech slot: the mixer writes it with empty text.
    let srt = srt::render(&[
        srt::Entry { start: 0.0, end: 1.0, text: "   ".into() },
        srt::Entry { start: 1.0, end: 2.0, text: "spoken".into() },
    ]);
    let blocks: Vec<&str> = srt.split("\n\n").filter(|b| !b.trim().is_empty()).collect();
    assert_eq!(blocks.len(), 1, "the empty cue is dropped: {srt}");
    assert_eq!(entry(&srt, 1).0, "1", "numbering does not leave a hole");
}

#[test]
fn a_multi_line_cue_becomes_one_caption_line() {
    let srt = srt::render(&[srt::Entry {
        start: 0.0,
        end: 1.0,
        text: "hai dòng\n  trong một cue ".into(),
    }]);
    assert_eq!(entry(&srt, 1).2, "hai dòng trong một cue");
}

#[test]
fn empty_cues_still_produce_a_valid_srt() {
    let srt = srt::render(&[]);
    assert_eq!(srt, "", "no captions is no file, not an empty entry");
}

#[test]
fn the_look_is_composed_the_way_the_template_lays_it_out() {
    let plan = plan::Plan::build(&two_acts(), &book("steps"), 0.8, template(), (1920, 1080)).unwrap();
    let steps = plan.steps(&[11, 12, 13], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[]).unwrap();
    let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "timeline.place",
            "timeline.place",
            "timeline.place",
            "graphics.newShape",
            "graphics.set",
            "graphics.newText",
            "graphics.set",
            "graphics.newText",
            "graphics.set",
            "markers.addChapter",
            "graphics.newText",
            "graphics.set",
            "graphics.newText",
            "graphics.set",
            "markers.addChapter",
            "graphics.newShape",
            "graphics.set",
            "graphics.newShape",
            "graphics.set",
            "graphics.newShape",
            "graphics.set",
            "graphics.newShape",
            "graphics.set",
            "graphics.newShape",
            "graphics.set",
            "graphics.newShape",
            "graphics.set",
            "captions.import",
            "captions.setStyle",
        ]
    );
    assert_eq!(steps[0].params, json!({"item": 11, "track": "A1", "seconds": 0.0}));
    assert_eq!(steps[2].params, json!({"item": 13, "track": "A1", "seconds": 16.6}));
    // The paper: the whole frame, under everything else.
    assert_eq!(steps[3].params["shape"], json!("rectangle"));
    assert_eq!(steps[3].params["size"], json!([1920, 1080]));
    assert_eq!(steps[3].params["seconds"], json!(22.6), "the paper spans the book");
    assert_eq!(steps[3].params["track"], json!(0), "V1");
    assert_eq!(
        steps[4].params["props"]["fill_color"],
        json!("#fafafa"),
        "a gradient has no FilmCraft command: its middle stop stands in"
    );
    // The act plate, in the template's own type.
    let label = &steps[5].params;
    assert_eq!(label["text"], json!("Hồi 1"), "the template's label, numbered");
    assert_eq!(label["font"], json!("SVN-Hottroof Semhesta"));
    assert_eq!(label["size"], json!(60.0), "unit 1.25 × act_label 2.5");
    assert_eq!(label["seconds"], json!(16.6), "up for the act, not a fixed six seconds");
    assert_eq!(label["track"], json!(6), "the label's own track, above the bar");
    assert_eq!(
        steps[7].params["track"],
        json!(7),
        "and the title's — a track holds one clip at a time"
    );
    assert_eq!(steps[6].params["props"]["fill_color"], json!("#9e3b2c"), "act_label ink");
    let title = &steps[7].params;
    assert_eq!(title["text"], json!("Lộ diện"));
    assert_eq!(title["font"], json!("FC Opulent Brush"));
    assert_eq!(title["size"], json!(144.0), "unit 1.25 × act_title 6");
    assert_eq!(steps[8].params["props"]["fill_color"], json!("#1c1b1a"), "act_title ink");
    assert_eq!(steps[9].params, json!({"name": "Lộ diện", "seconds": 0.0}));
    let second = &steps[10].params;
    assert_eq!(second["text"], json!("Hồi 2"));
    assert_eq!(second["time"], 4_216_665_600_000_i64, "act 2's plate rides its own clock");
    // The bar: the track, then one segment per act.
    let track = &steps[15].params;
    assert_eq!(track["track"], json!(1), "the groove on its own track");
    let size = track["size"].as_array().unwrap();
    assert!((size[0].as_f64().unwrap() - 1113.6).abs() < 0.01, "the timeline zone's width");
    assert_eq!(size[1], json!(14.0), "the template's thickness");
    assert_eq!(
        steps[16].params["props"]["fill_color"],
        json!("#a8a29459"),
        "the unfilled track is the segment colour at a third"
    );
    assert_eq!(steps[17].params["track"], json!(2), "and the first segment on its own, over it");
    assert_eq!(steps[19].params["track"], json!(3), "then the second, beside it in space, not time");
    assert_eq!(
        plan.video_tracks(&plan::Portraits::default(), 0),
        9,
        "the sequence is made with room for the whole look: 2 acts + paper, groove, 2 highlights, label, title, thumb"
    );
    assert_eq!(steps[18].params["props"]["fill_color"], json!("#a8a294"));
    // The captions, styled from the subtitle block.
    assert_eq!(steps[27].params, json!({"path": "/renders/book.srt", "name": "Storycast"}));
    let style = &steps[28].params;
    assert_eq!(style["font"], json!("UVN Gia Dinh Hep"));
    assert_eq!(style["size"], json!(48.0), "unit 1.25 × subtitle 2");
    assert_eq!(style["color"], json!("#1c1b1a"));
    assert_eq!(style["backgroundColor"], json!("#faf7f09e"), "the plate, alpha and all");
    assert_eq!(style["track"], json!("C1"));
}

#[test]
fn the_length_filmcraft_decodes_is_the_one_that_lays_out_the_edit() {
    let mut plan = plan::Plan::build(&two_acts(), &book("align"), 0.8, template(), (1920, 1080)).unwrap();
    // The mp3s run past their cue sheets, as the published ones do — by 1.5 s on
    // the first chapter. Placing the next on the cue sheet's figure would cut
    // that tail off.
    let durations: std::collections::HashMap<i64, f64> =
        [(11, 11.5), (12, 5.25), (13, 6.0)].into_iter().collect();
    plan.align(&[11, 12, 13], &durations, 0.8).unwrap();
    assert_eq!(
        plan.spans.iter().map(|s| s.start).collect::<Vec<f64>>(),
        vec![0.0, 12.3, 18.35],
        "each chapter starts past the last one's real end plus the gap"
    );
    assert_eq!(plan.spans[0].dur, 11.5, "chapter 1 keeps the tail it has");
    assert_eq!(plan.acts[1].start, 18.35, "and the act rides the new clock");
    assert!(
        plan.spans[1].start >= plan.spans[0].start + plan.spans[0].dur + 0.8,
        "no chapter's tail is overwritten"
    );
    assert_eq!(plan.total, 24.35);
    let srt = plan.srt();
    assert_eq!(
        entry(&srt, 3).1,
        "00:00:12,300 --> 00:00:16,300",
        "captions follow the aligned clock, not the cue sheet's"
    );
}

#[test]
fn an_item_filmcraft_gave_no_length_is_refused() {
    let mut plan = plan::Plan::build(&two_acts(), &book("no-length"), 0.8, template(), (1920, 1080)).unwrap();
    let durations: std::collections::HashMap<i64, f64> =
        [(11, 11.5), (12, 0.0), (13, 6.0)].into_iter().collect();
    let err = plan.align(&[11, 12, 13], &durations, 0.8).unwrap_err().to_string();
    assert!(err.contains("no length for chapter 2 (item 12)"), "{err}");
}

#[test]
fn aligning_refuses_a_short_item_list() {
    let mut plan = plan::Plan::build(&two_acts(), &book("align-short"), 0.8, template(), (1920, 1080)).unwrap();
    let durations: std::collections::HashMap<i64, f64> = [(11, 11.5), (12, 5.25)].into_iter().collect();
    let err = plan.align(&[11, 12], &durations, 0.8).unwrap_err().to_string();
    assert!(err.contains("returned 2 items for 3 chapters"), "{err}");
}

#[test]
fn a_second_is_an_exact_number_of_ticks() {
    assert_eq!(plan::ticks(0.0), 0);
    assert_eq!(plan::ticks(1.0), 254_016_000_000);
    assert_eq!(
        plan::ticks(16.6),
        4_216_665_600_000,
        "the tick base is what keeps an edit from drifting"
    );
    assert_eq!(
        filmcraft::TICKS_PER_SECOND,
        254_016_000_000.0,
        "and it is FilmCraft's, not ours to pick"
    );
}

#[test]
fn a_mismatch_between_items_and_chapters_is_refused() {
    let plan = plan::Plan::build(&two_acts(), &book("mismatch"), 0.8, template(), (1920, 1080)).unwrap();
    let err = plan
        .steps(&[11, 12], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[])
        .unwrap_err()
        .to_string();
    assert!(err.contains("returned 2 items for 3 chapters"), "{err}");
}

#[test]
fn the_script_is_one_json_object_per_line() {
    let plan = plan::Plan::build(&two_acts(), &book("script"), 0.8, template(), (1920, 1080)).unwrap();
    let steps = plan.steps(&[11, 12, 13], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[]).unwrap();
    let text = plan::script(&steps);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), steps.len());
    for (line, step) in lines.iter().zip(&steps) {
        let value: serde_json::Value =
            serde_json::from_str(line).expect("a script line is JSON");
        assert_eq!(value["id"], step.id);
        assert_eq!(value["params"], step.params);
    }
}

#[test]
fn a_single_act_manifest_loads_and_names_itself() {
    let dir = workspace("one");
    let path = dir.join("one.json");
    std::fs::write(&path, r#"{"name":"one-chapter","title":"T","chapters":[1]}"#).unwrap();
    let manifest = model::Manifest::load(&path).unwrap();
    assert_eq!(manifest.default_name(), "one-chapter");
    assert_eq!(manifest.acts.len(), 1);
    assert_eq!(manifest.acts[0].act, 1);
}

#[test]
fn an_unnamed_manifest_names_itself_after_its_acts() {
    let manifest: model::Manifest = serde_json::from_str(
        r#"{"acts":[{"act":1,"title":"A","chapters":[1]},{"act":4,"title":"B","chapters":[2]}]}"#,
    )
    .unwrap();
    assert_eq!(manifest.default_name(), "acts-01-04");
}

#[test]
fn canvas_is_a_pair_and_nothing_else() {
    assert_eq!(parse_canvas("1920x1080").unwrap(), (1920, 1080));
    assert_eq!(parse_canvas(" 1280 X 720 ").unwrap(), (1280, 720));
    assert!(parse_canvas("1920").unwrap_err().to_string().contains("WIDTHxHEIGHT"));
    assert!(parse_canvas("1920xwide").unwrap_err().to_string().contains("pixel count"));
    assert!(parse_canvas("0x1080").unwrap_err().to_string().contains("one pixel"));
}

#[test]
fn the_report_finds_the_counters_under_the_reply() {
    let reply = json!({"job": 1, "path": "/renders/out.mp4", "result": {"bytes": 2048, "frames": 30}});
    let line = export_line(&reply, Path::new("/renders/out.mp4"));
    assert!(line.contains("30 frames"), "{line}");
    assert!(line.contains("2048 bytes"), "{line}");
}

#[test]
fn a_reply_without_counters_still_names_the_file() {
    let line = export_line(&json!({"job": 1}), Path::new("/renders/out.mp4"));
    assert_eq!(line, "/renders/out.mp4");
}

#[test]
fn the_template_resolves_the_way_bm_video_resolves_it() {
    let t = template();
    assert_eq!(t.px("act_label", 1920), 60.0);
    assert_eq!(t.px("act_title", 1920), 144.0);
    assert_eq!(t.px("subtitle", 1920), 48.0);
    assert_eq!(t.px("act_title", 1280), 96.0, "the unit follows the frame, not the template");
    let zone = t.box_of("act_title", 1920, 1080).unwrap();
    assert!((zone.x - 633.6).abs() < 0.01, "{zone:?}");
    assert!((zone.w - 1113.6).abs() < 0.01, "{zone:?}");
    assert!((zone.y - 118.8).abs() < 0.01, "aligned to the illustration's top: {zone:?}");
    assert_eq!(t.label(3), "Hồi 3");
    assert_eq!(t.colour("subtitle_plate").as_deref(), Some("#faf7f09e"), "rgba() → #rrggbbaa");
    assert_eq!(t.colour("background").as_deref(), Some("#fafafa"), "a gradient's middle stop");
    assert_eq!(t.colour("timeline_track").as_deref(), Some("#1c1b1a1f"));
    assert_eq!(t.font("act_title"), Some("FC Opulent Brush"));
    assert_eq!(t.fps(), 30.0);
    let bar = t.bar(&[16.6, 6.0], 1920, 1080).unwrap();
    assert_eq!(bar.track_h, 14.0);
    assert!(
        (bar.cy - 553.2).abs() < 0.01,
        "the timeline zone's centre, its bottom aligned to the illustration's: {bar:?}"
    );
    assert_eq!(bar.segs.len(), 2, "one segment per act");
    assert_eq!(t.active_y_scale(), 1.4, "the template omits the lift; 1.4 is its default");
    let disc = t.thumb_disc(1080).expect("the template has a thumb");
    assert!((disc.dia - 52.0).abs() < 0.01, "radius 13 × scale 2, twice: {disc:?}");
    assert!((disc.ring_w - 8.32).abs() < 0.01, "the ring is the outer 32% of the radius: {disc:?}");
    assert!((bar.segs[0].0 - 633.6).abs() < 0.01, "{bar:?}");
    assert!(
        bar.segs[1].0 > bar.segs[0].0 + bar.segs[0].1,
        "segments leave the template's gap between them: {bar:?}"
    );
}

#[test]
fn a_template_font_this_machine_lacks_is_named_with_its_file() {
    let t = template();
    let missing = t.missing_fonts(&["fc opulent brush".into()]);
    let families: Vec<&str> = missing.iter().map(|(_, f, _)| f.as_str()).collect();
    assert_eq!(families, ["SVN-Hottroof Semhesta", "UVN Gia Dinh Hep"], "{missing:?}");
    let file = missing
        .iter()
        .find(|(role, _, _)| role == "subtitle")
        .map(|(_, _, file)| file.clone());
    assert_eq!(file.as_deref(), Some("tmp/UVNGiaDinhHep_R.TTF"), "the file to install it from");
}

#[test]
fn the_bar_raises_the_act_that_is_playing() {
    let plan = plan::Plan::build(&two_acts(), &book("playing"), 0.8, template(), (1920, 1080)).unwrap();
    let steps = plan.steps(&[11, 12, 13], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[]).unwrap();
    let first = &steps[21].params;
    assert_eq!(first["track"], json!(4), "its own track, over the segments");
    assert_eq!(first["time"], json!(0), "act 1 is playing from the top");
    assert_eq!(first["seconds"], json!(16.6), "and until act 2 takes over");
    assert!(
        (first["size"][1].as_f64().unwrap() - 20.0).abs() < 0.01,
        "raised: the template's 14 at 1.4: {first}"
    );
    assert_eq!(
        steps[22].params["props"]["fill_color"],
        json!("#9e3b2c"),
        "timeline_segment_active"
    );
    // The highlight sits exactly over the segment it raises.
    assert_eq!(first["position"], steps[17].params["position"]);
    assert_eq!(first["size"][0], steps[17].params["size"][0]);
    let second = &steps[23].params;
    assert_eq!(
        second["time"],
        json!(4_216_665_600_000_i64),
        "act 2's highlight rides its own window"
    );
    assert_eq!(second["seconds"], json!(6.0), "to the end of the book");
}

#[test]
fn the_thumb_runs_the_bar_and_is_set_moving_in_a_second_pass() {
    let plan = plan::Plan::build(&two_acts(), &book("thumb"), 0.8, template(), (1920, 1080)).unwrap();
    let steps = plan.steps(&[11, 12, 13], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[]).unwrap();
    let disc = &steps[25].params;
    assert_eq!(disc["shape"], json!("ellipse"), "the template's disc, not a square");
    assert_eq!(disc["size"], json!([52.0, 52.0]), "radius 13 × scale 2, twice");
    assert_eq!(disc["track"], json!(8), "above the plate, as the template stacks its layers");
    assert_eq!(disc["seconds"], json!(22.6), "the whole book");
    let style = &steps[26].params["props"];
    assert_eq!(style["fill_color"], json!("#1c1b1a"), "the thumb's ink");
    assert_eq!(style["strokeColor"], json!("#f4efe3"), "inside its ring");
    assert!(
        (style["strokeWidth"].as_f64().unwrap() - 8.32).abs() < 0.01,
        "the ring's width: {style}"
    );

    // The reply to `graphics.newShape` is the only thing carrying the clip id.
    let replies: Vec<serde_json::Value> = (0..steps.len())
        .map(|i| match i {
            25 => json!({"ok": true, "result": {"clip": 42, "layer": 0}}),
            _ => json!({"ok": true, "result": null}),
        })
        .collect();
    let moving = plan.animate(&steps, &replies, &plan::Portraits::default(), &element::Elements::default()).unwrap();
    assert_eq!(moving.len(), 7, "two properties, made animated and then set");
    assert_eq!(moving[0].id, "effects.addKeyframe");
    assert_eq!(
        moving[0].params,
        json!({"clip": 42, "effect": "graphic_shape", "param": "position", "time": 0}),
        "the effect that holds a graphic's position"
    );
    assert_eq!(moving[1].params["clip"], json!(42));
    let x = |i: usize| moving[i].params["props"]["position"][0].as_f64().unwrap();
    assert!((x(1) - 633.6).abs() < 0.01, "from the bar's left edge");
    assert!((x(2) - 1747.2).abs() < 0.01, "to its right, over the book");
    assert_eq!(
        moving[2].params["time"],
        json!(5_740_761_600_000_i64),
        "the end of the book is where the disc stops"
    );
    assert_eq!(
        moving[1].params["props"]["position"][1],
        json!(553.2),
        "and it travels the bar's centre line"
    );
    // The disc turns: a whole revolution every spin_s, three keys for the book.
    let turns: Vec<&serde_json::Value> = moving
        .iter()
        .filter(|s| s.params["props"].get("rotation").is_some())
        .map(|s| &s.params)
        .collect();
    assert_eq!(turns.len(), 3, "the angle is a straight line: {turns:?}");
    assert_eq!(turns[0]["props"]["rotation"], json!(0.0));
    assert_eq!(turns[1]["props"]["rotation"], json!(-360.0), "ccw, so it falls");
    assert_eq!(turns[1]["time"], json!(304_819_200_000_i64), "one turn takes 1.2 s");
    assert!(
        turns[2]["props"]["rotation"].as_f64().unwrap() < -360.0 * 18.0,
        "and the last key is the end of the book: {}",
        turns[2]["props"]["rotation"]
    );
    let err = plan
        .animate(&steps, &replies[..25], &plan::Portraits::default(), &element::Elements::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("no clip id for the thumb"), "{err}");
}

#[test]
fn a_template_without_a_thumb_is_not_animated() {
    let mut t = template();
    t.timeline.thumb = None;
    let plan = plan::Plan::build(&two_acts(), &book("no-thumb"), 0.8, t, (1920, 1080)).unwrap();
    let steps = plan.steps(&[11, 12, 13], Path::new("/renders/book.srt"), &plan::Portraits::default(), &[], &element::Elements::default(), &[]).unwrap();
    let replies: Vec<serde_json::Value> =
        (0..steps.len()).map(|_| json!({"ok": true, "result": null})).collect();
    assert!(plan.animate(&steps, &replies, &plan::Portraits::default(), &element::Elements::default()).unwrap().is_empty(), "nothing to set moving");
}



#[test]
fn the_clock_prints_a_length_a_person_reads() {
    assert_eq!(plan::clock(0.0), "00:00:00");
    assert_eq!(plan::clock(75.4), "00:01:15");
    assert_eq!(plan::clock(9272.0), "02:34:32");
}
