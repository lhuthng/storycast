//! Splitting one chapter into the windows a single digest answer can hold.
//!
//! A window is a **contiguous run of the chapter's prepared events**, and that
//! is the whole trick: `prepare_chapter` already gives every event a stable id
//! (`e0001`…), the source gate already proves each id was consumed exactly once
//! in source order, and both prompts already render from a `PreparedChapter`.
//! So a window is a `PreparedChapter` holding a slice of the same events with
//! the *chapter's* ids — the two rounds, the validators and the merge all run
//! per window unchanged, and no second contract exists to drift from.
//!
//! What decides where the cuts fall is `DigestSettings`: an explicit ceiling
//! (`chunk_sentences`, `chunk_chars`) for an operator who wants windows smaller
//! than the budget would make them, and `answer_tokens` for the case this
//! module exists for — a chapter whose answer does not fit under the 16384
//! tokens every backend caps at, where today the reply is cut mid-JSON and the
//! chapter is shelved after a repair call that fails the same way.
//!
//! Three invariants are worth stating because everything else depends on them:
//!
//! 1. **Whole events only.** A window's slice is event-aligned, never a
//!    character offset, because a segment's `source_id` names a whole event and
//!    `validate_source_alignment` would refuse a window that halved one.
//! 2. **Every event exactly once, in order.** The windows partition the
//!    chapter; `plan_windows` walks the events once and closes as it goes.
//! 3. **The cut falls at a sentence end when there is one.** Closing after the
//!    event that ended a sentence keeps a window's prose whole, which is what a
//!    reader hears: the seam between windows is a seam between sentences.

use super::{PreparedChapter, PreparedEvent};
use crate::config::DigestSettings;
use serde_json::{json, Value};

/// Characters of answer the estimate spends per token.
///
/// Deliberately pessimistic, and deliberately one number rather than a real
/// tokenizer: the estimate only has to be safe in **one** direction. Over-
/// estimating tokens splits a window that would have fit (one extra call),
/// under-estimating truncates an answer (a repair call, then a shelved
/// chapter), so the two errors are not symmetric and this errs toward the
/// cheap one. Vietnamese tokenizes worse than English on every backend in use
/// — a five-syllable name can be five tokens — which is why this is 2 and not
/// the 4 an English estimate would use.
///
/// Measured, not guessed: the live answer that [`SEGMENT_OVERHEAD`] describes
/// ran ~3 characters per token, so this charges half again as many tokens as
/// the backend spent. The margin is the point — the measurement is of *one*
/// backend, and a model that tokenizes Vietnamese worse would meet the cap
/// first. This constant and [`SEGMENT_OVERHEAD`] are a **pair**, and the pair is
/// what was calibrated: see the note there before changing either alone.
pub(crate) const CHARS_PER_TOKEN: usize = 2;

/// The JSON a segment answering one event costs on top of its own text: the id
/// it keys on, the mood/scene/music it declares, and the punctuation.
///
/// Per *event*, not per window, because it is the segments that scale: a window
/// of forty one-line paragraphs carries forty of these, while a window of four
/// paragraphs carries four.
///
/// **Calibrated with [`CHARS_PER_TOKEN`], and the pair is what the measurement
/// validates.** Taken apart on the same live answer (a 40 KB chapter, 219
/// segments), the JSON around a segment is **131** characters and the answer
/// spends a token per 3 characters — so this constant under-counts the JSON by
/// about 2x while the token charge over-counts by about 1.5x, and the two
/// errors cancel, and the chapter is charged **0.93 tokens per character of its
/// own text** where the answer spent 0.90 (2.8 characters of answer per
/// character of chapter, 3.1 characters per token). Raising this alone would
/// cut chapters that fit in one call into two, which is the one thing the plan
/// must not do — a chapter under the budget has to be asked the single-call
/// prompt, byte for byte.
const SEGMENT_OVERHEAD: usize = 64;

/// The largest a window may grow past its character target while waiting for a
/// sentence to end at, as a fraction: `chars + chars / 2`.
///
/// Without it, a chapter of one unpunctuated paragraph — a crawl that lost its
/// punctuation, which is a real thing this corpus has seen — would never close
/// a window at all, because the sentence rule can never fire and the character
/// rule would be waiting for a sentence end that never comes.
const OVERSHOOT: usize = 2;

/// One window: a half-open range of the chapter's events, with the counts the
/// log line quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Window {
    /// First event index, inclusive.
    pub from: usize,
    /// Last event index, exclusive.
    pub to: usize,
    /// Events in this window, `to - from`.
    pub events: usize,
    /// Weighted characters — see [`weight`]. What the budget is spent in.
    pub chars: usize,
    /// Sentence-final marks in this window's text, as counted by
    /// [`sentence_ends`].
    pub sentences: u32,
}

impl Window {
    /// This window's events as a `PreparedChapter` of their own, carrying the
    /// **chapter's** ids.
    ///
    /// Ids are not renumbered, and that is the point: the merged script has to
    /// answer the same `e0007` the chapter's own view named, or the source gate
    /// on the next pass would be validating a different chapter's ids.
    ///
    /// `unbalanced` is false here on purpose. An open quote delimiter is a fact
    /// about the whole chapter, it is already reported from the whole chapter by
    /// `split_summary`, and a window that re-reported it would say the same
    /// thing N times.
    pub(crate) fn prepared(&self, chapter: &PreparedChapter) -> PreparedChapter {
        slice(chapter, self.from, self.to)
    }
}

/// The events in `from..to` of `chapter`, as a `PreparedChapter`.
fn slice(chapter: &PreparedChapter, from: usize, to: usize) -> PreparedChapter {
    let events = chapter.events[from..to].to_vec();
    let value: Vec<Value> = events
        .iter()
        .map(|e| json!({"id": e.id, "kind": e.kind, "text": e.text}))
        .collect();
    PreparedChapter {
        prompt_json: serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".into()),
        events,
        unbalanced: false,
    }
}

/// What one event costs in the answer: its own text plus the JSON around it.
pub(crate) fn event_weight(event: &PreparedEvent) -> usize {
    event.text.chars().count() + SEGMENT_OVERHEAD
}

/// What a run of events costs in the answer, in weighted characters.
pub(crate) fn weight(events: &[PreparedEvent]) -> usize {
    events.iter().map(event_weight).sum()
}

/// The estimated token cost of `chars` weighted characters.
pub(crate) fn tokens(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Count the sentence-final marks in `text`, and say whether the text *ends* at
/// one — after any closing quote or bracket, because a spoken line ends
/// `…trượt tay."` and the quote is not a sentence.
///
/// A heuristic, and cheap on purpose: it decides where to cut **between** two
/// windows, so the cost of being wrong is a window that ends one sentence
/// earlier or later than it might have. No text is ever rewritten from it.
pub(crate) fn sentence_ends(text: &str) -> (u32, bool) {
    let mut count = 0u32;
    let mut last = None;
    for (i, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?' | '…') {
            count += 1;
            last = Some(i + ch.len_utf8());
        }
    }
    let ends = last.is_some_and(|at| {
        text[at..]
            .trim_matches(|c: char| {
                matches!(c, '"' | '“' | '”' | '「' | '」' | ')' | ']' | '}' | ' ')
            })
            .trim_end_matches(['\r', '\n'])
            .is_empty()
    });
    (count, ends)
}

/// Where a window is allowed to close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    /// Close at the first sentence-final event at or after this many sentences.
    /// `0` = no sentence rule.
    sentences: u32,
    /// Close once the window has spent this many weighted characters.
    /// `0` = no character rule.
    chars: usize,
}

impl Target {
    /// Resolve the settings into the budget one chapter is actually cut with.
    ///
    /// The character target and the answer budget are two ways of asking the
    /// same question, so the **smaller** wins: an explicit `chunk_chars` is an
    /// operator saying "these windows are too big for my reasons", and a budget
    /// that overwrote it upward would quietly undo the setting.
    ///
    /// The budget is spent as an **average**: the number of windows the total
    /// needs, then that total divided between them. A greedy fill to the last
    /// character that fits would instead put one window at the budget's edge
    /// and hand the remainder to a second one — and the remainder is the window
    /// with the *least* prose and the *most* `PLOT SO FAR`, which is the call
    /// that can afford it least. The share is where a window starts looking for
    /// a sentence to end at; a window that is already past it and sees a
    /// sentence end closes there rather than filling to the last character.
    fn resolve(events: &[PreparedEvent], settings: &DigestSettings) -> Target {
        let sentences = settings.chunk_sentences;
        let mut chars = settings.chunk_chars as usize;
        if settings.answer_tokens > 0 {
            let budget = settings.answer_tokens as usize * CHARS_PER_TOKEN;
            let total = weight(events);
            if total > budget {
                let windows = total.div_ceil(budget);
                let share = total.div_ceil(windows);
                chars = if chars == 0 { share } else { chars.min(share) };
            }
        }
        Target { sentences, chars }
    }

    /// Whether a window holding `sentences`/`chars` and ending at `ends` closes
    /// here.
    ///
    /// Both rules need the **sentence** to be finished, not merely the target
    /// met: a window that closed the moment it reached its share would end
    /// mid-sentence half the time, and the seam is what a listener hears. The
    /// character rule is the one exception, and only after [`OVERSHOOT`] —
    /// an event that never ends a sentence still has to be able to close a
    /// window, or a chapter of unpunctuated prose would never split at all.
    fn closes(&self, sentences: u32, chars: usize, ends: bool) -> bool {
        if self.sentences > 0 && sentences >= self.sentences && ends {
            return true;
        }
        if self.chars > 0 && chars >= self.chars && (ends || chars >= self.ceiling()) {
            return true;
        }
        false
    }

    /// The hard ceiling: the point past which a window closes even without a
    /// sentence end.
    fn ceiling(&self) -> usize {
        self.chars + self.chars / OVERSHOOT
    }
}

/// Cut `chapter` into the windows one digest answer can hold.
///
/// One pass, one cut at a time, and always at least one window: an empty
/// chapter gets a window holding no events rather than no windows, because the
/// two rounds still have to run and report — which is what the pre-window
/// digest did with an empty chapter, and this must not change that.
pub(crate) fn plan_windows(chapter: &PreparedChapter, settings: &DigestSettings) -> Vec<Window> {
    let events = chapter.events.as_slice();
    if events.is_empty() {
        return vec![Window {
            from: 0,
            to: 0,
            events: 0,
            chars: 0,
            sentences: 0,
        }];
    }
    let target = Target::resolve(events, settings);
    let mut windows = Vec::new();
    let mut from = 0usize;
    let mut sentences = 0u32;
    let mut chars = 0usize;
    for (i, event) in events.iter().enumerate() {
        let (count, ends) = sentence_ends(&event.text);
        sentences += count;
        chars += event_weight(event);
        let last = i + 1 == events.len();
        if last || target.closes(sentences, chars, ends) {
            windows.push(Window {
                from,
                to: i + 1,
                events: i + 1 - from,
                chars,
                sentences,
            });
            from = i + 1;
            sentences = 0;
            chars = 0;
        }
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_ANSWER_TOKENS;
    use crate::crawl::{CrawlOutcome, Provider};
    use bm_proto::CrawlSpec;
    use std::path::Path;

    /// A chapter of `lines` paragraphs, each three sentences. `prepare_chapter`
    /// splits narration on newlines, so this is one event per line, each ending
    /// at a sentence — the shape the corpus actually has.
    fn chapter(lines: usize) -> PreparedChapter {
        let text: String = (0..lines)
            .map(|i| format!("Đoạn văn số {i} mở đầu câu chuyện. Câu thứ hai ở đây. Câu thứ ba.\n"))
            .collect();
        super::super::prepare_chapter(&text)
    }

    /// A chapter of longer paragraphs — the shape real prose has, and the one
    /// the budget is calibrated against, since it is the *characters* that
    /// decide a window.
    fn prose(lines: usize) -> PreparedChapter {
        let text: String = (0..lines)
            .map(|i| {
                format!(
                    "Đoạn văn số {i} mở đầu câu chuyện bằng một câu dài hơn hẳn, kể rằng buổi chiều \
                     hôm ấy trời trở gió và người trong sân đứng im. Câu thứ hai ở đây cũng dài \
                     tương tự. Câu thứ ba khép lại đoạn.\n"
                )
            })
            .collect();
        super::super::prepare_chapter(&text)
    }

    /// What the default budget does to a chapter of a given size, printed.
    ///
    /// Not a claim about any one chapter — a table, because "will my chapter
    /// split?" is the question the whole windowing module exists to answer and
    /// the answer is a step function in the chapter's length. `lines` are
    /// paragraphs of the `prose` shape, which is what the corpus has.
    #[test]
    fn what_the_default_budget_does_to_a_chapter_of_each_size() {
        let budget_chars = DEFAULT_ANSWER_TOKENS as usize * CHARS_PER_TOKEN;
        println!("\nbudget: {DEFAULT_ANSWER_TOKENS} tokens = {budget_chars} weighted chars\n");
        println!(
            "{:>6}  {:>7}  {:>8}  {:>8}  {:>8}  {:>4}  events per window",
            "lines", "events", "chars", "weight", "tokens", "wins"
        );
        for lines in [10usize, 20, 50, 100, 150, 213, 300, 500] {
            let chapter = prose(lines);
            let text: usize = chapter.events.iter().map(|e| e.text.chars().count()).sum();
            let w = weight(&chapter.events);
            let windows = plan_windows(&chapter, &DigestSettings::default());
            assert_partitions(&chapter, &windows);
            for win in &windows {
                assert!(
                    tokens(win.chars) <= DEFAULT_ANSWER_TOKENS as usize,
                    "a window would overrun the answer budget: {win:?}"
                );
            }
            println!(
                "{lines:>6}  {:>7}  {text:>8}  {w:>8}  {:>8}  {:>4}  {:?}",
                chapter.events.len(),
                tokens(w),
                windows.len(),
                windows.iter().map(|x| x.events).collect::<Vec<_>>(),
            );
        }
        println!();
    }

    fn settings(sentences: u32, chars: u32, tokens: u32) -> DigestSettings {
        DigestSettings {
            chunk_sentences: sentences,
            chunk_chars: chars,
            answer_tokens: tokens,
        }
    }

    /// The invariant every plan must hold, whatever the target was: the windows
    /// partition the events, in order, with nothing empty and nothing lost.
    fn assert_partitions(chapter: &PreparedChapter, windows: &[Window]) {
        assert!(!windows.is_empty(), "a plan is never empty");
        let mut at = 0;
        for w in windows {
            assert_eq!(w.from, at, "windows are contiguous: {windows:?}");
            assert_eq!(w.events, w.to - w.from);
            assert!(w.events > 0 || chapter.events.is_empty());
            assert_eq!(
                w.chars,
                weight(&chapter.events[w.from..w.to]),
                "the quoted weight is the window's own: {w:?}"
            );
            assert_eq!(
                w.sentences,
                chapter.events[w.from..w.to]
                    .iter()
                    .map(|e| sentence_ends(&e.text).0)
                    .sum::<u32>(),
                "the quoted sentence count is the window's own: {w:?}"
            );
            at = w.to;
        }
        assert_eq!(at, chapter.events.len(), "the last window ends the chapter");
    }

    #[test]
    fn an_empty_chapter_is_one_window_of_nothing() {
        let empty = super::super::prepare_chapter("");
        assert!(empty.events.is_empty());
        let windows = plan_windows(&empty, &DigestSettings::default());
        // One, not zero: the two rounds still run and still report, which is
        // what the pre-window digest did with an empty chapter.
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0], Window { from: 0, to: 0, events: 0, chars: 0, sentences: 0 });
    }

    #[test]
    fn a_chapter_under_the_budget_is_one_window() {
        // The parity case, sized against the real corpus: its longest chapter is
        // ~13.6 KB, and one of these is 15 KB — longer than anything in it — and
        // still digests in one call, exactly as before.
        let c = prose(75);
        assert!(
            c.events.iter().map(|e| e.text.chars().count()).sum::<usize>() > 13_600,
            "the fixture has to be longer than the corpus's longest chapter"
        );
        let windows = plan_windows(&c, &DigestSettings::default());
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert_eq!(windows[0].from, 0);
        assert_eq!(windows[0].to, c.events.len());
    }

    #[test]
    fn no_budget_never_splits_however_long_the_chapter() {
        // `answer_tokens: 0` is the escape hatch back to the single-call path,
        // which is what makes it usable as a bisect: a chapter that behaves
        // differently windowed can be compared by changing one number.
        let c = chapter(4000);
        let windows = plan_windows(&c, &settings(0, 0, 0));
        assert_eq!(windows.len(), 1, "{} windows", windows.len());
        assert_partitions(&c, &windows);
    }

    #[test]
    fn a_chapter_over_the_budget_splits_and_keeps_every_event() {
        let c = chapter(400);
        let budget = 400 * CHARS_PER_TOKEN;
        let share = weight(&c.events).div_ceil(weight(&c.events).div_ceil(budget));
        let windows = plan_windows(&c, &settings(0, 0, 400));
        assert!(windows.len() > 1, "{windows:?}");
        assert_partitions(&c, &windows);
        // Every event here is one short paragraph, so a window closes one event
        // past its share at the most — an event too big to divide is its own
        // test below, because a window can never cut one.
        let last = windows.len() - 1;
        for (i, w) in windows.iter().enumerate() {
            assert!(
                w.chars <= share + share / OVERSHOOT,
                "a window over its ceiling: {w:?} vs {share}"
            );
            if i < last {
                // Every window but the last reaches the share it was given, so
                // the *number* of calls is the budget's, not one per paragraph.
                assert!(w.chars >= share, "a window under its share: {w:?}");
            }
        }
        assert!(
            windows.len() > 40,
            "the split is the budget's, not one window per paragraph: {}",
            windows.len()
        );
    }

    #[test]
    fn a_sentence_ceiling_splits_a_chapter_the_budget_would_not() {
        // An operator asking for small windows gets them even when the answer
        // would have fit: the setting is a ceiling of its own, not a hint to
        // the budget.
        let c = chapter(30);
        let windows = plan_windows(&c, &settings(6, 0, DEFAULT_ANSWER_TOKENS));
        assert_partitions(&c, &windows);
        assert!(
            windows.len() >= 10,
            "6 sentences is two lines, so 30 lines is at least 10 windows: {}",
            windows.len()
        );
        for w in &windows {
            assert!(w.sentences >= 6, "every window met the ceiling: {w:?}");
        }
    }

    #[test]
    fn a_paragraph_that_never_ends_a_sentence_still_closes_a_window() {
        // A crawl that lost its punctuation is a real shape in this corpus. The
        // sentence rule can never fire on it, so the character rule's ceiling is
        // the only thing that keeps windows finite — and a single event larger
        // than a whole window becomes a window of its own rather than being
        // split, which would break the source contract.
        let giant = "t".repeat(900);
        let text = format!("{giant}\nđoạn nhỏ một. đoạn nhỏ hai.\n");
        let c = super::super::prepare_chapter(&text);
        assert_eq!(c.events.len(), 2);
        let windows = plan_windows(&c, &settings(0, 0, 100));
        assert_partitions(&c, &windows);
        assert_eq!(windows.len(), 2, "the giant is a window of its own: {windows:?}");
        assert_eq!(windows[0].events, 1);
        assert!(windows[0].chars > 100 * CHARS_PER_TOKEN / 2);
    }

    #[test]
    fn windows_carry_the_chapters_own_ids() {
        // The merge depends on this: the script written for a window has to
        // answer the same ids the chapter's own view named, or the source gate
        // on the next pass validates a different chapter.
        let c = chapter(40);
        let windows = plan_windows(&c, &settings(6, 0, DEFAULT_ANSWER_TOKENS));
        assert!(windows.len() > 1);
        let mut seen: Vec<String> = Vec::new();
        for (i, w) in windows.iter().enumerate() {
            let slice = w.prepared(&c);
            assert_eq!(slice.events.len(), w.events, "window {i}");
            assert_eq!(
                slice.events[0].id, c.events[w.from].id,
                "window {i} starts where the chapter does"
            );
            assert!(!slice.unbalanced, "a window does not re-report the chapter");
            for (j, e) in slice.events.iter().enumerate() {
                assert_eq!(e.id, c.events[w.from + j].id, "window {i} event {j}");
                assert_eq!(e.kind, c.events[w.from + j].kind);
                assert_eq!(e.text, c.events[w.from + j].text);
            }
            // The prompt sees the same JSON the whole-chapter path renders.
            assert!(slice.prompt_json.contains(&format!("\"{}\"", slice.events[0].id)));
            seen.extend(slice.events.iter().map(|e| e.id.clone()));
        }
        let all: Vec<String> = c.events.iter().map(|e| e.id.clone()).collect();
        assert_eq!(seen, all, "every event, once, in source order");
    }

    #[test]
    fn sentence_ends_counts_marks_and_reports_finality() {
        assert_eq!(sentence_ends("Một câu."), (1, true));
        assert_eq!(sentence_ends("Một câu. Hai câu!"), (2, true));
        assert_eq!(sentence_ends("Một câu. Chưa hết"), (1, false));
        assert_eq!(sentence_ends("Không có dấu nào"), (0, false));
        assert_eq!(sentence_ends(""), (0, false));
        // A spoken line ends `…trượt tay."` and the closing quote is not a
        // sentence of its own.
        assert_eq!(sentence_ends("Hắn nói. \"Được thôi.\""), (2, true));
        assert_eq!(sentence_ends("Hắn nói \"Được thôi\""), (0, false));
        // An ellipsis is one sentence end, however it is spelled.
        assert_eq!(sentence_ends("Hắn ngập ngừng…"), (1, true));
    }

    // ───────────────────────── a real book, end to end ─────────────────────────

    /// A real chapter of the corpus: a live page's capture, shipped beside the
    /// page as its golden.
    ///
    /// Used as the *publisher's* text rather than as a comparison. The question
    /// this module exists for is "will my chapter split?", and synthetic
    /// paragraphs of a convenient length answer a question nobody asked — the
    /// real shape of a chapter is short paragraphs with dialogue in them, and
    /// that is what decides how many segments it produces.
    const REAL_CHAPTER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/crawl/truyencom-chapter.txt"
    ));

    /// The sample EPUB crawler, read from `samples/crawl/` rather than inlined:
    /// this test is a gate for that file as much as for the budget. Outside
    /// `adapters/`, because an EPUB is a format and no adapter's sites own it.
    fn epub_crawler() -> String {
        std::fs::read_to_string(format!(
            "{}/../../../samples/crawl/epub.lua",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("reading samples/crawl/epub.lua: {e}"))
    }

    /// A book, written as a real ZIP: container, manifest, spine, XHTML.
    ///
    /// **Not** shared with `crawl::epub`'s own builder, for the reason
    /// `crawl::script_tests` gives about its copy: a builder two suites share
    /// can hold a bug both suites agree with. Here this one is the only thing
    /// between the test and a claim about books on disk.
    fn book(path: &Path, chapters: &[(&str, String)]) {
        use std::io::Write;
        let file = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let o: zip::write::FileOptions<()> = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file("mimetype", o).unwrap();
        w.write_all(b"application/epub+zip").unwrap();
        w.start_file("META-INF/container.xml", o).unwrap();
        w.write_all(
            br#"<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
<rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        )
        .unwrap();
        let mut items = String::new();
        let mut refs = String::new();
        for (i, (name, body)) in chapters.iter().enumerate() {
            let id = format!("c{i}");
            items.push_str(&format!(r#"<item id="{id}" href="text/{name}.xhtml"/>"#));
            refs.push_str(&format!(r#"<itemref idref="{id}"/>"#));
            w.start_file(format!("OEBPS/text/{name}.xhtml"), o).unwrap();
            w.write_all(
                format!("<html><head><title>{name}</title></head><body>{body}</body></html>")
                    .as_bytes(),
            )
            .unwrap();
        }
        w.start_file("OEBPS/content.opf", o).unwrap();
        w.write_all(
            format!(
                r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><manifest>{items}</manifest><spine>{refs}</spine></package>"#
            )
            .as_bytes(),
        )
        .unwrap();
        w.finish().unwrap();
    }

    /// A chapter's paragraphs as the XHTML a publisher writes: one `<p>` each.
    fn xhtml(text: &str) -> String {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                // Ampersand first, or the ampersands this introduces are
                // escaped a second time.
                format!("<p>{}</p>", l.trim().replace('&', "&amp;").replace('<', "&lt;"))
            })
            .collect()
    }

    /// **The whole path, on a real book, with nothing stubbed.** A ZIP on disk,
    /// the shipped Lua crawler, the real Lua engine, the shared crawl boundary,
    /// the digest's own preparer, and the default budget — in that order.
    ///
    /// The question an operator asking for EPUB support actually has is "what
    /// will this do to my chapters?", and no unit test on either side answers
    /// it: the crawl tests stop at the text, and the budget tests start at
    /// prose that never went through a book. The seam between them is exactly
    /// where a book would be mishandled — a paragraph that survives the ZIP
    /// walk and then splits differently from a crawled page, or a chapter that
    /// turns out to need four calls because a book's paragraphs are shorter
    /// than the fixture's.
    #[test]
    fn what_the_default_budget_does_to_a_chapter_read_out_of_an_epub() {
        let dir = std::env::temp_dir().join(format!("bm-epub-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Chapter one is a real chapter. Chapters two and three are several of
        // them welded together, which is what a "chapter" becomes in the books
        // this pipeline is pointed at when the publisher merged several — and
        // three copies is deliberately *not* a whole number of windows, so the
        // cut lands inside a chapter rather than on the seam between copies.
        let one = xhtml(REAL_CHAPTER);
        let copies = |k: usize| (0..k).map(|_| one.clone()).collect::<String>();
        let three = copies(3);
        let eight = copies(8);
        book(
            &dir.join("book.epub"),
            &[("mot", one), ("hai", three), ("ba", eight)],
        );

        let mut spec = CrawlSpec {
            engine: "lua".into(),
            script: "epub.lua".into(),
            source: epub_crawler(),
            params: serde_json::Map::new(),
            ..Default::default()
        };
        spec.read_root = dir.clone();
        spec.params
            .insert("epub".into(), serde_json::json!("book.epub"));
        let provider = Provider::new(&spec);
        // The book knows its own length, so a range can be trimmed before it is
        // enqueued — the first thing a range this long would get wrong.
        let found = provider.discover(1, 9).unwrap().expect("a book");
        assert_eq!(found.total, Some(3));
        assert_eq!(found.chapters.len(), 3);

        let settings = DigestSettings::default();
        let budget_chars = DEFAULT_ANSWER_TOKENS as usize * CHARS_PER_TOKEN;
        println!("\nbudget: {DEFAULT_ANSWER_TOKENS} tokens = {budget_chars} weighted chars");
        println!(
            "{:>8}  {:>6}  {:>7}  {:>8}  {:>7}  {:>5}  windows",
            "chapter", "chars", "events", "weight", "tokens", "calls"
        );
        let mut plans: Vec<(u32, PreparedChapter, Vec<Window>)> = Vec::new();
        for n in 1..=3 {
            let text = match provider.crawl(n, None, 1).unwrap().outcome {
                CrawlOutcome::Text { text, .. } => text,
                other => panic!("chapter {n} did not crawl: {other:?}"),
            };
            let prepared = super::super::prepare_chapter(&text);
            let windows = plan_windows(&prepared, &settings);
            assert_partitions(&prepared, &windows);
            for w in &windows {
                assert!(
                    tokens(w.chars) <= DEFAULT_ANSWER_TOKENS as usize,
                    "a window would overrun the answer budget: {w:?}"
                );
            }
            let chars: usize = prepared.events.iter().map(|e| e.text.chars().count()).sum();
            let w = weight(&prepared.events);
            println!(
                "{n:>8}  {chars:>6}  {:>7}  {w:>8}  {:>7}  {:>5}  {:?}",
                prepared.events.len(),
                tokens(w),
                windows.len(),
                windows.iter().map(|x| x.events).collect::<Vec<_>>(),
            );
            plans.push((n, prepared, windows));
        }

        // (1) The headline, and the reason an EPUB does not need a setting of
        // its own: a real chapter of this corpus is **one** digest call, at
        // well under half the budget. A book's chapters are no longer than a
        // site's, because they are the same prose from the same author.
        let (_, real, real_windows) = &plans[0];
        assert_eq!(real_windows.len(), 1, "{real_windows:?}");
        assert!(
            tokens(real_windows[0].chars) * 2 < DEFAULT_ANSWER_TOKENS as usize,
            "a real chapter should sit under half the budget, not at its edge: {}",
            tokens(real_windows[0].chars)
        );
        // And it is not a chapter of three paragraphs: the prose arrives whole,
        // and the dialogue inside it is split out as its own events.
        assert!(real.events.len() > 50, "{}", real.events.len());
        assert!(
            real.events.iter().any(|e| e.kind == "dialogue"),
            "a chapter with quoted speech splits it out"
        );
        // And it is text, not markup: the tag stripper ran, or every event would
        // be one `<p>`-wrapped blob and the digest would be asked to speak it.
        assert!(
            !real
                .events
                .iter()
                .any(|e| e.text.contains('<') || e.text.contains("&amp;")),
            "a chapter read out of a book is text, not markup"
        );

        // (2) The cut, on chapters long enough to need one. Every window ends on
        // a sentence, which is the property that makes a seam between two
        // windows a seam between two sentences rather than mid-clause.
        for (n, long, windows) in &plans[1..] {
            assert!(windows.len() > 1, "chapter {n} should need a cut");
            println!("\n  chapter {n}: {} events, {} windows", long.events.len(), windows.len());
            println!("  window  events  weight  tokens    first    last  it ends on");
            for (i, w) in windows.iter().enumerate() {
                let first = &long.events[w.from];
                let last = &long.events[w.to - 1];
                assert!(
                    sentence_ends(&last.text).1,
                    "window {} of chapter {n} ends mid-sentence: {:?}",
                    i + 1,
                    last.text
                );
                println!(
                    "  {:>5}  {:>6}  {:>6}  {:>6}  {:>6}  {:>6}  {}",
                    i + 1,
                    w.events,
                    w.chars,
                    tokens(w.chars),
                    first.id,
                    last.id,
                    crate::util::head_chars(&last.text, 44)
                );
            }
        }
        let (_, long, _) = &plans[1];
        // (3) What a segment is, since "how many segments" is the operator's
        // actual question: one per event, keyed by the chapter's own ids.
        println!("\n  the first segments of the long chapter:");
        for e in long.events.iter().take(6) {
            println!(
                "  {} {:>9}  {}",
                e.id,
                e.kind,
                crate::util::head_chars(&e.text, 56)
            );
        }
        assert_eq!(
            long.events.len(),
            real.events.len() * 3,
            "three copies of a chapter make three copies of its events"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A real book, all the way to its segments, when there is one to point
    /// at.** Skipped unless `BM_BOOK` names an `.epub`, because a test that
    /// needs a 7 MB file nobody else has is not a test.
    ///
    /// Every other test here builds its own prose, which is right for testing
    /// and useless for looking at: the question an operator has about a book
    /// is "what will the narrator actually be asked to say", and that is ten
    /// lines of output on a real chapter and not a number in an assert.
    ///
    ///   BM_BOOK=/abs/path/book.epub cargo test -p bm-core --lib \
    ///     what_the_segments_of_a_real_book_look_like -- --nocapture
    #[test]
    fn what_the_segments_of_a_real_book_look_like() {
        let Ok(book) = std::env::var("BM_BOOK") else {
            return;
        };
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .expect("the workspace root");
        let book = std::path::PathBuf::from(&book)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(&book));
        let mut spec = CrawlSpec {
            engine: "lua".into(),
            script: "epub.lua".into(),
            source: epub_crawler(),
            params: serde_json::Map::new(),
            ..Default::default()
        };
        spec.read_root = root.clone();
        spec.params.insert(
            "epub".into(),
            serde_json::json!(book.to_string_lossy()),
        );
        let provider = Provider::new(&spec);
        let total = provider
            .discover(1, 999)
            .unwrap()
            .and_then(|d| d.total)
            .unwrap_or(0);
        println!("\n\x1b[1m{}\x1b[0m", book.display());
        println!("\x1b[1m{total} chapters, budget {DEFAULT_ANSWER_TOKENS} tokens = {} weighted chars\x1b[0m",
            DEFAULT_ANSWER_TOKENS as usize * CHARS_PER_TOKEN);

        for n in 1..=3u32 {
            if n > total {
                break;
            }
            let CrawlOutcome::Text { text, .. } = provider.crawl(n, None, 1).unwrap().outcome else {
                continue;
            };
            let prepared = super::super::prepare_chapter(&text);
            let windows = plan_windows(&prepared, &DigestSettings::default());
            let chars: usize = prepared.events.iter().map(|e| e.text.chars().count()).sum();
            let w = weight(&prepared.events);
            let sentences: u32 = prepared
                .events
                .iter()
                .map(|e| sentence_ends(&e.text).0)
                .sum();
            println!(
                "\n\x1b[1m=== chapter {n} ===\x1b[0m {chars} chars, {} events, {sentences} sentences, \
                 weight {w}, ~{} tokens, {} window(s)\n",
                prepared.events.len(),
                tokens(w),
                windows.len(),
            );
            println!(
                "  {:>5}  {:>9}  {:>6}  {:>4}  text",
                "id", "kind", "chars", "sent"
            );
            for e in &prepared.events {
                let head: String = e.text.chars().take(120).collect();
                println!(
                    "  {:>5}  {:>9}  {:>6}  {:>4}  {}",
                    e.id,
                    e.kind,
                    e.text.chars().count(),
                    sentence_ends(&e.text).0,
                    head
                );
            }
            for (i, win) in windows.iter().enumerate() {
                println!(
                    "  window {}: events {}..{}, {} weighted chars, ~{} tokens",
                    i + 1,
                    prepared.events[win.from].id,
                    prepared.events[win.to - 1].id,
                    win.chars,
                    tokens(win.chars)
                );
            }
            // The number that decides whether this book can be narrated at all:
            // how long one segment is, in the only unit a listener has.
            let longest = prepared
                .events
                .iter()
                .map(|e| e.text.chars().count())
                .max()
                .unwrap_or(0);
            println!(
                "  longest segment: {longest} chars (~{:.0}s of narration at 15 chars/s)",
                longest as f64 / 15.0
            );
        }

        // Every chapter, checked for the scanner's furniture and for a heading
        // that is still glued to its first sentence. Three chapters showing
        // clean is a sample; thirty-one is the book.
        if total > 0 {
            let mut dirty = 0usize;
            let mut split = 0usize;
            let mut chars = 0usize;
            for n in 1..=total {
                let CrawlOutcome::Text { text, .. } = provider.crawl(n, None, 1).unwrap().outcome
                else {
                    continue;
                };
                chars += text.chars().count();
                if text.contains("Goldenagato") || text.contains("mp4directs") {
                    dirty += 1;
                }
                // A heading on its own line is a first line of a few dozen
                // characters; one still glued to prose runs on into the chapter.
                let first = text.lines().next().unwrap_or_default();
                if first.len() <= 80 && first.to_lowercase().contains("chapter") {
                    split += 1;
                }
            }
            println!(
                "\n  all {total} chapters: {chars} chars, {dirty} still carrying the scan watermark, \
                 {split} with the heading split off"
            );
            assert_eq!(dirty, 0, "{dirty} chapters still narrate the scanner's watermark");
        }
    }

    #[test]
    fn the_estimate_is_conservative_in_the_safe_direction() {
        // 1000 characters of chapter text, one event.
        let c = chapter(1);
        let w = weight(&c.events);
        assert!(w > c.events[0].text.chars().count());
        assert!(
            tokens(w) >= w / 2,
            "characters never cost less than two per token"
        );
    }
}
