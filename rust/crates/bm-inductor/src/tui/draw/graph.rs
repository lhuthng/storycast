//! The Machines pane drawn as a rack: the inductor's console, the boxes it
//! drives, and the bus between them.
//!
//! **The picture is a bus because the transport is.** The inductor dials every
//! box — `ssh` to push, `http` for `GET /status` and `POST /task` — and nothing
//! ever dials the inductor, so there is no box-to-box edge to draw. A mesh of
//! arrows would look more like a cluster diagram and would say the wrong thing.
//!
//! **Every box is drawn in a frame, and that is load-bearing.** The first
//! version hung bare art off a `│`, and the complaint was that the lines were
//! disconnected and confusing — which was true. A drop that ends in empty space
//! above a glyph is a stroke, not a connection: the eye has to guess whether
//! the rail belongs to the box below it. A frame ends it in a corner on a
//! visible edge, which is the one thing that makes "this is plugged into that"
//! unambiguous. The frame's side bars also fill the rows beside the art's lower
//! two thirds, which had nothing in them.
//!
//! **It is left-anchored on purpose.** The console sits at a fixed column, so a
//! wide terminal does not slide it into the middle of an otherwise empty pane
//! and the boxes scroll past it.
//!
//! **It merges the Workers pane rather than repeating it.** A node carries the
//! animal the box's worker reports — the same word the Workers pane, the event
//! log and Stats already call it — and the task it is on with the chapter, so
//! one glance answers "who is working on what".
//!
//! The shape lives in [`plan`], and the painter walks it. That is the point of
//! the indirection: the pane's height and its contents are then one
//! description seen twice, rather than two arithmetic blocks that have to be
//! kept equal.

use crate::tui::{
    app::App,
    model::{current_work, graph_mark, inductor_label, machine_alias, node_stage, work_label},
    style::{machine_tint, selection_bg, style_bold_of, style_of, Conn},
};
use bm_proto::now_secs;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

/// The inductor, as a console: a screen on a stand. Four rows by five columns,
/// so it tiles against a server the way two faces of the same rack do.
pub(crate) const HUB: [&str; 4] = ["/---\\", "|   |", "|___|", " \\_/ "];

/// A box, as a rack unit. Four rows by five columns.
pub(crate) const SERVER: [&str; 4] = [" ___ ", "|[_]|", "|+ ;|", "`---'"];

const ART_W: u16 = 5;
/// Art, a space, then the label. Fourteen, because `digest 12 50%` plus its
/// leading space is fourteen and a work line that loses its last character is a
/// percentage that reads as something else.
const LABEL_W: u16 = 14;
/// The whole cell, **frame included**: a bar, the art, the label, a bar. The
/// art is five columns with a space at each end, so it supplies the gap between
/// itself and the label. The selection background covers exactly this, so the
/// highlighted box is a rectangle and not a patch around two lines of text.
const NODE_W: u16 = 1 + ART_W + LABEL_W + 1;
/// One blank column between two frames, so two boxes never touch.
const PITCH: u16 = NODE_W + 1;
/// Where the first node's frame starts, measured from the pane's left edge.
const COL0: u16 = ART_W + 3;
/// The bus leaves the hub from the middle of its art — and a node's drop lands on
/// the middle of the *art*, not the middle of the cell, so the line meets the
/// machine and not the frame around it.
const DROP_IN_NODE: u16 = 1 + ART_W / 2;
/// The bus leaves the hub from the middle of its art.
const HUB_MID: u16 = ART_W / 2;

/// One band of servers: the rail they hang from, the frame's top edge, four
/// rows of art, and its bottom edge.
const BAND_H: u16 = 1 + 1 + 4 + 1;
/// The console, and the spine that runs down from it. The lean form is the
/// console's name alone, which is the same picture with the art left off.
const CHASSIS: u16 = HUB.len() as u16 + 1;
const LEAN_CHASSIS: u16 = 2;

/// Interior rows one band of servers needs, chassis included.
pub(crate) const FULL_H: u16 = CHASSIS + BAND_H;
pub(crate) const LEAN_H: u16 = LEAN_CHASSIS + BAND_H;

/// The richest form that fits `avail` interior rows, or `None` for none of them.
///
/// `None` is the caller's signal to draw the table instead: a rack with the
/// servers' legs cut off is worse than a table, and saying so is better than
/// showing either without comment.
pub(crate) fn form_for(avail: u16) -> Option<bool> {
    if avail >= FULL_H {
        Some(true)
    } else if avail >= LEAN_H {
        Some(false)
    } else {
        None
    }
}

/// How many bands of servers fit in `avail` interior rows. Never zero: a rack
/// with no servers on it is not a rack, so a pane one row short of a band gets
/// one and the border gets the clipping.
pub(crate) fn bands_for(avail: u16, hub_art: bool) -> usize {
    let chassis = if hub_art { CHASSIS } else { LEAN_CHASSIS };
    (avail.saturating_sub(chassis) / BAND_H).max(1) as usize
}

/// How many nodes fit side by side in `w` interior columns.
///
/// Never zero: a node wider than the terminal is still drawn and clipped, so a
/// narrow window degrades to a list rather than to a blank pane.
pub(crate) fn columns_for(w: u16) -> usize {
    (w.saturating_sub(COL0) / PITCH).max(1) as usize
}

/// The window's top band, for a cursor at `selected` in a window at `first`.
///
/// Follows the cursor and never runs off the end of the cluster, the same two
/// rules the table's `clamp_scroll` follows — expressed in bands because a band
/// is the rack's unit. The window is *stored*, not derived: `←` and `→` move
/// the cursor and `↑` and `↓` move it by a row, and a window that recomputed
/// itself from the cursor would jump a whole band on every press down.
pub(crate) fn first_band(
    selected: usize,
    first: usize,
    cols: usize,
    bands: usize,
    total_bands: usize,
) -> usize {
    let last = total_bands.saturating_sub(bands);
    let band = selected / cols.max(1);
    if band < first {
        band.min(last)
    } else if band >= first + bands {
        (band + 1 - bands).min(last)
    } else {
        first.min(last)
    }
}

/// One *display* row of the picture, in draw order.
///
/// One row, not one cell: the plan is the pane's height as well as its
/// contents, so it cannot list an art row once per node and then have two
/// places disagree about how tall the picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    /// One row of the console, or the single line it is replaced by in the lean
    /// form. The name rides on row 0 either way.
    Hub { row: usize },
    /// The spine under the console, running down to the first band.
    Spine,
    /// The bus, from the spine to the band's last node.
    Rail { band: usize, last: bool },
    /// The boxes' top edges, with the drop arriving on them as a `┴`.
    Lid { band: usize },
    /// One row of every node's art in a band, between its frame's edges.
    Art { band: usize, row: usize },
    /// The boxes' bottom edges. The bus stops here on the last band.
    Sill { band: usize, last: bool },
    /// Boxes the window did not reach. Present only when there are some.
    More { hidden: usize },
}

/// The rows the picture is made of — **the single description of its shape**.
/// Its length is the height, and the painter walks the same list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub rows: Vec<Row>,
    /// Which form this is: the console drawn, or its name alone.
    pub hub_art: bool,
    /// The window: the first cluster node drawn, its band, and its width.
    pub first: usize,
    pub bands: usize,
    pub cols: usize,
    pub total: usize,
}

/// The picture for a window, in one description.
///
/// `avail` is the interior height the pane actually got, so the plan is the
/// same arithmetic whether the caller is sizing the pane or drawing into it —
/// which is the only way the two cannot disagree about how tall the rack is.
pub(crate) fn plan(avail: u16, w: u16, total: usize, first: usize, hub_art: bool) -> Plan {
    let cols = columns_for(w);
    let total_bands = total.div_ceil(cols).max(1);
    // A window shows whole bands and is full whenever there are enough boxes to
    // fill it: a rack that went half-empty on every scroll would be a rack
    // reporting on its own padding.
    let bands = bands_for(avail, hub_art).min(total_bands);
    let hub_rows = if hub_art { HUB.len() } else { 1 };
    let mut rows = Vec::with_capacity(PLAN_MAX);
    for row in 0..hub_rows {
        rows.push(Row::Hub { row });
    }
    rows.push(Row::Spine);
    for b in 0..bands {
        let last = b + 1 == bands;
        rows.push(Row::Rail {
            band: first + b,
            last,
        });
        rows.push(Row::Lid { band: first + b });
        for row in 0..SERVER.len() {
            rows.push(Row::Art {
                band: first + b,
                row,
            });
        }
        rows.push(Row::Sill {
            band: first + b,
            last,
        });
    }
    // The window is `bands × cols` boxes whatever the scroll position, so the
    // count of what is *not on screen* is the same on every page — which is both
    // honest ("these are the ones you are not looking at") and what keeps the
    // pane the same height on every page.
    let hidden = total.saturating_sub(bands * cols);
    if hidden > 0 {
        rows.push(Row::More { hidden });
    }
    Plan {
        rows,
        hub_art,
        first,
        bands,
        cols,
        total,
    }
}

/// Room for the longest plan the painter can build, so the rows vector does not
/// reallocate mid-draw. Bounded by the pane, not by the cluster.
const PLAN_MAX: usize = 40;

pub(crate) fn draw_graph(
    f: &mut ratatui::Frame,
    app: &mut App,
    area: Rect,
    block: Block<'static>,
    hub_art: bool,
) {
    let colour = app.colour();
    let now = now_secs();
    let inner = block.inner(area);
    f.render_widget(block, area);
    let w = inner.width;
    if w == 0 || inner.height == 0 {
        return;
    }
    let avail = inner.height;
    let total = app.machines.len();
    let cols = columns_for(w);
    let total_bands = total.div_ceil(cols).max(1);
    let bands = bands_for(avail, hub_art).min(total_bands);
    // The window is the painter's to move: the keys only move the cursor, and
    // the drawer pulls the window along when the cursor would be off screen. Two
    // owners of one offset is how a pane ends up disagreeing with the keys.
    app.graph_band = first_band(app.selected, app.graph_band, cols, bands, total_bands);
    // Published for `↑` and `↓`, which move the cursor a *row of the rack*, and
    // cannot know the rack's width from where they run. Same bargain the log's
    // PageUp already makes with `events_rows`.
    app.graph_cols = cols;
    let p = plan(avail, w, total, app.graph_band, hub_art);

    // The spine is one continuous line from the console to the last rail, so
    // every row between them carries it. Drawing it row by row out of the plan
    // is how it ends up as a floating `│` beside nothing.
    let spine_from = p.rows.iter().position(|r| *r == Row::Spine);
    let last_rail = p.rows.iter().rposition(|r| matches!(r, Row::Rail { .. }));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in p.rows.iter().enumerate() {
        let spine = match (spine_from, last_rail) {
            (Some(from), Some(last)) => i > from && i < last,
            _ => false,
        };
        lines.push(match *row {
            Row::Hub { row } => hub_row(app, row, hub_art, colour, w),
            Row::Spine => gutter(w, colour),
            Row::Rail { band, last } => rail_row(w, &p, band, last, colour),
            Row::Lid { band } => lid_row(w, &p, band, true, Row2 { colour, spine }),
            Row::Art { band, row } => art_row(app, &p, band, row, now, w, Row2 { colour, spine }),
            Row::Sill { band, last } => lid_row(
                w,
                &p,
                band,
                false,
                Row2 {
                    colour,
                    spine: spine && !last,
                },
            ),
            Row::More { hidden } => Line::from(Span::styled(
                format!(" {hidden} more — ↑↓←→"),
                style_of(colour, Color::Yellow),
            )),
        });
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// The console, and on its first row the name of the thing driving them all.
///
/// The name is the inductor's own, never `127.0.0.1:8901`: a socket is where
/// this dashboard happens to be pointed, and a picture that labels the
/// coordinator by its address teaches the reader nothing they can use.
fn hub_row(app: &App, row: usize, hub_art: bool, colour: bool, w: u16) -> Line<'static> {
    let mut ink = Ink::new();
    if hub_art {
        ink.put(0, HUB[row], style_bold_of(colour, Color::Cyan));
    }
    if row == 0 {
        let name = inductor_label(&app.machines);
        ink.put(
            ART_W as usize + 1,
            &name,
            style_bold_of(colour, Color::Cyan),
        );
        // The link's own state, in its own colour, beside the name — never on a
        // row of its own, which would cost the picture a line it does not have.
        let (mark, level) = match app.conn {
            Conn::Up => ("● up", Color::Green),
            Conn::Unknown => ("○ …", Color::Yellow),
            Conn::Down(_) => ("✗ down", Color::Red),
        };
        let at = ART_W as usize + 2 + name.chars().count();
        ink.put(at, mark, style_of(colour, level));
    }
    ink.finish(w)
}

/// The spine on its own row: one column, under the console.
fn gutter(w: u16, colour: bool) -> Line<'static> {
    let mut row = vec![' '; w as usize];
    row[HUB_MID as usize] = '│';
    Line::from(Span::styled(
        row.into_iter().collect::<String>(),
        style_of(colour, Color::Cyan),
    ))
}

/// The bus: out of the spine, across to the band's last node, and closed after
/// it — or carried on down the spine when another band follows.
fn rail_row(w: u16, p: &Plan, band: usize, last: bool, colour: bool) -> Line<'static> {
    let centres = band_centres(p, band);
    let mut row = vec![' '; w as usize];
    row[HUB_MID as usize] = if last { '└' } else { '├' };
    if let Some(end) = centres.last().copied() {
        for c in (HUB_MID + 1)..=end {
            row[c as usize] = '─';
        }
        for c in &centres {
            row[*c as usize] = '┬';
        }
        if let Some(cell) = row.get_mut(end as usize + 1) {
            *cell = if last { '┘' } else { '┤' };
        }
    }
    Line::from(Span::styled(
        row.into_iter().collect::<String>(),
        style_of(colour, Color::Cyan),
    ))
}

/// What every row painter needs that is not about the picture's shape: whether
/// to honour the theme, and whether the spine passes through this row.
#[derive(Clone, Copy)]
struct Row2 {
    colour: bool,
    spine: bool,
}

/// A frame edge: `┌───┴───┐` across the top of every box in a band, with the
/// drop arriving on the `┴`, or `└───────┘` along the bottom.
///
/// **This is the fix for the lines reading as disconnected.** The drop used to
/// end in the empty space above a bare glyph; now it ends on a corner of a
/// drawn edge, and the eye can see the box it is plugged into.
fn lid_row(w: u16, p: &Plan, band: usize, top: bool, ctx: Row2) -> Line<'static> {
    let (spine, colour) = (ctx.spine, ctx.colour);
    let mut row = vec![' '; w as usize];
    if spine {
        row[HUB_MID as usize] = '│';
    }
    let start = band * p.cols;
    let end = ((band + 1) * p.cols).min(p.total);
    for i in start..end {
        let x = node_x(p, i) as usize;
        let (l, r, bar) = if top {
            ('┌', '┐', '─')
        } else {
            ('└', '┘', '─')
        };
        if let Some(c) = row.get_mut(x) {
            *c = l;
        }
        for c in x + 1..x + NODE_W as usize - 1 {
            if let Some(cell) = row.get_mut(c) {
                *cell = bar;
            }
        }
        if let Some(cell) = row.get_mut(x + NODE_W as usize - 1) {
            *cell = r;
        }
        if top {
            // The drop lands on the middle of the *art*, so the line meets the
            // machine rather than the frame hanging around it.
            if let Some(cell) = row.get_mut(x + DROP_IN_NODE as usize) {
                *cell = '┴';
            }
        }
    }
    Line::from(Span::styled(
        row.into_iter().collect::<String>(),
        style_of(colour, Color::Cyan),
    ))
}

/// One art row of a band, between its frame's edges.
///
/// The art takes the box's *stage* colour: a cluster mid-render is a wall of
/// cyan and a digest is a wall of magenta, carrying the Workers pane's reading
/// over rather than inventing a second one. A fault outranks it, and idle is
/// not the same grey as never-contacted. **The selection is a background across
/// the whole framed cell on every row** — frame bar, art, label and frame bar —
/// so the marked box is a rectangle, not a patch around two lines of text.
fn art_row(
    app: &App,
    p: &Plan,
    band: usize,
    row: usize,
    now: u64,
    w: u16,
    ctx: Row2,
) -> Line<'static> {
    let (colour, spine) = (ctx.colour, ctx.spine);
    let mut ink = Ink::new();
    if spine {
        ink.put(HUB_MID as usize, "│", Style::default());
    }
    let start = band * p.cols;
    let end = ((band + 1) * p.cols).min(p.total);
    for i in start..end {
        let m = &app.machines[i];
        let x = node_x(p, i) as usize;
        let base = if i == app.selected {
            Style::default().bg(selection_bg())
        } else {
            Style::default()
        };
        let work = work_label(m);
        let tint = machine_tint(&work, node_stage(&app.machines, &app.beats, &m.addr, now));
        // The label is two lines, on the art's first two rows: which box it is,
        // and what it is on. The alias is the animal the worker reports — the
        // word the Workers pane, the log and Stats already use — so one box has
        // one name everywhere.
        let label = match row {
            0 => format!(
                "{} {}",
                graph_mark(m),
                machine_alias(&app.machines, &app.beats, &m.addr, now)
            ),
            1 => current_work(&app.machines, &app.beats, &m.addr, now),
            _ => String::new(),
        };
        // The cell is always the full width, so the selection's background is a
        // rectangle whichever row the eye lands on.
        let cell = format!(
            "│{}{}│",
            SERVER[row],
            fit(&format!(" {label}"), LABEL_W as usize)
        );
        ink.put(x, &cell, style_of(colour, tint).patch(base));
    }
    ink.finish(w)
}

/// Absolute column of the `i`th node's frame in the window.
fn node_x(p: &Plan, i: usize) -> u16 {
    COL0 + (i % p.cols) as u16 * PITCH
}

/// Absolute column of the middle of each present node's art, in a band.
fn band_centres(p: &Plan, band: usize) -> Vec<u16> {
    let start = band * p.cols;
    let end = ((band + 1) * p.cols).min(p.total);
    (start..end).map(|i| node_x(p, i) + DROP_IN_NODE).collect()
}

/// A row under construction: text placed at absolute columns, with the gaps
/// filled in as it goes.
///
/// Absolute placement is the point — every glyph in this picture is either at a
/// node's column or at the middle of a node's art, and a painter that counted
/// characters as it wrote them would drift out of column the moment one name
/// was longer than the last.
struct Ink {
    spans: Vec<Span<'static>>,
    at: usize,
}

impl Ink {
    fn new() -> Self {
        Ink {
            spans: Vec::new(),
            at: 0,
        }
    }

    /// Write `text` starting at `col`, padding whatever is between.
    fn put(&mut self, col: usize, text: &str, style: Style) {
        if col > self.at {
            self.spans.push(Span::raw(" ".repeat(col - self.at)));
            self.at = col;
        }
        self.at += text.chars().count();
        self.spans.push(Span::styled(text.to_string(), style));
    }

    /// Extend to the pane's width, so a selected cell's background reaches the
    /// border instead of stopping at the last glyph.
    fn finish(mut self, w: u16) -> Line<'static> {
        if self.at < w as usize {
            self.spans.push(Span::raw(" ".repeat(w as usize - self.at)));
        }
        Line::from(self.spans)
    }
}

/// Exactly `w` columns: clipped, never padded past it.
fn fit(s: &str, w: usize) -> String {
    let mut out: String = s.chars().take(w).collect();
    let len = out.chars().count();
    if len < w {
        out.push_str(&" ".repeat(w - len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_arts_are_the_same_size_so_they_read_as_one_rack() {
        // The console and the server are two faces of the same picture. If one
        // of them grew a column, the bus would stop meeting the middle of the
        // node and the whole drawing would lean.
        assert_eq!(HUB.len(), SERVER.len());
        for (h, s) in HUB.iter().zip(SERVER.iter()) {
            assert_eq!(h.chars().count(), s.chars().count(), "{h:?} / {s:?}");
        }
        assert_eq!(HUB[0].chars().count() as u16, ART_W);
        assert_eq!(NODE_W, 1 + ART_W + LABEL_W + 1, "frame, art, label, frame");
        // The label is wide enough for the longest thing that goes in it.
        let widest = fit(&format!(" {}", "x".repeat(40)), LABEL_W as usize);
        assert_eq!(widest.chars().count(), LABEL_W as usize);
    }

    #[test]
    fn nodes_tile_across_and_bands_stack_below() {
        // A 120-column pane is five framed boxes, a 74-column compact pane is
        // three. A terminal too narrow for even one still gets one, clipped.
        assert_eq!(columns_for(118), 5);
        assert_eq!(columns_for(74), 3);
        assert_eq!(columns_for(5), 1);
        // Every node the count promises fits inside the width it was given.
        for w in [20u16, 40, 74, 118, 160] {
            let cols = columns_for(w);
            let used = COL0 + (cols as u16 - 1) * PITCH + NODE_W;
            assert!(used <= w || cols == 1, "{cols} nodes need {used} of {w}");
        }
        // Bands are what the extra height buys, and a frame costs a row each
        // end: eleven is one band, eighteen two, and a pane one row short still
        // gets a rack rather than a border.
        assert_eq!(bands_for(FULL_H, true), 1);
        assert_eq!(bands_for(FULL_H + BAND_H, true), 2);
        assert_eq!(bands_for(FULL_H + BAND_H - 1, true), 1);
        assert_eq!(bands_for(3, true), 1, "never zero bands");
    }

    #[test]
    fn the_plan_is_the_height_and_the_picture_is_the_plan() {
        // The one property worth holding: the pane's height and the rows it
        // draws come from one description, so a change to the shape cannot land
        // in one and miss the other.
        let p = plan(FULL_H + BAND_H, 118, 8, 0, true);
        assert_eq!(p.rows.len() as u16, CHASSIS + 2 * BAND_H, "two bands");
        assert_eq!(p.rows.first(), Some(&Row::Hub { row: 0 }));
        assert_eq!(p.rows.get(HUB.len()), Some(&Row::Spine));
        let rail = HUB.len() + 1;
        assert_eq!(
            p.rows.get(rail),
            Some(&Row::Rail {
                band: 0,
                last: false
            }),
            "the bus leaves the spine"
        );
        assert_eq!(p.rows.get(rail + 1), Some(&Row::Lid { band: 0 }));
        assert_eq!(p.rows.get(rail + 2), Some(&Row::Art { band: 0, row: 0 }));
        // The first band carries the bus on; the second closes it. That is the
        // difference between a spine and a row of dashes.
        assert_eq!(
            p.rows.get(rail + BAND_H as usize),
            Some(&Row::Rail {
                band: 1,
                last: true
            })
        );
        assert_eq!(
            p.rows.last(),
            Some(&Row::Sill {
                band: 1,
                last: true
            })
        );
        // A cluster smaller than one window is not padded with empty bands.
        let one = plan(FULL_H + BAND_H, 118, 2, 0, true);
        assert_eq!(one.rows.len() as u16, CHASSIS + BAND_H);
        // The lean form is the same picture with the console's art replaced by
        // its name — three rows shorter, and nothing else different.
        let lean = plan(LEAN_H + BAND_H, 118, 3, 0, false);
        assert_eq!(lean.rows.len() as u16, LEAN_CHASSIS + BAND_H);
        assert_eq!(lean.rows[0], Row::Hub { row: 0 });
        assert_eq!(lean.rows[1], Row::Spine);
    }

    #[test]
    fn the_window_is_full_and_its_leftovers_are_counted() {
        // A window shows whole bands and is full whenever there is enough to
        // fill it: a rack that went half-empty on every scroll would be a rack
        // reporting on its own padding.
        let p = plan(FULL_H + BAND_H, 118, 40, 0, true);
        assert_eq!(p.bands, 2, "two bands fill it");
        assert_eq!(p.cols, 5);
        assert_eq!(
            p.rows.last(),
            Some(&Row::More { hidden: 30 }),
            "ten shown of forty: thirty are not"
        );
        // And the count does not change with the scroll — the window is always
        // `bands × cols` boxes — so the pane's height is the same on every page.
        let scrolled = plan(FULL_H + BAND_H, 118, 40, 6, true);
        assert_eq!(scrolled.rows.len(), p.rows.len());
        assert_eq!(scrolled.rows.last(), Some(&Row::More { hidden: 30 }));
        // Everything on screen means no count row at all.
        let all = plan(FULL_H + BAND_H, 118, 10, 0, true);
        assert_eq!(
            all.rows.last(),
            Some(&Row::Sill {
                band: 1,
                last: true
            })
        );
    }

    #[test]
    fn the_window_follows_the_cursor_and_stays_in_range() {
        // Sixteen boxes at four columns is four bands, two on screen. Selecting
        // the last must show it, without running the window off the cluster.
        assert_eq!(first_band(0, 0, 4, 2, 4), 0);
        assert_eq!(first_band(4, 0, 4, 2, 4), 0, "band 2 is on the first page");
        assert_eq!(first_band(8, 0, 4, 2, 4), 1, "one band down");
        assert_eq!(first_band(15, 0, 4, 2, 4), 2, "the last band, not past it");
        // And back up. A cursor already inside the window must not drag it
        // along — that is what made the earlier, remembered-offset version
        // unusable: every press down scrolled the rack out from under you.
        assert_eq!(first_band(10, 2, 4, 2, 4), 2, "the window does not jump");
        assert_eq!(first_band(7, 2, 4, 2, 4), 1, "a band up when it must");
        assert_eq!(first_band(0, 2, 4, 2, 4), 0);
        // A short cluster never scrolls, whatever is selected.
        assert_eq!(first_band(2, 0, 4, 2, 1), 0);
    }

    #[test]
    fn the_richer_form_is_chosen_and_the_table_is_the_fallback() {
        // Twelve interior rows is the console; nine is its name; anything under
        // that is a rack with the servers' legs cut off, and the caller draws
        // the table instead of showing either silently broken.
        assert_eq!(form_for(20), Some(true));
        assert_eq!(form_for(FULL_H), Some(true));
        assert_eq!(form_for(FULL_H - 1), Some(false));
        assert_eq!(form_for(LEAN_H), Some(false));
        assert_eq!(form_for(LEAN_H - 1), None);
        // And the height a form asks for is the one that form was chosen for,
        // so the layout and the painter cannot disagree about the shape.
        for avail in [LEAN_H, LEAN_H + 1, FULL_H, FULL_H + 1, 30] {
            let form = form_for(avail).expect("this row count fits something");
            assert!(
                bands_for(avail, form) >= 1,
                "{avail} rows fit a form that then draws nothing"
            );
        }
    }

    #[test]
    fn the_bus_runs_down_the_spine_and_lands_on_a_corner_of_a_box() {
        let p = plan(FULL_H, 118, 3, 0, true);
        let w = 118u16;
        let centres = band_centres(&p, 0);
        assert_eq!(centres.len(), 3, "a node the width promised");
        let bus = rail_row(w, &p, 0, true, false).to_string();
        // `└` at the middle of the console, `┬` over each node, `┘` past the
        // last. The whole edge is one row, so the bus cannot be misread as
        // something that goes somewhere else.
        assert_eq!(bus.chars().nth(HUB_MID as usize), Some('└'));
        for c in &centres {
            assert_eq!(bus.chars().nth(*c as usize), Some('┬'), "at {c}");
        }
        let last = *centres.last().unwrap() as usize;
        assert_eq!(bus.chars().nth(last + 1), Some('┘'));
        // Nothing between the spine and the last node is blank: a gap in the
        // bus would read as a box that is not connected.
        assert!(
            bus.chars()
                .skip(HUB_MID as usize)
                .take(last + 2 - HUB_MID as usize)
                .all(|c| c != ' '),
            "{bus}"
        );
        // **The drop must end on a drawn edge.** This is the whole point of the
        // frame: a line that stops in the blank space above a bare glyph reads
        // as a stroke, not a connection.
        let lid = lid_row(
            w,
            &p,
            0,
            true,
            Row2 {
                colour: false,
                spine: false,
            },
        )
        .to_string();
        for (slot, c) in centres.iter().enumerate() {
            assert_eq!(
                lid.chars().nth(*c as usize),
                Some('┴'),
                "the drop lands on a box's top edge, at {c}"
            );
            let x = node_x(&p, slot) as usize;
            assert_eq!(lid.chars().nth(x), Some('┌'), "the frame's left edge");
            assert_eq!(
                lid.chars().nth(x + NODE_W as usize - 1),
                Some('┐'),
                "the frame's right edge"
            );
            // And the top edge is unbroken either side of the drop, so the box
            // reads as a box rather than as two brackets.
            assert!(lid.chars().skip(x).take(NODE_W as usize).all(|c| c != ' '));
        }
        // The bottom edge closes where the top one opened, and on the middle of
        // the art it is a plain bar: the bus has arrived.
        let sill = lid_row(
            w,
            &p,
            0,
            false,
            Row2 {
                colour: false,
                spine: false,
            },
        )
        .to_string();
        for slot in 0..3 {
            let x = node_x(&p, slot) as usize;
            assert_eq!(sill.chars().nth(x), Some('└'));
            assert_eq!(sill.chars().nth(x + NODE_W as usize - 1), Some('┘'));
            assert_eq!(sill.chars().nth(x + DROP_IN_NODE as usize), Some('─'));
        }
        // A band with another below it carries the bus on instead of closing it.
        let two = plan(FULL_H + BAND_H, 118, 3, 0, true);
        let carried = rail_row(w, &two, 0, false, false).to_string();
        assert_eq!(carried.chars().nth(HUB_MID as usize), Some('├'));
        assert_eq!(carried.chars().nth(last + 1), Some('┤'));
    }

    #[test]
    fn every_row_of_a_box_is_the_same_width_so_the_selection_is_a_rectangle() {
        // The bug this replaced: the selection's background covered the label on
        // the first two rows and stopped dead on the other two, so the marked box
        // was an L rather than a rectangle. The cell is now written whole.
        let mut app = App::new("http://127.0.0.1:8901");
        app.machines = vec![bm_proto::Machine::new(
            "52.2.2.2", "thang", 4, None, "worker",
        )];
        let p = plan(FULL_H, 118, 1, 0, true);
        for row in 0..SERVER.len() {
            let line = art_row(
                &app,
                &p,
                0,
                row,
                0,
                118,
                Row2 {
                    colour: false,
                    spine: false,
                },
            );
            // Box-drawing glyphs are three bytes each, so the cell is cut out of
            // a `Vec<char>` — slicing the string by byte would hand back a
            // handful of glyphs and call the test a pass.
            let text: Vec<char> = line
                .spans
                .iter()
                .flat_map(|s| s.content.as_ref().chars())
                .collect();
            let x = node_x(&p, 0) as usize;
            let cell: String = text[x..x + NODE_W as usize].iter().collect();
            assert_eq!(cell.chars().count(), NODE_W as usize, "{cell:?}");
            assert_eq!(cell.chars().next(), Some('│'), "the frame's left bar");
            assert_eq!(
                cell.chars().last(),
                Some('│'),
                "the frame's right bar on row {row}: {cell:?}"
            );
        }
    }
}
