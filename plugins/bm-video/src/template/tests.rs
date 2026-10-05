use super::*;

#[test]
fn hex_colour_is_opaque() {
    assert_eq!(Rgba::parse("#9e3b2c").unwrap(), Rgba { r: 158, g: 59, b: 44, a: 255 });
}

#[test]
fn rgba_colour_scales_its_alpha() {
    assert_eq!(Rgba::parse("rgba(28,27,26,.12)").unwrap(), Rgba { r: 28, g: 27, b: 26, a: 31 });
    assert_eq!(Rgba::parse("rgb(250,247,240)").unwrap(), Rgba { r: 250, g: 247, b: 240, a: 255 });
}

#[test]
fn a_colour_it_cannot_read_is_an_error() {
    assert!(Rgba::parse("#fff").is_err());
    assert!(Rgba::parse("mauve").is_err());
}

#[test]
fn the_radial_gradient_reads_its_geometry_and_stops() {
    let g = RadialGradient::parse(
        "radial-gradient(140% 140% at 50% 0%, #ffffff 0%, #fafafa 55%, #f0f0f0 100%)",
    )
    .unwrap();
    assert_eq!((g.rx, g.ry, g.cx, g.cy), (1.4, 1.4, 0.5, 0.0));
    assert_eq!(g.stops.len(), 3);
    assert_eq!(g.stops[1], (0.55, Rgba { r: 250, g: 250, b: 250, a: 255 }));
}

#[test]
fn a_zone_aligned_to_a_later_one_still_resolves() {
    // `act_title` sorts before `illustration`, so the first pass cannot see its
    // alignment target: the fixup has to survive that and iterate.
    let t: Template = serde_json::from_str(
        r#"{"canvas":{"size":[1920,1080]},
            "zones":{
              "act_title":{"anchor":"left-center","at":[0.33,0.9],"size":[0.58,0.1],"align_top":"illustration"},
              "illustration":{"anchor":"top-left","at":[0.05,0.11],"size":[0.26,0.46],"aspect":1}
            }}"#,
    )
    .unwrap();
    let boxes = t.boxes(1920, 1080);
    assert_eq!(boxes["act_title"].top, boxes["illustration"].top);
    assert_ne!(boxes["act_title"].top, 0.9 * 1080.0 - 0.05 * 1080.0);
}

#[test]
fn an_omitted_optional_object_keeps_its_documented_default() {
    // The shipped template has no `timeline.segment` and no `subtitle.style`.
    // A derived `Default` would hand back 0.0 / 0 here, which silently drops the
    // active bar segment and uncaps the caption line count.
    let t: Template = serde_json::from_str(
        r#"{"canvas":{"size":[1920,1080]},"timeline":{"track":{"thickness":14}},
            "subtitle":{"min_s":1.0,"max_s":7.0}}"#,
    )
    .unwrap();
    assert_eq!(t.timeline.segment.active_y_scale, 1.4);
    assert_eq!(t.subtitle.style.max_lines, 2);
    assert_eq!(t.timeline.track.thickness, 14.0);
    assert_eq!(t.subtitle.fade_s, 0.3);
}

#[test]
fn no_layers_key_keeps_the_order_it_was_hardcoded_in() {
    // A template written before the composition was data must still render.
    let t: Template = serde_json::from_str(r#"{"canvas":{"size":[100,100]}}"#).unwrap();
    let kinds: Vec<&str> = t.layers.iter().map(|l| l.kind.as_str()).collect();
    assert_eq!(kinds, ["paper", "act_plate", "bar", "thumb", "captions", "portraits"]);
}

#[test]
fn a_composition_the_renderer_cannot_draw_is_refused_at_load() {
    let layers = |json: &str| -> Vec<Layer> { serde_json::from_str(json).unwrap() };
    // A typo in a layer kind would otherwise draw nothing, silently.
    let bad = layers(r#"[{"kind":"captionz"}]"#);
    assert!(check_layers(&bad).is_err());
    let no_asset = layers(r#"[{"kind":"sprite"}]"#);
    assert!(check_layers(&no_asset).is_err());
    let bad_motion = layers(r#"[{"kind":"sprite","asset":"a.png","motion":{"name":"float"}}]"#);
    assert!(check_layers(&bad_motion).is_err());
    let good = layers(
        r#"[{"kind":"sprite","zone":"illustration","asset":"a.png","motion":{"name":"travel","period_s":4}}]"#,
    );
    assert!(check_layers(&good).is_ok());
}

#[test]
fn a_zone_aligned_to_a_footer_sits_inside_it() {
    let t: Template = serde_json::from_str(
        r#"{"canvas":{"size":[100,100]},
            "zones":{
              "bar":{"anchor":"left-top","at":[0.0,0.0],"size":[1.0,0.1],"align_bottom":"footer"},
              "footer":{"anchor":"left-top","at":[0.0,0.8],"size":[1.0,0.2]}
            }}"#,
    )
    .unwrap();
    let boxes = t.boxes(100, 100);
    assert_eq!(boxes["bar"].top, boxes["footer"].top + boxes["footer"].height - boxes["bar"].height);
}
