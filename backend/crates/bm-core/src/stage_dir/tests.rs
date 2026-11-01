use super::*;

fn workspace(name: &str) -> (PathBuf, crate::Layout) {
    let dir = std::env::temp_dir().join(format!("bm-stage-dir-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("backend")).unwrap();
    std::fs::write(dir.join("backend/Cargo.toml"), "[workspace]").unwrap();
    let layout = crate::Layout::new(&dir);
    (dir, layout)
}

fn set_segments(l: &crate::Layout, raw: &str) {
    let path = l.settings();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = crate::config::Settings::load(&path);
    s.segments_dir = raw.into();
    s.save(&path).unwrap();
}

fn plan(l: &crate::Layout, kind: StageDir, to: &std::path::Path) -> StageDirChange {
    StageDirChange {
        kind,
        raw: to.display().to_string(),
        from: l.stage_dir(kind),
        to: to.to_path_buf(),
    }
}

#[test]
fn the_plan_resolves_and_refuses_the_other_stage_folder() {
    let (root, l) = workspace("plan");
    let change = l.plan_stage_dir(StageDir::Output, "drive/out").unwrap();
    assert_eq!(change.to, root.join("drive/out"));
    assert_eq!(change.from, root.join("output"));
    assert_eq!(change.raw, "drive/out");
    assert_eq!(StageDir::Output.key(), "output_dir");

    // Empty is the workspace default, kept as empty.
    let change = l.plan_stage_dir(StageDir::Output, "   ").unwrap();
    assert_eq!(change.to, root.join("output"));
    assert_eq!(change.raw, "");
    assert_eq!(
        l.plan_stage_dir(StageDir::Segments, " ").unwrap().to,
        l.audio()
    );

    // The other stage's folder, configured or default, is refused by name.
    set_segments(&l, "takes");
    let err = l
        .plan_stage_dir(StageDir::Output, "takes")
        .unwrap_err()
        .to_string();
    assert!(err.contains("other stage's folder"), "{err}");
    assert!(err.contains("takes"), "{err}");
    let err = l
        .plan_stage_dir(StageDir::Segments, "output")
        .unwrap_err()
        .to_string();
    assert!(err.contains("other stage's folder"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_move_into_a_missing_folder_takes_the_whole_folder() {
    let (root, l) = workspace("move-whole");
    let out = l.output();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("Ch.1 - One.mp3"), "audio").unwrap();
    std::fs::write(out.join("Ch.1 - One.cues.json"), "{}").unwrap();

    let to = root.join("drive/out");
    let moved = l.move_stage_dir(&plan(&l, StageDir::Output, &to)).unwrap();
    assert_eq!(moved.entries, 2);
    assert!(to.join("Ch.1 - One.mp3").is_file());
    assert!(to.join("Ch.1 - One.cues.json").is_file());
    assert!(
        !out.exists(),
        "nobody else had the name, so one rename took the folder"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_move_into_an_existing_empty_folder_goes_entry_by_entry() {
    let (root, l) = workspace("move-empty");
    let out = l.output();
    std::fs::create_dir_all(out.join("nested")).unwrap();
    std::fs::write(out.join("Ch.1 - One.mp3"), "audio").unwrap();
    std::fs::write(out.join("nested/note.txt"), "x").unwrap();
    let to = root.join("drive/out");
    std::fs::create_dir_all(&to).unwrap();

    let moved = l.move_stage_dir(&plan(&l, StageDir::Output, &to)).unwrap();
    assert_eq!(moved.entries, 2);
    assert!(to.join("Ch.1 - One.mp3").is_file());
    assert_eq!(
        std::fs::read_to_string(to.join("nested/note.txt")).unwrap(),
        "x",
        "directories travel whole"
    );
    assert!(
        !out.exists(),
        "the emptied folder goes too, not just its entries"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_move_refuses_a_folder_that_already_holds_something() {
    let (root, l) = workspace("move-occupied");
    let out = l.output();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("Ch.1 - One.mp3"), "audio").unwrap();
    let to = root.join("drive/out");
    std::fs::create_dir_all(&to).unwrap();
    std::fs::write(to.join("keep.txt"), "mine").unwrap();

    let err = l
        .move_stage_dir(&plan(&l, StageDir::Output, &to))
        .unwrap_err()
        .to_string();
    assert!(err.contains("not empty"), "{err}");
    assert!(
        err.contains("keep.txt"),
        "the refusal names what is there: {err}"
    );
    assert!(out.join("Ch.1 - One.mp3").is_file(), "nothing moved");
    assert_eq!(
        std::fs::read_dir(&to).unwrap().count(),
        1,
        "and nothing was written over it"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_move_refuses_a_folder_that_overlaps_the_old_one() {
    let (root, l) = workspace("move-inside");
    let out = l.output();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("Ch.1 - One.mp3"), "audio").unwrap();

    let inside = out.join("backup");
    let err = l
        .move_stage_dir(&plan(&l, StageDir::Output, &inside))
        .unwrap_err()
        .to_string();
    assert!(err.contains("overlaps"), "{err}");
    let above = root.clone();
    let err = l
        .move_stage_dir(&plan(&l, StageDir::Output, &above))
        .unwrap_err()
        .to_string();
    assert!(err.contains("overlaps"), "{err}");
    assert!(out.join("Ch.1 - One.mp3").is_file(), "nothing moved");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_move_refuses_to_carry_the_other_stage_along() {
    let (root, l) = workspace("move-carries");
    set_segments(&l, "output/takes");
    let out = l.output();
    std::fs::create_dir_all(out.join("takes")).unwrap();
    std::fs::write(out.join("takes/f1.wav"), "take").unwrap();
    assert_eq!(l.audio(), out.join("takes"));

    let to = root.join("drive/out");
    let err = l
        .move_stage_dir(&plan(&l, StageDir::Output, &to))
        .unwrap_err()
        .to_string();
    assert!(err.contains("sits inside"), "{err}");
    assert!(out.join("takes/f1.wav").is_file(), "nothing moved");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_empty_old_folder_just_gives_way() {
    let (root, l) = workspace("move-none");
    std::fs::create_dir_all(l.output()).unwrap();
    let to = root.join("drive/out");
    let moved = l.move_stage_dir(&plan(&l, StageDir::Output, &to)).unwrap();
    assert_eq!(moved.entries, 0);
    assert!(!l.output().exists(), "the empty folder is not left behind");
    assert!(to.is_dir(), "and the new one is ready");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_missing_old_folder_moves_nothing() {
    let (root, l) = workspace("move-missing");
    let to = root.join("drive/out");
    let moved = l.move_stage_dir(&plan(&l, StageDir::Output, &to)).unwrap();
    assert_eq!(moved.entries, 0);
    assert!(
        !to.exists(),
        "a move does not create what it has nothing for"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn apply_saves_the_typed_path_and_creates_the_folder() {
    let (root, l) = workspace("apply");
    let change = l.plan_stage_dir(StageDir::Output, "drive/out").unwrap();
    l.apply_stage_dir(&change).unwrap();
    assert_eq!(
        crate::config::Settings::load(&l.settings()).output_dir,
        "drive/out"
    );
    assert_eq!(l.output(), root.join("drive/out"));
    assert!(
        l.output().is_dir(),
        "pointing at a missing folder creates it"
    );

    // Empty restores the default, and takes follow the segment setting.
    set_segments(&l, "takes");
    let change = l.plan_stage_dir(StageDir::Segments, "").unwrap();
    l.apply_stage_dir(&change).unwrap();
    assert_eq!(l.audio(), root.join("data/audio"));
    assert_eq!(
        l.seg_dir("vieneu", 7),
        root.join("data/audio/segments-default-vieneu-07")
    );
    std::fs::remove_dir_all(&root).ok();
}
