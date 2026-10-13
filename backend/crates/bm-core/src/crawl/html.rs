//! The HTML half of the host ABI: what a crawl script may ask of a page.
//! [`readable`] is the generic escape hatch — the "I have no idea what this
//! site's container is called" path. It is a heuristic on purpose, and it is

use anyhow::{anyhow, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One element, as a script sees it: its text, its own markup, its attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    /// Visible text, whitespace squeezed and trimmed.
    pub text: String,
    /// The element's own markup, so a script can re-select inside it.
    pub html: String,
    /// Every attribute, `class` and `href` included.
    #[serde(default)]
    pub attrs: BTreeMap<String, String>,
}

impl Element {
    /// One attribute, or "" — the spelling a script actually wants
    pub fn attr(&self, name: &str) -> String {
        self.attrs.get(name).cloned().unwrap_or_default()
    }
}

fn parse_selector(sel: &str) -> Result<Selector> {
    Selector::parse(sel).map_err(|e| anyhow!("selector {sel:?} is not valid CSS: {e}"))
}

/// The text of the first match, whitespace-squeezed. Empty when nothing
pub fn select(html: &str, sel: &str) -> Result<String> {
    let selector = parse_selector(sel)?;
    let doc = Html::parse_document(html);
    Ok(doc
        .select(&selector)
        .next()
        .map(|el| squeeze(&el.text().collect::<String>()))
        .unwrap_or_default())
}

/// The text of the first match, **as prose**: block boundaries become blank
pub fn select_text(html: &str, sel: &str) -> Result<String> {
    let selector = parse_selector(sel)?;
    let doc = Html::parse_document(html);
    Ok(doc
        .select(&selector)
        .next()
        .map(|el| blocks(&el.inner_html()))
        .unwrap_or_default())
}

/// Every match, in document order.
pub fn select_all(html: &str, sel: &str) -> Result<Vec<Element>> {
    let selector = parse_selector(sel)?;
    let doc = Html::parse_document(html);
    Ok(doc.select(&selector).map(element_of).collect::<Vec<_>>())
}

/// One element's attributes, text and markup.
fn element_of(el: scraper::ElementRef<'_>) -> Element {
    let attrs: BTreeMap<String, String> = el
        .value()
        .attrs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Element {
        text: squeeze(&el.text().collect::<String>()),
        html: el.inner_html(),
        attrs,
    }
}

/// Resolve `href` against the page it was found on.
pub fn abs_url(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return String::new();
    }
    match reqwest::Url::parse(base).and_then(|b| b.join(href)) {
        Ok(u) => u.to_string(),
        Err(_) => href.to_string(),
    }
}

/// The generic "just give me the prose" heuristic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readable {
    #[serde(default)]
    pub title: String,
    pub text: String,
}

/// Containers worth scoring, best guess last-resort is `<body>`.
const CANDIDATES: [&str; 6] = [
    "article",
    "main",
    "div[id*=content]",
    "div[class*=content]",
    "div[class*=chapter]",
    "body",
];

/// Extract the prose without knowing the site.
pub fn readable(html: &str) -> Readable {
    let doc = Html::parse_document(html);
    let title = ["h1", "title"]
        .iter()
        .filter_map(|s| parse_selector(s).ok())
        .find_map(|s| doc.select(&s).next())
        .map(|el| squeeze(&el.text().collect::<String>()))
        .unwrap_or_default();

    let link_sel = parse_selector("a").ok();
    let best = CANDIDATES
        .iter()
        .filter_map(|c| parse_selector(c).ok())
        .flat_map(|s| doc.select(&s).collect::<Vec<_>>())
        .map(|el| {
            let text = el.text().collect::<String>();
            let links: usize = link_sel
                .as_ref()
                .map(|s| {
                    el.select(s)
                        .map(|a| a.text().collect::<String>().chars().count())
                        .sum()
                })
                .unwrap_or(0);
            let score = text.chars().count().saturating_sub(links * 3);
            (score, element_of(el))
        })
        .filter(|(_, el)| !el.text.is_empty())
        .max_by_key(|(score, _)| *score)
        .map(|(_, el)| el);

    let text = best
        .map(|el| blocks(&el.html))
        .unwrap_or_else(|| crate::crawl::strip_tags_raw(html));
    Readable {
        title,
        text: squeeze_lines(&text),
    }
}

/// Block-level text of a fragment, one blank line between blocks.
fn blocks(fragment: &str) -> String {
    super::sanitize_chapter_text(&super::strip_tags_raw(fragment))
}

/// Collapse runs of blank lines to one, trim the ends.
fn squeeze_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            // One blank line between paragraphs, never two.
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push("");
            }
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Whitespace inside one element is not meaning: sites break lines mid-sentence.
pub fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"
        <html><head><title>Book index</title></head><body>
        <nav><a href="/start">Start</a><a href="/end">End</a></nav>
        <article class="chapter">
          <h1>Chương 34: Bí ẩn</h1>
          <div class="body">
            <p>Hắn bước vào phòng.</p>
            <p>Cánh cửa kêu một tiếng.</p>
          </div>
        </article>
        <div id="content-list">
          <a href="chuong-1">Chương 1</a>
          <a href="/truyen/x/chuong-2">Chương 2</a>
          <a href="https://other.example/c3">Chương 3</a>
        </div>
        </body></html>"#;

    #[test]
    fn select_reads_text_and_select_all_reads_attributes() {
        assert_eq!(select(PAGE, "h1").unwrap(), "Chương 34: Bí ẩn");
        assert_eq!(
            select(PAGE, ".body p").unwrap(),
            "Hắn bước vào phòng.",
            "first match wins"
        );
        // No match is empty, not an error: the length guard is the diagnosis.
        assert_eq!(select(PAGE, "table td").unwrap(), "");
        let links = select_all(PAGE, "div#content-list a").unwrap();
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].text, "Chương 1");
        assert_eq!(links[0].attr("href"), "chuong-1");
        assert_eq!(
            links[2].attrs.get("href").unwrap(),
            "https://other.example/c3"
        );
        // An attribute that is not there is "" rather than a panic.
        assert_eq!(links[0].attr("id"), "");
        // Leniency, documented rather than wished away: `scraper` accepts an
        assert_eq!(select(PAGE, "a[href").unwrap(), "Start");
        // A match that is genuinely absent is empty, which is what a script's
        assert_eq!(select(PAGE, "nav a.never").unwrap(), "");
    }

    #[test]
    fn select_text_keeps_the_paragraphs_select_would_squeeze_away() {
        let text = select_text(PAGE, "article").unwrap();
        assert!(
            text.contains("Hắn bước vào phòng.\n\nCánh cửa kêu một tiếng."),
            "{text:?}"
        );
        // The headline comes along, because it is inside the container.
        assert!(text.starts_with("Chương 34: Bí ẩn"), "{text:?}");
        // The same page through `select` is one line, which is the whole reason
        assert_eq!(
            select(PAGE, "article").unwrap(),
            "Chương 34: Bí ẩn Hắn bước vào phòng. Cánh cửa kêu một tiếng."
        );
        // No match is empty, not an error: it is what a script's fallback keys
        assert_eq!(select_text(PAGE, "table td").unwrap(), "");
    }

    #[test]
    fn abs_url_folds_the_three_kinds_of_href_a_listing_mixes() {
        let base = "https://site.example/truyen/x/chuong-1";
        assert_eq!(
            abs_url(base, "chuong-2"),
            "https://site.example/truyen/x/chuong-2"
        );
        assert_eq!(
            abs_url(base, "/truyen/y/chuong-9"),
            "https://site.example/truyen/y/chuong-9"
        );
        assert_eq!(
            abs_url(base, "https://other.example/c3"),
            "https://other.example/c3"
        );
        assert_eq!(abs_url(base, "  "), "");
        // Unparseable input is handed back, not swallowed: the fetch error then
        assert_eq!(abs_url("not a url", "relative"), "relative");
    }

    #[test]
    fn readable_finds_the_prose_and_not_the_navigation() {
        let r = readable(PAGE);
        assert_eq!(r.title, "Chương 34: Bí ẩn");
        assert!(r.text.contains("Hắn bước vào phòng."), "{}", r.text);
        assert!(r.text.contains("Cánh cửa kêu một tiếng."), "{}", r.text);
        // The paragraph break survives as a blank line between the two.
        assert!(
            r.text.contains("Hắn bước vào phòng.\n\nCánh cửa"),
            "{}",
            r.text
        );
    }

    #[test]
    fn readable_scores_links_out_of_the_running() {
        // A navigation-heavy div and a prose div of similar raw length: the one
        let html = r#"<body>
            <div class="content">
              <p>The room was quiet, and the door had a lock on it that nobody had ever turned.</p>
            </div>
            <div id="content-nav">
              <a href="/a">First chapter link here</a>
              <a href="/b">Second chapter link here</a>
              <a href="/c">Third chapter link here</a>
            </div>
          </body>"#;
        let r = readable(html);
        assert!(r.text.contains("The room was quiet"), "{}", r.text);
        assert!(!r.text.contains("Second chapter"), "{}", r.text);
    }

    #[test]
    fn readable_never_returns_script_bodies() {
        // `body` is the last-resort candidate, so a page with nothing else must
        let html = "<html><body><script>var x = 'Cánh cửa';</script><p>Thật vậy.</p></body></html>";
        let r = readable(html);
        assert!(r.text.contains("Thật vậy."), "{}", r.text);
        assert!(!r.text.contains("var x"), "{}", r.text);
    }
}
