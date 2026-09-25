//! Per-host pacing: the one mitigation for hammering a single site.
//!
//! A cluster makes one website's problem worse by exactly the number of boxes
//! pointed at it. Ten workers leasing crawl tasks against one host is ten
//! requests arriving together, and the failure that follows is not "we got
//! rate limited" but "the site decided we are a scraper" — usually several
//! chapters in, mid-run, with the text already spoken for the chapters that
//! made it.
//!
//! Rotating an IP is *not* the fix: the new address gets throttled just the
//! same, because the pacing was the problem. So the fetch itself is spaced.
//!
//! **Grain.** This is per **process**, keyed by host: one worker's sequential
//! crawl tasks cannot exceed the interval, and `fetch` inside a script's own
//! listing walk is spaced too, which is where a burst is most likely (a walk
//! issues twenty requests in a second). It deliberately does **not** coordinate
//! across boxes — that needs the inductor to hold a per-host token when it
//! offers a task, which is a scheduler change with its own trade-offs; this
//! bounds the blast radius of the n-boxes-one-site case to `n / interval`
//! requests per second, which is the difference between suspicious and banned.
//!
//! `0` disables it, and that is a supported value: a local fixture server or an
//! intranet mirror does not want a sleep per request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One process-wide table of "when may I next ask this host".
///
/// Process-wide rather than per-task on purpose: a worker runs crawl tasks one
/// after another, so a per-task table would reset the clock at every chapter
/// boundary — the exact moment a fresh burst starts.
static LAST_HIT: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// The host to key on: scheme-less, port-less, lowercased.
///
/// `https://site.example:8443/x` and `http://site.example/y` are one site for
/// the purpose of not annoying it, and a `www.` prefix is the same site under
/// another name.
pub fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

/// How long to sleep before the next fetch of `host`.
///
/// Returns the interval it applied, so a caller can log it: a pace that is
/// silently in force is a pace nobody can explain when a crawl gets slow.
pub fn wait_for(host: &str, pace: Duration) -> Duration {
    if pace.is_zero() || host.is_empty() {
        return Duration::ZERO;
    }
    let sleep = {
        let mut guard = LAST_HIT.lock().unwrap_or_else(|e| e.into_inner());
        let table = guard.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        let next = table.get(host).map(|last| *last + pace).unwrap_or(now);
        table.insert(host.to_string(), next.max(now));
        next.saturating_duration_since(now)
    };
    if !sleep.is_zero() {
        std::thread::sleep(sleep);
    }
    sleep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_normalised_so_one_site_has_one_bucket() {
        for (url, want) in [
            ("https://site.example/truyen/x", "site.example"),
            ("http://Site.Example:8443/a", "site.example"),
            ("https://www.site.example/a", "site.example"),
            ("https://user:pw@site.example/a", "site.example"),
            ("site.example/a", "site.example"),
            ("", ""),
        ] {
            assert_eq!(host_of(url), want, "{url}");
        }
    }

    #[test]
    fn the_first_fetch_of_a_host_is_not_delayed_and_the_next_one_is() {
        // A distinct host per test: the table is process-wide, since that is the
        // property the pacing depends on.
        let host = "pacing-test.example";
        assert_eq!(wait_for(host, Duration::from_millis(120)), Duration::ZERO);
        let slept = wait_for(host, Duration::from_millis(120));
        assert!(slept > Duration::from_millis(20), "slept {slept:?}");
        // A different host is not held back by the first one's clock.
        assert_eq!(
            wait_for("pacing-other.example", Duration::from_millis(120)),
            Duration::ZERO
        );
        // And zero means zero: no sleep, no bookkeeping.
        assert_eq!(wait_for(host, Duration::ZERO), Duration::ZERO);
        assert_eq!(wait_for("", Duration::from_secs(5)), Duration::ZERO);
    }
}
