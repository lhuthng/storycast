use super::quotes::effective_text;
use super::quotes::repaired_txt;
use super::quotes::strip_punctuation;
use super::*;
use crate::paths::Layout;

/// The reported failure, verbatim in shape: an opener on line 3 that never
/// closes, so the narration after it is read as speech.
const BROKEN: &str = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\nHắn quay lưng bước đi.";

#[test]
fn the_gate_names_the_paragraph_the_unpaired_quote_opened_on() {
    // The whole point: a fact no window can see, reported with somewhere to
    // look. Paragraph 3 opens a speech that never closes. Counted as a
    // paragraph, because that is how the chapter reads — and how the
    // proofread prompt names it.
    let findings = quote_findings(BROKEN);
    // The same damage is visible two ways — the net check names the opener,
    // and the span it drags in crosses a paragraph break — so both fire.
    // The ladder deduplicates by asking about the text once.
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
    // dialogue B lost its opener. Every count is even, the net is balanced,
    // and every window of the text reads fine — but the pairing SHIFTS. A's
    // opener swallows the paragraph under it, and the mark that should have
    // closed A is spent closing B instead, so the last span runs past its
    // own paragraph too. Two structural facts, one even count.
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
    // paragraph, no span crossing a break — the opener just sits welded to
    // the last word of the narration before it, so the scanner is inside a
    // speech from that word onwards. A lost mark and a lost colon look the
    // same here; either way the handover is wrong, and only the gate that
    // steps back OVER the opening delimiter can see it.
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
    // length guard is what keeps a quoted term from ever reaching the gate.
    let handed_over = broken.replacen("gật đầu\"Được", "gật đầu, nói: \"Được", 1);
    let found = quote_findings(&handed_over);
    assert!(found.is_empty(), "{found:?}");
    let term = "Tràng \"cuồng phong bạo vũ\" hiện ra trong đầu hắn.";
    let found = quote_findings(term);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_healthy_multi_paragraph_chapter_trips_nothing() {
    // The false-positive guard: real dialogue habits — a paragraph-broken
    // speech is REOPENED per paragraph, narration between speeches ends
    // with sentence punctuation, and a colon hands over to speech.
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

#[test]
fn the_verifier_admits_punctuation_edits_and_refuses_rewrites() {
    // A proofread's whole licence: the closer goes back, spacing settles.
    let fixed = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\"\nHắn quay lưng bước đi.";
    assert!(quote_findings(fixed).is_empty());
    assert_eq!(strip_punctuation(fixed), strip_punctuation(BROKEN));

    // Everything a creative model does instead. Each is a rewrite wearing
    // a proofread's clothes, and the filter catches all of them: changed
    // words, dropped sentences, added ones, and reordered text.
    for rewrite in [
        BROKEN.replace("Ta sẽ đi", "Ta sẽ không đi"),
        BROKEN.replace("Hắn quay lưng bước đi.", ""),
        format!("{BROKEN}\nVà cả những gì sau đó."),
        BROKEN.replace("Hắn nhìn ra cửa sổ.", "Hắn quay sang cửa khác."),
    ] {
        assert_ne!(
            strip_punctuation(&rewrite),
            strip_punctuation(BROKEN),
            "this rewrite must not pass the verifier: {rewrite}"
        );
    }
}

#[test]
fn a_repaired_sidecar_is_reused_and_the_original_is_never_touched() {
    let dir = std::env::temp_dir().join(format!("bm-quote-gate-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("data/chapters")).unwrap();
    let layout = Layout::resolve(&dir).expect("a workspace layout");
    std::fs::write(layout.chapter_txt(77), BROKEN).unwrap();

    // First digest: unbalanced, no sidecar yet, so the original is what the
    // digest would read and the gate fires on it.
    assert!(!quote_findings(&effective_text(&layout, 77, BROKEN)).is_empty());

    // The proofread's answer lands in the sidecar.
    let fixed = "Chương 77: Cô đơn\nHắn nhìn ra cửa sổ.\n\"Ta sẽ đi.\"\nHắn quay lưng bước đi.";
    std::fs::write(repaired_txt(&layout, 77), fixed).unwrap();

    // Second digest: balanced, so no further proofread call, and the
    // crawled chapter is still exactly what the crawl wrote.
    let text = effective_text(&layout, 77, BROKEN);
    assert!(quote_findings(&text).is_empty());
    assert_eq!(text, fixed);
    assert_eq!(
        std::fs::read_to_string(layout.chapter_txt(77)).unwrap(),
        BROKEN,
        "the repair must never overwrite the crawled chapter"
    );

    // A sidecar that is *still* unbalanced is not trusted: it falls back to
    // the original so the gate trips again rather than a bad repair being
    // believed twice.
    std::fs::write(repaired_txt(&layout, 77), "vẫn hỏng \"một câu").unwrap();
    assert_eq!(effective_text(&layout, 77, BROKEN), BROKEN);
    std::fs::remove_dir_all(&dir).ok();
}
