use super::*;
use std::path::PathBuf;

fn tree(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bm-migrate-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::create_dir_all(root.join("data").join("script")).unwrap();
    root
}

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn palette(root: &std::path::Path, body: &str) {
    write(
        root,
        "assets/scene-map.json",
        &format!("{{\"music_palette\": {body}}}"),
    );
}

fn injects(root: &std::path::Path, body: &str) {
    write(root, "assets/inject-pool.json", body);
}

fn script(root: &std::path::Path, chapter: u32, body: &str) -> PathBuf {
    let path = root
        .join("data")
        .join("script")
        .join(format!("{chapter}.json"));
    std::fs::write(&path, body).unwrap();
    path
}

fn load(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn history(pack: &str, palette: &[(&str, &str)], inject: &[(&str, &str)]) -> Migrations {
    Migrations {
        format: FORMAT,
        pack: pack.into(),
        history: vec![Step {
            version: "2026-11-02".into(),
            palette: palette
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            inject: inject
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }],
    }
}

#[test]
fn a_tree_without_a_file_has_no_history() {
    let root = tree("absent");
    assert!(read(&root).unwrap().is_none());
}

#[test]
fn a_newer_format_is_refused_rather_than_half_read() {
    let root = tree("format");
    write(
        &root,
        MIGRATIONS_FILE,
        r#"{"format": 99, "pack": "x", "history": []}"#,
    );
    let err = read(&root).unwrap_err().to_string();
    assert!(err.contains("newer than this build"), "{err}");
}

#[test]
fn the_same_rename_from_two_packs_is_one_rename() {
    let docs = vec![
        history("a", &[("quiet", "calm")], &[]),
        history("b", &[("quiet", "calm")], &[]),
    ];
    let merged = merge(&docs).unwrap();
    assert_eq!(
        merged.palette.get("quiet").map(String::as_str),
        Some("calm")
    );
}

#[test]
fn two_packs_that_disagree_about_one_name_are_refused() {
    let docs = vec![
        history("a", &[("quiet", "calm")], &[]),
        history("b", &[("quiet", "still")], &[]),
    ];
    let err = merge(&docs).unwrap_err().to_string();
    assert!(err.contains("disagree"), "{err}");
}

#[test]
fn a_history_that_grew_one_release_at_a_time_lands_on_the_last_name() {
    let docs = vec![Migrations {
        format: FORMAT,
        pack: "x".into(),
        history: vec![
            Step {
                version: "1".into(),
                palette: [("quiet".to_string(), "calm".to_string())]
                    .into_iter()
                    .collect(),
                inject: BTreeMap::new(),
            },
            Step {
                version: "2".into(),
                palette: [("calm".to_string(), "serene".to_string())]
                    .into_iter()
                    .collect(),
                inject: BTreeMap::new(),
            },
        ],
    }];
    let merged = merge(&docs).unwrap();
    assert_eq!(
        merged.palette.get("quiet").map(String::as_str),
        Some("serene")
    );
    assert_eq!(
        merged.palette.get("calm").map(String::as_str),
        Some("serene")
    );
}

#[test]
fn apply_rewrites_the_names_a_script_holds_and_nothing_else() {
    let root = tree("rewrite");
    palette(
        &root,
        r#"{"calm": {"tags": ["soft"]}, "none": {"tags": []}}"#,
    );
    injects(
        &root,
        r#"{"paper-rustle": {"tags": ["paper"], "files": ["injects/p.mp3"]}}"#,
    );
    let path = script(
        &root,
        4,
        r#"{"segments":[
            {"speaker":"A","text":"x","music":"quiet"},
            {"sound":"page-turn"},
            {"stop":"page-turn"},
            {"sound_tags":["paper"]},
            {"speaker":"B","text":"y","music":"none"}
        ]}"#,
    );
    let layout = Layout::new(&root);
    let merged = merge(&[history(
        "x",
        &[("quiet", "calm")],
        &[("page-turn", "paper-rustle")],
    )])
    .unwrap();

    let report = apply(&layout, &merged).unwrap();
    assert_eq!(report.chapters, [4]);
    assert_eq!(report.segments, 3, "one music plus two inject directives");
    assert_eq!(report.applied.len(), 2);

    let doc = load(&path);
    let segs = doc["segments"].as_array().unwrap();
    assert_eq!(segs[0]["music"], "calm");
    assert_eq!(segs[1]["sound"], "paper-rustle");
    assert_eq!(segs[2]["stop"], "paper-rustle");
    assert_eq!(
        segs[3]["sound_tags"][0], "paper",
        "a tag query is not a name, so it stays"
    );
}

#[test]
fn apply_keeps_a_name_the_workspace_still_declares() {
    let root = tree("kept");
    // Both names are live: the workspace authored its own palette on top.
    palette(
        &root,
        r#"{"quiet": {"tags": ["soft"]}, "calm": {"tags": ["soft"]},
            "none": {"tags": []}}"#,
    );
    let path = script(
        &root,
        5,
        r#"{"segments":[{"speaker":"A","text":"x","music":"quiet"}]}"#,
    );
    let layout = Layout::new(&root);
    let merged = merge(&[history("x", &[("quiet", "calm")], &[])]).unwrap();

    let report = apply(&layout, &merged).unwrap();
    assert!(report.applied.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(load(&path)["segments"][0]["music"], "quiet");
}

#[test]
fn a_target_the_live_tree_never_gained_is_reported_not_written() {
    let root = tree("missing");
    // Neither end of the rename is live: this workspace never had the mood.
    palette(&root, r#"{"none": {"tags": []}, "other": {"tags": []}}"#);
    let path = script(
        &root,
        6,
        r#"{"segments":[{"speaker":"A","text":"x","music":"quiet"}]}"#,
    );
    let layout = Layout::new(&root);
    let merged = merge(&[history("x", &[("quiet", "calm")], &[])]).unwrap();

    let report = apply(&layout, &merged).unwrap();
    assert_eq!(report.missing.len(), 1);
    assert!(report.applied.is_empty());
    assert_eq!(load(&path)["segments"][0]["music"], "quiet");
}

#[test]
fn applying_twice_changes_nothing_the_second_time() {
    let root = tree("twice");
    palette(
        &root,
        r#"{"calm": {"tags": ["soft"]}, "none": {"tags": []}}"#,
    );
    script(
        &root,
        7,
        r#"{"segments":[{"speaker":"A","text":"x","music":"quiet"}]}"#,
    );
    let layout = Layout::new(&root);
    let merged = merge(&[history("x", &[("quiet", "calm")], &[])]).unwrap();

    let first = apply(&layout, &merged).unwrap();
    let second = apply(&layout, &merged).unwrap();
    assert_eq!(first.chapters, [7]);
    assert!(
        !second.changed(),
        "a cumulative history is applied by every update, so it must be idempotent"
    );
}
