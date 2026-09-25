//! Stage 1 — fetch a chapter and reduce it to plain prose.
//!
//! Ported from `ingest.py`. The Python version leaned on BeautifulSoup with a
//! regex fallback; this is the fallback path, hardened. The heuristics are
//! deliberately identical (same start hints, same stop markers, same
//! boilerplate filters) so the extracted text matches what the legacy
//! pipeline produced.

use crate::util::atomic_write;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Lines that mark where the chapter body begins.
const START_HINTS: [&str; 4] = ["Chương 1", "Chương ", "Thượng bộ", "Một bộ"];

/// Lines that mark the end of the chapter body (site navigation).
const STOP_MARKERS: [&str; 5] = [
    "Chương trước",
    "Chương sau",
    "Truyện cùng thể loại",
    "Nền tảng đọc truyện",
    "Chính sách bảo mật",
];

/// Site boilerplate that survives the tag strip.
const JUNK: [&str; 6] = [
    "Storya",
    "thể loại",
    "Đọc online",
    "Cài đặt",
    "cập nhật",
    "miễn phí",
];

const SITE_BYLINE_PREFIX: &str = "Người Trên Vạn Người - Chương";

/// Tags whose closing boundary should become a line break.
const BLOCK_TAGS: [&str; 15] = [
    "br",
    "p",
    "div",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "section",
    "article",
    "main",
    "blockquote",
];

/// Download a page with a browser-ish user agent.
pub async fn fetch_html(url: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent("Mozilla/5.0")
        .build()
        .context("building http client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GET {url} -> HTTP {status}");
    }
    let body = resp.text().await.context("reading response body")?;
    Ok(body)
}

/// Remove `<tag>...</tag>` blocks entirely, content included.
/// Case-insensitive; used to drop `<script>` and `<style>` before stripping tags.
fn remove_block(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(&open) {
        let start = pos + rel;
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(rel_end) => {
                let end = start + rel_end;
                match html[end..].find('>') {
                    Some(rel_gt) => pos = end + rel_gt + 1,
                    None => {
                        pos = html.len();
                    }
                }
            }
            None => {
                pos = html.len();
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

/// Return the inside of the first `<tag>...</tag>` element, if present.
fn inner_of(html: &str, tag: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let start = lower.find(&open)?;
    let body_start = html[start..].find('>')? + start + 1;
    let end = lower[body_start..].find(&close)? + body_start;
    Some(html[body_start..end].to_string())
}

/// Replace tags with whitespace, emitting line breaks at block boundaries.
fn strip_tags(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'>' {
                j += 1;
            }
            if start <= html.len() && j <= html.len() {
                let inner = &html[start..j];
                let name: String = inner
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if BLOCK_TAGS.contains(&name.as_str()) {
                    out.push('\n');
                }
            }
            i = if j < bytes.len() { j + 1 } else { j };
        } else {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Decode the handful of entities that actually show up in story pages.
///
/// `pub(crate)`, not private: the digest's `prepare_chapter` runs the same
/// decoder over chapters read with [`read_local`]. Chapters crawled before the
/// generic numeric-entity support landed (the `&#x27;két&#x27;` shape) keep the
/// raw forms on disk, and the digest is the stage where the mismatch bites —
/// the model reads `&#x27;` and answers `'`, then the source gate refuses the
/// one-character difference for every racer and the chapter can never digest.
/// One decoder, run at the same boundary the crawler decodes at, so prepared
/// text and the model's natural output agree again.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        let mut matched = false;
        for (entity, repl) in [
            ("&nbsp;", " "),
            ("&quot;", "\""),
            ("&apos;", "'"),
            ("&#39;", "'"),
            ("&lt;", "<"),
            ("&gt;", ">"),
            ("&amp;", "&"),
        ] {
            if let Some(t) = tail.strip_prefix(entity) {
                out.push_str(repl);
                rest = t;
                matched = true;
                break;
            }
        }
        if !matched {
            // ponytail: generic numeric entities (&#39; &#x27;) — named list alone
            // leaves hex variants like &#x27; raw in source, model decodes to '
            // and source gate then fails verbatim compare. The body spans the
            // whole entity INCLUDING the leading `&` (tail starts at it): slicing
            // from 1 dropped the `&`, every `strip_prefix("&#…")` missed, and the
            // branch silently passed numeric entities through raw — which is how
            // `&#x27;két&#x27;` reached disk after this code existed.
            if let Some(semi) = tail.find(';').filter(|&p| p < 10) {
                let body = &tail[..semi];
                let code = if let Some(hex) = body
                    .strip_prefix("&#x")
                    .or_else(|| body.strip_prefix("&#X"))
                {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(dec) = body.strip_prefix("&#") {
                    dec.parse::<u32>().ok()
                } else {
                    None
                };
                if let Some(c) = code.and_then(char::from_u32) {
                    out.push(c);
                    rest = &tail[semi + 1..];
                    continue;
                }
            }
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

fn is_chapter_heading(line: &str) -> bool {
    match line.strip_prefix("Chương ") {
        Some(rest) => rest
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false),
        None => false,
    }
}

/// True for a standalone line copied from Storya rather than the novel.
///
/// These markers are deliberately conjunctive. A story may legitimately say
/// that a task was completed, include an author's `PS:`, or mention a platform;
/// only the site-shaped combinations are removed. In particular, the false
/// completion footer needs all three fingerprints (`Hệ thống`, `chiếc đỉnh`,
/// `hậu cung`, and `Truyện đã hoàn thành`) so ordinary prose is never mistaken
/// for site metadata.
fn is_storya_artifact(line: &str) -> bool {
    let line = line.trim();
    let lower = line.to_lowercase();
    let normalized = lower.split_whitespace().collect::<Vec<_>>().join(" ");

    normalized == "cài đặt đọc"
        || normalized == "người trên vạn người"
        || ((lower.contains("đọc online")
            || lower.contains("cập nhật nhanh nhất")
            || lower.contains("nền tảng đọc truyện"))
            && lower.contains("storya"))
        || (lower.contains("hệ thống")
            && lower.contains("chiếc đỉnh")
            && lower.contains("hậu cung")
            && lower.contains("truyện đã hoàn thành"))
        || lower.starts_with("ps:")
        || lower.starts_with("p/s:")
}

/// Remove known site metadata and decode entities at the chapter boundary.
///
/// This runs for freshly crawled pages, existing local chapter files, and the
/// digest preparer. Keeping one function at all three boundaries is what makes
/// a workspace created by an older binary safe: bad source data is repaired on
/// read instead of being blessed by the source-alignment gate and spoken.
pub(crate) fn sanitize_chapter_text(text: &str) -> String {
    let decoded = decode_entities(text);
    let mut paragraphs = Vec::new();
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() || is_storya_artifact(line) {
            continue;
        }
        // Storya repeats the chapter as `81. Chương 81: ...` beside the real
        // `Chương 81: ...` headline. The numbered copy is metadata; the clean
        // headline is retained and spoken once by the title renderer.
        let bytes = line.as_bytes();
        let numbered_heading = bytes.first().is_some_and(u8::is_ascii_digit)
            && line
                .split_once(". ")
                .map(|(_, rest)| is_chapter_heading(rest))
                .unwrap_or(false);
        if numbered_heading {
            continue;
        }
        paragraphs.push(line);
    }
    if paragraphs.is_empty() {
        return String::new();
    }
    format!("{}\n", paragraphs.join("\n\n"))
}

/// Extract readable chapter text from a story page.
pub fn clean_storya_html(html: &str) -> String {
    let html = remove_block(html, "script");
    let html = remove_block(&html, "style");
    // Prefer the semantic container when the site provides one.
    let scoped = inner_of(&html, "article")
        .or_else(|| inner_of(&html, "main"))
        .or_else(|| inner_of(&html, "body"))
        .unwrap_or(html);
    let text = decode_entities(&strip_tags(&scoped));

    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Slice between the chapter body start and the nav/footer junk.
    let mut start = 0usize;
    for (i, ln) in lines.iter().enumerate() {
        if START_HINTS.iter().any(|h| ln.contains(h)) && ln.chars().count() < 120 {
            start = i.saturating_sub(1);
            break;
        }
        let lower = ln.to_lowercase();
        if ln.contains("Thái Cực Quyền") || lower.contains("xuyên không") {
            start = i;
            break;
        }
    }
    let end = lines
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, l)| STOP_MARKERS.iter().any(|m| l.contains(m)))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());

    let mut body: Vec<String> = lines[start..end].to_vec();
    body.retain(|l| !l.starts_with(SITE_BYLINE_PREFIX));

    let title = body.iter().find(|l| is_chapter_heading(l)).cloned();
    let start_idx = body
        .iter()
        .position(|l| {
            l.chars().count() > 40 && !is_chapter_heading(l) && !JUNK.iter().any(|k| l.contains(k))
        })
        .unwrap_or(0);

    let mut out: Vec<String> = Vec::new();
    if let Some(t) = title {
        out.push(t);
    }
    out.extend(body[start_idx..].iter().cloned());

    let joined = out.join("\n\n");
    sanitize_chapter_text(&joined)
}

/// Read a chapter that is already on disk.
pub fn read_local(path: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sanitize_chapter_text(&text))
}

/// Fetch (or read) a chapter and persist the cleaned text.
pub async fn ingest(url: Option<&str>, file: Option<&Path>, out: &Path) -> Result<PathBuf> {
    let text = match (url, file) {
        (Some(u), _) => clean_storya_html(&fetch_html(u, 30).await?),
        (None, Some(f)) => read_local(f)?,
        (None, None) => anyhow::bail!("provide a url or a file"),
    };
    if text.chars().count() < 200 {
        anyhow::bail!(
            "ingested text suspiciously short ({} chars) — selector may have missed",
            text.chars().count()
        );
    }
    atomic_write(out, &text)?;
    Ok(out.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_styles_and_nav() {
        let sample = "<html><body><article><h1>Chương 1</h1>\
            <p>Thái Cực Quyền hay.</p><p>\"Chào!\" nàng nói.</p>\
            <nav>Chương trước</nav><script>var x = 1 < 2;</script>\
            </article></body></html>";
        let out = clean_storya_html(sample);
        assert!(out.contains("Thái Cực Quyền hay."), "{out}");
        assert!(out.contains("\"Chào!\" nàng nói."), "{out}");
        assert!(!out.contains("Chương trước"), "{out}");
        assert!(!out.contains("var x"), "script body leaked: {out}");
    }

    #[test]
    fn removes_script_blocks_before_tag_stripping() {
        let html = "<p>a</p><script>if (1 < 2) { document.write('<p>junk</p>'); }</script><p>b</p>";
        let out = clean_storya_html(html);
        assert!(out.contains('a') && out.contains('b'));
        assert!(!out.contains("junk"), "{out}");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(
            decode_entities("a &amp; b &lt;c&gt; &quot;d&quot;"),
            "a & b <c> \"d\""
        );
        assert_eq!(decode_entities("100% &nbsp;ok"), "100%  ok");
        assert_eq!(decode_entities("bare & ampersand"), "bare & ampersand");
    }

    #[test]
    fn decodes_numeric_entities_in_both_radixes() {
        // The ch79 shape, verbatim: the generic branch used to slice off the
        // leading `&`, so every numeric entity passed through raw — and the
        // digest's source gate then refused the decoded answer the model
        // naturally gives, stranding the chapter on every racer.
        assert_eq!(
            decode_entities("&#x27;két&#x27; một tiếng"),
            "'két' một tiếng"
        );
        assert_eq!(decode_entities("&#39;hắn&#39; nói"), "'hắn' nói");
        assert_eq!(decode_entities("&quot;Ừm&quot;"), "\"Ừm\"");
        // A semicolon nearby but no entity body stays raw, not mangled.
        assert_eq!(decode_entities("a & b; c"), "a & b; c");
    }

    #[test]
    fn chapter_heading_detection() {
        assert!(is_chapter_heading("Chương 12"));
        assert!(is_chapter_heading("Chương 12: Tên"));
        assert!(!is_chapter_heading("Chương trước"));
        assert!(!is_chapter_heading("Mở đầu"));
    }

    #[test]
    fn byline_and_junk_are_dropped() {
        let html = format!(
            "<body><h1>Chương 2: Tên chương</h1><p>{SITE_BYLINE_PREFIX} 2</p>\
             <p>Storya thể loại Đọc online</p>\
             <p>Đây là một câu văn dài đủ để vượt qua ngưỡng bốn mươi ký tự.</p></body>"
        );
        let out = clean_storya_html(&html);
        assert!(out.starts_with("Chương 2: Tên chương"), "{out}");
        assert!(!out.contains(SITE_BYLINE_PREFIX), "{out}");
        assert!(!out.contains("Đọc online"), "{out}");
        assert!(out.contains("Đây là một câu văn dài"), "{out}");
    }

    #[test]
    fn sanitizes_storya_metadata_without_touching_story_prose() {
        let raw = "Chương 81: Liền phòng ngự\n\n81. Chương 81: Liền phòng ngự\n\nCài đặt đọc\n\nNgười Trên Vạn Người\n\nNgười Trên Vạn Người thuộc thể loại Xuyên Không, chương 81 tiếp tục diễn biến hấp dẫn của câu chuyện. Đọc online miễn phí, cập nhật nhanh nhất tại Storya - nền tảng đọc truyện chất lượng cao.\n\nHắn đã hoàn thành nhiệm vụ.\n\nHệ thống thực thể dưới dạng chiếc đỉnh. Main bá, không hậu cung. Truyện đã hoàn thành\n\nPS: sẽ cập nhật sau.\n\nCánh cửa k&#x27;két&#x27; một tiếng.";
        let out = sanitize_chapter_text(raw);

        assert!(out.starts_with("Chương 81: Liền phòng ngự\n\nHắn đã hoàn thành nhiệm vụ."));
        assert!(out.ends_with("Cánh cửa k'két' một tiếng.\n"));
        assert!(!out.contains("81. Chương"));
        assert!(!out.contains("Storya"));
        assert!(!out.contains("Truyện đã hoàn thành"));
        assert!(!out.contains("PS:"));
        assert!(!out.contains("&#"));
    }

    #[tokio::test]
    async fn ingest_rejects_suspiciously_short_text() {
        let dir = std::env::temp_dir().join("bm-crawl-test");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("tiny.txt");
        std::fs::write(&src, "too short").unwrap();
        let out = dir.join("out.txt");
        let err = ingest(None, Some(&src), &out).await.unwrap_err();
        assert!(err.to_string().contains("suspiciously short"), "{err}");
    }

    #[tokio::test]
    async fn ingest_roundtrips_a_local_file() {
        let dir = std::env::temp_dir().join("bm-crawl-test");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("ok.txt");
        let body = "Chương 5: Thử nghiệm\n\n".to_string()
            + &"Nội dung chương này đủ dài để vượt qua ngưỡng kiểm tra. ".repeat(6);
        std::fs::write(&src, &body).unwrap();
        let out = dir.join("ok-out.txt");
        ingest(None, Some(&src), &out).await.unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.contains("Thử nghiệm"));
        assert!(written.ends_with('\n'));
    }
}
