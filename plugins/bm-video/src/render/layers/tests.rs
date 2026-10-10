use super::*;

/// A sprite 20x10 px resting at the centre of a 200x100 zone.
fn sprite(tracks: Vec<MotionTrack>) -> Sprite {
    Sprite {
        digest: [0; 32],
        frames: vec![Arc::new(RgbaImage::new(20, 10))],
        cx: 100.0,
        cy: 200.0,
        zone_w: 200.0,
        zone_h: 100.0,
        tracks,
        period: 2.0,
        total: 60.0,
    }
}

fn track(prop: &str, from: f64, to: f64, dur: f64, ease: &str) -> MotionTrack {
    MotionTrack {
        prop: prop.into(),
        from,
        to,
        dur_s: Some(dur),
        ease: ease.into(),
        repeat: 1,
        pingpong: false,
        align: "start".into(),
    }
}

#[test]
fn a_sprite_with_no_tracks_holds_still() {
    let s = sprite(Vec::new());
    for f in [0, 17, 1799] {
        assert_eq!(s.at(f, 30), (90, 195, 0, 1.0));
    }
}

#[test]
fn a_looping_track_returns_to_itself_one_period_later() {
    // The property the whole design leans on: the pose depends on the time
    // alone, so a chunk composes the same in any order, on any box.
    let mut t = track("y", 0.0, 1.0, 2.0, "linear");
    t.repeat = 0;
    let s = sprite(vec![t]);
    assert_eq!(s.value("y", 0.0), 0.0);
    assert!((s.value("y", 0.5) - 0.25).abs() < 1e-9);
    assert!((s.value("y", 14.5) - 0.25).abs() < 1e-9);
}

#[test]
fn an_in_out_sin_oscillation_is_a_sine() {
    let mut t = track("y", -1.0, 1.0, 1.0, "in-out-sin");
    t.repeat = 0;
    t.pingpong = true;
    let s = sprite(vec![t]);
    for f in 0..40 {
        let time = f as f64 / 10.0;
        let sine = -(std::f64::consts::TAU * time / 2.0).cos();
        assert!(
            (s.value("y", time) - sine).abs() < 1e-9,
            "at {time}s: {} vs {sine}",
            s.value("y", time)
        );
    }
}

#[test]
fn a_fade_in_and_a_fade_out_are_two_segments() {
    let fade_in = track("opacity", 0.0, 1.0, 0.5, "out");
    let mut fade_out = track("opacity", 1.0, 0.0, 0.5, "in");
    fade_out.align = "end".into();
    let s = sprite(vec![fade_in, fade_out]);
    assert_eq!(s.value("opacity", 0.0), 0.0);
    assert!((s.value("opacity", 0.25) - 0.75).abs() < 1e-9, "eased, not halved");
    assert_eq!(s.value("opacity", 1.0), 1.0, "held in between");
    assert_eq!(s.value("opacity", 59.5), 1.0, "the fade out starts at the end, less its dur");
    assert!((s.value("opacity", 59.75) - 0.75).abs() < 1e-9);
    assert_eq!(s.value("opacity", 60.0), 0.0);
}

#[test]
fn an_ease_is_a_pace_between_two_values() {
    assert_eq!(ease("linear", 0.25), 0.25);
    assert_eq!(ease("in-out-cubic", 0.5), 0.5);
    assert_eq!(ease("in-out-squared", 0.25), ease("in-out-quad", 0.25));
    assert_eq!(ease("out", 0.0), 0.0);
    assert_eq!(ease("out", 1.0), 1.0);
    assert_eq!(ease("in-out-sin", 0.0), 0.0);
    assert_eq!(ease("in-out-sin", 1.0), 1.0);
}

#[test]
fn the_presets_are_the_tracks_they_always_were() {
    let zone = Rect { left: 1000.0, top: 800.0, width: 200.0, height: 100.0 };
    let bob = preset_tracks(&Motion { name: "bob".into(), period_s: Some(2.0), ..Default::default() }, zone, 0.0, 2.0).unwrap();
    let s = sprite(bob.clone());
    assert!(bob[0].pingpong, "a bob turns around, it does not restart");
    for f in 0..80 {
        assert!(s.value("y", f as f64 / 40.0).abs() <= 0.05 + 1e-9);
    }
    let travel = preset_tracks(&Motion { name: "travel".into(), period_s: Some(2.0), ..Default::default() }, zone, 80.0, 2.0).unwrap();
    assert_eq!(travel[0].prop, "x");
    assert!((travel[0].to - 0.6).abs() < 1e-9, "it crosses the zone, less its own width");
    let spin = preset_tracks(&Motion { name: "spin".into(), period_s: Some(2.0), ..Default::default() }, zone, 0.0, 2.0).unwrap();
    assert_eq!((spin[0].prop.as_str(), spin[0].to), ("rotate", -360.0), "ccw");
    assert!(preset_tracks(&Motion { name: "wobble".into(), ..Default::default() }, zone, 0.0, 2.0).is_err());
}

#[test]
fn a_track_that_does_not_come_back_after_a_period_is_refused() {
    let art = RgbaImage::new(10, 10);
    let one_shot = vec![track("rotate", 0.0, 90.0, 2.0, "linear")];
    let err = poses(&one_shot, &art, 30, 2.0, 60.0).unwrap_err().to_string();
    assert!(err.contains("period_s"), "{err}");
    let repeating = vec![{
        let mut t = track("rotate", 0.0, 90.0, 1.0, "linear");
        t.repeat = 0;
        t
    }];
    assert_eq!(poses(&repeating, &art, 30, 2.0, 60.0).unwrap().len(), 60);
}

#[test]
fn a_track_the_renderer_cannot_draw_is_refused_rather_than_ignored() {
    assert!(check_tracks(&[track("wobble", 0.0, 1.0, 1.0, "linear")]).is_err());
    assert!(check_tracks(&[track("y", 0.0, 1.0, 1.0, "ease-in-out")]).is_err());
    assert!(check_tracks(&[track("opacity", 0.0, 1.0, 1.0, "in-out-squared")]).is_ok());
}
