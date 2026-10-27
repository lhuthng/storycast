use super::*;
use crate::digest::quotes::keep_repaired;

fn fixture(tag: &str) -> (PathBuf, Layout) {
    let dir = std::env::temp_dir().join(format!("bm-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.chapters()).unwrap();
    (dir, layout)
}

#[test]
fn a_failed_later_step_keeps_the_proofread_it_already_paid_for() {
    let (_dir, layout) = fixture("resume-keeps");
    let original = "Trời hôm nay đẹp. Hắn nói: “Đi thôi.”";
    std::fs::write(layout.chapter_txt(51), original).unwrap();
    keep_repaired(&layout, 51, original, original, &json!([]));

    assert!(
        kept(&layout, 51, original).is_none(),
        "no failure marker means a new digest, which proofreads"
    );

    keep(&layout, 51, original);
    assert_eq!(
        kept(&layout, 51, original),
        Some((original.to_string(), "proofread".into()))
    );

    stop(&layout, 51, original, "sound");
    assert_eq!(
        kept(&layout, 51, original),
        Some((original.to_string(), "sound".into())),
        "the retry reads the step it stopped at"
    );

    clear(&layout, 51);
    assert!(kept(&layout, 51, original).is_none());
}

#[test]
fn a_new_source_or_a_faulted_sidecar_starts_over() {
    let (_dir, layout) = fixture("resume-starts-over");
    let original = "Trời hôm nay đẹp. Hắn nói: “Đi thôi.”";
    std::fs::write(layout.chapter_txt(51), original).unwrap();
    keep_repaired(&layout, 51, original, original, &json!([]));
    keep(&layout, 51, original);

    assert!(
        kept(&layout, 51, "Một chương khác.").is_none(),
        "a re-crawled chapter is a new chapter"
    );
    assert!(!path(&layout, 51).exists(), "and the stale marker is gone");

    // A sidecar that fails the scan is not read back over, however the marker
    // was written: the proofread asks again.
    let broken = "Hắn nói: “Đi thôi.";
    keep_repaired(&layout, 51, original, broken, &json!([]));
    keep(&layout, 51, original);
    assert!(kept(&layout, 51, original).is_none());
    assert!(!path(&layout, 51).exists());
}
