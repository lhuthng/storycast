use super::*;
use serde_json::json;

fn workspace() -> (tempfile::TempDir, Layout) {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path());
    layout.ensure().unwrap();
    bm_core::write_json(&layout.bible(), &json!({"characters": []})).unwrap();
    (d, layout)
}

fn script(layout: &Layout, n: u32, speakers: &[&str]) {
    let segments: Vec<Value> = speakers
        .iter()
        .map(|s| json!({"speaker": s, "text": "một câu"}))
        .collect();
    bm_core::write_json(&layout.script(n), &json!({"segments": segments})).unwrap();
}

fn hearing(index: &SpeakerIndex, name: &str) -> Vec<u32> {
    index.chapters_hearing(&[name.to_string()])
}

#[test]
fn the_index_follows_scripts_by_stamp_and_prunes_gone_ones() {
    let (_d, layout) = workspace();
    script(&layout, 1, &["A"]);
    script(&layout, 2, &["B"]);

    let index = SpeakerIndex::load(&layout);
    assert_eq!(hearing(&index, "A"), vec![1]);
    assert_eq!(hearing(&index, "B"), vec![2]);
    assert!(layout.speaker_index().is_file(), "the cache is persisted");

    // A rewrite changes the stamp (the byte length differs too, so a coarse
    // clock alone cannot hide it) and the answer follows.
    script(&layout, 2, &["A"]);
    let index = SpeakerIndex::load(&layout);
    assert_eq!(hearing(&index, "A"), vec![1, 2]);
    assert!(hearing(&index, "B").is_empty());

    std::fs::remove_file(layout.script(1)).unwrap();
    let index = SpeakerIndex::load(&layout);
    assert_eq!(hearing(&index, "A"), vec![2]);
}

#[test]
fn an_unchanged_book_reuses_the_index_instead_of_rewriting_it() {
    let (_d, layout) = workspace();
    script(&layout, 1, &["A"]);
    SpeakerIndex::load(&layout);
    let first = std::fs::metadata(layout.speaker_index())
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    SpeakerIndex::load(&layout);
    let second = std::fs::metadata(layout.speaker_index())
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(first, second, "a fresh index is read, not rebuilt");
}

#[test]
fn a_bible_alias_resolves_and_a_bible_edit_refreshes_it() {
    let (_d, layout) = workspace();
    script(&layout, 1, &["Anh Áo Đen"]);
    assert!(hearing(&SpeakerIndex::load(&layout), "Lâu Bản Vĩ").is_empty());

    bm_core::write_json(
        &layout.bible(),
        &json!({"characters": [{
            "name": "Lâu Bản Vĩ",
            "proper_aliases": ["Lâu Bản Vĩ", "Anh Áo Đen"]
        }]}),
    )
    .unwrap();
    let index = SpeakerIndex::load(&layout);
    assert_eq!(hearing(&index, "Lâu Bản Vĩ"), vec![1]);
}

#[test]
fn a_render_plan_alone_keeps_a_chapter_in_the_swap() {
    let (_d, layout) = workspace();
    script(&layout, 1, &["B"]);
    let take = |speaker: &str| {
        json!({
            "chapter": 1, "engine": "vieneu",
            "takes": [{
                "pos": 0, "tag": "", "speaker": speaker, "voice": "v", "text": "x",
                "temperature": 0.5, "silence_p": 0.2, "take_key": "k", "file": "f.wav"
            }]
        })
    };
    bm_core::write_json(&layout.plan(1), &take("A")).unwrap();
    assert_eq!(hearing(&SpeakerIndex::load(&layout), "A"), vec![1]);

    // A script edit that keeps the same plan must not drop the take.
    script(&layout, 1, &["B", "B"]);
    assert_eq!(hearing(&SpeakerIndex::load(&layout), "A"), vec![1]);

    // Re-planning without A drops the chapter even though the script is unchanged.
    bm_core::write_json(&layout.plan(1), &take("B")).unwrap();
    assert!(hearing(&SpeakerIndex::load(&layout), "A").is_empty());
}

#[test]
fn an_absorbed_spelling_still_matches_after_the_bible_renames_it() {
    let (_d, layout) = workspace();
    script(&layout, 1, &["Huyền Vũ lão tổ"]);
    bm_core::write_json(
        &layout.bible(),
        &json!({"characters": [{
            "name": "Huyền Vũ",
            "proper_aliases": ["Huyền Vũ", "Huyền Vũ lão tổ"]
        }]}),
    )
    .unwrap();
    let index = SpeakerIndex::load(&layout);
    assert_eq!(hearing(&index, "Huyền Vũ lão tổ"), vec![1]);
    assert_eq!(hearing(&index, "Huyền Vũ"), vec![1]);
}
