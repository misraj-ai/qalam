//! **L7 — table reconstruction from ruled lines.**
//!
//! A PDF does not contain tables. It contains text, and lines, and the *reader*
//! infers a table. This module does the same inference: cluster the ruled lines
//! (from `content.rs`) into a grid, then assign each line of text to the cell
//! that contains it.
//!
//! Ruled tables are the tractable case, and the only one attempted here.
//! Borderless tables — where alignment alone implies the columns — are a
//! stretch goal and near-greenfield for RTL (PLAN.md §1, §8).
//!
//! # Conservative on purpose
//!
//! A false table is worse than a missed one. Missing a table leaves the text in
//! reading order, which is merely unstructured; inventing one shreds a
//! paragraph into cells. So a grid must have at least two rows *and* two
//! columns, its lines must actually meet, and the whole thing carries a
//! confidence score derived from how much of the grid is really drawn.
//!
//! # Right-to-left
//!
//! Columns are numbered in **reading order**, so on an Arabic page column 0 is
//! the rightmost. That is the one substantive difference from a Latin table
//! reconstructor, and it means `rows[0][0]` is the cell a reader starts at.

use crate::arabic::TextLine;
use crate::content::RuledLine;
use crate::layout::Item;
use crate::types::Rect;

/// Lines closer together than this are the same grid boundary.
///
/// Table borders are often drawn twice — once per adjoining cell — and land a
/// fraction of a point apart.
const MERGE_TOLERANCE: f64 = 2.0;

/// A boundary must run at least this fraction of the table's extent to count.
///
/// Stops a short rule inside one cell from being read as a full row divider.
const MIN_SPAN_FRACTION: f64 = 0.5;

/// Fewer boundaries than this in either direction is not a table.
///
/// Three lines make two rows (or columns), so this is "at least 2x2".
const MIN_BOUNDARIES: usize = 3;

/// A detected grid: the coordinates its cells are bounded by.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    /// Column boundaries, left to right. `xs.len() - 1` columns.
    pub xs: Vec<f64>,
    /// Row boundaries, top to bottom (descending y, as reading order runs).
    pub ys: Vec<f64>,
    /// The area the grid covers.
    pub bbox: Rect,
    /// How much of the grid is actually drawn, 0.0 to 1.0.
    pub confidence: f64,
    /// Whether the grid came from ink rather than from alignment.
    pub ruled: bool,
}

impl Grid {
    /// Whether this grid was drawn rather than inferred.
    pub fn was_ruled(&self) -> bool {
        self.ruled
    }

    /// Number of rows.
    pub fn row_count(&self) -> usize {
        self.ys.len().saturating_sub(1)
    }

    /// Number of columns.
    pub fn column_count(&self) -> usize {
        self.xs.len().saturating_sub(1)
    }

    /// The rectangle of one cell, in PDF coordinates.
    ///
    /// `column` is in **reading order**: 0 is the rightmost cell on an RTL
    /// page, the leftmost on an LTR one.
    pub fn cell_box(&self, row: usize, column: usize, rtl: bool) -> Option<Rect> {
        let top = *self.ys.get(row)?;
        let bottom = *self.ys.get(row + 1)?;

        // `xs` runs left to right; reading order may not.
        let index = if rtl {
            self.xs.len().checked_sub(column + 2)?
        } else {
            column
        };
        let left = *self.xs.get(index)?;
        let right = *self.xs.get(index + 1)?;

        Some(Rect::new(left, bottom, right, top))
    }
}

/// One cell's contents.
#[derive(Debug, Clone)]
pub struct Cell {
    /// The text in the cell, lines joined with spaces.
    ///
    /// Spaces rather than newlines: a cell's line breaks are the column's
    /// width talking, not the author's.
    pub text: String,
    /// Where the cell is.
    pub bbox: Rect,
    /// Row index, counting from the top.
    pub row: usize,
    /// Column index in reading order — 0 is rightmost on an RTL page.
    pub column: usize,
}

/// A reconstructed table.
#[derive(Debug, Clone)]
pub struct Table {
    /// Cells by row, then by column in reading order.
    pub rows: Vec<Vec<Cell>>,
    /// The area the table covers.
    pub bbox: Rect,
    /// How much of the grid was actually drawn, 0.0 to 1.0.
    ///
    /// Reconstruction is best-effort and this says how much to trust it
    /// (PLAN.md §1). A fully ruled grid scores 1.0.
    pub confidence: f64,
}

impl Table {
    /// How many cells hold any text.
    pub fn filled_cells(&self) -> usize {
        self.rows
            .iter()
            .flatten()
            .filter(|c| !c.text.is_empty())
            .count()
    }

    /// The fraction of cells holding text, 0.0 to 1.0.
    pub fn filled_fraction(&self) -> f64 {
        let total: usize = self.rows.iter().map(Vec::len).sum();
        if total == 0 {
            return 0.0;
        }
        self.filled_cells() as f64 / total as f64
    }

    /// Whether this looks like a real table rather than decoration.
    ///
    /// # The frame problem
    ///
    /// Geometry alone cannot tell a table from a **doubled rectangle**. Page 4
    /// of the Arabic corpus draws a rounded frame twice, one offset behind the
    /// other as a shadow: four horizontal rules and four vertical ones, every
    /// one spanning the full extent. That is indistinguishable from a 3x3 grid
    /// — and reading it as one shredded a paragraph into empty cells, which the
    /// golden file caught.
    ///
    /// What separates them is not where the lines are but **what is inside**. A
    /// real table fills most of its cells; a frame has everything in the middle
    /// one and nothing anywhere else.
    ///
    /// Deliberately strict: missing a table leaves text merely unstructured,
    /// while inventing one destroys it.
    pub fn is_plausible(&self) -> bool {
        /// A table must have at least this share of its cells filled.
        const MIN_FILLED_FRACTION: f64 = 0.5;
        /// ...and this many filled cells outright, so a 2x2 with two filled
        /// cells does not pass on ratio alone.
        const MIN_FILLED_CELLS: usize = 4;

        self.filled_cells() >= MIN_FILLED_CELLS && self.filled_fraction() >= MIN_FILLED_FRACTION
    }

    /// Number of rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Number of columns.
    pub fn column_count(&self) -> usize {
        self.rows.first().map_or(0, Vec::len)
    }
}

/// A run of collinear lines merged into one boundary.
#[derive(Debug, Clone)]
struct Boundary {
    /// The shared coordinate: y for a row divider, x for a column divider.
    at: f64,
    /// How far the boundary runs along the other axis.
    from: f64,
    /// How far the boundary runs along the other axis.
    to: f64,
}

impl Boundary {
    fn length(&self) -> f64 {
        self.to - self.from
    }

    /// Does this boundary overlap another's extent?
    fn overlaps(&self, from: f64, to: f64) -> bool {
        self.from < to && from < self.to
    }
}

/// Find the grids on a page.
///
/// Returns one [`Grid`] per table. A page with no ruled table returns an empty
/// list, which is the usual answer.
pub fn detect(lines: &[RuledLine]) -> Vec<Grid> {
    let horizontals = merge(lines.iter().filter(|l| l.is_horizontal()), true);
    let verticals = merge(lines.iter().filter(|l| l.is_vertical()), false);

    if horizontals.len() < MIN_BOUNDARIES || verticals.len() < MIN_BOUNDARIES {
        return Vec::new();
    }

    let mut grids = Vec::new();
    for group in group_rows(&horizontals) {
        if let Some(grid) = build_grid(&group, &verticals) {
            grids.push(grid);
        }
    }
    grids
}

/// Collapse collinear, overlapping segments into single boundaries.
///
/// `along_x` selects the axis: horizontal lines share a y and span an x range.
fn merge<'a>(lines: impl Iterator<Item = &'a RuledLine>, along_x: bool) -> Vec<Boundary> {
    let mut raw: Vec<Boundary> = lines
        .map(|l| {
            let (at, from, to) = if along_x {
                ((l.y0 + l.y1) / 2.0, l.x0.min(l.x1), l.x0.max(l.x1))
            } else {
                ((l.x0 + l.x1) / 2.0, l.y0.min(l.y1), l.y0.max(l.y1))
            };
            Boundary { at, from, to }
        })
        .collect();

    // `total_cmp` gives `f64` the total order `sort_by` needs; a NaN
    // coordinate sorts to one end rather than corrupting the result.
    raw.sort_by(|a, b| a.at.total_cmp(&b.at));

    let mut merged: Vec<Boundary> = Vec::new();
    for boundary in raw {
        match merged.last_mut() {
            // Same coordinate within tolerance: extend the existing boundary
            // rather than adding a second one. Borders drawn once per adjoining
            // cell would otherwise double every divider.
            Some(last) if (boundary.at - last.at).abs() <= MERGE_TOLERANCE => {
                last.from = last.from.min(boundary.from);
                last.to = last.to.max(boundary.to);
            }
            _ => merged.push(boundary),
        }
    }
    merged
}

/// Group row boundaries into candidate tables.
///
/// Two dividers belong to the same table when they overlap horizontally. A page
/// with a table at the top and another at the bottom produces two groups,
/// because although both are horizontal they need not share an x range — and
/// even when they do, the gap between them is broken by the requirement below
/// that a group's members be mutually overlapping.
fn group_rows(horizontals: &[Boundary]) -> Vec<Vec<Boundary>> {
    /// A gap this many times the page's typical row spacing starts a new table.
    ///
    /// Rows are regularly spaced; a jump several times further than the page's
    /// usual spacing is a different object, not a very tall row.
    const OUTLIER_FACTOR: f64 = 3.0;

    // The typical spacing is measured across the **whole page**, not
    // accumulated as each group grows. Judging incrementally cannot work: the
    // first two boundaries of a group have no spacing to compare against, so
    // any jump at all is accepted — and one isolated rule near the foot of the
    // page then joins the table above it and stretches the group's height
    // until no real column is tall enough to qualify. That is precisely how a
    // form XObject's decorative line destroyed a table (PLAN.md §10.16).
    let limit = median_gap(horizontals).map(|m| m * OUTLIER_FACTOR);

    let mut groups: Vec<Vec<Boundary>> = Vec::new();

    for boundary in horizontals {
        // Sorted by coordinate, so only the most recent group can match.
        let extend = groups
            .last()
            .and_then(|g| g.last())
            .is_some_and(|previous| {
                previous.overlaps(boundary.from, boundary.to)
                    && limit.is_none_or(|limit| (boundary.at - previous.at) <= limit)
            });

        match (extend, groups.last_mut()) {
            (true, Some(group)) => group.push(boundary.clone()),
            _ => groups.push(vec![boundary.clone()]),
        }
    }

    groups
        .into_iter()
        .filter(|g| g.len() >= MIN_BOUNDARIES)
        .collect()
}

/// The median spacing between consecutive boundaries on the page.
///
/// Median rather than mean because the outliers are exactly what this is meant
/// to identify, and a mean would be dragged towards them.
fn median_gap(boundaries: &[Boundary]) -> Option<f64> {
    if boundaries.len() < 2 {
        return None;
    }
    let mut gaps: Vec<f64> = boundaries.windows(2).map(|w| w[1].at - w[0].at).collect();
    gaps.sort_by(f64::total_cmp);

    let median = gaps[gaps.len() / 2];
    // A page whose rules are nearly coincident would give a median near zero,
    // and every real gap would then look like an outlier.
    (median > 1.0).then_some(median)
}

/// Turn a group of row dividers plus the page's column dividers into a grid.
fn build_grid(rows: &[Boundary], verticals: &[Boundary]) -> Option<Grid> {
    let top = rows.iter().map(|b| b.at).fold(f64::NEG_INFINITY, f64::max);
    let bottom = rows.iter().map(|b| b.at).fold(f64::INFINITY, f64::min);
    let left = rows.iter().map(|b| b.from).fold(f64::INFINITY, f64::min);
    let right = rows.iter().map(|b| b.to).fold(f64::NEG_INFINITY, f64::max);

    let width = right - left;
    let height = top - bottom;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    // Columns are the vertical dividers that actually run down this table.
    let columns: Vec<&Boundary> = verticals
        .iter()
        .filter(|v| v.overlaps(bottom, top) && v.length() >= height * MIN_SPAN_FRACTION)
        .collect();

    if columns.len() < MIN_BOUNDARIES {
        return None;
    }

    // Keep only the row dividers that really span the table, so a short rule
    // inside one cell cannot masquerade as a divider.
    let kept: Vec<&Boundary> = rows
        .iter()
        .filter(|r| r.length() >= width * MIN_SPAN_FRACTION)
        .collect();
    if kept.len() < MIN_BOUNDARIES {
        return None;
    }

    // Rows top to bottom: PDF's y grows upwards, so that is descending.
    let mut ys: Vec<f64> = kept.iter().map(|b| b.at).collect();
    ys.sort_by(|a, b| b.total_cmp(a));

    let mut xs: Vec<f64> = columns.iter().map(|b| b.at).collect();
    xs.sort_by(f64::total_cmp);

    // How complete the grid is: the mean fraction of the table's extent that
    // each divider actually covers. A fully ruled table scores 1.0; one with
    // dividers that stop short scores lower, and says so.
    let row_coverage: f64 = kept
        .iter()
        .map(|b| (b.length() / width).min(1.0))
        .sum::<f64>()
        / kept.len() as f64;
    let column_coverage: f64 = columns
        .iter()
        .map(|b| (b.length() / height).min(1.0))
        .sum::<f64>()
        / columns.len() as f64;

    Some(Grid {
        bbox: Rect::new(left, bottom, right, top),
        confidence: (row_coverage + column_coverage) / 2.0,
        ruled: true,
        xs,
        ys,
    })
}

// ---------------------------------------------------------------------------
// Borderless tables
//
// A ruled table announces itself; a borderless one has to be inferred from
// alignment alone, which PLAN.md §1 flags as a stretch goal and near-greenfield
// for RTL. The inference is deliberately timid — see `MIN_BORDERLESS_COLUMNS`.
// ---------------------------------------------------------------------------

/// A borderless table must have at least this many rows.
///
/// Alignment only becomes evidence when it repeats. Three rows of prose can
/// line up by chance; a dozen cannot.
const MIN_BORDERLESS_ROWS: usize = 5;

/// ...and at least this many columns.
///
/// **Three is not enough, and this is the hard part of the whole feature.**
/// Page 8 of `test_for_arabic_barser.pdf` sets two columns of numbered cards;
/// the badges form a third column between them, every row shares a baseline,
/// and the result is geometrically *indistinguishable* from a table. The cells
/// even have the right length. What tells them apart is that one is wrapped
/// prose continuing from row to row — which is a fact about language, not about
/// where the ink is.
///
/// So the bar is set where geometry can still carry it. Each additional
/// aligned column multiplies the improbability of coincidence, and a
/// four-column layout of prose is rare where a four-column table is ordinary.
///
/// The cost is real and worth stating: a genuine three-column borderless table
/// is missed. That is the trade `is_plausible` already makes for ruled grids —
/// a false table destroys text, a missed one merely leaves it unstructured.
const MIN_BORDERLESS_COLUMNS: usize = 4;

/// How wide a gap between columns must be, in ems.
///
/// Lower than a page-level gutter, because inside a table the columns are close
/// together, but still well above the widest word space.
const MIN_BORDERLESS_GAP_EMS: f64 = 0.8;

/// A row further from its neighbour than this multiple of the typical spacing
/// ends the table.
const ROW_OUTLIER_FACTOR: f64 = 2.5;

/// The share of rows that must reach across at least two columns.
///
/// A band where most rows occupy a single column is a list beside a margin
/// note, not a table.
const MIN_MULTI_COLUMN_ROWS: f64 = 0.6;

/// Detect tables that are aligned rather than ruled.
///
/// Returns a [`Grid`] per candidate. The caller still has to fill it and check
/// [`Table::is_plausible`], exactly as for a ruled grid — geometry proposes,
/// content disposes (PLAN.md §10.11).
pub fn detect_borderless(items: &[Item]) -> Vec<Grid> {
    let rows = group_rows_by_baseline(items);
    if rows.len() < MIN_BORDERLESS_ROWS {
        return Vec::new();
    }

    let mut grids = Vec::new();
    for band in regular_bands(&rows) {
        if let Some(grid) = grid_from_band(&band) {
            grids.push(grid);
        }
    }
    grids
}

/// One row of a candidate table: the items sharing a baseline.
struct Row {
    baseline: f64,
    top: f64,
    bottom: f64,
    items: Vec<Item>,
}

/// Group items into rows by baseline, top of the page first.
fn group_rows_by_baseline(items: &[Item]) -> Vec<Row> {
    let mut sorted: Vec<&Item> = items.iter().collect();
    sorted.sort_by(|a, b| b.y0.total_cmp(&a.y0));

    let mut rows: Vec<Row> = Vec::new();
    for item in sorted {
        // The same tolerance the text pipeline uses, so a row here is the same
        // thing a line is there.
        let tolerance = (item.size * 0.3).max(0.5);
        match rows.last_mut() {
            Some(row) if (row.baseline - item.y0).abs() <= tolerance => {
                row.top = row.top.max(item.y1);
                row.bottom = row.bottom.min(item.y0);
                row.items.push(*item);
            }
            _ => rows.push(Row {
                baseline: item.y0,
                top: item.y1,
                bottom: item.y0,
                items: vec![*item],
            }),
        }
    }

    merge_split_rows(&mut rows);
    rows
}

/// Rejoin rows that a small baseline difference tore apart.
///
/// A table row is a horizontal *band*, and the cells in it need not share a
/// baseline: a different font, a different script or a smaller size shifts one
/// by a point or two. Page 3 of `22.pdf` sets its row numbers about 3pt below
/// the rest of their row, which split every row in two — doubling the row count
/// and leaving half of them covering a sliver of the width, so no column
/// boundary could gather the support it needed (PLAN.md §10.24).
///
/// The test that makes this safe is **horizontal disjointness**. Two pieces of
/// text at the same x cannot be the same row however close their baselines;
/// two at different x, a fraction of a line apart, are one row in different
/// columns. Consecutive lines of a paragraph overlap in x almost completely,
/// so they are never merged.
fn merge_split_rows(rows: &mut Vec<Row>) {
    /// How far apart two pieces of one row may sit, in ems.
    const MAX_BASELINE_DRIFT: f64 = 0.6;

    let mut i = 0;
    while i + 1 < rows.len() {
        let em = median_item_size(&rows[i].items).min(median_item_size(&rows[i + 1].items));
        let drift = rows[i].baseline - rows[i + 1].baseline;

        if drift.abs() <= em * MAX_BASELINE_DRIFT && !rows_overlap(&rows[i], &rows[i + 1]) {
            let merged = rows.remove(i + 1);
            rows[i].top = rows[i].top.max(merged.top);
            rows[i].bottom = rows[i].bottom.min(merged.bottom);
            rows[i].items.extend(merged.items);
            // Do not advance: a row may have been split into three.
            continue;
        }
        i += 1;
    }
}

/// Do two rows occupy any of the same horizontal space?
fn rows_overlap(a: &Row, b: &Row) -> bool {
    let extent = |row: &Row| {
        let lo = row.items.iter().fold(f64::INFINITY, |m, i| m.min(i.x0));
        let hi = row.items.iter().fold(f64::NEG_INFINITY, |m, i| m.max(i.x1));
        (lo, hi)
    };
    let (a_lo, a_hi) = extent(a);
    let (b_lo, b_hi) = extent(b);
    a_lo < b_hi && b_lo < a_hi
}

/// The median type size among some items.
fn median_item_size(items: &[Item]) -> f64 {
    let mut sizes: Vec<f64> = items.iter().map(|i| i.size).collect();
    if sizes.is_empty() {
        return 10.0;
    }
    sizes.sort_by(f64::total_cmp);
    sizes[sizes.len() / 2].max(1.0)
}

/// Split rows into runs that are regularly spaced.
///
/// The same reasoning as the ruled path (§10.19): table rows sit at a steady
/// pitch, and a jump several times that pitch is a different object on the page.
fn regular_bands(rows: &[Row]) -> Vec<Vec<&Row>> {
    if rows.len() < 2 {
        return Vec::new();
    }

    let mut gaps: Vec<f64> = rows
        .windows(2)
        .map(|w| w[0].baseline - w[1].baseline)
        .collect();
    gaps.sort_by(f64::total_cmp);
    let typical = gaps[gaps.len() / 2].max(1.0);

    let mut bands: Vec<Vec<&Row>> = vec![vec![&rows[0]]];
    for pair in rows.windows(2) {
        let step = pair[0].baseline - pair[1].baseline;
        if step > typical * ROW_OUTLIER_FACTOR {
            bands.push(Vec::new());
        }
        bands
            .last_mut()
            .expect("a band is always open")
            .push(&pair[1]);
    }

    bands
        .into_iter()
        .filter(|b| b.len() >= MIN_BORDERLESS_ROWS)
        .collect()
}

/// Turn one band of rows into a grid, if its columns line up.
fn grid_from_band(band: &[&Row]) -> Option<Grid> {
    let em = median_size(band);
    let all: Vec<Item> = band.iter().flat_map(|r| r.items.iter().copied()).collect();

    // The columns are found by **vote**, not by one projection over the band.
    //
    // A single projection asks that *every* row leave the band empty, and one
    // row that ignores the columns then hides all of them. Page 12 of `12.pdf`
    // has several: the title above the table, the footer below it, and a
    // section heading in the middle, each spanning the full width. Projected
    // together with the body they left exactly one gap where there are five.
    let gutter = em * MIN_BORDERLESS_GAP_EMS;
    let first_left = all.iter().fold(f64::INFINITY, |a, i| a.min(i.x0));
    let first_right = all.iter().fold(f64::NEG_INFINITY, |a, i| a.max(i.x1));
    let boundaries = voted_boundaries(band, first_left, first_right, gutter);
    if boundaries.len() + 1 < MIN_BORDERLESS_COLUMNS {
        return None;
    }

    // Trim the ends back to rows that actually respect those boundaries.
    //
    // A band reaches as far as the row spacing stays regular, which on a page
    // whose table fills it is further than the table: page 3 of `22.pdf` picks
    // up the letterhead above and the notes below. Those rows run their ink
    // straight across the boundaries, and left in place they were chopped into
    // cells that split words.
    //
    // Only the *ends* are trimmed. An interior row that spans — a section
    // heading inside a financial statement — belongs to the table, and cutting
    // there would break one table into two.
    let band = trim_to_respecting_rows(band, &boundaries, gutter);
    if band.len() < MIN_BORDERLESS_ROWS {
        return None;
    }

    // The extent may have shrunk with the trim, so measure it again.
    let all: Vec<Item> = band.iter().flat_map(|r| r.items.iter().copied()).collect();
    let left = all.iter().fold(f64::INFINITY, |a, i| a.min(i.x0));
    let right = all.iter().fold(f64::NEG_INFINITY, |a, i| a.max(i.x1));
    let boundaries = voted_boundaries(&band, left, right, gutter);
    if boundaries.len() + 1 < MIN_BORDERLESS_COLUMNS {
        return None;
    }
    let band = &band[..];

    let mut xs = vec![left];
    xs.extend(boundaries.iter().copied());
    xs.push(right);

    // Most rows must actually reach across more than one column, or this is a
    // list with something in the margin rather than a table.
    let spanning = band
        .iter()
        .filter(|row| {
            let lo = row.items.iter().fold(f64::INFINITY, |a, i| a.min(i.x0));
            let hi = row.items.iter().fold(f64::NEG_INFINITY, |a, i| a.max(i.x1));
            boundaries.iter().any(|b| lo < *b && *b < hi)
        })
        .count();
    if (spanning as f64) < band.len() as f64 * MIN_MULTI_COLUMN_ROWS {
        return None;
    }

    // Row boundaries sit halfway between neighbouring baselines, with the band
    // ends extended by half a step so the first and last rows are enclosed.
    let mut ys = Vec::with_capacity(band.len() + 1);
    ys.push(band[0].top);
    for pair in band.windows(2) {
        ys.push((pair[0].bottom + pair[1].top) / 2.0);
    }
    ys.push(band[band.len() - 1].bottom);

    let bbox = Rect::new(left, ys[ys.len() - 1], right, ys[0]);
    Some(Grid {
        xs,
        ys,
        bbox,
        // Nothing was drawn, so nothing about this is certain. Ruled grids earn
        // their confidence from ink; an inferred one cannot, and saying so is
        // the point of the field.
        confidence: 0.5,
        ruled: false,
    })
}

/// Cut a band back to the run of rows that honour the column boundaries.
///
/// A row honours them when, for every boundary it reaches across, it leaves a
/// gap of at least `gutter` there. Rows are dropped from the front and the back
/// only.
fn trim_to_respecting_rows<'a>(band: &[&'a Row], boundaries: &[f64], gutter: f64) -> Vec<&'a Row> {
    /// How many of the columns a row must reach into to look like a table row.
    ///
    /// A letterhead sits in two corners of the page with a wide gap between,
    /// so it *honours* every boundary while looking nothing like a row. What
    /// gives it away is that it occupies two columns out of eight.
    const MIN_OCCUPANCY: f64 = 0.4;

    let columns = boundaries.len() + 1;
    let respects = |row: &Row| {
        let honours = boundaries.iter().all(|b| {
            let reaches =
                row.items.iter().any(|i| i.x0 < *b) && row.items.iter().any(|i| i.x1 > *b);
            !reaches || row_leaves_gap(row, *b, gutter)
        });

        let occupied = (0..columns)
            .filter(|c| {
                let lo = if *c == 0 {
                    f64::NEG_INFINITY
                } else {
                    boundaries[c - 1]
                };
                let hi = boundaries.get(*c).copied().unwrap_or(f64::INFINITY);
                row.items.iter().any(|i| i.x0 < hi && i.x1 > lo)
            })
            .count();

        honours && occupied as f64 >= columns as f64 * MIN_OCCUPANCY
    };

    let start = band.iter().position(|r| respects(r)).unwrap_or(band.len());
    let end = band
        .iter()
        .rposition(|r| respects(r))
        .map_or(start, |i| i + 1);
    band[start..end].to_vec()
}

/// Does this row leave an empty band of `gutter` around `x`?
fn row_leaves_gap(row: &Row, x: f64, gutter: f64) -> bool {
    // The nearest ink to the left of `x`, and to the right.
    let before = row
        .items
        .iter()
        .filter(|i| i.x1 <= x)
        .fold(f64::NEG_INFINITY, |m, i| m.max(i.x1));
    let after = row
        .items
        .iter()
        .filter(|i| i.x0 >= x)
        .fold(f64::INFINITY, |m, i| m.min(i.x0));

    // Ink straddling `x` closes the gap entirely.
    if row.items.iter().any(|i| i.x0 < x && x < i.x1) {
        return false;
    }
    after - before >= gutter
}

/// Where the rows agree there is a gap.
///
/// Each row votes only over its **own** extent, so a short row does not claim
/// the empty margin beyond its end as a column boundary. A position becomes a
/// boundary when most of the rows that reach across it leave it clear.
fn voted_boundaries(band: &[&Row], left: f64, right: f64, min_width: f64) -> Vec<f64> {
    /// The share of the rows crossing a position that must leave it empty.
    const SUPPORT: f64 = 0.75;
    /// ...and the share of all rows that must reach across it at all, so a
    /// boundary is not decided by two rows in a corner.
    const MIN_REACH: f64 = 0.5;

    let width = (right - left).ceil().max(1.0) as usize;
    if width > 20_000 {
        // A page this wide is not a table; refuse rather than allocate for it.
        return Vec::new();
    }

    // Per x bucket: how many rows reach across it, and how many leave it empty.
    let mut reaching = vec![0usize; width + 1];
    let mut empty = vec![0usize; width + 1];

    for row in band {
        let lo = row.items.iter().fold(f64::INFINITY, |a, i| a.min(i.x0));
        let hi = row.items.iter().fold(f64::NEG_INFINITY, |a, i| a.max(i.x1));

        // Mark this row's ink once, then read the whole extent off it.
        let mut ink = vec![false; width + 1];
        for item in &row.items {
            let a = bucket(item.x0, left, width);
            let b = bucket(item.x1, left, width);
            for slot in ink.iter_mut().take(b + 1).skip(a) {
                *slot = true;
            }
        }

        for x in bucket(lo, left, width)..=bucket(hi, left, width) {
            reaching[x] += 1;
            if !ink[x] {
                empty[x] += 1;
            }
        }
    }

    // A bucket is a candidate when enough rows cross it and enough of those
    // leave it clear.
    let rows = band.len() as f64;
    let candidate: Vec<bool> = (0..=width)
        .map(|x| {
            reaching[x] as f64 >= rows * MIN_REACH
                && empty[x] as f64 >= reaching[x] as f64 * SUPPORT
        })
        .collect();

    // Runs of candidates wide enough to be gutters; the boundary is the middle.
    let mut out = Vec::new();
    let mut run = 0usize;
    for (x, marked) in candidate.iter().enumerate() {
        if *marked {
            run += 1;
            continue;
        }
        if run as f64 >= min_width {
            out.push(left + (x as f64 - run as f64 / 2.0));
        }
        run = 0;
    }
    if run as f64 >= min_width {
        out.push(left + (width as f64 - run as f64 / 2.0));
    }
    out
}

/// Map an x coordinate to its bucket, clamped to the band.
fn bucket(x: f64, left: f64, width: usize) -> usize {
    ((x - left).max(0.0).round() as usize).min(width)
}

/// The median type size across a band of rows./// The median type size across a band of rows.
fn median_size(band: &[&Row]) -> f64 {
    let mut sizes: Vec<f64> = band
        .iter()
        .flat_map(|r| r.items.iter().map(|i| i.size))
        .collect();
    if sizes.is_empty() {
        return 10.0;
    }
    sizes.sort_by(f64::total_cmp);
    sizes[sizes.len() / 2].max(1.0)
}

/// Fill a grid's cells with the text that falls inside them.
///
/// Returns the table and the indices of the text lines it consumed, so the
/// caller can keep them out of the ordinary flow — a paragraph and a table cell
/// must not both claim the same words.
pub fn fill(grid: &Grid, lines: &[TextLine], rtl: bool) -> (Table, Vec<usize>) {
    let mut rows = Vec::new();
    let mut consumed = Vec::new();

    for row in 0..grid.row_count() {
        let mut cells = Vec::new();
        for column in 0..grid.column_count() {
            let Some(bbox) = grid.cell_box(row, column, rtl) else {
                continue;
            };

            // A line belongs to the cell containing its centre. Centres rather
            // than overlap: a line that pokes a little past a border still has
            // exactly one home.
            let mut found: Vec<&TextLine> = Vec::new();
            for (index, line) in lines.iter().enumerate() {
                let cx = (line.bbox.x0 + line.bbox.x1) / 2.0;
                let cy = (line.bbox.y0 + line.bbox.y1) / 2.0;
                if cx >= bbox.x0 && cx <= bbox.x1 && cy >= bbox.y0 && cy <= bbox.y1 {
                    found.push(line);
                    consumed.push(index);
                }
            }

            // Order a cell's lines for reading: down the cell, and within a
            // line by direction. They arrive in the order the page-level
            // segmentation happened to produce, which for a narrow cell is not
            // reading order at all.
            found.sort_by(|a, b| {
                b.baseline.total_cmp(&a.baseline).then(if rtl {
                    b.bbox.x0.total_cmp(&a.bbox.x0)
                } else {
                    a.bbox.x0.total_cmp(&b.bbox.x0)
                })
            });
            let text: Vec<&str> = found.iter().map(|l| l.text.as_str()).collect();

            cells.push(Cell {
                text: text.join(" "),
                bbox,
                row,
                column,
            });
        }
        rows.push(cells);
    }

    consumed.sort_unstable();
    consumed.dedup();

    (
        Table {
            rows,
            bbox: grid.bbox,
            confidence: grid.confidence,
        },
        consumed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bidi::Direction;
    use crate::types::{Color, Style, TextRenderMode};

    /// Horizontal rule at `y`, from `x0` to `x1`.
    fn h(y: f64, x0: f64, x1: f64) -> RuledLine {
        RuledLine {
            x0,
            y0: y,
            x1,
            y1: y,
        }
    }

    /// Vertical rule at `x`, from `y0` to `y1`.
    fn v(x: f64, y0: f64, y1: f64) -> RuledLine {
        RuledLine {
            x0: x,
            y0,
            x1: x,
            y1,
        }
    }

    /// A fully ruled grid: `rows` x `columns` cells, 100pt wide, 20pt tall.
    fn ruled(rows: usize, columns: usize) -> Vec<RuledLine> {
        let width = columns as f64 * 100.0;
        let height = rows as f64 * 20.0;
        let mut lines = Vec::new();
        for r in 0..=rows {
            let y = height - r as f64 * 20.0;
            lines.push(h(y, 0.0, width));
        }
        for c in 0..=columns {
            lines.push(v(c as f64 * 100.0, 0.0, height));
        }
        lines
    }

    fn text_line(s: &str, bbox: Rect) -> TextLine {
        TextLine {
            text: s.to_string(),
            baseline: bbox.y0,
            bbox,
            direction: Direction::Rtl,
            style: Style {
                color: Color::BLACK,
                font: String::new(),
                size: 10.0,
                render_mode: TextRenderMode::Fill,
            },
            unresolved: 0,
            glyph_count: s.chars().count(),
        }
    }

    #[test]
    fn a_ruled_grid_is_detected() {
        let grids = detect(&ruled(3, 4));
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].row_count(), 3);
        assert_eq!(grids[0].column_count(), 4);
        // Fully drawn, so nothing is being inferred.
        assert!((grids[0].confidence - 1.0).abs() < 1e-9);
    }

    #[test]
    fn borders_drawn_twice_are_merged_into_one() {
        // Adjoining cells each draw the border between them, landing a
        // fraction of a point apart. Without merging, every divider doubles
        // and the grid gains a row of hairline-thin cells.
        let mut lines = ruled(2, 2);
        lines.extend(ruled(2, 2).iter().map(|l| RuledLine {
            y0: l.y0 + 0.4,
            y1: l.y1 + 0.4,
            ..*l
        }));

        let grids = detect(&lines);
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].row_count(), 2);
    }

    #[test]
    fn a_lone_pair_of_lines_is_not_a_table() {
        // Two rules under a heading are not a 1x1 table.
        assert!(detect(&[h(100.0, 0.0, 200.0), h(80.0, 0.0, 200.0)]).is_empty());
    }

    #[test]
    fn a_short_rule_inside_a_cell_is_not_a_divider() {
        // An underline in one cell spans a fraction of the width and must not
        // become a row boundary.
        let mut lines = ruled(2, 3);
        lines.push(h(30.0, 10.0, 40.0));
        let grids = detect(&lines);
        assert_eq!(grids[0].row_count(), 2, "the short rule became a divider");
    }

    #[test]
    fn columns_are_numbered_in_reading_order() {
        let grid = &detect(&ruled(2, 3))[0];

        // RTL: column 0 is the rightmost cell, where an Arabic reader starts.
        let first_rtl = grid.cell_box(0, 0, true).expect("cell exists");
        assert_eq!(first_rtl.x0, 200.0);

        // LTR: column 0 is the leftmost.
        let first_ltr = grid.cell_box(0, 0, false).expect("cell exists");
        assert_eq!(first_ltr.x0, 0.0);
    }

    #[test]
    fn rows_run_top_to_bottom() {
        // PDF's y grows upwards, so row 0 is the *highest* band.
        let grid = &detect(&ruled(3, 2))[0];
        let first = grid.cell_box(0, 0, true).expect("cell exists");
        let last = grid.cell_box(2, 0, true).expect("cell exists");
        assert!(first.y0 > last.y0, "row 0 should be the top row");
    }

    #[test]
    fn text_lands_in_the_cell_that_contains_it() {
        let grid = &detect(&ruled(2, 2))[0];
        let lines = [
            // Top-right cell, which is column 0 in RTL reading order.
            text_line("A", Rect::new(110.0, 25.0, 190.0, 35.0)),
            // Top-left cell, column 1.
            text_line("B", Rect::new(10.0, 25.0, 90.0, 35.0)),
        ];

        let (table, consumed) = fill(grid, &lines, true);
        assert_eq!(consumed.len(), 2, "both lines should be claimed");
        assert_eq!(table.rows[0][0].text, "A");
        assert_eq!(table.rows[0][1].text, "B");
        assert_eq!(table.rows[1][0].text, "");
    }

    #[test]
    fn a_distant_rule_does_not_swallow_the_table() {
        // Page 6 of `bar_Persons.pdf`. A form XObject draws a short decorative
        // rule near the foot of the page that happens to overlap the table
        // above it horizontally. Grouped with the table's rows it stretched the
        // group from 147pt tall to 463pt, and the column filter — which asks
        // that a divider run at least half the table's height — then rejected
        // every real column. The table vanished, silently, into loose lines.
        let mut lines = ruled(4, 3);
        // Well below everything, overlapping in x, and nothing in between.
        lines.push(h(-400.0, 10.0, 200.0));

        let grids = detect(&lines);
        assert_eq!(grids.len(), 1, "the stray rule broke detection");
        assert_eq!(grids[0].row_count(), 4);
        assert_eq!(grids[0].column_count(), 3);
    }

    #[test]
    fn rows_of_two_separate_tables_are_not_merged() {
        // Two grids far apart on one page, overlapping in x. The gap between
        // them dwarfs either one's row height, which is what tells them apart.
        let mut lines = ruled(3, 2);
        lines.extend(ruled(3, 2).iter().map(|l| RuledLine {
            y0: l.y0 + 400.0,
            y1: l.y1 + 400.0,
            ..*l
        }));

        assert_eq!(detect(&lines).len(), 2);
    }

    // ---- borderless tables -----------------------------------------------

    /// A grid of aligned items with no ruling: `rows` x `columns`.
    fn aligned(rows: usize, columns: usize, cell: f64) -> Vec<Item> {
        let mut items = Vec::new();
        for row in 0..rows {
            for column in 0..columns {
                let x = 40.0 + column as f64 * (cell + 20.0);
                let y = 700.0 - row as f64 * 14.0;
                items.push(Item {
                    x0: x,
                    x1: x + cell,
                    y0: y,
                    y1: y + 10.0,
                    size: 10.0,
                });
            }
        }
        items
    }

    #[test]
    fn an_aligned_grid_is_detected_without_any_ruling() {
        let grids = detect_borderless(&aligned(10, 5, 60.0));
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].row_count(), 10);
        assert_eq!(grids[0].column_count(), 5);
        // Nothing was drawn, so nothing is certain — and the grid says so.
        assert!(grids[0].confidence < 1.0);
        assert!(!grids[0].was_ruled());
    }

    #[test]
    fn too_few_columns_is_not_a_table() {
        // Three aligned columns is exactly what a two-column card layout with
        // badges between them looks like, and page 8 of the Arabic corpus is
        // one. Geometry cannot tell them apart, so the bar sits above it.
        assert!(detect_borderless(&aligned(10, 3, 60.0)).is_empty());
        assert!(detect_borderless(&aligned(10, 2, 60.0)).is_empty());
    }

    #[test]
    fn too_few_rows_is_not_a_table() {
        // Alignment is only evidence when it repeats.
        assert!(detect_borderless(&aligned(3, 5, 60.0)).is_empty());
    }

    #[test]
    fn a_paragraph_is_not_a_table() {
        // Full-width lines with no internal gaps: nothing to align on.
        let mut items = Vec::new();
        for row in 0..20 {
            items.push(Item {
                x0: 40.0,
                x1: 540.0,
                y0: 700.0 - row as f64 * 14.0,
                y1: 700.0 - row as f64 * 14.0 + 10.0,
                size: 10.0,
            });
        }
        assert!(detect_borderless(&items).is_empty());
    }

    #[test]
    fn a_row_split_by_baseline_drift_is_rejoined() {
        // `22.pdf` sets its row numbers about 3pt below the rest of their row.
        // Grouped by baseline alone that splits every row in two, and half the
        // pieces then cover a sliver of the width — so no boundary could gather
        // the support it needed and the table vanished.
        let mut items = aligned(10, 5, 60.0);
        for row in 0..10 {
            // A narrow number column, off to the side and slightly lower.
            items.push(Item {
                x0: 460.0,
                x1: 470.0,
                y0: 700.0 - row as f64 * 14.0 - 3.0,
                y1: 700.0 - row as f64 * 14.0 + 7.0,
                size: 10.0,
            });
        }

        let grids = detect_borderless(&items);
        assert_eq!(grids.len(), 1, "the drifting column split every row");
        assert_eq!(grids[0].row_count(), 10, "rows were counted twice");
    }

    #[test]
    fn lines_at_the_same_x_are_never_merged_into_one_row() {
        // The guard that makes the rejoin safe: two pieces of text at the same
        // x cannot be one row however close their baselines, or consecutive
        // lines of a paragraph would collapse together.
        let mut items = Vec::new();
        for row in 0..10 {
            items.push(Item {
                x0: 40.0,
                x1: 300.0,
                // Deliberately tight — half a line apart.
                y0: 700.0 - row as f64 * 5.0,
                y1: 700.0 - row as f64 * 5.0 + 10.0,
                size: 10.0,
            });
        }
        // Overlapping in x, so nothing merges and there is no table here.
        assert!(detect_borderless(&items).is_empty());
    }

    #[test]
    fn a_letterhead_is_trimmed_off_the_top() {
        // A letterhead sits in two corners with a wide gap between, so it
        // honours every column boundary while looking nothing like a row.
        // Occupancy is what gives it away — two columns out of six, the same
        // proportion as the two-of-eight in `22.pdf`.
        let mut items = aligned(10, 6, 60.0);
        for corner in [40.0, 440.0] {
            items.push(Item {
                x0: corner,
                x1: corner + 60.0,
                y0: 716.0,
                y1: 726.0,
                size: 10.0,
            });
        }

        let grids = detect_borderless(&items);
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].row_count(), 10, "the letterhead joined the table");
    }

    #[test]
    fn a_row_that_ignores_the_columns_does_not_hide_them() {
        // A title above the table and a footer below it span the full width.
        // Projected together with the body they leave one gap where there are
        // four; the rows have to vote instead (PLAN.md §10.23).
        let mut items = aligned(12, 5, 60.0);
        for (i, y) in [714.0, 700.0 - 12.0 * 14.0].into_iter().enumerate() {
            items.push(Item {
                x0: 40.0,
                x1: 440.0,
                y0: y - i as f64,
                y1: y - i as f64 + 10.0,
                size: 10.0,
            });
        }
        let grids = detect_borderless(&items);
        assert_eq!(grids.len(), 1, "a spanning row hid the columns");
        assert_eq!(grids[0].column_count(), 5);
    }

    #[test]
    fn a_doubled_picture_frame_is_not_a_table() {
        // Page 4 of the Arabic corpus: a rounded rectangle drawn twice, one
        // offset behind the other as a shadow. Geometrically a 3x3 grid; in
        // truth a frame with a paragraph in the middle.
        let lines = vec![
            h(544.5, 119.4, 485.0),
            h(535.3, 110.3, 475.9),
            h(381.6, 119.4, 485.0),
            h(372.4, 110.3, 475.9),
            v(90.4, 410.5, 515.5),
            v(81.3, 401.4, 506.4),
            v(514.0, 410.5, 515.5),
            v(504.8, 401.4, 506.4),
        ];

        // Detection alone cannot tell the difference — the geometry really is
        // a grid — so this must not be asserted away at the wrong layer.
        let grids = detect(&lines);
        if let Some(grid) = grids.first() {
            let paragraph = [text_line(
                "هذا الدليل إرشادي",
                Rect::new(150.0, 450.0, 450.0, 470.0),
            )];
            let (table, _) = fill(grid, &paragraph, true);
            assert!(
                !table.is_plausible(),
                "a frame with one filled cell must be rejected"
            );
        }
    }

    #[test]
    fn a_real_table_is_plausible() {
        let grid = &detect(&ruled(2, 3))[0];
        let lines: Vec<TextLine> = (0..2)
            .flat_map(|row| {
                (0..3).map(move |col| {
                    text_line(
                        "x",
                        Rect::new(
                            col as f64 * 100.0 + 10.0,
                            (1 - row) as f64 * 20.0 + 5.0,
                            col as f64 * 100.0 + 90.0,
                            (1 - row) as f64 * 20.0 + 15.0,
                        ),
                    )
                })
            })
            .collect();

        let (table, consumed) = fill(grid, &lines, true);
        assert_eq!(consumed.len(), 6);
        assert!(table.is_plausible());
        assert_eq!(table.filled_cells(), 6);
    }

    #[test]
    fn confidence_falls_when_the_grid_is_only_partly_drawn() {
        // Dividers that stop halfway across say the reconstruction is a guess.
        let mut lines = ruled(3, 3);
        lines[1] = h(40.0, 0.0, 160.0);
        let grid = &detect(&lines)[0];
        assert!(grid.confidence < 1.0 && grid.confidence > 0.7);
    }
}
