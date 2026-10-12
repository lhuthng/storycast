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
fn a_badges_frames_are_the_ones_a_straight_line_cannot_hold() {
    let t = template();
    let ratio = portrait::ratio(&t, Path::new("../.."), 1080);
    let react = t.sticker().unwrap().react.clone();
    // A syllable, its release, and then a hold: the badge settles and stays put.
    let amounts: [f32; 9] = [0.0, 1.0, 0.6, 0.36, 0.216, 0.13, 0.05, 0.05, 0.05];
    let frames = portrait::badge_frames(&amounts, ratio, &react);
    assert!(frames.contains(&1), "the syllable moves it far more than the tolerance: {frames:?}");
    assert_eq!(frames.first(), Some(&0), "from the badge at rest");
    assert_eq!(frames.last(), Some(&(amounts.len() - 1)), "to the badge at rest again");
    assert!(frames.len() < amounts.len(), "so not every frame is keyed: {frames:?}");
    // A syllable and then nothing moving: the attack is keyed, the hold is not.
    let held: [f32; 5] = [0.0, 1.0, 1.0, 1.0, 1.0];
    assert_eq!(
        portrait::badge_frames(&held, ratio, &react),
        vec![0, 1, 4],
        "the frame it moves on, its end, and none of the hold between them"
    );
    assert_eq!(
        portrait::badge_frames(&[] as &[f32], ratio, &react),
        Vec::<usize>::new(),
        "nothing to key"
    );
    // What those keys draw: FilmCraft lines two of them up. The one rule holds
    // the badge's own size within its tolerance of the measured reaction.
    let height = |a: f32| portrait::size_pct(ratio, &react, a).1;
    let mut worst = 0.0f64;
    for f in 0..amounts.len() {
        let i = frames.iter().rposition(|k| *k <= f).unwrap();
        let drawn = match frames.get(i + 1) {
            None => height(amounts[frames[i]]),
            Some(next) => {
                let (a, b) = (frames[i], *next);
                let (va, vb) = (height(amounts[a]), height(amounts[b]));
                va + (vb - va) * ((f - a) as f64 / (b - a) as f64)
            }
        };
        worst = worst.max((drawn - height(amounts[f])).abs());
    }
    assert!(worst <= 0.51, "the line sits {worst:.3} percent off the reaction");
}

#[test]
fn a_measured_badge_jumps_on_the_syllable_and_rests_between() {
    let mut plan =
        plan::Plan::build(&two_acts(), &book("badge-env"), 0.8, template(), (1920, 1080)).unwrap();
    let portraits = plan.portraits(Path::new("../..")).unwrap();
    // `listen` measures this with ffmpeg, one pass a chapter; a test can hand
    // the numbers over instead: a syllable on frames 10..20, silence either side.
    let env: Vec<f32> = (0..=300).map(|f| if (10..20).contains(&f) { 1.0 } else { 0.0 }).collect();
    for span in plan.spans.iter_mut() {
        span.env = env.clone();
    }
    let steps = plan
        .steps(&[11, 12, 13], Path::new("/renders/book.srt"), &portraits, &[7, 8], &element::Elements::default(), &[])
        .unwrap();
    let tracks: Vec<String> =
        (0..portraits.lanes).map(|l| format!("V{}", plan.portrait_track(l) + 1)).collect();
    let badges: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.id == "timeline.place"
                && s.params["track"]
                    .as_str()
                    .map(|t| tracks.iter().any(|n| n == t))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();
    // Every clip answers with the id FilmCraft would report for it, the thumb's
    // placed still and its fallback disc included, so the pass can find them all.
    let replies: Vec<serde_json::Value> = (0..steps.len())
        .map(|i| json!({"ok": true, "result": {"clip": 100 + i, "clips": [100 + i]}}))
        .collect();
    let moving = plan.animate(&steps, &replies, &portraits, &element::Elements::default()).unwrap();
    let theirs: Vec<&plan::Step> =
        moving.iter().filter(|s| s.params["clip"] == json!(100 + badges[0])).collect();
    // The chapter starts at 0 and runs at the template's frame rate, so a key's
    // time over one frame is the frame it is on.
    let per = (filmcraft::TICKS_PER_SECOND / plan.fps).round() as i64;
    let at = |param: &str| -> Vec<(i64, f64)> {
        theirs
            .iter()
            .filter(|s| s.id == "effects.setParam" && s.params["param"] == json!(param))
            .map(|s| {
                (
                    s.params["time"].as_i64().unwrap() / per,
                    s.params["value"].as_f64().unwrap(),
                )
            })
            .collect()
    };
    let (height, width) = (at("scale"), at("scale_width"));
    assert!(
        height.len() == width.len() && height.iter().zip(&width).all(|(a, b)| a.0 == b.0),
        "the two axes are keyed together: {height:?} {width:?}"
    );
    assert_eq!(height.first().map(|(f, _)| *f), Some(0), "from the cue's own first frame");
    assert!(
        (height.first().unwrap().1 - 92.857).abs() < 0.01,
        "with the badge at rest: {height:?}"
    );
    assert!(
        height.iter().any(|(f, _)| *f == 9),
        "the frame before the syllable is keyed, so the attack is not ramped: {height:?}"
    );
    assert!(
        height.iter().any(|(f, v)| *f == 10 && (v - 103.071).abs() < 0.01),
        "and its peak: {height:?}"
    );
    assert!(
        (width.iter().find(|(f, _)| *f == 10).unwrap().1 - 86.357).abs() < 0.01,
        "squashed on the syllable: {width:?}"
    );
    assert!(height.len() < 61, "and far short of a key a frame: {}", height.len());
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
    assert_eq!(thumb_moves.len(), 7, "position, rotation and the disc's size");
    assert_eq!(
        thumb_moves.iter().filter(|s| s.id == "effects.addKeyframe").count(),
        2,
        "one key each way, before its values are set"
    );
    assert!(
        thumb_moves.iter().all(|s| s.params["effect"] == json!("motion")),
        "a placed still carries its movement on `motion`"
    );
    // A constant rate is a straight line, so its ends are all it needs: two
    // keys, the second one a whole revolution per `spin_s` after the first.
    let thumb_cfg = template().timeline.thumb.unwrap();
    let sign = if thumb_cfg.direction.eq_ignore_ascii_case("cw") { 1.0 } else { -1.0 };
    let turns: Vec<(f64, f64)> = thumb_moves
        .iter()
        .filter(|s| s.id == "effects.setParam" && s.params["param"] == json!("rotation"))
        .map(|s| {
            (
                s.params["time"].as_i64().unwrap() as f64 / filmcraft::TICKS_PER_SECOND,
                s.params["value"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(turns.len(), 2, "the ends of one constant turn: {turns:?}");
    assert_eq!(turns[0], (0.0, 0.0));
    assert!(
        (turns[1].1 - sign * 360.0 * turns[1].0 / thumb_cfg.spin_s).abs() < 1e-6,
        "and it turns a whole revolution every {}s: {turns:?}",
        thumb_cfg.spin_s
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
