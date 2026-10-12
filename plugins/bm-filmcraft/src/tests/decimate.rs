use super::*;
use serde_json::json;
use std::path::Path;
use std::time::Instant;

/// The frames the definition picks, by the scan the fast path replaces: the
/// first chord that leaves the curve, keyed at the frame before it.
fn scanned(v: impl Fn(usize) -> f64, frames: usize, tol: f64) -> Vec<usize> {
    let leaves = |a: usize, b: usize| {
        let (va, vb) = (v(a), v(b));
        let span = (b - a) as f64;
        (a + 1..b).any(|j| (v(j) - (va + (vb - va) * ((j - a) as f64 / span))).abs() > tol)
    };
    let mut out = vec![0usize];
    let (mut last, mut i) = (0usize, 1usize);
    while i < frames {
        if leaves(last, i) {
            let keep = if i - 1 > last { i - 1 } else { i };
            out.push(keep);
            last = keep;
        }
        i += 1;
    }
    if out.last() != Some(&(frames - 1)) {
        out.push(frames - 1);
    }
    out
}

/// One sprite of `motion`, over a book `total` seconds long.
fn sprite_of(name: &str, motion: serde_json::Value, total: f64) -> element::Elements {
    let (_dir, template) = sprite_template(name, motion);
    let look = look::Look::load(&template).unwrap();
    element::Elements::build(&look, Path::new("../.."), 1920, 1080, total).unwrap()
}

/// The unit each property is drawn in, as `Writer::channel` passes it.
fn unit_of(el: &element::Element, prop: &str) -> f64 {
    match prop {
        "x" => el.zone_w,
        "y" => el.zone_h,
        "scale" => el.scale_pct,
        "opacity" => 100.0,
        _ => 1.0,
    }
}

/// Every shape a template writes: one-shots, a preset, a repeat, a fade in and
/// out with a hold between them, and two tracks on one property.
fn motions() -> Vec<serde_json::Value> {
    vec![
        json!({"period_s": 2.0, "tracks": [
            {"prop": "y", "from": -0.05, "to": 0.05, "dur_s": 1.0, "ease": "in-out-sin", "repeat": 0, "pingpong": true}]}),
        json!({"name": "bob", "period_s": 2.0}),
        json!({"name": "travel"}),
        json!({"name": "spin", "period_s": 1.2}),
        json!({"tracks": [{"prop": "x", "from": 0.0, "to": 0.2, "dur_s": 3.0, "ease": "in-out"}]}),
        json!({"tracks": [{"prop": "x", "from": 0.0, "to": 0.2, "dur_s": 3.0, "ease": "linear"}]}),
        json!({"tracks": [{"prop": "rotate", "from": -8.0, "to": 8.0, "dur_s": 2.0, "ease": "in-cubic"}]}),
        json!({"period_s": 1.0, "tracks": [
            {"prop": "rotate", "from": 0.0, "to": -360.0, "dur_s": 0.5, "ease": "linear", "repeat": 0}]}),
        json!({"tracks": [
            {"prop": "opacity", "from": 0, "to": 1, "dur_s": 0.5, "ease": "out"},
            {"prop": "opacity", "from": 1, "to": 0, "dur_s": 0.5, "ease": "in", "align": "end"}]}),
        json!({"period_s": 3.0, "tracks": [
            {"prop": "scale", "from": 0.8, "to": 1.1, "dur_s": 1.5, "ease": "in-out-sin", "repeat": 0, "pingpong": true},
            {"prop": "x", "from": -0.02, "to": 0.02, "dur_s": 0.7, "ease": "in-out-sin", "repeat": 0, "pingpong": true}]}),
        json!({"tracks": [{"prop": "y", "from": 0.0, "to": 0.0}]}),
        json!({"period_s": 2.0, "tracks": [{"prop": "y", "from": -0.05, "to": 0.05, "dur_s": 1.0, "ease": "out-cubic", "repeat": 0, "pingpong": true}]}),
    ]
}

#[test]
fn the_slopes_pick_the_frames_the_scan_picked() {
    // What the keying rule is allowed to change is the cost of the decimation,
    // not the frames it keeps — so it is held to the scan itself, over every
    // shape a template writes and at two lengths.
    let fps = 30.0f64;
    let mut kept = 0;
    for total in [22.6f64, 60.0] {
        let frames = (total * fps).round() as usize;
        for (n, motion) in motions().into_iter().enumerate() {
            let sprites = sprite_of(&format!("decimate-{n}-{}", total as u32), motion, total);
            for el in &sprites.list {
                for tr in &el.tracks {
                    let unit = unit_of(el, &tr.prop);
                    let curve = |f: usize| {
                        motion::value(&el.tracks, &tr.prop, f as f64 / fps, total, el.period) * unit
                    };
                    let fast = motion::decimate(curve, frames);
                    assert_eq!(
                        fast,
                        scanned(curve, frames, motion::TOL),
                        "the {} track of motion {n} over {total} s",
                        tr.prop
                    );
                    kept += fast.len();
                }
            }
        }
    }
    // The comparison says nothing if the curves were never bent enough to key:
    // these shapes keep thousands of keys between them.
    assert!(kept > 1000, "only {kept} keys were compared");
}

#[test]
fn the_frames_are_kept_where_the_curve_is_not_straight() {
    // A property no track moves is one value, not a key a frame; a linear one
    // needs its ends alone, however long the book is.
    let fps = 30.0f64;
    let frames = (120.0 * fps).round() as usize;
    let flat = |_: usize| 0.0;
    assert_eq!(motion::decimate(flat, frames), vec![0, frames - 1]);
    let sweep = |f: usize| f as f64 / frames as f64;
    assert_eq!(motion::decimate(sweep, frames), vec![0, frames - 1]);
}

/// A real book's length, by hand, because the scan it replaces took minutes
/// there: `cargo test --release a_books_motion -- --ignored --nocapture`.
#[test]
#[ignore]
fn a_books_motion_is_keyed_at_book_length() {
    // This repo's book: 30 chapters, 9245.6 s of cue sheet, 277_368 frames.
    let (fps, total) = (30.0f64, 9245.6f64);
    let cases = [
        ("a hold after a fade", json!({"tracks": [
            {"prop": "opacity", "from": 0, "to": 1, "dur_s": 0.5, "ease": "out"},
            {"prop": "opacity", "from": 1, "to": 0, "dur_s": 0.5, "ease": "in", "align": "end"}]})),
        ("a linear sweep", json!({"tracks": [{"prop": "x", "from": 0.0, "to": 0.2, "dur_s": total, "ease": "linear"}]})),
        ("a sine bob", json!({"period_s": 2.0, "tracks": [
            {"prop": "y", "from": -0.05, "to": 0.05, "dur_s": 1.0, "ease": "in-out-sin", "repeat": 0, "pingpong": true}]})),
    ];
    for (n, (name, motion)) in cases.into_iter().enumerate() {
        let sprites = sprite_of(&format!("decimate-book-{n}"), motion, total);
        let steps = sprites.place(&[7], 0, total).unwrap();
        let replies: Vec<serde_json::Value> = steps
            .iter()
            .map(|_| json!({"ok": true, "result": {"clips": [42]}}))
            .collect();
        let t = Instant::now();
        let moving = sprites.keys(&steps, &replies, 0, fps, total).unwrap();
        let secs = t.elapsed().as_secs_f64();
        println!("  {name}: {} steps in {:.0} ms", moving.len(), secs * 1000.0);
        assert!(secs < 30.0, "{name} took {secs:.1} s");
    }
}
