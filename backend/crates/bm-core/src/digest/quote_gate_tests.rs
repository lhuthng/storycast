use super::quotes::effective_text;
use super::quotes::judge_repair;
use super::quotes::repair_complaint;
use super::quotes::keep_repaired;
use super::quotes::needs_proofread;
use super::quotes::repaired_txt;
use super::quotes::RepairVerdict;
use super::*;
use crate::paths::Layout;

/// The reported failure, verbatim in shape: an opener on line 3 that never
const BROKEN: &str = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\nHắn quay lưng bước đi.";

#[test]
fn the_gate_names_the_paragraph_the_unpaired_quote_opened_on() {
    // The whole point: a fact no window can see, reported with somewhere to
    let findings = quote_findings(BROKEN);
    // The same damage is visible two ways — the net check names the opener,
    let kinds: Vec<_> = findings.iter().map(|f| (f.kind, f.paragraph)).collect();
    assert_eq!(
        kinds,
        vec![("unclosed quote", 3), ("swallowed paragraph", 3)],
        "{findings:?}"
    );
    assert!(findings[1].text.contains("Ta sẽ đi"), "{findings:?}");
    // Paired text trips nothing, so the common chapter costs one scan.
    assert!(quote_findings("Hắn nói: \"Ta sẽ đi.\" Rồi hắn bước đi.").is_empty());
    // A chapter with no dialogue is legal, not a fault.
    assert!(quote_findings("Chương 1\nHắn đi dọc con đường.").is_empty());
}

#[test]
fn two_dialogues_each_missing_one_mark_trip_the_gate_even_though_the_count_is_even() {
    // The scenario that kills a parity check: dialogue A lost its closer,
    let broken = "Hắn bước tới. \"Ai đó?\" một giọng nói vọng lại từ phía sau, \
vừa vang lên thì hắn đã quay đầu lại. \"Là ngươi sao, Dịch Phong?
Hắn không đáp. \"Sao ngươi lại ở đây?\"
Cô bước ra khỏi bóng tối, ta đã chờ ngươi lâu lắm rồi.\"";
    let findings = quote_findings(broken);
    assert!(
        findings.iter().all(|f| f.kind == "swallowed paragraph"),
        "the even-count mispair trips the structural gate, never the net one: \
         {findings:?}"
    );
    assert_eq!(
        findings.len(),
        2,
        "both shifted spans are caught: {findings:?}"
    );
    assert!(
        findings[0].text.contains("Dịch Phong"),
        "the first span swallows the narration under it: {findings:?}"
    );
    // And parity alone would have blessed exactly this text.
    assert_eq!(
        broken.matches('"').count(),
        6,
        "six marks, every one of them paired"
    );
}

#[test]
fn a_long_speech_glued_to_prose_trips_the_second_gate() {
    // The other mis-split a net check cannot see: even, balanced, one
    let broken = "Hắn nhìn cô ta, gật đầu\"Được rồi. Ta hiểu chuyện gì cần phải làm, và ta cũng biết mình phải đi đâu. Ngươi cứ ở lại đây mà chờ, đừng đi theo, vì nếu ngươi đi theo thì chỉ có chết thôi. Ta không muốn thấy ngươi chết.\"";
    let findings = quote_findings(broken);
    assert!(
        findings.iter().any(|f| f.kind == "welded prose"),
        "the welded handover must fire: {findings:?}"
    );
    assert!(
        !findings.iter().any(|f| f.kind == "unclosed quote"),
        "and the net is fine here, which is the point: {findings:?}"
    );
    assert_eq!(broken.matches('"').count() % 2, 0, "the count is even");

    // The same speech handed over properly is a healthy chapter, and the
    let handed_over = broken.replacen("gật đầu\"Được", "gật đầu, nói: \"Được", 1);
    let found = quote_findings(&handed_over);
    assert!(found.is_empty(), "{found:?}");
    let term = "Tràng \"cuồng phong bạo vũ\" hiện ra trong đầu hắn.";
    let found = quote_findings(term);
    assert!(found.is_empty(), "{found:?}");
}

/// The complaint is the model's instruction, so a finding it cannot act on is
/// a finding it will leave in place: a chapter came back untouched from
/// `swallowed paragraph` alone, and fixed once the rule was spelled out.
#[test]
fn the_complaint_says_what_the_rule_is_not_just_the_scans_word_for_it() {
    let broken = "Hắn gật đầu.\n\n\"Sao...\nSao lại có người cường đại đến vậy?\"";
    let findings = quote_findings(broken);
    assert!(
        findings.iter().any(|f| f.kind == "swallowed paragraph"),
        "{findings:?}"
    );
    let complaint = repair_complaint(&findings);
    assert!(
        complaint.contains("every paragraph of a speech needs its own pair"),
        "the rule, not only the symptom: {complaint}"
    );
    assert!(
        complaint.contains("paragraph 2"),
        "and where the speech starts: {complaint}"
    );
}

#[test]
fn a_quote_that_opens_a_paragraph_is_never_a_weld() {
    // The corpus false positive: a long speech opening its own paragraph, under
    // a paragraph that happens to end without punctuation. Nothing is welded —
    // the speech stands on its own line.
    let text = "Hắn nhìn quanh, không thấy ai cả\n\n\"Được rồi, ta ở đây vẫn còn chút việc \
cần giải quyết, các ngươi cứ ra phía sau xem thử đi, khi nào bắt đầu công việc chính \
thì các ngươi tự quyết định!\" Dịch Phong thong thả nói.\n";
    let found = quote_findings(text);
    assert!(
        found.is_empty(),
        "a paragraph-opening quote is not a weld: {found:?}"
    );
    // And the same words welded mid-line still fire.
    let welded = text.replacen("không thấy ai cả\n\n", "không thấy ai cả ", 1);
    assert!(
        quote_findings(&welded).iter().any(|f| f.kind == "welded prose"),
        "the mid-line weld is the fault gate 2 is for: {:?}",
        quote_findings(&welded)
    );
}

#[test]
fn a_healthy_multi_paragraph_chapter_trips_nothing() {
    // The false-positive guard: real dialogue habits — a paragraph-broken
    let healthy = "Cảnh tượng trước mắt làm hắn sững người.

Cả thảm cỏ đã cháy đen, khói vẫn còn tỉ tít bay lên sau đám cháy vừa lụt.

- Đây là chuyện gì đã xảy ra?

Lạc Lan Tuyết hỏi. \"Ngươi không biết gì sao?\" — nàng quay sang hắn, mắt rưng rưng.

Hắn lắc đầu: \"Ta cũng không rõ nữa. Tràng \"cuồng phong bạo vũ\" của hắn lại hiện ra trong đầu.\"";
    assert!(
        quote_findings(healthy).is_empty(),
        "{:?}",
        quote_findings(healthy)
    );
}

/// A proofread answer, in the shape the pass is asked to return.
fn repair_answer(text: &str) -> String {
    json!({"text": text, "changes": ["fixed"]}).to_string()
}

#[test]
fn the_repair_takes_any_answered_chapter_and_refuses_a_non_answer() {
    // The pass is semantic, so code checks one thing only: that a chapter came
    // back. Which words and which marks belong where is the model's call, and a
    // rail that guessed at it would only ever refuse a correct proofread.
    let fixed = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\"\nHắn quay lưng bước đi.";
    assert!(matches!(
        judge_repair(&repair_answer(fixed)),
        RepairVerdict::Fixed(..)
    ));
    for non_answer in [
        "not json at all",
        "{}",
        r#"{"text": ""}"#,
        r#"{"changes": []}"#,
    ] {
        assert!(
            matches!(judge_repair(non_answer), RepairVerdict::Reject(_)),
            "not a chapter: {non_answer}"
        );
    }
}


#[test]
fn a_repaired_chapter_is_reused_and_the_original_is_never_touched() {
    let dir = std::env::temp_dir().join(format!("bm-quote-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data/chapters")).unwrap();
    let layout = Layout::resolve(&dir).expect("a workspace layout");
    std::fs::write(layout.chapter_txt(77), BROKEN).unwrap();

    // First digest: unbalanced, no proofread yet, so the crawled chapter is what
    // the pipeline reads and the pass is still owed.
    assert!(needs_proofread(&layout, 77, BROKEN));
    assert_eq!(effective_text(&layout, 77, BROKEN), BROKEN);
    assert!(!quote_findings(BROKEN).is_empty());

    // The accepted proofread is recorded, and the digest reads it from then on.
    let fixed = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\"\nHắn quay lưng bước đi.";
    keep_repaired(&layout, 77, BROKEN, fixed, &json!(["closed the speech"]));
    assert!(!needs_proofread(&layout, 77, BROKEN), "the pass is behind it");
    let text = effective_text(&layout, 77, BROKEN);
    assert!(quote_findings(&text).is_empty());
    assert_eq!(text, fixed);
    assert_eq!(
        std::fs::read_to_string(layout.chapter_txt(77)).unwrap(),
        BROKEN,
        "the proofread must never overwrite the crawled chapter"
    );

    // A re-crawled chapter is a new chapter: a proofread written for the old
    // text is not read back over the new one.
    let recrawled = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n";
    std::fs::write(layout.chapter_txt(77), recrawled).unwrap();
    assert!(needs_proofread(&layout, 77, recrawled));
    assert_eq!(effective_text(&layout, 77, recrawled), recrawled);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_manual_flow_asks_the_proofread_first_instead_of_refusing() {
    // The reported failure: `:digest` refused a chapter the worker would have
    // proofread. Prompt 1 is that proofread, and it owns the text.
    let dir = std::env::temp_dir().join(format!("bm-manual-proofread-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    crate::profile::install_fixture(&dir).expect("fixture profile");
    let layout = Layout::new(&dir);
    std::fs::create_dir_all(layout.chapters()).unwrap();
    std::fs::write(layout.chapter_txt(77), BROKEN).unwrap();

    let first = manual_prompt(&layout, "vieneu", 77, None).expect("a first round");
    assert_eq!(first.round, Round::Repair, "the proofread opens the chapter");
    assert!(first.text.contains("\"Ta sẽ đi."), "the chapter rides along");
    assert!(
        !first.text.contains("{chapter_text}"),
        "no placeholder leaks"
    );

    // A pasted proofread lands in the sidecar — never over the chapter — and
    // hands back the cast prompt for the corrected text.
    let fixed = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\"\nHắn quay lưng bước đi.";
    let answer = manual_accept(&layout, "vieneu", 77, Round::Repair, &repair_answer(fixed), None)
        .expect("the proofread is usable");
    assert!(quote_findings(&effective_text(&layout, 77, BROKEN)).is_empty());
    assert_eq!(
        std::fs::read_to_string(layout.chapter_txt(77)).unwrap(),
        BROKEN,
        "the crawled chapter is never overwritten"
    );
    assert_eq!(
        answer.prompt.map(|p| p.round),
        Some(Round::Attribution),
        "round 1's cast prompt follows the proofread"
    );

    // Whatever chapter the model returned is taken: judging a proofread is
    // the operator's review, not a rail's.
    std::fs::remove_file(repaired_txt(&layout, 77)).ok();
    manual_accept(
        &layout,
        "vieneu",
        77,
        Round::Repair,
        &repair_answer("Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\nHắn quay lưng bước đi."),
        None,
    )
    .expect("any answered chapter");
    std::fs::remove_dir_all(&dir).ok();
}
