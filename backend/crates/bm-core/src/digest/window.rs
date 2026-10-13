//! Splitting one chapter into the windows a single digest answer can hold.

use super::{PreparedChapter, PreparedEvent};
use crate::config::DigestSettings;
use serde_json::{json, Value};

/// Characters of answer the estimate spends per token.
pub(crate) const CHARS_PER_TOKEN: usize = 2;

/// The JSON a segment answering one event costs on top of its own text: the id
const SEGMENT_OVERHEAD: usize = 64;

/// The largest a window may grow past its character target while waiting for a
const OVERSHOOT: usize = 2;

/// One window: a half-open range of the chapter's events, with the counts the
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
    pub sentences: u32,
}

impl Window {
    /// This window's events as a `PreparedChapter` of their own, carrying the
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
        // Parity is a fact about the WHOLE chapter and is decided on the whole
        unbalanced_at: None,
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
    sentences: u32,
    /// Close once the window has spent this many weighted characters.
    chars: usize,
}

impl Target {
    /// Resolve the settings into the budget one chapter is actually cut with.
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
    fn ceiling(&self) -> usize {
        self.chars + self.chars / OVERSHOOT
    }
}

/// Cut `chapter` into the windows one digest answer can hold.
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
    fn chapter(lines: usize) -> PreparedChapter {
        let text: String = (0..lines)
            .map(|i| format!("Đoạn văn số {i} mở đầu câu chuyện. Câu thứ hai ở đây. Câu thứ ba.\n"))
            .collect();
        super::super::prepare_chapter(&text)
    }

    /// A chapter of longer paragraphs — the shape real prose has, and the one
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
    /// Not a claim about any one chapter — a table, because "will my chapter
    /// split?" is the question the whole windowing module exists to answer and
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
        assert_eq!(windows.len(), 1);
        assert_eq!(
            windows[0],
            Window {
                from: 0,
                to: 0,
                events: 0,
                chars: 0,
                sentences: 0
            }
        );
    }

    #[test]
    fn a_chapter_under_the_budget_is_one_window() {
        // The parity case, sized against the real corpus: its longest chapter is
        let c = prose(75);
        assert!(
            c.events
                .iter()
                .map(|e| e.text.chars().count())
                .sum::<usize>()
                > 13_600,
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
        let last = windows.len() - 1;
        for (i, w) in windows.iter().enumerate() {
            assert!(
                w.chars <= share + share / OVERSHOOT,
                "a window over its ceiling: {w:?} vs {share}"
            );
            if i < last {
                // Every window but the last reaches the share it was given, so
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
        let giant = "t".repeat(900);
        let text = format!("{giant}\nđoạn nhỏ một. đoạn nhỏ hai.\n");
        let c = super::super::prepare_chapter(&text);
        assert_eq!(c.events.len(), 2);
        let windows = plan_windows(&c, &settings(0, 0, 100));
        assert_partitions(&c, &windows);
        assert_eq!(
            windows.len(),
            2,
            "the giant is a window of its own: {windows:?}"
        );
        assert_eq!(windows[0].events, 1);
        assert!(windows[0].chars > 100 * CHARS_PER_TOKEN / 2);
    }

    #[test]
    fn windows_carry_the_chapters_own_ids() {
        // The merge depends on this: the script written for a window has to
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
            assert!(
                slice.unbalanced_at.is_none(),
                "a window does not re-report the chapter"
            );
            for (j, e) in slice.events.iter().enumerate() {
                assert_eq!(e.id, c.events[w.from + j].id, "window {i} event {j}");
                assert_eq!(e.kind, c.events[w.from + j].kind);
                assert_eq!(e.text, c.events[w.from + j].text);
            }
            // The prompt sees the same JSON the whole-chapter path renders.
            assert!(slice
                .prompt_json
                .contains(&format!("\"{}\"", slice.events[0].id)));
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
    const REAL_CHAPTER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/crawl/truyencom-chapter.txt"
    ));

    /// The example EPUB crawler, read from `crawlers/examples/` rather than
    fn epub_crawler() -> String {
        std::fs::read_to_string(format!(
            "{}/../../../crawlers/examples/epub.lua",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("reading crawlers/examples/epub.lua: {e}"))
    }

    /// A book, written as a real ZIP: container, manifest, spine, XHTML.
    fn book(path: &Path, chapters: &[(&str, String)]) {
        use std::io::Write;
        let file = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let o: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
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
                format!(
                    "<p>{}</p>",
                    l.trim().replace('&', "&amp;").replace('<', "&lt;")
                )
            })
            .collect()
    }

    /// **The whole path, on a real book, with nothing stubbed.** A ZIP on disk,
    /// The question an operator asking for EPUB support actually has is "what
    /// will this do to my chapters?", and no unit test on either side answers
    #[test]
    fn what_the_default_budget_does_to_a_chapter_read_out_of_an_epub() {
        let dir = std::env::temp_dir().join(format!("bm-epub-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Chapter one is a real chapter. Chapters two and three are several of
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
        let (_, real, real_windows) = &plans[0];
        assert_eq!(real_windows.len(), 1, "{real_windows:?}");
        assert!(
            tokens(real_windows[0].chars) * 2 < DEFAULT_ANSWER_TOKENS as usize,
            "a real chapter should sit under half the budget, not at its edge: {}",
            tokens(real_windows[0].chars)
        );
        // And it is not a chapter of three paragraphs: the prose arrives whole,
        assert!(real.events.len() > 50, "{}", real.events.len());
        assert!(
            real.events.iter().any(|e| e.kind == "dialogue"),
            "a chapter with quoted speech splits it out"
        );
        // And it is text, not markup: the tag stripper ran, or every event would
        assert!(
            !real
                .events
                .iter()
                .any(|e| e.text.contains('<') || e.text.contains("&amp;")),
            "a chapter read out of a book is text, not markup"
        );

        // (2) The cut, on chapters long enough to need one. Every window ends on
        for (n, long, windows) in &plans[1..] {
            assert!(windows.len() > 1, "chapter {n} should need a cut");
            println!(
                "\n  chapter {n}: {} events, {} windows",
                long.events.len(),
                windows.len()
            );
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
        spec.params
            .insert("epub".into(), serde_json::json!(book.to_string_lossy()));
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
            let CrawlOutcome::Text { text, .. } = provider.crawl(n, None, 1).unwrap().outcome
            else {
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
                let first = text.lines().next().unwrap_or_default();
                if first.len() <= 80 && first.to_lowercase().contains("chapter") {
                    split += 1;
                }
            }
            println!(
                "\n  all {total} chapters: {chars} chars, {dirty} still carrying the scan watermark, \
                 {split} with the heading split off"
            );
            assert_eq!(
                dirty, 0,
                "{dirty} chapters still narrate the scanner's watermark"
            );
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
