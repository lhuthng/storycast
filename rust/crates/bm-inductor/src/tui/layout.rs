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

/// Pane heights, per tier. Named rather than inlined so the compile-time guards
/// below and the renderer cannot drift apart.
///
/// **These are floors, not the heights the panes get.** A fixed row count per
/// pane is wrong in both directions at once: a one-box cluster was shown an
/// eight-row Machines pane that was mostly border, and a nine-box cluster had
/// three machines clipped with nothing saying so. Each pane is now given
/// `max(its floor, its content)`, and the slack goes to Logs — see `draw`.
///
/// The header is the one-row identity strip (workspace, profile, engine,
/// chapter range, theme chip). The floors below are what must fit when there is
/// almost nothing to show, and the guards prove the sum fits the tier.
pub(crate) const FULL_HEADER_H: u16 = 1;

/// Machines, full tier: border + table header, then one row a machine. The
/// floor is the empty state (two lines of explanation) and the ceiling is a
/// long roster, which scrolls rather than eating the dashboard.
pub(crate) const FULL_MACHINES_MIN_H: u16 = 3;

pub(crate) const FULL_MACHINES_MAX_H: u16 = 6;

/// Workers, full tier: border + table header + one row a live worker.
///
/// The ceiling is 11 so eight workers are all visible at once, which is the
/// most a cluster this size ever has. Past that the pane scrolls rather than
/// taking the log's rows.
pub(crate) const FULL_WORKERS_MIN_H: u16 = 3;

pub(crate) const FULL_WORKERS_MAX_H: u16 = 11;

/// Tasks and Stats, full tier: border + one line a stage, next to Stats.
pub(crate) const FULL_TASKS_MIN_H: u16 = 3;

pub(crate) const FULL_TASKS_MAX_H: u16 = 6;

/// **Logs is the flexible pane.** Every other height is its content's, so the
/// terminal's spare rows land here rather than in empty borders.
pub(crate) const FULL_EVENTS_MIN_H: u16 = 5;

pub(crate) const FULL_FOOTER_H: u16 = 3;

/// Same floors, compact tier. Tasks and Stats are drawn here too — the tier
/// used to drop them and leave a roll-up in the footer, which meant a 76-column
/// terminal could not show what was failing.
pub(crate) const COMPACT_MACHINES_MIN_H: u16 = 3;

pub(crate) const COMPACT_MACHINES_MAX_H: u16 = 4;

pub(crate) const COMPACT_WORKERS_MIN_H: u16 = 3;

pub(crate) const COMPACT_WORKERS_MAX_H: u16 = 5;

pub(crate) const COMPACT_TASKS_MIN_H: u16 = 3;

pub(crate) const COMPACT_TASKS_MAX_H: u16 = 3;

pub(crate) const COMPACT_EVENTS_MIN_H: u16 = 4;

pub(crate) const COMPACT_FOOTER_H: u16 = 4;

/// Key hints, on two lines each.
///
/// A single line was 161 characters, so it was clipped on *every* terminal —
/// and the part that fell off the right-hand end held the least guessable keys.
/// The compact tier gets shorter labels because it has 76 columns to work with;
/// every key is described in full on the help screen, which `?` opens.
// `f pane` gave its slot to `z park`: this line is 98 of its 100 columns, and of
// the two `f` is the one an operator finds without being told (a focus cycle is
// the first thing anyone tries) while `z` is a new verb nobody would guess. Both
// remain in full on the help screen, which is what `?` opens.
pub(crate) const KEYS_FULL: [&str; 2] = [
    "Tab jobs · K tasks · i inspect · P policy · z park · D digest · c crawl · R run · S cast · M mouse",
    ":add :prov :drop :translate :crawl :retry :reconcile :backend :stop :swap :voices · r · ? · q",
];

pub(crate) const KEYS_COMPACT: [&str; 2] = [
    "Tab jobs · K tasks · i inspect · P policy · z park · R run · S cast",
    ":add :prov :drop :translate :crawl :stop :retry :swap :voices · M copy · ? q",
];

/// Compact-tier column widths. The full tier has slack and keeps its widths
/// inline; these are the ones that must fit inside `MIN_W`, so they are named
/// and checked while compiling.
///
/// `machine, kind, ip, workers, policy, state, seen` — the `tts` column is
/// dropped. Seven short columns now, so `machine`/`ip` stay narrow enough that
/// the whole identity of a box reads in the width of one address.
///
/// `workers` carries 8: the header word is seven columns and a Length that
/// cannot hold its own header clips to `work`, which read as a column that
/// was never meant to be there.
pub(crate) const COMPACT_MACHINE_COLS: [u16; 7] = [11, 5, 14, 8, 9, 13, 5];

/// `alias, stage, ch, progress, activity` — `machine` is dropped.
pub(crate) const COMPACT_WORKER_COLS: [u16; 5] = [14, 8, 5, 17, 16];

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

/// The sound-design overlay's own size, when the terminal can hold it. Named
/// because the column sets below are chosen against it: a table wider than
/// `SOUND_OVERLAY_W - 2` would be a set that can never be selected, which is
/// how a "wide" layout quietly becomes dead code.
pub(crate) const SOUND_OVERLAY_W: u16 = 104;

pub(crate) const SOUND_OVERLAY_H: u16 = 28;

/// Column widths for the sound-design table: `sound, tags, takes, shape,
/// status`.
///
/// Two sets for the same reason the cast table has two: the overlay is
/// `SOUND_OVERLAY_W` when the terminal can hold it and shrinks to the frame
/// minus its own padding when it cannot, and a squeezed table clips every
/// column at once. The narrow set is what fits the 72-column overlay a
/// minimum-width terminal gives it — `MIN_W` minus the 2-column pad each side.
pub(crate) const SOUND_COLS_WIDE: [u16; 5] = [20, 30, 5, 23, 22];

pub(crate) const SOUND_COLS_NARROW: [u16; 5] = [16, 20, 5, 13, 14];

/// The sound-design action bar, in the pieces the draw styles separately.
///
/// The remove key is the only one whose availability is a fact about the
/// highlighted entry, so it is the only one drawn differently — and the bar is
/// the one place the screen states that a key is dead, so it must not clip.
/// Split rather than written inline for exactly that reason: the guard below
/// measures the longest form, and a clipped warning is no warning. The `d`
/// itself is pushed by the draw, between the head and the remove piece.
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
//
// The **floors** are what must fit, because that is the case where there is
// nothing to show and every pane is at its minimum: a longer list of content
// takes rows from Logs, which is the pane meant to absorb them.
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
