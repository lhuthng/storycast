use super::*;

fn held() -> Sprite {
    Sprite {
        digest: [0; 32],
        frames: vec![Arc::new(RgbaImage::new(4, 4))],
        x0: 100.0,
        y0: 200.0,
        travel: 0.0,
        bob: 0.0,
        period: 2.0,
    }
}

#[test]
fn a_loop_closes_on_its_period() {
    // The property the whole design leans on: the pose depends on the frame
    // index alone, and returns to itself one period later.
    assert_eq!(phase(0, 30, 2.0), 0.0);
    assert!((phase(15, 30, 2.0) - 0.25).abs() < 1e-9);
    assert!((phase(15, 30, 2.0) - phase(75, 30, 2.0)).abs() < 1e-9);
}

#[test]
fn a_sprite_that_travels_returns_to_where_it_started() {
    let mut s = held();
    s.travel = 120.0;
    assert_eq!(s.at(0, 30).0, 100);
    assert_eq!(s.at(30, 30).0, 160); // half a 2s period
    assert_eq!(s.at(60, 30).0, 100); // a whole one
}

#[test]
fn a_sprite_that_bobs_or_holds_still_never_leaves_its_zone() {
    let s = held();
    for f in [0, 17, 44, 61] {
        assert_eq!(s.at(f, 30), (100, 200, 0));
    }
    let mut b = held();
    b.bob = 10.0;
    // A sine: neither overshoots the amplitude nor sits still.
    let ys: Vec<i32> = [0, 8, 15, 23, 30].iter().map(|f| b.at(*f, 30).1).collect();
    assert!(ys.iter().all(|y| (190..=210).contains(y)));
    assert!(ys.iter().any(|y| *y != 200));
}
