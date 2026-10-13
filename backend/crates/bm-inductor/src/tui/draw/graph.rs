//! The Machines pane drawn as a rack: the inductor's console, the boxes it

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
pub(crate) const HUB: [&str; 4] = ["/---\\", "|   |", "|___|", " \\_/ "];

/// A box, as a rack unit. Four rows by five columns.
pub(crate) const SERVER: [&str; 4] = [" ___ ", "|[_]|", "|+ ;|", "`---'"];

const ART_W: u16 = 5;
/// Art, a space, then the label. Fourteen, because `digest 12 50%` plus its
const LABEL_W: u16 = 14;
/// The whole cell, **frame included**: a bar, the art, the label, a bar. The
const NODE_W: u16 = 1 + ART_W + LABEL_W + 1;
/// One blank column between two frames, so two boxes never touch.
const PITCH: u16 = NODE_W + 1;
/// Where the first node's frame starts, measured from the pane's left edge.
const COL0: u16 = ART_W + 3;
/// The bus leaves the hub from the middle of its art — and a node's drop lands on
const DROP_IN_NODE: u16 = 1 + ART_W / 2;
/// The bus leaves the hub from the middle of its art.
const HUB_MID: u16 = ART_W / 2;

/// One band of servers: the rail they hang from, the frame's top edge, four
const BAND_H: u16 = 1 + 1 + 4 + 1;
/// The console, and the spine that runs down from it. The lean form is the
const CHASSIS: u16 = HUB.len() as u16 + 1;
const LEAN_CHASSIS: u16 = 2;

/// Interior rows one band of servers needs, chassis included.
pub(crate) const FULL_H: u16 = CHASSIS + BAND_H;
pub(crate) const LEAN_H: u16 = LEAN_CHASSIS + BAND_H;

/// The richest form that fits `avail` interior rows, or `None` for none of them.
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
pub(crate) fn bands_for(avail: u16, hub_art: bool) -> usize {
    let chassis = if hub_art { CHASSIS } else { LEAN_CHASSIS };
    (avail.saturating_sub(chassis) / BAND_H).max(1) as usize
}

/// How many nodes fit side by side in `w` interior columns.
pub(crate) fn columns_for(w: u16) -> usize {
    (w.saturating_sub(COL0) / PITCH).max(1) as usize
}

/// The window's top band, for a cursor at `selected` in a window at `first`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    /// One row of the console, or the single line it is replaced by in the lean
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
pub(crate) fn plan(avail: u16, w: u16, total: usize, first: usize, hub_art: bool) -> Plan {
    let cols = columns_for(w);
    let total_bands = total.div_ceil(cols).max(1);
    // A window shows whole bands and is full whenever there are enough boxes to
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
    app.graph_band = first_band(app.selected, app.graph_band, cols, bands, total_bands);
    // Published for `↑` and `↓`, which move the cursor a *row of the rack*, and
    app.graph_cols = cols;
    let p = plan(avail, w, total, app.graph_band, hub_art);

    // The spine is one continuous line from the console to the last rail, so
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
#[derive(Clone, Copy)]
struct Row2 {
    colour: bool,
    spine: bool,
}

/// A frame edge: `┌───┴───┐` across the top of every box in a band, with the
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
        assert_eq!(bands_for(FULL_H, true), 1);
        assert_eq!(bands_for(FULL_H + BAND_H, true), 2);
        assert_eq!(bands_for(FULL_H + BAND_H - 1, true), 1);
        assert_eq!(bands_for(3, true), 1, "never zero bands");
    }

    #[test]
    fn the_plan_is_the_height_and_the_picture_is_the_plan() {
        // The one property worth holding: the pane's height and the rows it
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
        let lean = plan(LEAN_H + BAND_H, 118, 3, 0, false);
        assert_eq!(lean.rows.len() as u16, LEAN_CHASSIS + BAND_H);
        assert_eq!(lean.rows[0], Row::Hub { row: 0 });
        assert_eq!(lean.rows[1], Row::Spine);
    }

    #[test]
    fn the_window_is_full_and_its_leftovers_are_counted() {
        // A window shows whole bands and is full whenever there is enough to
        let p = plan(FULL_H + BAND_H, 118, 40, 0, true);
        assert_eq!(p.bands, 2, "two bands fill it");
        assert_eq!(p.cols, 5);
        assert_eq!(
            p.rows.last(),
            Some(&Row::More { hidden: 30 }),
            "ten shown of forty: thirty are not"
        );
        // And the count does not change with the scroll — the window is always
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
        assert_eq!(first_band(0, 0, 4, 2, 4), 0);
        assert_eq!(first_band(4, 0, 4, 2, 4), 0, "band 2 is on the first page");
        assert_eq!(first_band(8, 0, 4, 2, 4), 1, "one band down");
        assert_eq!(first_band(15, 0, 4, 2, 4), 2, "the last band, not past it");
        // And back up. A cursor already inside the window must not drag it
        assert_eq!(first_band(10, 2, 4, 2, 4), 2, "the window does not jump");
        assert_eq!(first_band(7, 2, 4, 2, 4), 1, "a band up when it must");
        assert_eq!(first_band(0, 2, 4, 2, 4), 0);
        // A short cluster never scrolls, whatever is selected.
        assert_eq!(first_band(2, 0, 4, 2, 1), 0);
    }

    #[test]
    fn the_richer_form_is_chosen_and_the_table_is_the_fallback() {
        // Twelve interior rows is the console; nine is its name; anything under
        assert_eq!(form_for(20), Some(true));
        assert_eq!(form_for(FULL_H), Some(true));
        assert_eq!(form_for(FULL_H - 1), Some(false));
        assert_eq!(form_for(LEAN_H), Some(false));
        assert_eq!(form_for(LEAN_H - 1), None);
        // And the height a form asks for is the one that form was chosen for,
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
        assert_eq!(bus.chars().nth(HUB_MID as usize), Some('└'));
        for c in &centres {
            assert_eq!(bus.chars().nth(*c as usize), Some('┬'), "at {c}");
        }
        let last = *centres.last().unwrap() as usize;
        assert_eq!(bus.chars().nth(last + 1), Some('┘'));
        // Nothing between the spine and the last node is blank: a gap in the
        assert!(
            bus.chars()
                .skip(HUB_MID as usize)
                .take(last + 2 - HUB_MID as usize)
                .all(|c| c != ' '),
            "{bus}"
        );
        // **The drop must end on a drawn edge.** This is the whole point of the
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
            assert!(lid.chars().skip(x).take(NODE_W as usize).all(|c| c != ' '));
        }
        // The bottom edge closes where the top one opened, and on the middle of
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
