use super::*;
use serde_json::json;
use std::path::Path;

#[test]
fn a_speakers_name_picks_a_portrait_or_falls_back_to_the_anonymous_one() {
    let t = template();
    let sticker = t.sticker().expect("the repo's template has portraits");
    let root = Path::new("../..");
    let art = |name: &str| portrait::art_for(sticker, root, name).map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
    assert_eq!(art("Bành Anh").as_deref(), Some("Bành Anh.png"), "the map's own spelling");
    assert_eq!(art("BÀNH ANH").as_deref(), Some("Bành Anh.png"), "and a case difference is still them");
    assert_eq!(art("Người kể").as_deref(), Some("anonymous.png"), "nothing maps this name: the fallback");
    assert_eq!(art("Narrator"), None, "the narrator speaks without a portrait");
    assert_eq!(art("  người dẫn chuyện "), None, "as does the Vietnamese spelling of it");
    assert_eq!(art("   "), None, "and a cue with no speaker at all");
    let mut no_fallback = sticker.clone();
    no_fallback.fallback = None;
    assert_eq!(portrait::art_for(&no_fallback, root, "Người kể"), None);
}

#[test]
fn a_badge_is_the_thumbs_own_scale_and_squashes_on_the_syllable() {
    let t = template();
    let ratio = portrait::ratio(&t, Path::new("../.."), 1080);
    // The thumb is 52 px across on a 1080 frame, drawn from a 56 px image.
    assert!((ratio - 52.0 / 56.0).abs() < 1e-6, "{ratio}");
    let react = t.sticker().unwrap().react.clone();
    let (w, h) = portrait::size_pct(ratio, &react, 0.0);
    assert!((w - 92.857).abs() < 0.01 && (h - 92.857).abs() < 0.01, "at rest: {w} × {h}");
    let (w, h) = portrait::size_pct(ratio, &react, 1.0);
    assert!((w - 86.357).abs() < 0.01, "a syllable pulls it in: {w}");
    assert!((h - 103.071).abs() < 0.01, "and stretches it tall: {h}");
    // `bm-video` never scales the badge for a different frame; the badge keeps
    // its share of the frame instead, as the bar's thickness does.
    assert!((portrait::ratio(&t, Path::new("../.."), 2160) - 2.0 * 52.0 / 56.0).abs() < 1e-6);
}

#[test]
fn the_envelope_attacks_on_the_syllable_and_releases_after() {
    let react = look::React {
        scale_x: 0.07,
        scale_y: 0.11,
        release: 0.14,
        floor_db: Some(-90.0),
        ceil_db: Some(-20.0),
        ..Default::default()
    };
    // One loud audio frame at 0, silence at 0.1 s: three video frames at 30 fps.
    let report = "frame:0 pts:0 pts_time:0\nlavfi.astats.Overall.RMS_level=-10\n\
                  frame:3 pts:3 pts_time:0.1\nlavfi.astats.Overall.RMS_level=-120\n";
    let a = portrait::amounts(report, 30.0, 3, &react);
    assert!((a[0] - 1.0).abs() < 1e-6, "instant on the syllable: {a:?}");
    let decay = (-1.0f64 / (0.14 * 30.0)).exp();
    assert!((a[1] as f64 - decay).abs() < 1e-4, "then it settles: {a:?}");
    assert!((a[2] as f64 - decay * decay).abs() < 1e-4, "frame by frame: {a:?}");
}

#[test]
fn the_envelope_takes_its_floor_from_the_chapter_itself() {
    // Quiet then loud, in one chapter: the floor is its own 25th percentile, not
    // a number picked for another chapter.
    let react = look::React { release: 0.0, ..Default::default() };
    let report = "frame:0 pts_time:0\nlavfi.astats.Overall.RMS_level=-30\n\
                  frame:3 pts_time:0.1\nlavfi.astats.Overall.RMS_level=-10\n";
    let a = portrait::amounts(report, 30.0, 2, &react);
    assert!(a[0] < 1e-6, "the quiet frame is the floor: {a:?}");
    assert!((a[1] - 1.0).abs() < 1e-6, "and the loud one is the ceiling: {a:?}");
    assert_eq!(portrait::amounts("", 30.0, 3, &react), vec![0.0; 3], "no report is no reaction");
}

#[test]
fn only_the_frames_that_move_the_badge_are_keyed() {
    let keys = portrait::keys(&[0.0, 1.0, 1.0, 0.5, 0.25, 0.1, 0.0], 0.02);
    assert!(keys.contains(&(1, 1.0)), "the attack is kept exactly: {keys:?}");
    assert!(!keys.iter().any(|(f, _)| *f == 2), "a hold between two keys needs none");
    assert!(keys.iter().any(|(f, _)| *f == 6), "the last frame is the badge at rest again");
    assert!(keys.len() < 7, "so the script does not carry every frame: {keys:?}");
    assert_eq!(portrait::keys(&[], 0.02), Vec::new());
}

#[test]
fn a_books_speakers_place_a_badge_that_rides_its_own_cue() {
    let plan = plan::Plan::build(&two_acts(), &book("portraits"), 0.8, template(), (1920, 1080)).unwrap();
    // The fixture's cues name "Người kể", which the template does not map: the
    // anonymous fallback is the badge, and the art is in the repo.
    let portraits = plan.portraits(Path::new("../..")).unwrap();
    assert_eq!(
        portraits.files.len(),
        2,
        "one portrait for the whole book, and the thumb's own picture: {:?}",
        portraits.files
    );
    assert_eq!(portraits.thumb, Some(0), "the thumb's art leads the import list");
    assert!(
        (portraits.thumb_pct - 52.0 / 56.0 * 100.0).abs() < 0.01,
        "drawn at the template's own disc: {}",
        portraits.thumb_pct
    );
    assert_eq!(portraits.badges.iter().filter(|b| b.is_some()).count(), 4, "one per cue");
    assert_eq!(portraits.lanes, 1, "the cues butt up, so one track carries them");
    assert!(portraits.missing.is_empty(), "{:?}", portraits.missing);
    assert_eq!(portraits.at, [0.5, 0.6852], "the template's own spot");

    let steps = plan
        .steps(&[11, 12, 13], Path::new("/renders/book.srt"), &portraits, &[7, 8], &element::Elements::default(), &[])
        .unwrap();
    // A badge is a place on the portraits' track; the chapters go on A1 and the
    // thumb's own picture sits one track below.
    let mut badge_steps: Vec<usize> = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        if s.id == "timeline.place" && s.params["track"] == json!("V10") {
            badge_steps.push(i);
        }
    }
    let badges: Vec<&plan::Step> = badge_steps.iter().map(|i| &steps[*i]).collect();
    let thumb_place = steps
        .iter()
        .find(|s| s.id == "timeline.place" && s.params["track"] == json!("V9"))
        .expect("the thumb's picture is placed on its own track");
    assert_eq!(thumb_place.params["item"], json!(7), "the template's thumb art");
    assert_eq!(thumb_place.params["duration"], json!(plan::ticks(22.6)));
    assert_eq!(badges.len(), 4, "the chapters go on A1 and the badges on a video track");
    assert_eq!(badges[0].params["track"], json!("V10"), "above the thumb's own track");
    // The thumb's art leads the import list, so the portraits come after it.
    assert_eq!(badges[0].params["item"], json!(8), "the imported portrait");
    assert_eq!(badges[0].params["duration"], json!(plan::ticks(2.0)), "up for its own cue");
    assert_eq!(badges[0].params["time"], json!(0));
    assert_eq!(badges[1].params["time"], json!(plan::ticks(2.0)), "and the next one takes over");

    // Take the clip ids FilmCraft would report back, and animate from those.
    // This template has art for the thumb, so it is the placed still — the
    // ellipse is only what a template without art falls back to.
    let ellipse = steps
        .iter()
        .position(|s| s.params.get("shape").and_then(serde_json::Value::as_str) == Some("ellipse"));
    let thumb_step = steps
        .iter()
        .position(|s| s.params["track"] == json!("V9"))
        .unwrap();
    let replies: Vec<serde_json::Value> = steps
        .iter()
        .enumerate()
        .map(|(i, _)| match (ellipse == Some(i), i == thumb_step, badge_steps.contains(&i)) {
            (true, _, _) => json!({"ok": true, "result": {"clip": 1}}),
            (_, true, _) => json!({"ok": true, "result": {"clips": [2]}}),
            (_, _, true) => json!({"ok": true, "result": {"clips": [100 + i]}}),
            _ => json!({"ok": true, "result": null}),
        })
        .collect();
    let moving = plan.animate(&steps, &replies, &portraits, &element::Elements::default()).unwrap();
    // The thumb is a still clip, so its own movement lives on its motion layer.
    let thumb_moves: Vec<&plan::Step> =
        moving.iter().filter(|s| s.params["clip"] == json!(2)).collect();
    assert_eq!(thumb_moves.len(), 8, "position, rotation and the disc's size");
    assert_eq!(
        thumb_moves.iter().filter(|s| s.id == "effects.addKeyframe").count(),
        2,
        "one key each way, before its values are set"
    );
    assert!(
        thumb_moves.iter().all(|s| s.params["effect"] == json!("motion")),
        "a placed still carries its movement on `motion`"
    );
    assert!(
        thumb_moves.iter().any(|s| s.params["param"] == json!("rotation")
            && s.params["value"] == json!(-360.0)),
        "and it turns a whole revolution: {thumb_moves:?}"
    );
    assert!(
        thumb_moves.iter().any(|s| s.params["param"] == json!("scale")
            && s.params["value"] == json!(portraits.thumb_pct)),
        "drawn at the template's own disc, not fitted to a box"
    );
    let first = badge_steps[0];
    let clip = json!(100 + first);
    let theirs: Vec<&plan::Step> = moving.iter().filter(|s| s.params["clip"] == clip).collect();
    assert_eq!(
        theirs.iter().filter(|s| s.id == "effects.addKeyframe").count(),
        2,
        "one key each way for the two axes"
    );
    let param = |name: &str| {
        theirs
            .iter()
            .find(|s| s.id == "effects.setParam" && s.params["param"] == json!(name))
            .map(|s| s.params["value"].clone())
            .unwrap()
    };
    assert_eq!(param("uniform_scale"), json!(false), "so the two axes move apart");
    assert_eq!(param("position"), json!([960.0, 740.0]), "the template's spot on this frame");
    assert!(
        (param("scale").as_f64().unwrap() - 92.857).abs() < 0.01,
        "at rest with no measurement: {:?}",
        param("scale")
    );
}
