use super::*;
use serde_json::json;
use std::path::Path;

/// A plan over its own book, the template's sprites, and the steps FilmCraft
/// would get for them.
fn sprite_steps(
    template: &Path,
    name: &str,
) -> (plan::Plan, element::Elements, Vec<plan::Step>, Vec<serde_json::Value>) {
    let look = look::Look::load(template).unwrap();
    let plan = plan::Plan::build(&two_acts(), &book(name), 0.8, look, (1920, 1080)).unwrap();
    let total = plan.total;
    let sprites = element::Elements::build(&plan.look, Path::new("../.."), 1920, 1080, total).unwrap();
    let steps = sprites
        .place(&[7], plan.sprite_track(0, 0), total)
        .unwrap();
    let replies: Vec<serde_json::Value> = steps
        .iter()
        .map(|_| json!({"ok": true, "result": {"clips": [42]}}))
        .collect();
    (plan, sprites, steps, replies)
}

#[test]
fn a_sprite_is_placed_in_its_zone_and_sized_to_fit_it() {
    let (_dir, template) = sprite_template("sprite-placed", json!({"period_s": 2.0}));
    let look = look::Look::load(&template).unwrap();
    let sprites = element::Elements::build(&look, Path::new("../.."), 1920, 1080, 10.0).unwrap();
    let zone = look.box_of("illustration", 1920, 1080).unwrap();
    assert_eq!(sprites.files.len(), 1, "the art is imported once");
    assert!(sprites.missing.is_empty());
    let el = &sprites.list[0];
    assert_eq!((el.cx, el.cy), (zone.x + zone.w / 2.0, zone.y + zone.h / 2.0));
    // In the illustration zone the height is what binds, so the picture is
    // drawn at the zone's height, in its own pixels — as `bm-video` fits it.
    let natural = portrait::png_size(&sprites.files[0]).unwrap();
    let fit = (zone.w / natural.0 as f64).min(zone.h / natural.1 as f64);
    assert!((el.scale_pct - fit * 100.0).abs() < 0.01, "{}", el.scale_pct);
    let (plan, _, steps, _) = sprite_steps(&template, "sprite-placed-book");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].id, "timeline.place");
    assert_eq!(steps[0].params["item"], json!(7));
    assert_eq!(steps[0].params["track"], json!("V10"), "above the look's own last track");
    assert_eq!(steps[0].params["duration"], json!(plan::ticks(plan.total)));
    assert_eq!(plan.sprite_track(1, 0), plan.sprite_track(0, 0) + 1, "a badge lane moves it up");
}

#[test]
fn a_bobbing_sprite_is_keyed_as_the_sine_it_asks_for() {
    let (_dir, template) = sprite_template(
        "sprite-bob",
        json!({"period_s": 2.0, "tracks": [
            {"prop": "y", "from": -0.04, "to": 0.04, "dur_s": 1.0, "ease": "in-out-sin", "repeat": 0, "pingpong": true}
        ]}),
    );
    let (plan, sprites, steps, replies) = sprite_steps(&template, "sprite-bob-book");
    let moving = sprites
        .keys(&steps, &replies, plan.sprite_track(0, 0), 30.0, plan.total)
        .unwrap();
    let el = &sprites.list[0];
    assert_eq!(el.tracks.len(), 1);
    // Position is one property, so every key carries both axes: y on the curve,
    // x where the zone holds it.
    let placed: Vec<&plan::Step> = moving
        .iter()
        .filter(|s| s.id == "effects.setParam" && s.params["param"] == json!("position"))
        .collect();
    assert!(placed.len() > 10, "a book of 2s bobs is keyed many times: {}", placed.len());
    assert!(placed.len() < 300, "but not once a frame, which 22.6s would need 679 of: {}", placed.len());
    assert_eq!(moving[0].id, "effects.addKeyframe", "the property is animated before it is set");
    assert_eq!(moving[1].params["param"], json!("position"));
    for s in &placed {
        let t = s.params["time"].as_i64().unwrap() as f64 / filmcraft::TICKS_PER_SECOND;
        let want = el.cy - 0.04 * (std::f64::consts::PI * t).cos() * el.zone_h;
        let x = s.params["value"][0].as_f64().unwrap();
        let y = s.params["value"][1].as_f64().unwrap();
        assert!((y - want).abs() < 0.01, "at {t}s: {y} vs {want}");
        assert_eq!(x, el.cx, "no track moves it sideways");
    }
    // What no track moves is set once, and only once: the size it was fitted at.
    let scale: Vec<&plan::Step> = moving
        .iter()
        .filter(|s| s.params["param"] == json!("scale"))
        .collect();
    assert_eq!(scale.len(), 1, "{scale:?}");
    assert_eq!(scale[0].params["value"], json!(el.scale_pct));
    assert_eq!(scale[0].params["effect"], json!("motion"));
    let opacity: Vec<&plan::Step> = moving
        .iter()
        .filter(|s| s.params["effect"] == json!("opacity"))
        .collect();
    assert_eq!(opacity.len(), 1, "nothing fades it: {opacity:?}");
    assert_eq!(opacity[0].params["value"], json!(100.0));
}

#[test]
fn a_fade_in_and_a_fade_out_are_two_tracks_of_one_property() {
    let (_dir, template) = sprite_template(
        "sprite-fade",
        json!({"tracks": [
            {"prop": "opacity", "from": 0, "to": 1, "dur_s": 0.5, "ease": "out"},
            {"prop": "opacity", "from": 1, "to": 0, "dur_s": 0.5, "ease": "in", "align": "end"}
        ]}),
    );
    let (plan, sprites, steps, replies) = sprite_steps(&template, "sprite-fade-book");
    let moving = sprites
        .keys(&steps, &replies, plan.sprite_track(0, 0), 30.0, plan.total)
        .unwrap();
    let keys: Vec<(f64, f64)> = moving
        .iter()
        .filter(|s| s.params["effect"] == json!("opacity") && s.id == "effects.setParam")
        .map(|s| {
            (
                s.params["time"].as_i64().unwrap() as f64 / filmcraft::TICKS_PER_SECOND,
                s.params["value"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(keys.first().unwrap().1, 0.0, "it starts transparent: {keys:?}");
    // The book's last frame starts before the fade out has run its half second,
    // so the last thing drawn is nearly, not quite, gone.
    assert!(keys.last().unwrap().1 < 15.0, "and ends nearly transparent: {:?}", keys.last());
    assert!(
        keys.last().unwrap().0 > plan.total - 0.05,
        "the fade out takes the last half second: {:?}",
        keys.last()
    );
    assert!(
        keys.iter().any(|(t, v)| *t <= 0.5 && *v > 90.0),
        "and it is opaque by the end of the fade in: {keys:?}"
    );
    assert!(keys.len() < 40, "decimated, not a key a frame: {}", keys.len());
    // A one-shot move is held where it finished, at no cost in keys.
    let (_dir, template) = sprite_template(
        "sprite-one-shot",
        json!({"tracks": [{"prop": "x", "from": 0, "to": 0.2, "dur_s": 3.0, "ease": "in-out"}]}),
    );
    let (plan, sprites, steps, replies) = sprite_steps(&template, "sprite-one-shot-book");
    let moving = sprites
        .keys(&steps, &replies, plan.sprite_track(0, 0), 30.0, plan.total)
        .unwrap();
    let placed: Vec<&plan::Step> = moving
        .iter()
        .filter(|s| s.id == "effects.setParam" && s.params["param"] == json!("position"))
        .collect();
    assert!(placed.len() > 4, "a three second eased move is keyed along it: {}", placed.len());
    assert!(
        placed.len() < 120,
        "but far short of the 679 frames it is held across: {}",
        placed.len()
    );
    let last = placed.last().unwrap();
    let el = &sprites.list[0];
    let want = el.cx + 0.2 * el.zone_w;
    let got = last.params["value"][0].as_f64().unwrap();
    assert!((got - want).abs() < 0.001, "{got} vs {want}");
    let last_frame = (plan.total * 30.0).round() - 1.0;
    assert_eq!(
        last.params["time"],
        json!(plan::frame_ticks(last_frame / 30.0, 30.0)),
        "held to the last frame inside the clip, not the instant it stops"
    );
}

#[test]
fn between_two_keys_the_straight_line_holds_the_curve() {
    // FilmCraft tweens two keys in a straight line, so the keys are only right if
    // the line between each pair stays inside the decimation's tolerance. The
    // other sprite tests check the keys themselves; this checks the drawing.
    let (_dir, template) = sprite_template(
        "sprite-tween",
        json!({"period_s": 2.0, "tracks": [
            {"prop": "x", "from": 0.0, "to": 0.08, "dur_s": 3.0, "ease": "in-out"},
            {"prop": "y", "from": -0.05, "to": 0.05, "dur_s": 1.0, "ease": "in-out-sin", "repeat": 0, "pingpong": true},
            {"prop": "rotate", "from": -8.0, "to": 8.0, "dur_s": 1.0, "ease": "in-out-sin", "repeat": 0, "pingpong": true},
            {"prop": "opacity", "from": 0, "to": 1, "dur_s": 0.5, "ease": "out"},
            {"prop": "opacity", "from": 1, "to": 0, "dur_s": 0.5, "ease": "in", "align": "end"}]}),
    );
    let (plan, sprites, steps, replies) = sprite_steps(&template, "sprite-tween-book");
    let (fps, total) = (30.0, plan.total);
    let moving = sprites
        .keys(&steps, &replies, plan.sprite_track(0, 0), fps, total)
        .unwrap();
    let el = &sprites.list[0];
    let ticks = |s: &plan::Step| s.params["time"].as_i64().unwrap() as f64 / filmcraft::TICKS_PER_SECOND;
    let keys = |param: &str| -> Vec<(f64, Vec<f64>)> {
        moving
            .iter()
            .filter(|s| s.id == "effects.setParam" && s.params["param"] == json!(param))
            .map(|s| {
                let v = &s.params["value"];
                let nums = match v.as_array() {
                    Some(a) => a.iter().map(|x| x.as_f64().unwrap()).collect(),
                    None => vec![v.as_f64().unwrap()],
                };
                (ticks(s), nums)
            })
            .collect()
    };
    // The line FilmCraft draws, at an instant: the segment it is in, held before
    // the first key and after the last.
    let drawn = |ks: &[(f64, Vec<f64>)], t: f64, axis: usize| -> f64 {
        match ks.iter().rposition(|(kt, _)| *kt <= t + 1e-9) {
            None => ks[0].1[axis],
            Some(i) if i + 1 >= ks.len() => ks[i].1[axis],
            Some(i) => {
                let (t0, v0) = (ks[i].0, ks[i].1[axis]);
                let (t1, v1) = (ks[i + 1].0, ks[i + 1].1[axis]);
                if (t1 - t0).abs() < 1e-12 {
                    v1
                } else {
                    v0 + (v1 - v0) * ((t - t0) / (t1 - t0))
                }
            }
        }
    };
    let frames = (total * fps).round() as usize;
    let mut worst = [0.0f64; 3];
    for f in 0..frames {
        let t = f as f64 / fps;
        let pos = keys("position");
        let (dx, dy) = (
            (drawn(&pos, t, 0) - (el.cx + motion::value(&el.tracks, "x", t, total, el.period) * el.zone_w)).abs(),
            (drawn(&pos, t, 1) - (el.cy + motion::value(&el.tracks, "y", t, total, el.period) * el.zone_h)).abs(),
        );
        worst[0] = worst[0].max(dx);
        worst[1] = worst[1].max(dy);
        let rot = keys("rotation");
        let dr = (drawn(&rot, t, 0) - motion::value(&el.tracks, "rotate", t, total, el.period)).abs();
        worst[2] = worst[2].max(dr);
    }
    println!(
        "      worst drawn-vs-curve: x {:.3} px, y {:.3} px, rotate {:.3}° over {frames} frames",
        worst[0], worst[1], worst[2]
    );
    // Half a pixel and half a degree is the tolerance the keys are picked to,
    // and the line between two of them is the segment the chord was tested
    // against — so it sits just inside it.
    assert!(worst[0] <= 0.51, "x wanders {:.3} px from the curve", worst[0]);
    assert!(worst[1] <= 0.51, "y wanders {:.3} px from the curve", worst[1]);
    assert!(worst[2] <= 0.51, "rotate wanders {:.3}°", worst[2]);
}

#[test]
fn a_sprite_the_template_gets_wrong_says_so() {
    let dir = workspace("sprite-errors");
    let write = |name: &str, layer: serde_json::Value| {
        let path = dir.join(name);
        let doc = json!({
            "zones": {"illustration": {"anchor": "top-left", "at": [0.05, 0.11], "size": [0.26, 0.46]}},
            "layers": [layer],
        });
        std::fs::write(&path, doc.to_string()).unwrap();
        look::Look::load(&path).unwrap()
    };
    let build = |look: &look::Look| element::Elements::build(look, Path::new("../.."), 1920, 1080, 10.0);
    let err = build(&write("no-asset.json", json!({"kind": "sprite"})))
        .unwrap_err()
        .to_string();
    assert!(err.contains("names no asset"), "{err}");
    let err = build(&write(
        "no-zone.json",
        json!({"kind": "sprite", "zone": "nowhere", "asset": "tmp/thumb-single.png"}),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("nowhere"), "{err}");
    let err = build(&write(
        "bad-prop.json",
        json!({"kind": "sprite", "asset": "tmp/thumb-single.png", "motion": {"tracks": [{"prop": "wobble"}]}}),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("wobble"), "{err}");
    let err = build(&write(
        "bad-ease.json",
        json!({"kind": "sprite", "asset": "tmp/thumb-single.png", "motion": {"tracks": [{"prop": "y", "ease": "ease-in-out"}]}}),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("ease-in-out"), "{err}");
    // Art the template names but the disk has not got is a warning, not a stop.
    let missing = build(&write(
        "no-art.json",
        json!({"kind": "sprite", "zone": "illustration", "asset": "tmp/nowhere.png"}),
    ))
    .unwrap();
    assert!(missing.list.is_empty(), "nothing to place");
    assert_eq!(missing.missing.len(), 1);
}

#[test]
fn the_presets_are_read_the_way_bm_video_reads_them() {
    let (_, template) = sprite_template("sprite-presets", json!({"name": "bob", "period_s": 2.0}));
    let look = look::Look::load(&template).unwrap();
    let sprites = element::Elements::build(&look, Path::new("../.."), 1920, 1080, 10.0).unwrap();
    let el = &sprites.list[0];
    assert_eq!(el.tracks.len(), 1);
    assert_eq!(el.tracks[0].prop, "y");
    assert!(el.tracks[0].pingpong, "a bob turns around, it does not restart");
    assert_eq!((el.tracks[0].from, el.tracks[0].to), (-0.05, 0.05), "the default amplitude");
    let at = |t: f64| motion::value(&el.tracks, "y", t, 10.0, el.period);
    assert!((at(0.0) + 0.05).abs() < 1e-9);
    assert!((at(1.0) - 0.05).abs() < 1e-9);
    assert!(at(0.5).abs() < 1e-9, "a sine, through zero");
}
