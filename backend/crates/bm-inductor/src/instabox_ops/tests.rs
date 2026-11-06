use super::*;

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn init_writes_the_definition_and_will_not_clobber_one_without_force() {
    let dir = scratch();
    let root = dir.path();
    let first = init(root, false).unwrap();
    assert!(first[0].contains("wrote"), "{first:?}");
    assert!(InstaboxConfig::path(root).is_file());

    // A second run keeps the definition — and the key it may hold.
    let again = init(root, false).unwrap();
    assert!(again[0].contains("already exists"), "{again:?}");
    assert!(again[0].contains("--force"), "it must say how: {again:?}");
    assert!(init(root, true).unwrap()[0].contains("wrote"));
}

#[test]
fn show_names_the_missing_key_and_what_a_launch_will_do() {
    let dir = scratch();
    let root = dir.path();
    let lines = show(root).join("\n");
    assert!(lines.contains("(none yet"), "{lines}");
    assert!(lines.contains("`ibx login`"), "{lines}");
    assert!(lines.contains("worker port published"), "{lines}");

    init(root, false).unwrap();
    let lines = show(root).join("\n");
    assert!(lines.contains("key: none"), "{lines}");
    assert!(lines.contains("1 thing(s) still to set"), "{lines}");
}

#[test]
fn show_reads_a_stored_key_as_stored_and_masks_it() {
    let dir = scratch();
    let root = dir.path();
    init(root, false).unwrap();
    let mut cfg = InstaboxConfig::load(root);
    cfg.api_key = "ibk_abcdefghijklmnopqrstuvwxyz".into();
    cfg.save(root).unwrap();

    let shown = show(root);
    let lines = shown.join("\n");
    assert!(lines.contains("(stored)"), "{lines}");
    // The head and tail tell two keys apart; the middle must not be printed.
    assert!(lines.contains("ibk_…wxyz"), "{lines}");
    assert!(!lines.contains("abcdefghijklmnop"), "{lines}");
    assert!(lines.contains("nothing missing"), "{lines}");
}

#[test]
fn mask_never_reveals_a_whole_key() {
    assert_eq!(mask("short"), "…");
    assert_eq!(mask("ibk_0123456789abcd"), "ibk_…abcd");
    // Twelve characters is the shortest key that still fits the two ends.
    assert_eq!(mask("012345678901"), "…");
}

#[test]
fn an_empty_key_clears_it_and_touches_nothing_else() {
    let dir = scratch();
    let root = dir.path();
    let mut cfg = InstaboxConfig::load(root);
    cfg.api_key = "ibk_something".into();
    cfg.max_workers = 3;
    cfg.save(root).unwrap();

    let out = login(root, LoginArgs::default(), Some("  ".into())).unwrap();
    assert!(out[0].contains("cleared"), "{out:?}");
    let back = InstaboxConfig::load(root);
    assert!(back.api_key.is_empty());
    assert_eq!(back.max_workers, 3, "the rest of the definition survives");
}

#[test]
fn a_key_with_a_space_is_refused_before_anything_is_written() {
    // The proof call is two round trips, so a typo this obvious is caught here
    let dir = scratch();
    let root = dir.path();
    let before = std::fs::read_to_string(InstaboxConfig::path(root)).unwrap_or_default();

    let err = login(root, LoginArgs::default(), Some("ibk_a b".into())).unwrap_err();
    assert!(err.to_string().contains("no spaces"), "{err}");
    let after = std::fs::read_to_string(InstaboxConfig::path(root)).unwrap_or_default();
    assert_eq!(before, after);
}

#[test]
fn login_with_no_key_at_all_is_a_prompt_not_a_clear() {
    let dir = scratch();
    let err = login(dir.path(), LoginArgs::default(), None).unwrap_err();
    assert!(err.to_string().contains("--api-key"), "{err}");
}

#[cfg(unix)]
#[test]
fn the_definition_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch();
    let root = dir.path();
    init(root, false).unwrap();
    let mode = std::fs::metadata(InstaboxConfig::path(root))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "mode {mode:o} is readable by others");
}
