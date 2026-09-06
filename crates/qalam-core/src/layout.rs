//! **L6 — layout analysis: reading order from geometry.**
//!
//! Grouping glyphs by baseline alone is wrong the moment a page has more than
//! one column. Three cards side by side share every baseline, so a line-by-line
//! reading picks up one fragment from each card and interleaves three separate
//! paragraphs into nonsense — even though every character decoded perfectly.
//!
//! Nothing in the PDF says "these are three columns". Untagged files (the
//! common case, PLAN.md §10.3) store only glyphs and positions, so the
//! structure has to be **reconstructed from geometry**.
//!
//! # Recursive XY-cut
//!
//! The classic algorithm, and it fits the problem exactly:
//!
//! 1. Project every glyph onto the x axis and onto the y axis.
//! 2. Find the widest empty band — a *gutter* between columns, or a *gap*
//!    between stacked blocks.
//! 3. Split there, and recurse into each half.
//! 4. Stop when no band is wide enough to be meaningful.
//!
//! Splitting at the **widest** gap, rather than at every gap at once, is what
//! makes it work. Page 6 of the test fixture measures like this:
//!
//! ```text
//!   whole page      widest gap: 116pt horizontal  → intro above, cards below
//!   card band       widest gap:  51pt vertical    → right card | rest
//!   remaining pair  widest gap:  43pt vertical    → middle card | left card
//! ```
//!
//! Note there is **no page-wide vertical gutter at all** — the full-width intro
//! paragraph crosses all three. A single global column-detection pass finds
//! nothing; only the recursion, having first cut the intro away, exposes them.
//!
//! # Right-to-left
//!
//! The one substantive difference from a Latin layout engine: when a region
//! splits into columns, RTL reading order takes the **rightmost first**
//! (PLAN.md §3, L6). Vertical order is unaffected — pages are read top to
//! bottom in every direction.

/// One item to be laid out: a glyph reduced to its box.
///
/// Deliberately not [`crate::types::Glyph`]. Layout needs nothing but geometry,
/// and saying so in the type keeps this module testable with a handful of
/// rectangles instead of a synthetic PDF.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Item {
    /// Left edge.
    pub x0: f64,
    /// Right edge.
    pub x1: f64,
    /// Bottom edge.
    pub y0: f64,
    /// Top edge.
    pub y1: f64,
    /// Type size, used to scale the "how wide is a real gutter" thresholds.
    pub size: f64,
}

/// Which axis a region was split along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// A vertical cut: the region splits into side-by-side columns.
    Columns,
    /// A horizontal cut: the region splits into stacked bands.
    Rows,
}

// --- thresholds -----------------------------------------------------------
//
// All relative to the region's median type size, so they hold for 6pt
// footnotes and 40pt headings alike. Gathered here because they are judgement
// calls a corpus should tune, not facts about PDF.

/// A vertical gutter must be at least this many ems wide to be a column break.
///
/// Set well above the widest inter-word space so that a gap between words can
/// never be mistaken for a column boundary.
const MIN_GUTTER_EMS: f64 = 1.5;

/// The width demanded of a gutter in a **tall** region, in ems.
///
/// A gap in the x-projection means *no glyph anywhere in the region* occupies
/// that band — so in a region of many lines it is not a word space that
/// happened to be wide, it is a band every single line agreed to leave empty.
/// The taller the region, the less plausible that is as coincidence, and the
/// less the raw width has to prove on its own.
///
/// This matters in practice: the magazine pages in `9.pdf` set two columns of
/// 11pt text with a **14pt** gutter — 1.27 em, comfortably visible to a reader
/// and comfortably under the 1.5 em a short region must clear. Every page of
/// the document merged its columns line by line because of that 2.5pt
/// (PLAN.md §10.19).
const MIN_GUTTER_EMS_TALL: f64 = 0.9;

/// How wide each side of a column cut must be, in ems.
///
/// A column has to be able to hold words. A narrower strip than this is
/// furniture — a margin rule, a bullet, or the ring of numbered badges running
/// down the edge of each column on page 14 of `test_for_arabic_barser.pdf`,
/// which the projection sees as a perfectly good gutter and which splitting
/// tears away from the text it numbers.
const MIN_COLUMN_EMS: f64 = 5.0;

/// How tall a region must be, in ems, before the relaxed threshold applies.
///
/// Roughly five lines of body text. Below that a coincidentally aligned run of
/// wide word spaces is still conceivable, so the strict width is kept.
const TALL_REGION_EMS: f64 = 8.0;

/// A horizontal gap must be this many ems tall to separate two blocks.
///
/// Above normal line leading, so the lines of one paragraph stay together.
const MIN_ROW_GAP_EMS: f64 = 1.0;

/// A region must be at least this tall, in ems, before a column split is
/// considered at all.
///
/// Without this guard a single line of text is a candidate for column
/// splitting, and any wide word space would tear it in half.
const MIN_MULTILINE_HEIGHT_EMS: f64 = 1.8;

/// Give up subdividing past this depth.
const MAX_DEPTH: usize = 24;

/// Fewer items than this is not worth cutting further.
const MIN_ITEMS_TO_SPLIT: usize = 4;

/// Partition items into regions, ordered for reading.
///
/// Returns groups of indices into `items`. Concatenating the groups in the
/// order returned, and reading each group top to bottom, is the reading order.
///
/// `rtl` selects the column ordering: right-to-left when true.
pub fn segment(items: &[Item], rtl: bool) -> Vec<Vec<usize>> {
    if items.is_empty() {
        return Vec::new();
    }

    let all: Vec<usize> = (0..items.len()).collect();
    let mut out = Vec::new();
    cut(items, all, rtl, 0, &mut out);
    out
}

/// Split one region, then recurse into its parts.
///
/// # Rust lesson: an output parameter instead of a return value
///
/// Pushing into `out` rather than returning a `Vec` from each level avoids
/// allocating a fresh vector at every node of the recursion and concatenating
/// them on the way back up. The order is still exact, because each level
/// pushes its parts in reading order before returning.
fn cut(items: &[Item], region: Vec<usize>, rtl: bool, depth: usize, out: &mut Vec<Vec<usize>>) {
    if depth >= MAX_DEPTH || region.len() < MIN_ITEMS_TO_SPLIT {
        out.push(region);
        return;
    }

    let em = median_size(items, &region);
    let height = span(items, &region, |i| (i.y0, i.y1)).map_or(0.0, |(lo, hi)| hi - lo);

    // A single line must never be split into "columns" — every word space
    // would qualify.
    let columns = if height >= em * MIN_MULTILINE_HEIGHT_EMS {
        // Both sides must be wide enough to be columns; see `MIN_COLUMN_EMS`.
        widest_gap(items, &region, |i| (i.x0, i.x1), em * MIN_COLUMN_EMS)
    } else {
        None
    };
    // Rows have no such constraint: a one-line band between two paragraphs is a
    // perfectly ordinary thing to cut away.
    let rows = widest_gap(items, &region, |i| (i.y0, i.y1), 0.0);

    // How wide a gutter has to be depends on how much of the page agreed to
    // leave it empty — see `MIN_GUTTER_EMS_TALL`.
    let min_gutter = em
        * if height >= em * TALL_REGION_EMS {
            MIN_GUTTER_EMS_TALL
        } else {
            MIN_GUTTER_EMS
        };

    // Take whichever cut is more pronounced, provided it clears its threshold.
    // This is what lets a full-width heading be removed before the columns
    // underneath it are looked for.
    let choice = match (columns, rows) {
        (Some(c), Some(r)) if c.width >= min_gutter && c.width > r.width => {
            Some((Axis::Columns, c.at))
        }
        (_, Some(r)) if r.width >= em * MIN_ROW_GAP_EMS => Some((Axis::Rows, r.at)),
        (Some(c), _) if c.width >= min_gutter => Some((Axis::Columns, c.at)),
        _ => None,
    };

    let Some((axis, at)) = choice else {
        // No meaningful structure left: this region is a leaf.
        out.push(region);
        return;
    };

    // `partition` splits into (below the cut, above the cut) on the chosen axis.
    let (low, high): (Vec<usize>, Vec<usize>) = region.into_iter().partition(|&i| {
        let item = &items[i];
        match axis {
            Axis::Columns => item.x1 <= at,
            Axis::Rows => item.y1 <= at,
        }
    });

    // A cut that puts everything on one side would recurse forever.
    if low.is_empty() || high.is_empty() {
        out.push(if low.is_empty() { high } else { low });
        return;
    }

    // Reading order. PDF's y grows upwards, so the *high* band is the top of
    // the page and comes first. Columns depend on direction: rightmost first
    // for Arabic, leftmost first for Latin.
    let (first, second) = match axis {
        Axis::Rows => (high, low),
        Axis::Columns if rtl => (high, low),
        Axis::Columns => (low, high),
    };

    cut(items, first, rtl, depth + 1, out);
    cut(items, second, rtl, depth + 1, out);
}

/// A gap found between items along one axis.
#[derive(Debug, Clone, Copy)]
struct Gap {
    /// How wide the empty band is.
    width: f64,
    /// The coordinate to split at — the near edge of the band.
    at: f64,
}

/// Find the widest empty band between the items' projections onto one axis.
///
/// `extent` picks the axis: `|i| (i.x0, i.x1)` projects onto x.
///
/// Only gaps *between* items count. Margins at either end are not gaps, which
/// falls out of the sweep naturally and is what we want: the blank left margin
/// of a page is not a column boundary.
fn widest_gap(
    items: &[Item],
    region: &[usize],
    extent: fn(&Item) -> (f64, f64),
    min_side: f64,
) -> Option<Gap> {
    let mut spans: Vec<(f64, f64)> = region.iter().map(|&i| extent(&items[i])).collect();
    if spans.len() < 2 {
        return None;
    }

    // `total_cmp` gives `f64` the total order `sort_by` requires; a NaN
    // coordinate sorts to one end instead of corrupting the result.
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));

    let start = spans[0].0;
    let end = spans.iter().fold(f64::NEG_INFINITY, |acc, s| acc.max(s.1));

    let mut best: Option<Gap> = None;
    // The furthest right (or highest) any item has reached so far. Overlapping
    // items must not appear to leave a gap, so this is a running maximum.
    let mut reach = spans[0].1;

    for &(lo, hi) in &spans[1..] {
        let width = lo - reach;
        // A gap that leaves too little on either side is not a boundary
        // between two parts of the page; it is an edge with furniture beyond
        // it. `min_side` is 0 where that distinction does not apply.
        let room = reach - start >= min_side && end - lo >= min_side;

        if width > 0.0 && room && best.is_none_or(|b| width > b.width) {
            best = Some(Gap { width, at: reach });
        }
        reach = reach.max(hi);
    }
    best
}

/// The total extent of a region along one axis.
fn span(items: &[Item], region: &[usize], extent: fn(&Item) -> (f64, f64)) -> Option<(f64, f64)> {
    region.iter().fold(None, |acc, &i| {
        let (lo, hi) = extent(&items[i]);
        Some(match acc {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        })
    })
}

/// The median type size in a region, used as the em for every threshold.
///
/// Median rather than mean: one 40pt heading among 500 words of 9pt body text
/// should not move the thresholds.
fn median_size(items: &[Item], region: &[usize]) -> f64 {
    let mut sizes: Vec<f64> = region.iter().map(|&i| items[i].size).collect();
    if sizes.is_empty() {
        // Any positive number; the region will not be split anyway.
        return 1.0;
    }
    sizes.sort_by(f64::total_cmp);
    let mid = sizes[sizes.len() / 2];
    // A zero or nonsensical size would make every threshold zero and every gap
    // a split. Fall back to a typical body size.
    if mid > 0.0 {
        mid
    } else {
        10.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A word-sized box: `x` to `x + width`, on the baseline `y`.
    fn word(x: f64, y: f64, width: f64) -> Item {
        Item {
            x0: x,
            x1: x + width,
            y0: y,
            y1: y + 9.0,
            size: 9.0,
        }
    }

    /// Which group each index landed in, for readable assertions.
    fn group_of(groups: &[Vec<usize>], index: usize) -> Option<usize> {
        groups.iter().position(|g| g.contains(&index))
    }

    #[test]
    fn a_single_column_page_is_not_split() {
        // Three stacked lines of one paragraph: normal leading, no real gaps.
        let items = vec![
            word(100.0, 700.0, 300.0),
            word(100.0, 688.0, 300.0),
            word(100.0, 676.0, 300.0),
            word(100.0, 664.0, 300.0),
        ];
        assert_eq!(segment(&items, true).len(), 1);
    }

    #[test]
    fn three_cards_split_into_three_regions_rightmost_first() {
        // The shape of page 6's card row: three columns, 40pt gutters, two
        // lines each. Reading order for Arabic is right to left.
        let mut items = Vec::new();
        for column in 0..3 {
            for line in 0..2 {
                let x = 100.0 + column as f64 * 150.0;
                items.push(word(x, 500.0 - line as f64 * 12.0, 110.0));
            }
        }

        let groups = segment(&items, true);
        assert_eq!(groups.len(), 3, "expected three columns, got {groups:?}");

        // Items 4 and 5 are the rightmost column (x = 400), and must come
        // first. Items 0 and 1 are the leftmost, and come last.
        assert_eq!(group_of(&groups, 4), Some(0));
        assert_eq!(group_of(&groups, 0), Some(2));
    }

    #[test]
    fn latin_pages_order_columns_left_to_right() {
        let items = vec![
            word(100.0, 500.0, 80.0),
            word(100.0, 488.0, 80.0),
            word(300.0, 500.0, 80.0),
            word(300.0, 488.0, 80.0),
        ];
        let groups = segment(&items, false);
        assert_eq!(groups.len(), 2);
        // The left column (items 0, 1) comes first.
        assert_eq!(group_of(&groups, 0), Some(0));
    }

    #[test]
    fn a_full_width_heading_is_cut_away_before_the_columns_are_found() {
        // This is the case that makes the recursion necessary. The heading
        // crosses both gutters, so a single global x-projection finds no
        // column boundary anywhere on the page. Only after the heading is
        // separated by the horizontal gap do the columns appear.
        let mut items = vec![word(100.0, 700.0, 400.0), word(100.0, 688.0, 400.0)];
        for column in 0..2 {
            for line in 0..3 {
                let x = 100.0 + column as f64 * 250.0;
                items.push(word(x, 600.0 - line as f64 * 12.0, 200.0));
            }
        }

        // Confirm the premise: no gutter exists across the page as a whole.
        let all: Vec<usize> = (0..items.len()).collect();
        assert!(
            widest_gap(&items, &all, |i| (i.x0, i.x1), 0.0).is_none(),
            "the heading should span both columns"
        );

        let groups = segment(&items, true);
        assert_eq!(groups.len(), 3, "heading plus two columns");
        // The heading is first, then the right column, then the left.
        assert_eq!(group_of(&groups, 0), Some(0));
        assert_eq!(group_of(&groups, 1), Some(0));
        assert_eq!(group_of(&groups, 5), Some(1)); // right column
        assert_eq!(group_of(&groups, 2), Some(2)); // left column
    }

    #[test]
    fn a_tall_region_admits_a_narrower_gutter() {
        // The magazine pages in `9.pdf`: two columns of 11pt text with a 14pt
        // gutter — 1.27 em. Every page merged its columns line by line because
        // the threshold demanded 1.5 em. A band that *every* line of a tall
        // region agreed to leave empty is not a coincidence.
        let columns = |lines: usize| {
            let mut items = Vec::new();
            for column in 0..2 {
                for line in 0..lines {
                    let x = 100.0 + column as f64 * 214.0;
                    items.push(Item {
                        x0: x,
                        x1: x + 200.0,
                        y0: 500.0 - line as f64 * 14.0,
                        y1: 500.0 - line as f64 * 14.0 + 11.0,
                        size: 11.0,
                    });
                }
            }
            segment(&items, true).len()
        };

        assert_eq!(columns(20), 2, "a tall two-column region should split");
        // Three lines is not enough for the band to prove itself, so the
        // stricter width still applies and the region stays whole.
        assert_eq!(columns(3), 1, "a short region keeps the strict threshold");
    }

    #[test]
    fn a_narrow_strip_is_not_a_column() {
        // Page 14 of `test_for_arabic_barser.pdf` runs a ring of numbered
        // badges down the inside edge of each column. The projection sees a
        // perfectly good gutter beside them, and splitting there tears every
        // number away from the item it numbers.
        let mut items = Vec::new();
        for line in 0..20 {
            let y = 500.0 - line as f64 * 14.0;
            // A one-character badge, then a wide gap, then the text.
            if line % 5 == 0 {
                items.push(Item {
                    x0: 100.0,
                    x1: 110.0,
                    y0: y,
                    y1: y + 11.0,
                    size: 11.0,
                });
            }
            items.push(Item {
                x0: 128.0,
                x1: 400.0,
                y0: y,
                y1: y + 11.0,
                size: 11.0,
            });
        }

        assert_eq!(
            segment(&items, true).len(),
            1,
            "the badge strip was split off as a column"
        );
    }

    #[test]
    fn a_wide_word_space_never_splits_a_single_line() {
        // A table-of-contents line — title, leader gap, page number — is one
        // line with a big hole in it. Without the multi-line guard the hole
        // reads as a column gutter and tears the line in two.
        let items = vec![word(100.0, 700.0, 60.0), word(400.0, 700.0, 10.0)];
        assert_eq!(segment(&items, true).len(), 1);
    }

    #[test]
    fn overlapping_items_do_not_fake_a_gap() {
        // Sorting by left edge alone would see a gap between item 0 and item 2
        // if item 1 were not tracked as extending the reach.
        let items = vec![
            word(100.0, 700.0, 200.0),
            word(120.0, 700.0, 200.0),
            word(310.0, 700.0, 50.0),
        ];
        let all: Vec<usize> = (0..items.len()).collect();
        assert!(widest_gap(&items, &all, |i| (i.x0, i.x1), 0.0).is_none());
    }

    #[test]
    fn stacked_blocks_are_ordered_top_to_bottom() {
        // PDF's y grows upwards, so the larger y is the top of the page and
        // must be read first. Getting this backwards would reverse every page.
        let items = vec![
            word(100.0, 200.0, 100.0),
            word(100.0, 188.0, 100.0),
            word(100.0, 700.0, 100.0),
            word(100.0, 688.0, 100.0),
        ];
        let groups = segment(&items, true);
        assert_eq!(groups.len(), 2);
        assert_eq!(
            group_of(&groups, 2),
            Some(0),
            "the top block must come first"
        );
    }

    #[test]
    fn an_empty_page_yields_no_regions() {
        assert!(segment(&[], true).is_empty());
    }

    #[test]
    fn thresholds_scale_with_the_type_size() {
        // A 20pt gap separates columns of 9pt text, but is ordinary word
        // spacing at 40pt. The same geometry must give different answers.
        let columns = |size: f64| {
            let mut items = Vec::new();
            for column in 0..2 {
                for line in 0..3 {
                    items.push(Item {
                        x0: 100.0 + column as f64 * 120.0,
                        x1: 100.0 + column as f64 * 120.0 + 100.0,
                        y0: 500.0 - line as f64 * size * 1.4,
                        y1: 500.0 - line as f64 * size * 1.4 + size,
                        size,
                    });
                }
            }
            segment(&items, true).len()
        };

        assert_eq!(columns(9.0), 2, "9pt text: a 20pt gap is a gutter");
        assert_eq!(columns(40.0), 1, "40pt text: the same gap is not");
    }
}
