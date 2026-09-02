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
}

impl Grid {
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
        xs,
        ys,
    })
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
