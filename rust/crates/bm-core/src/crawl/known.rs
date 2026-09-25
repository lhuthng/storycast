//! Which crawler belongs to which website — a list, so that pasting a URL can
//! say "we know this one".
//!
//! This is **not** a lookup table that decides anything. It is a list of facts
//! an operator would otherwise have to remember or go and find: the host, the
//! bundled script written for it, the shape that script is written against, and
//! the settings block that makes it run. `bm-inductor check <url>` and the TUI
//! both read it to turn "here is a URL" into "here is a URL *and* the crawler we
//! already have for it", which is the difference between one paste and a
//! fifteen-minute investigation.
//!
//! The failure mode to design against is a **stale entry reading as a
//! confident one**. Every line here was checked against a real chapter fetch, and
//! a site that has since started answering with a challenge is recorded as such
//! rather than quietly deleted — see [`KnownSite::caveat`]. An entry that says
//! "this is refused" is worth more than no entry, because it saves the reading of
//! a 403 as a puzzle.
//!
//! The registry is deliberately **not** exhaustive and not auto-updating. It is
//! the set this project has actually verified; a site that is missing from it is
//! not a site that cannot be crawled, it is a site nobody has written down yet,
//! and the workflow for that is the same as for a site that was never here:
//! `check` it, copy `assets/crawl/templates/truyencom.lua`, edit the selectors.

use std::fmt;

/// One website we have a crawler for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownSite {
    /// The host, lowercase, **no scheme and no `www.`**.
    ///
    /// Matching is `host == this` or `host ends with .this`, so `storya.click`
    /// covers `www.storya.click` and a listing subdomain, but not `notstorya.click`
    /// — the leading dot is the whole reason the suffix test is written the way
    /// it is.
    pub host: &'static str,
    /// The bundled script written for this site.
    ///
    /// Copied into the workspace's own `crawl/` rather than pointed at from
    /// `assets/`, so that editing it does not edit the copy the next book uses.
    pub script: &'static str,
    /// `params` the site needs, if any. `(key, example value)`.
    ///
    /// A site whose URLs cannot be templated needs its *book* URL here, and that
    /// is the one entry that is per-book rather than per-site — the example is
    /// there to be replaced.
    pub params: &'static [(&'static str, &'static str)],
    /// The site's `url_template`, or `""` when it has none and needs `discover`.
    pub url_template: &'static str,
    /// `crawl.max_fetches`. `0` means the built-in default is right.
    pub max_fetches: u32,
    /// `crawl.max_seconds`. `0` means the built-in default is right.
    pub max_seconds: u64,
    /// What the site *is*, in one line — the shape the script is written against,
    /// and the thing worth knowing before editing a selector.
    pub shape: &'static str,
    /// The language the chapters are written in.
    ///
    /// Not trivia. The whole speech half of this project is **Vietnamese**:
    /// `sea-g2p` turns text into phonemes, and it is a Vietnamese grapheme-to-
    /// phoneme model. An English chapter will be *spelled* by a Vietnamese
    /// model — every syllable boundary it guesses is a Vietnamese one — so the
    /// audio comes out mispronounced rather than wrong, and no error is raised
    /// anywhere. Recording the language next to the site is the only place that
    /// fact can be seen before a hundred chapters are rendered.
    pub language: &'static str,
    /// Something that will bite, if there is anything. `None` for a site that
    /// simply works.
    ///
    /// A challenge served as a plain `200` belongs here, not in `shape`: it is
    /// invisible unless you go looking, and it turns every symptom downstream —
    /// empty text, a refused block, a stall — into a mystery.
    pub caveat: Option<&'static str>,
}

/// The bundled Storya path. **Not a default** — see
/// [`crate::crawl::DEFAULT_SCRIPT`], which is what this is for.
const STORYA: &str = "assets/crawl/templates/storya.lua";

/// Every site this project has verified, in the order a reader should meet them:
/// the one that works out of the box, then the shapes worth learning from.
pub fn known_sites() -> &'static [KnownSite] {
    &[
        KnownSite {
            host: "storya.click",
            script: STORYA,
            params: &[],
            url_template: "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}",
            max_fetches: 0,
            max_seconds: 0,
            shape: "`/{book}/chuong-{n}` — templatable, one fetch a chapter. The \
                    crawler the pipeline shipped before scripted crawls existed, still \
                    bundled and parity-tested; a new workspace names no crawler at all.",
            language: "Vietnamese",
            caveat: None,
        },
        KnownSite {
            host: "truyencom.com",
            script: "assets/crawl/templates/truyencom.lua",
            params: &[],
            url_template: "https://truyencom.com/{book}/chuong-{n}.html",
            max_fetches: 64,
            max_seconds: 180,
            shape: "Templatable, but the chapter *list* is paginated — `discover` walks the page links.",
            language: "Vietnamese",
            caveat: None,
        },
        KnownSite {
            host: "readnovelfull.com",
            script: "assets/crawl/templates/readnovelfull.lua",
            params: &[(
                "book",
                "https://readnovelfull.com/the-sword-god-of-the-universe.html",
            )],
            // Empty on purpose: see `shape`.
            url_template: "",
            // A walk that follows `next_chap` spends a fetch per chapter, and the
            // default 64 is sized for a single chapter.
            max_fetches: 400,
            max_seconds: 900,
            shape: "`/{book}/chapter-{n}-{title-slug}.html` — the number is in the URL but not last, \
                    so nothing can invent the slug. The book page also lists only the first ~30 \
                    chapters with no pagination, so `discover` walks the `next_chap` chain instead.",
            language: "English",
            caveat: None,
        },
        KnownSite {
            host: "webnovel.com",
            script: "assets/crawl/templates/webnovel.lua",
            params: &[(
                "book",
                "https://www.webnovel.com/book/tu-chan-lieu-thien-quan_13320161405417805",
            )],
            url_template: "",
            max_fetches: 128,
            max_seconds: 300,
            shape: "English, slug discovery over the catalogue's 14 chapter columns.",
            language: "English — but `/vi/` is the Vietnamese edition of the same catalogue",
            caveat: Some(
                "Refused behind Cloudflare: 403 whatever we send. The template is correct and \
                 cannot be run without a `cf_clearance` cookie in `crawl.headers`; a browser \
                 User-Agent alone does not get in.",
            ),
        },
        KnownSite {
            host: "truyenfull.vn",
            script: "",
            params: &[],
            url_template: "",
            max_fetches: 0,
            max_seconds: 0,
            shape: "The old `truyenfull.vn` now redirects to `truyenfull.live`.",
            language: "Vietnamese",
            caveat: Some(
                "Answers 200 with a Cloudflare interstitial rather than a 403 — the one shape \
                 that reads as success. `bm-inductor check` is what tells the two apart.",
            ),
        },
        KnownSite {
            host: "lightnovel.vn",
            script: "",
            params: &[],
            url_template: "",
            max_fetches: 0,
            max_seconds: 0,
            shape: "Reader lives at `hub.lightnovel.vn/reader?book=<uuid>`.",
            language: "Vietnamese",
            caveat: Some(
                "A Next.js app: the chapter is fetched by JavaScript and is not in the HTML at \
                 all. Needs a browser, which this crawler deliberately has not.",
            ),
        },
        KnownSite {
            host: "novelfull.com",
            script: "",
            params: &[],
            url_template: "",
            max_fetches: 0,
            max_seconds: 0,
            shape: "—",
            language: "English",
            caveat: Some("Cloudflare: 403 to every request."),
        },
        KnownSite {
            host: "truyenthanh.vn",
            script: "",
            params: &[],
            url_template: "",
            max_fetches: 0,
            max_seconds: 0,
            shape: "—",
            language: "Vietnamese",
            caveat: Some("Upstream is broken: 500."),
        },
    ]
}

/// The site a URL belongs to, if we have one.
///
/// Deliberately forgiving about what it is handed, because the caller is a
/// person pasting from a browser address bar: a bare `readnovelfull.com/book.html`
/// with no scheme is the common case, not the edge one, and a suggestion that
/// does not appear because a scheme is missing is a suggestion that never
/// appears.
pub fn for_url(url: &str) -> Option<&'static KnownSite> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // The host is whatever comes before the first `/`, `?` or `#`; the scheme,
    // if there is one, is whatever comes before the first `:`. Both are stripped
    // by testing each candidate host against the table rather than by trusting
    // a parse, so a bad URL degrades to "no match" instead of a panic.
    let rest = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    // Userinfo (`user@host`) and a port are not part of the identity.
    let host = match authority.rsplit_once('@') {
        Some((_, h)) => h,
        None => authority,
    };
    let host = host.split(':').next().unwrap_or("");
    // An entry is stored without `www.`, and a leading dot is exactly what keeps
    // `evil-storya.click.example` from matching `storya.click`.
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    let bare = host.strip_prefix("www.").unwrap_or(&host);

    known_sites()
        .iter()
        .find(|s| bare == s.host || host == s.host || host.ends_with(&format!(".{}", s.host)))
}

/// The site for a host already reduced to its host part, for callers that have
/// one and do not want [`for_url`]'s leniency.
pub fn for_host(host: &str) -> Option<&'static KnownSite> {
    for_url(host)
}

/// The note to show for a URL we recognise, in the plain-text form both the CLI
/// and the TUI print.
///
/// One function for both, because the two were going to drift: a TUI that names a
/// different crawler than `check` does is a TUI that has taught somebody
/// something false. Indented two spaces, which is what the caller's other lines
/// already use.
pub fn note(site: &KnownSite) -> String {
    let mut s = String::new();
    s.push_str(&format!("\n  known site: {}\n", site.host));
    if site.is_crawlable() {
        s.push_str(&format!("    crawler: {}\n", site.script));
        push_wrapped(&mut s, "    shape:   ", site.shape);
    } else {
        s.push_str("    no bundled crawler for this site\n");
    }
    push_wrapped(&mut s, "    text:    ", site.language);
    // The one thing about the language that is a *consequence* rather than a
    // fact. Stated here because it is silent everywhere else: a Vietnamese
    // grapheme-to-phoneme model applied to English raises nothing, it just
    // guesses every syllable boundary and mispronounces the book.
    if !site.language.starts_with("Vietnamese") {
        push_wrapped(
            &mut s,
            "    heads up: ",
            "the voices and the G2P are Vietnamese, so this text will be pronounced \
             against Vietnamese syllable rules. Expect it to sound wrong, not to error.",
        );
    }
    // The caveat comes **before** the block, not after it. It is the sentence
    // that changes what the reader should do with the block, and a warning
    // printed below a thing you are about to copy is a warning nobody reads.
    if let Some(caveat) = site.caveat {
        push_wrapped(&mut s, "    note:     ", caveat);
    }
    if site.is_crawlable() {
        s.push_str("    paste into this workspace's settings.json:\n");
        for line in site.settings_block().lines() {
            s.push_str(&format!("      {line}\n"));
        }
    }
    s
}

/// Append `"<first-line><text wrapped to 78 columns>"`.
///
/// A 200-character line in an 80-column terminal wraps on the terminal's terms,
/// which loses the indent and reads as a wall. The shapes are sentences written
/// once and read in a hurry, so they are folded here rather than shortened.
fn push_wrapped(out: &mut String, label: &str, text: &str) {
    const WIDTH: usize = 78;
    let mut line = String::from(label);
    let mut column = label.len();
    let mut first = true;
    for word in text.split_whitespace() {
        if column + word.len() + 1 > WIDTH && column > label.len() {
            out.push_str(line.trim_end());
            out.push('\n');
            line = String::from("               ");
            column = line.len();
        }
        if first {
            line.push_str(word);
            first = false;
        } else {
            line.push(' ');
            line.push_str(word);
        }
        column += word.len() + 1;
    }
    out.push_str(&line);
    out.push('\n');
}

impl KnownSite {
    /// Whether this site has a crawler we can actually run.
    ///
    /// A `false` here is not a gap in the registry — the blocked sites are
    /// listed *because* they are blocked, and knowing that is the answer.
    pub fn is_crawlable(&self) -> bool {
        !self.script.is_empty()
    }

    /// The `"crawl"` and `url_template` lines to paste into a workspace's
    /// `settings.json`.
    ///
    /// Built rather than stored, because a stored copy is a second thing to keep
    /// true: the moment `CrawlSettings` grows a field, a hand-written example
    /// block is quietly wrong and nobody finds out until it does nothing.
    pub fn settings_block(&self) -> String {
        let mut s = String::from("  \"url_template\": ");
        s.push_str(&json_string(self.url_template));
        s.push_str(",\n  \"crawl\": {\n");
        s.push_str("    \"mode\": \"script\",\n");
        s.push_str(&format!("    \"script\": {},\n", json_string(self.script)));
        if !self.params.is_empty() {
            s.push_str("    \"params\": {\n");
            for (i, (k, v)) in self.params.iter().enumerate() {
                s.push_str(&format!(
                    "      {}: {}{}\n",
                    json_string(k),
                    json_string(v),
                    if i + 1 == self.params.len() { "" } else { "," }
                ));
            }
            s.push_str("    },\n");
        }
        if self.max_fetches != 0 {
            s.push_str(&format!("    \"max_fetches\": {},\n", self.max_fetches));
        }
        if self.max_seconds != 0 {
            s.push_str(&format!("    \"max_seconds\": {},\n", self.max_seconds));
        }
        s.push_str("    \"pace_ms\": 750\n  }");
        s
    }
}

impl fmt::Display for KnownSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} — {}{}",
            self.host,
            if self.is_crawlable() {
                self.script
            } else {
                "no crawler"
            },
            self.caveat.map(|c| format!(" ({c})")).unwrap_or_default()
        )
    }
}

/// A JSON string literal, quoted and escaped.
///
/// Not a general encoder — it handles what a template path, a URL and a host
/// contain, and escapes a control character and a quote for anything else, which
/// is enough to make the output always parseable.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_url_finds_its_site() {
        for url in [
            "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-1",
            "http://storya.click/x",
            "https://www.storya.click/truyen/a/chuong-5",
            "  https://readnovelfull.com/the-sword-god-of-the-universe.html  ",
            // A scheme is the *common* omission, not the edge case: people paste
            // what is in the address bar's path column half the time.
            "readnovelfull.com/the-sword-god-of-the-universe.html",
            "storya.click",
            "https://storya.click:443/x",
            "https://storya.click/x?y=1#z",
        ] {
            assert!(
                for_url(url).is_some(),
                "no site for {url:?} — a suggestion that does not appear is worse than none"
            );
        }
    }

    #[test]
    fn a_url_from_another_site_finds_nothing() {
        for url in [
            "",
            "   ",
            "https://example.com/chapter-1",
            "https://notstorya.click/x",
            // The suffix test must not be fooled by a host that merely *ends* with
            // the letters: this is the case a plain `ends_with` gets wrong.
            "https://evil-storya.click.example.com/x",
            "https://storya.click.evil.example/x",
        ] {
            assert!(for_url(url).is_none(), "matched {url:?} wrongly");
        }
    }

    #[test]
    fn the_registry_points_at_scripts_that_are_there() {
        // A registry that names a file we do not ship is worse than no registry:
        // it is a confident answer that fails at the moment it is trusted.
        let root = format!("{}/../../../assets", env!("CARGO_MANIFEST_DIR"));
        for site in known_sites() {
            if !site.is_crawlable() {
                continue;
            }
            let path = format!("{root}/{}", site.script.strip_prefix("assets/").unwrap());
            assert!(
                std::path::Path::new(&path).is_file(),
                "{} names a script that is not there: {path}",
                site.host
            );
        }
    }

    #[test]
    fn every_crawlable_site_can_be_pasted_as_settings() {
        // The block is the actual deliverable, so it has to be JSON and has to
        // round-trip into the shape `Settings` accepts.
        for site in known_sites().iter().filter(|s| s.is_crawlable()) {
            let block = site.settings_block();
            let value: serde_json::Value = serde_json::from_str(&format!("{{{block}}}"))
                .unwrap_or_else(|e| panic!("{} produced invalid JSON: {e}\n{block}", site.host));
            assert_eq!(value["crawl"]["mode"], "script", "{}", site.host);
            assert_eq!(
                value["crawl"]["script"], site.script,
                "{} must name its own script",
                site.host
            );
            assert!(
                value["url_template"].is_string(),
                "{} must carry a url_template, empty or not",
                site.host
            );
            for (k, _) in site.params {
                assert!(
                    value["crawl"]["params"][k].is_string(),
                    "{} is missing the param {k:?}",
                    site.host
                );
            }
        }
    }

    #[test]
    fn a_site_that_needs_discover_says_so_by_emptying_its_template() {
        // `url_template` non-empty *and* a crawler with a `discover` is a
        // contradiction the host resolves silently, in favour of the template.
        for site in known_sites().iter().filter(|s| s.is_crawlable()) {
            if site.params.iter().any(|(k, _)| *k == "book") {
                assert!(
                    site.url_template.is_empty(),
                    "{} takes a book URL, so its chapter URLs must come from discover",
                    site.host
                );
            }
        }
    }

    #[test]
    fn a_blocked_site_is_listed_with_what_blocked_it() {
        // The entries with no crawler are the reason the table exists. An entry
        // that says only "no crawler" is a dead end; one that says why sends the
        // reader away on purpose.
        for site in known_sites().iter().filter(|s| !s.is_crawlable()) {
            assert!(
                site.caveat.is_some(),
                "{} has no crawler and no reason given",
                site.host
            );
        }
    }

    #[test]
    fn hosts_are_stored_the_way_the_matcher_expects_them() {
        for site in known_sites() {
            assert!(!site.host.contains("://"), "{}", site.host);
            assert!(!site.host.starts_with("www."), "{}", site.host);
            assert_eq!(site.host, site.host.to_ascii_lowercase(), "{}", site.host);
        }
    }

    #[test]
    fn the_storya_entry_is_the_one_the_migration_uses() {
        // Tied to the constant deliberately. They answer the same question from
        // two directions — "what did the old settings point at" — and there is
        // no other entry that is a migration rather than a suggestion.
        //
        // Neither of them is a *default*. A workspace created now has no
        // crawler at all; this only ever describes a settings file that
        // predates the `crawl` block, and calling it a default is how the
        // registry ended up telling a reader that a new workspace would
        // silently start fetching from Storya.
        let storya = known_sites()
            .iter()
            .find(|s| s.host == "storya.click")
            .expect("a storya entry");
        assert_eq!(storya.script, crate::crawl::DEFAULT_SCRIPT);
        assert!(!crate::crawl::DEFAULT_SCRIPT.is_empty());
    }

    #[test]
    fn the_note_names_the_crawler_and_the_caveat_and_nothing_else() {
        // The two halves exist for different readers: the crawler is what to run,
        // the caveat is what to believe when it does not. Dropping either is the
        // way this feature goes wrong — a suggestion with no warning is worse
        // than none, because it is a recommendation.
        for site in known_sites() {
            let n = note(site);
            assert!(n.contains(&format!("known site: {}", site.host)), "{n}");
            if site.is_crawlable() {
                assert!(
                    n.contains(site.script),
                    "{site} did not name its crawler:\n{n}"
                );
                assert!(
                    n.contains("settings.json"),
                    "{site} offered nothing to paste"
                );
            } else {
                assert!(n.contains("no bundled crawler"), "{n}");
            }
            match site.caveat {
                // Compared on the folded text: the note wraps to 78 columns, so
                // the caveat is not contiguous in it. A caveat is a whole
                // sentence though — dropping a *word* is not the failure to
                // guard against here, dropping the caveat is.
                Some(c) => assert!(
                    unfold(&n).contains(&unfold(c)),
                    "{site} lost its caveat:\n{n}"
                ),
                None => assert!(!n.contains("note:  "), "{site} invented a note:\n{n}"),
            }
        }
    }

    #[test]
    fn the_language_is_stated_and_a_non_vietnamese_one_warns() {
        // The voices and the G2P are Vietnamese. Nothing downstream knows or
        // cares what language a site is, so this note is the only place the
        // mismatch can be seen — and a mismatch here is silent: it produces
        // mispronounced audio, not an error.
        for site in known_sites() {
            let n = note(site);
            assert!(!site.language.is_empty(), "{} has no language", site.host);
            assert!(
                unfold(&n).contains(&unfold(site.language)),
                "{} does not state its language:\n{n}",
                site.host
            );
            let warns = n.contains("heads up:");
            assert_eq!(
                warns,
                !site.language.starts_with("Vietnamese"),
                "{} and its warning disagree:\n{n}",
                site.host
            );
        }
    }

    /// The note with its wrapping undone: every run of whitespace becomes one
    /// space, so a phrase can be found across a line break.
    fn unfold(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn the_note_stays_inside_eighty_columns() {
        // The one hard constraint on a box: the reader has to be able to see the
        // whole suggestion at once, and a long shape or caveat is exactly where
        // that breaks. The pasted block is exempt — a JSON line cannot be folded
        // without ceasing to be pasteable — but it is checked for being valid,
        // which is the constraint that actually applies to it.
        for site in known_sites() {
            let n = note(site);
            match n.split_once("settings.json:") {
                Some((prose, block)) => {
                    for line in prose
                        .lines()
                        .chain(
                            block
                                .lines()
                                .take_while(|l| !l.trim_start().starts_with('"')),
                        )
                        .chain(std::iter::once(
                            "    paste into this workspace's settings.json:",
                        ))
                    {
                        assert!(
                            line.chars().count() <= 80,
                            "{} has a {}-column line:\n{line}",
                            site.host,
                            line.chars().count()
                        );
                    }
                    let body: String = block
                        .lines()
                        .map(|l| l.trim())
                        .collect::<Vec<_>>()
                        .join("\n");
                    // The block is the members of one object, not an object.
                    let parsed: serde_json::Value = serde_json::from_str(&format!("{{{body}}}"))
                        .unwrap_or_else(|e| panic!("{}: {e}\n{body}", site.host));
                    assert!(parsed["crawl"].is_object(), "{}", site.host);
                }
                // A site we cannot crawl has nothing to paste, so there is no
                // block to exempt and the whole note is prose.
                None => {
                    assert!(!site.is_crawlable(), "{} lost its block", site.host);
                    for line in n.lines() {
                        assert!(
                            line.chars().count() <= 80,
                            "{} has a {}-column line:\n{line}",
                            site.host,
                            line.chars().count()
                        );
                    }
                }
            }
        }
    }
}
