//! SubRip: the caption file `captions.import` reads.

/// One caption on the assembled clock.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// The captions that have words in them, numbered in order. A line with no
/// text carries no caption: an empty block would import as one.
pub fn render(entries: &[Entry]) -> String {
    let mut out = String::new();
    let mut n = 0;
    for e in entries {
        let text = one_line(&e.text);
        if text.is_empty() {
            continue;
        }
        n += 1;
        out.push_str(&format!("{n}\n{} --> {}\n{text}\n\n", stamp(e.start), stamp(e.end)));
    }
    out
}

/// `hh:mm:ss,mmm`, the only stamp SubRip accepts.
fn stamp(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// A cue's text as one line: a break inside a cue would be read as two.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
