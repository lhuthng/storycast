use super::*;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bm-upload-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn local(number: u32, sha: &str) -> Local {
    Local {
        number,
        path: PathBuf::from(format!("/out/Ch.{number} - T.mp3")),
        title: "T".into(),
        duration_s: Some(60),
        sha256: sha.into(),
        size: 10,
    }
}

fn remote(number: u32, sha: &str) -> RemoteChapter {
    RemoteChapter {
        number,
        sha256: sha.into(),
    }
}

#[test]
fn the_manifest_diff_is_the_hash() {
    let mine = vec![local(1, "aa"), local(2, "bb"), local(3, "cc")];
    let theirs = Manifest {
        chapters: vec![remote(1, "AA"), remote(2, "zz"), remote(9, "dd")],
    };
    let plan = plan(&mine, &theirs);
    assert_eq!(plan[0].action, Action::Unchanged, "case-insensitive match");
    assert_eq!(plan[1].action, Action::Replace);
    assert_eq!(plan[2].action, Action::Create, "absent from the manifest");
    assert_eq!(plan.len(), mine.len(), "server-only chapters are ignored");
}

#[test]
fn a_dry_run_counts_and_names_every_chapter() {
    let mine = vec![local(1, "aa"), local(2, "bb"), local(3, "cc")];
    let theirs = Manifest {
        chapters: vec![remote(2, "zz")],
    };
    let planned = plan(&mine, &theirs);
    let report = Report::of(&planned);
    assert_eq!(
        (report.created, report.replaced, report.unchanged),
        (2, 1, 0)
    );
    let lines: Vec<String> = planned.iter().map(|p| p.describe()).collect();
    assert_eq!(lines, ["ch1 new", "ch2 replace", "ch3 new"]);
}

#[test]
fn an_empty_manifest_creates_every_chapter() {
    let mine = vec![local(1, "aa")];
    let plan = plan(&mine, &Manifest::default());
    assert_eq!(plan[0].action, Action::Create);
}

#[test]
fn only_the_published_name_is_a_chapter() {
    assert_eq!(parse_output_name("Ch.17 - Bí Tịch.mp3"), Some((17, "Bí Tịch")));
    assert_eq!(parse_output_name("Ch.7 - A - B.mp3"), Some((7, "A - B")));
    assert_eq!(parse_output_name("Ch.7 - A.mp3.cues.json"), None);
    assert_eq!(parse_output_name("chapter-17.mp3"), None);
    assert_eq!(parse_output_name("Ch.x - A.mp3"), None);
}

#[test]
fn the_output_folder_is_hashed_newest_title_wins() {
    let root = tmpdir("layout");
    let layout = crate::Layout::new(&root);
    let out = layout.output();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("Ch.1 - Old.mp3"), b"old bytes").unwrap();
    std::fs::write(out.join("Ch.1 - New.mp3"), b"new bytes").unwrap();
    std::fs::write(out.join("Ch.2 - Two.mp3"), b"second").unwrap();
    std::fs::write(out.join("notes.txt"), b"not a chapter").unwrap();
    std::fs::write(
        out.join("Ch.2 - Two.cues.json"),
        r#"{"duration_s": 2413.4}"#,
    )
    .unwrap();

    let found = chapters(&layout).unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].number, 1);
    assert_eq!(found[0].sha256, sha256_hex(b"new bytes"));
    assert_eq!(found[1].duration_s, Some(2413), "the sidecar's clock, rounded");
    assert_eq!(found[1].sha256, sha256_hex(b"second"));
}

#[test]
fn sha256_matches_the_published_test_vector() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn one_chapter_is_found_by_number() {
    let root = tmpdir("one");
    let layout = crate::Layout::new(&root);
    let out = layout.output();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("Ch.4 - Old.mp3"), b"old").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(out.join("Ch.4 - New.mp3"), b"newer").unwrap();
    std::fs::write(out.join("Ch.5 - Other.mp3"), b"other").unwrap();

    let found = chapter(&layout, 4).unwrap().expect("ch4");
    assert_eq!(found.number, 4);
    assert_eq!(found.sha256, sha256_hex(b"newer"));
    assert_eq!(found.path.file_name().unwrap(), "Ch.4 - New.mp3");
    assert!(chapter(&layout, 9).unwrap().is_none());
}
