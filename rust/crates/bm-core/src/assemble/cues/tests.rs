use super::*;
use crate::ambience::Slot;

fn slot(speaker: &str, start: f64, end: f64) -> Slot {
    Slot {
        wav: PathBuf::from("/dev/null"),
        scene: String::new(),
        music: String::new(),
        speaker: speaker.to_string(),
        injects: Vec::new(),
        start,
        end,
        gap_ms: 0,
        pause_ms: 0,
        inject_ms: 0,
    }
}

#[test]
fn cues_pair_each_slot_with_its_text_and_number_from_one() {
    let slots = vec![slot("Narrator", 0.0, 1.5), slot("Dịch Phong", 1.8, 3.0)];
    let texts = vec![vec!["mở đầu".to_string()], vec!["ngươi tới đây".to_string()]];
    let c = Cues::build(7, &slots, &texts);
    assert_eq!(c.version, 2);
    assert_eq!(c.chapter, 7);
    assert_eq!(c.cues.len(), 2);
    assert_eq!(c.cues[0].i, 1, "lines are numbered from 1, like :speaker");
    assert_eq!(c.cues[1].speaker, "Dịch Phong");
    assert_eq!(c.cues[1].text, "ngươi tới đây");
    assert_eq!(c.cues[1].start, 1.8);
    assert_eq!(c.duration_s, 3.0, "the end of the last line");
}

#[test]
fn a_batched_turn_gets_one_cue_per_segment_spanning_the_wav() {
    // One TTS call, three script segments: the captions must not merge them.
    let slots = vec![slot("Hệ thống", 10.0, 20.0)];
    let texts = vec![vec![
        "Tuổi tác: 20.".to_string(),
        "Tu vi: Phàm nhân.".to_string(),
        "Thành tựu: Quyền pháp.".to_string(),
    ]];
    let c = Cues::build(1, &slots, &texts);
    assert_eq!(c.cues.len(), 3);
    assert_eq!(c.cues[0].i, 1);
    assert_eq!(c.cues[2].i, 3);
    assert_eq!(c.cues[0].start, 10.0);
    assert_eq!(c.cues[1].start, c.cues[0].end, "the segments are contiguous");
    assert_eq!(c.cues[2].end, 20.0, "the last one ends on the wav");
    // "Thành tựu: Quyền pháp." is the longest, so it holds the largest slice.
    assert!(c.cues[2].end - c.cues[2].start > c.cues[0].end - c.cues[0].start);
    assert!(c.cues[2].end - c.cues[2].start > c.cues[1].end - c.cues[1].start);
}

#[test]
fn a_short_text_list_leaves_the_tail_empty_rather_than_failing() {
    let slots = vec![slot("Narrator", 0.0, 1.0), slot("Narrator", 1.0, 2.0)];
    let c = Cues::build(1, &slots, &[vec!["only one".to_string()]]);
    assert_eq!(c.cues[1].text, "");
}

#[test]
fn no_slots_is_zero_duration_not_a_panic() {
    let c = Cues::build(3, &[], &[]);
    assert!(c.cues.is_empty());
    assert_eq!(c.duration_s, 0.0);
}

#[test]
fn the_sidecar_sits_beside_the_mp3() {
    let p = cues_path(Path::new("/out/Ch.12 - Bí Tịch Của Dịch Phong.mp3"));
    assert_eq!(
        p,
        PathBuf::from("/out/Ch.12 - Bí Tịch Của Dịch Phong.cues.json")
    );
    assert!(!p.to_string_lossy().contains(".mp3"), "the .mp3 is replaced");
}

#[test]
fn write_round_trips_and_leaves_no_temp_file() {
    let dir = std::env::temp_dir().join("bm-cues-roundtrip");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("Ch.1 - A B.cues.json");
    let c = Cues::build(1, &[slot("Narrator", 0.0, 2.5)], &[vec!["xin chào".to_string()]]);
    write(&path, &c).unwrap();
    assert!(path.exists(), "the sidecar lands");
    assert!(
        !path
            .with_file_name(format!(
                "{}.tmp",
                path.file_name().unwrap().to_string_lossy()
            ))
            .exists(),
        "the temp file is renamed away, not left behind"
    );
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(back["cues"][0]["text"], "xin chào");
    assert_eq!(back["cues"][0]["end"], 2.5);
    let _ = std::fs::remove_dir_all(&dir);
}
