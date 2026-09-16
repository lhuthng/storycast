//! Responsive tiers: when each pane fits, and the guards that prove it.
/// Hard floor. Below this the dashboard is not merely cramped, it is
/// misleading: table columns clip mid-word and a reversed-cursor row can look
/// like a different row than the one selected. Rather than render a lie, say
/// what is wrong and what to do.
pub(crate) const MIN_W: u16 = 76;

pub(crate) const MIN_H: u16 = 20;

/// Above this the full five-pane dashboard fits without squeezing Logs,
/// which is the one pane that must stay readable.
pub(crate) const FULL_W: u16 = 100;

pub(crate) const FULL_H: u16 = 32;

/// How much room the terminal has.
///
/// Three tiers rather than a single pass/fail threshold: an 80×24 terminal is
/// the default on most setups, so refusing to draw at 100×32 would blank the
/// dashboard for almost everyone. Compact keeps every pane that carries live
/// state and folds only the Tasks summary into the footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Size {
    /// Below `MIN_W` × `MIN_H` — draw the guard panel and nothing else.
    TooSmall,
    /// Usable, but the Tasks pane is collapsed into the footer.
    Compact,
    /// Everything fits.
    Full,
}

pub(crate) fn size_class(w: u16, h: u16) -> Size {
    if w < MIN_W || h < MIN_H {
        Size::TooSmall
    } else if w < FULL_W || h < FULL_H {
        Size::Compact
    } else {
        Size::Full
    }
}

/// Pane heights, per tier. Named rather than inlined so the compile-time guard
/// below and the renderer cannot drift apart.
pub(crate) const FULL_MACHINES_H: u16 = 8;

pub(crate) const FULL_WORKERS_H: u16 = 8;

pub(crate) const FULL_TASKS_H: u16 = 7;

pub(crate) const FULL_EVENTS_MIN_H: u16 = 5;

pub(crate) const FULL_FOOTER_H: u16 = 3;

pub(crate) const COMPACT_MACHINES_H: u16 = 6;

pub(crate) const COMPACT_WORKERS_H: u16 = 6;

pub(crate) const COMPACT_EVENTS_MIN_H: u16 = 4;

pub(crate) const COMPACT_FOOTER_H: u16 = 4;

/// Key hints, on two lines each.
///
/// A single line was 161 characters, so it was clipped on *every* terminal —
/// and the part that fell off the right-hand end held the least guessable keys.
/// The compact tier gets shorter labels because it has 76 columns to work with;
/// every key is described in full on the help screen, which `?` opens.
pub(crate) const KEYS_FULL: [&str; 2] = [
    ":a add · :p provision · :d drop · :t translate · :c crawl · :e eta · i inspect",
    ":u retry · :m rec · :B back · :X stop · :s swap · :v voices · :S cast · K tasks · r · ? · C · q",
];

pub(crate) const KEYS_COMPACT: [&str; 2] = [
    ":a add · :p prov · :d drop · :t trans · :c crawl · :B back · :X stop",
    ":u · :m · :v voices · :s swap · :S cast · K tasks · r · ? · q quit · : cmd",
];

/// Compact-tier column widths. The full tier has slack and keeps its widths
/// inline; these are the ones that must fit inside `MIN_W`, so they are named
/// and checked while compiling.
///
/// `addr, workers, role, state, seen` — the `tts` column is dropped.
pub(crate) const COMPACT_MACHINE_COLS: [u16; 5] = [15, 7, 8, 13, 8];

/// `worker, stage, ch, progress, activity, eta` — `machine` is dropped.
pub(crate) const COMPACT_WORKER_COLS: [u16; 6] = [14, 8, 5, 17, 16, 8];

/// Column widths for the cast table, in two sets.
///
/// The dashboard floor is 76 columns, but the wide cast table needs 92 — so on
/// the terminal sizes where the dashboard is *most* useful the table would be
/// squeezed and every column clipped together. The narrow set drops `gender`
/// (the picker shows it in full) so the speaker, the voice and the verdict stay
/// readable. `lang` is absent from both: it is the constant `vi-VN` and so
/// carries no information, and it lives in the overlay title instead.
///
/// `speaker, voice, gender, accent, status`
pub(crate) const CAST_COLS_WIDE: [u16; 5] = [24, 20, 7, 14, 27];

/// `speaker, voice, accent, status`
pub(crate) const CAST_COLS_NARROW: [u16; 4] = [18, 16, 13, 23];

/// Sum of a column list, in a form `const` evaluation accepts.
pub(crate) const fn cols(xs: &[u16]) -> u16 {
    let mut i = 0;
    let mut total = 0;
    while i < xs.len() {
        total += xs[i];
        i += 1;
    }
    total
}

/// Display width of a hint line. Every glyph in these strings occupies one
/// column, so counting UTF-8 lead bytes is exact — and `str::chars().count()`
/// is not available in a `const` context.
pub(crate) const fn width_of(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        if b[i] & 0xC0 != 0x80 {
            n += 1;
        }
        i += 1;
    }
    n
}

// Proved at compile time: a widened column, a taller pane or one more key hint
// must not silently start clipping on the smallest terminal of its tier.
const _: () = assert!(
    COMPACT_MACHINES_H + COMPACT_WORKERS_H + COMPACT_EVENTS_MIN_H + COMPACT_FOOTER_H <= MIN_H,
    "the compact tier must fit inside MIN_H"
);
const _: () = assert!(
    FULL_MACHINES_H + FULL_WORKERS_H + FULL_TASKS_H + FULL_EVENTS_MIN_H + FULL_FOOTER_H <= FULL_H,
    "the full tier must fit inside FULL_H"
);
const _: () = assert!(
    cols(&COMPACT_MACHINE_COLS) + 2 <= MIN_W,
    "compact machines columns plus borders must fit MIN_W"
);
const _: () = assert!(
    cols(&COMPACT_WORKER_COLS) + 2 <= MIN_W,
    "compact workers columns plus borders must fit MIN_W"
);
const _: () = assert!(width_of(KEYS_FULL[0]) <= FULL_W as usize, "key line 1 overflows the full tier");
const _: () = assert!(width_of(KEYS_FULL[1]) <= FULL_W as usize, "key line 2 overflows the full tier");
const _: () = assert!(width_of(KEYS_COMPACT[0]) <= MIN_W as usize, "key line 1 overflows compact");
const _: () = assert!(width_of(KEYS_COMPACT[1]) <= MIN_W as usize, "key line 2 overflows compact");
const _: () = assert!(
    cols(&CAST_COLS_NARROW) + 4 <= MIN_W,
    "the narrow cast table plus two sets of borders must fit the smallest terminal"
);
