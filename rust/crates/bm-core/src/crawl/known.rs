//! Which crawler belongs to which website — a list, so that pasting a URL can
//! both read it to turn "here is a URL" into "here is a URL *and* the crawler we
//! already have for it", which is the difference between one paste and a

use std::fmt;
use std::sync::OnceLock;

use serde::Deserialize;

/// One website we have a crawler for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownSite {
    /// The host, lowercase, **no scheme and no `www.`**.
    pub host: &'static str,
    /// The bundled script written for this site.
    pub script: &'static str,
    /// `params` the site needs, if any. `(key, example value)`.
    pub params: &'static [(&'static str, &'static str)],
    /// The site's `url_template`, or `""` when it has none and needs `discover`.
    pub url_template: &'static str,
    /// `crawl.max_fetches`. `0` means the built-in default is right.
    pub max_fetches: u32,
    /// `crawl.max_seconds`. `0` means the built-in default is right.
    pub max_seconds: u64,
    /// What the site *is*, in one line — the shape the script is written against,
    pub shape: &'static str,
    /// The language the chapters are written in.
    pub language: &'static str,
    /// Something that will bite, if there is anything. `None` for a site that
    pub caveat: Option<&'static str>,
}

/// The global registry, embedded at compile time.
pub const REGISTRY_JSON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../crawlers/knownsites.json"
));

/// The registry file as it is written: `script` relative to `crawlers/`.
#[derive(Deserialize)]
struct RegistryFile {
    #[serde(default)]
    sites: Vec<RegistrySite>,
}

#[derive(Deserialize)]
struct RegistrySite {
    host: String,
    #[serde(default)]
    script: String,
    #[serde(default)]
    params: Vec<(String, String)>,
    #[serde(default)]
    url_template: String,
    #[serde(default)]
    max_fetches: u32,
    #[serde(default)]
    max_seconds: u64,
    #[serde(default)]
    shape: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    caveat: Option<String>,
}

/// Leak a parsed string into a `'static` field.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

impl KnownSite {
    /// One entry, with every string/param promoted to `'static`.
    fn from_registry(s: RegistrySite) -> Self {
        // A script is stored relative to `crawlers/`; the setting a caller
        let script = if s.script.trim().is_empty() {
            String::new()
        } else {
            format!("crawlers/{}", s.script.trim())
        };
        let params: Vec<(&'static str, &'static str)> = s
            .params
            .into_iter()
            .map(|(k, v)| (leak(k), leak(v)))
            .collect();
        KnownSite {
            host: leak(s.host),
            script: leak(script),
            params: Box::leak(params.into_boxed_slice()),
            url_template: leak(s.url_template),
            max_fetches: s.max_fetches,
            max_seconds: s.max_seconds,
            shape: leak(s.shape),
            language: leak(s.language),
            caveat: s.caveat.map(leak),
        }
    }
}

/// Every site this project has verified, in the order a reader should meet them:
pub fn known_sites() -> &'static [KnownSite] {
    static REGISTRY: OnceLock<Vec<KnownSite>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let file: RegistryFile = serde_json::from_str(REGISTRY_JSON)
            .expect("crawlers/knownsites.json is embedded and must parse");
        file.sites
            .into_iter()
            .map(KnownSite::from_registry)
            .collect()
    })
}

/// The site a URL belongs to, if we have one.
pub fn for_url(url: &str) -> Option<&'static KnownSite> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    // The host is whatever comes before the first `/`, `?` or `#`; the scheme,
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
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    let bare = host.strip_prefix("www.").unwrap_or(&host);

    known_sites()
        .iter()
        .find(|s| bare == s.host || host == s.host || host.ends_with(&format!(".{}", s.host)))
}

/// The site for a host already reduced to its host part, for callers that have
pub fn for_host(host: &str) -> Option<&'static KnownSite> {
    for_url(host)
}

/// The note to show for a URL we recognise, in the plain-text form both the CLI
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
    if !site.language.starts_with("Vietnamese") {
        push_wrapped(
            &mut s,
            "    heads up: ",
            "the voices and the G2P are Vietnamese, so this text will be pronounced \
             against Vietnamese syllable rules. Expect it to sound wrong, not to error.",
        );
    }
    // The caveat comes **before** the block, not after it. It is the sentence
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
    pub fn is_crawlable(&self) -> bool {
        !self.script.is_empty()
    }

    /// The `"crawl"` and `url_template` lines to paste into a workspace's
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
            "https://evil-storya.click.example.com/x",
            "https://storya.click.evil.example/x",
        ] {
            assert!(for_url(url).is_none(), "matched {url:?} wrongly");
        }
    }

    #[test]
    fn the_registry_points_at_scripts_that_are_there() {
        // A registry that names a file we do not ship is worse than no registry:
        let root = format!("{}/../../..", env!("CARGO_MANIFEST_DIR"));
        for site in known_sites() {
            if !site.is_crawlable() {
                continue;
            }
            let path = format!("{root}/{}", site.script);
            assert!(
                std::path::Path::new(&path).is_file(),
                "{} names a script that is not there: {path}",
                site.host
            );
        }
        // The one every workspace leans on: `DEFAULT_SCRIPT` is what a settings
        let default = format!("{root}/{}", crate::crawl::DEFAULT_SCRIPT);
        assert!(
            std::path::Path::new(&default).is_file(),
            "the default crawler is not there: {default}"
        );
    }

    #[test]
    fn every_crawlable_site_can_be_pasted_as_settings() {
        // The block is the actual deliverable, so it has to be JSON and has to
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
    fn unfold(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn the_note_stays_inside_eighty_columns() {
        // The one hard constraint on a box: the reader has to be able to see the
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
