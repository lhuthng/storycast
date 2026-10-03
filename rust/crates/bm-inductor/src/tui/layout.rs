//! Responsive tiers: when each pane fits, and the guards that prove it.
pub(crate) const MIN_W: u16 = 76;

pub(crate) const MIN_H: u16 = 20;

/// Above this the full five-pane dashboard fits without squeezing Logs,
pub(crate) const FULL_W: u16 = 100;

pub(crate) const FULL_H: u16 = 32;

/// How much room the terminal has.
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

/// Pane heights, per tier. Named rather than inlined so the compile-time guards
pub(crate) const FULL_HEADER_H: u16 = 1;

/// Machines, full tier: border + table header, then one row a machine. The
pub(crate) const FULL_MACHINES_MIN_H: u16 = 3;

pub(crate) const FULL_MACHINES_MAX_H: u16 = 6;

/// Workers, full tier: border + table header + one row a live worker.
pub(crate) const FULL_WORKERS_MIN_H: u16 = 3;

pub(crate) const FULL_WORKERS_MAX_H: u16 = 11;

/// Tasks and Stats, full tier: border + one line a stage, next to Stats.
pub(crate) const FULL_TASKS_MIN_H: u16 = 3;

pub(crate) const FULL_TASKS_MAX_H: u16 = 6;

/// **Logs is the flexible pane.** Every other height is its content's, so the
pub(crate) const FULL_EVENTS_MIN_H: u16 = 5;

pub(crate) const FULL_FOOTER_H: u16 = 3;

/// Same floors, compact tier. Tasks and Stats are drawn here too — the tier
pub(crate) const COMPACT_MACHINES_MIN_H: u16 = 3;

pub(crate) const COMPACT_MACHINES_MAX_H: u16 = 4;

pub(crate) const COMPACT_WORKERS_MIN_H: u16 = 3;

pub(crate) const COMPACT_WORKERS_MAX_H: u16 = 5;

pub(crate) const COMPACT_TASKS_MIN_H: u16 = 3;

pub(crate) const COMPACT_TASKS_MAX_H: u16 = 3;

pub(crate) const COMPACT_EVENTS_MIN_H: u16 = 4;

pub(crate) const COMPACT_FOOTER_H: u16 = 4;

/// Key hints, on two lines each.
pub(crate) const KEYS_FULL: [&str; 2] = [
    "Tab jobs · K tasks · i inspect · P policy · z park · D digest · c crawl · R run · S cast · L llm",
    ":add :prov :drop :translate :crawl :retry :script :speaker :m :backend :stop · M mouse · ? q",
];

pub(crate) const KEYS_COMPACT: [&str; 2] = [
    "Tab jobs · K tasks · i inspect · P policy · z park · R run · S cast · L llm",
    ":add :prov :translate :stop :retry :script :swap · M copy · ? q",
];

/// Compact-tier column widths. The full tier has slack and keeps its widths
pub(crate) const COMPACT_MACHINE_COLS: [u16; 7] = [11, 5, 14, 12, 9, 13, 5];

/// `alias, stage, ch, progress, activity` — `machine` is dropped.
pub(crate) const COMPACT_WORKER_COLS: [u16; 5] = [14, 8, 5, 17, 16];

/// Column widths for the cast table, in two sets.
pub(crate) const CAST_COLS_WIDE: [u16; 3] = [30, 24, 9];

/// `speaker, voice, shared`
pub(crate) const CAST_COLS_NARROW: [u16; 3] = [20, 18, 7];

/// The cast overlay's own width, when the terminal can hold it. Named for the
pub(crate) const CAST_OVERLAY_W: u16 = 108;

/// The sound-design overlay's own size, when the terminal can hold it. Named
pub(crate) const SOUND_OVERLAY_W: u16 = 104;

pub(crate) const SOUND_OVERLAY_H: u16 = 28;

/// Column widths for the sound-design table: `sound, tags, takes, shape,
pub(crate) const SOUND_COLS_WIDE: [u16; 5] = [20, 30, 5, 23, 22];

pub(crate) const SOUND_COLS_NARROW: [u16; 5] = [16, 20, 5, 13, 14];

/// The sound-design action bar, in the pieces the draw styles separately.
pub(crate) const SOUND_KEYS_HEAD: &str = "↑↓ · ←→ tab · a add · e edit · l level · ";

pub(crate) const SOUND_KEYS_REMOVE: &str = " remove · ";

pub(crate) const SOUND_KEYS_REMOVE_DEAD: &str = " remove ✗ in use · ";

pub(crate) const SOUND_KEYS_TAIL: &str = "R · Esc close";

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
const _: () = assert!(
    COMPACT_MACHINES_MIN_H
        + COMPACT_WORKERS_MIN_H
        + COMPACT_TASKS_MIN_H
        + COMPACT_EVENTS_MIN_H
        + COMPACT_FOOTER_H
        <= MIN_H,
    "the compact tier's floors must fit inside MIN_H"
);
const _: () = assert!(
    FULL_HEADER_H
        + FULL_MACHINES_MIN_H
        + FULL_WORKERS_MIN_H
        + FULL_TASKS_MIN_H
        + FULL_EVENTS_MIN_H
        + FULL_FOOTER_H
        <= FULL_H,
    "the full tier's floors must fit inside FULL_H"
);
const _: () = assert!(
    cols(&COMPACT_MACHINE_COLS) + 2 <= MIN_W,
    "compact machines columns plus borders must fit MIN_W"
);
const _: () = assert!(
    cols(&COMPACT_WORKER_COLS) + 2 <= MIN_W,
    "compact workers columns plus borders must fit MIN_W"
);
const _: () = assert!(
    width_of(KEYS_FULL[0]) <= FULL_W as usize,
    "key line 1 overflows the full tier"
);
const _: () = assert!(
    width_of(KEYS_FULL[1]) <= FULL_W as usize,
    "key line 2 overflows the full tier"
);
const _: () = assert!(
    width_of(KEYS_COMPACT[0]) <= MIN_W as usize,
    "key line 1 overflows compact"
);
const _: () = assert!(
    width_of(KEYS_COMPACT[1]) <= MIN_W as usize,
    "key line 2 overflows compact"
);
const _: () = assert!(
    cols(&CAST_COLS_NARROW) + 4 <= MIN_W,
    "the narrow cast table plus two sets of borders must fit the smallest terminal"
);
const _: () = assert!(
    cols(&CAST_COLS_WIDE) + 2 <= CAST_OVERLAY_W - 2,
    "the wide cast table must fit the overlay it is only chosen on — \
     otherwise the wide set is unreachable and the table is always narrow"
);
const _: () = assert!(
    cols(&SOUND_COLS_NARROW) + 2 <= MIN_W - 4,
    "the narrow sound-design table must fit the overlay a minimum-width terminal gives it"
);
const _: () = assert!(
    cols(&SOUND_COLS_WIDE) + 2 <= SOUND_OVERLAY_W - 2,
    "the wide sound-design table must fit the overlay it is only chosen on — \
     otherwise the wide set is unreachable and the table is always narrow"
);
const _: () = assert!(
    width_of(SOUND_KEYS_HEAD)
        + 1 // the `d` the draw pushes between the two pieces
        + width_of(SOUND_KEYS_REMOVE_DEAD)
        + width_of(SOUND_KEYS_TAIL)
        <= MIN_W as usize - 2,
    "the sound-design action bar must fit the overlay at the minimum width, \
     warning and all — a clipped `✗ in use` is no warning"
);
