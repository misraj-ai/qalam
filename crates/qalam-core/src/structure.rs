//! **L1.5 — the tagged-PDF structure reader.**
//!
//! The answer to "is the structure already in the file?", decided per document
//! at run time (PLAN.md §3).
//!
//! When a PDF is *tagged* it carries a `/StructTreeRoot`: a tree of logical
//! elements — `/P`, `/H1`, `/Figure`, `/Table` — in **document order**. That
//! order is the reading order, stated by whoever made the file rather than
//! inferred from where the ink landed. Each element names the marked-content
//! ids (`/MCID`) it owns, and `content.rs` records which glyphs each id covers.
//! Put together, the tree says exactly which glyphs to read in which order.
//!
//! # Why this is a bonus and not the plan
//!
//! Most PDFs are untagged — the test corpus contains not one `/StructTreeRoot`
//! — so the geometric reconstruction in `layout.rs` is the workhorse and this
//! is the lucky case (PLAN.md §10.3). Everything here therefore degrades to
//! "no opinion" rather than to an error: [`ReadingOrder::from_structure`]
//! returns `None` whenever the tree cannot carry its weight, and the caller
//! falls back to geometry without knowing why.
//!
//! A tree can fail to carry its weight in several ordinary ways: it may cover
//! only part of a page, name marked-content ids the content stream never
//! defines, or exist with the `/Marked` flag absent. Those are all normal, and
//! none of them is worth a warning.

use crate::content::McidSpan;
use crate::types::RawStructure;

/// A page's glyphs, ordered by what the structure tree says.
#[derive(Debug, Clone)]
pub struct ReadingOrder {
    /// Runs of glyph indices, in reading order.
    ///
    /// Each run is one structure element's content — a paragraph, a heading —
    /// so the grouping is the document's own idea of a block, not ours.
    pub runs: Vec<StructRun>,
}

/// One structure element's glyphs on a page.
#[derive(Debug, Clone)]
pub struct StructRun {
    /// The element's `/S` tag: `P`, `H1`, `Figure`, …
    pub tag: String,
    /// Nesting depth in the tree.
    pub depth: usize,
    /// Half-open glyph ranges, in the order the element lists them.
    ///
    /// Ranges rather than a flat index list: a paragraph is a handful of
    /// contiguous spans, and storing it as thousands of individual indices
    /// would be a great deal of memory for no gain.
    pub ranges: Vec<(usize, usize)>,
}

impl StructRun {
    /// How many glyphs this run covers.
    pub fn glyph_count(&self) -> usize {
        self.ranges.iter().map(|(start, end)| end - start).sum()
    }
}

/// Is this structure type **inline**, living inside a paragraph rather than
/// forming one of its own?
///
/// ISO 32000 §14.8.4 splits structure types in two: *block-level* elements
/// (BLSE) that stack down the page, and *inline-level* ones (ILSE) that flow
/// within a line. `/Span` is the common inline type, and producers emit a great
/// many of them — `bar_Persons.pdf` has **3,638**, one around every number and
/// every change of styling.
///
/// Treating each as a block is catastrophic, and quietly so: every number
/// inside a sentence becomes its own paragraph, so
/// `يوضح الجدول رقم (3) أن حوالي ربع المسجلين` breaks into five pieces with the
/// digits stranded on their own lines. The text is all present and every
/// character is right, which is exactly why it is easy to ship.
///
/// Anything unrecognised is treated as block-level: a custom tag that
/// `/RoleMap` did not translate is far more likely to be a paragraph style than
/// an inline span, and mistaking a block for inline would glue unrelated
/// paragraphs together.
fn is_inline(tag: &str) -> bool {
    matches!(
        tag,
        "Span"
            | "Quote"
            | "Note"
            | "Reference"
            | "BibEntry"
            | "Code"
            | "Link"
            | "Annot"
            | "Ruby"
            | "RB"
            | "RT"
            | "RP"
            | "Warichu"
            | "WT"
            | "WP"
    )
}

/// The share of a page's glyphs a tree must account for to be worth using.
///
/// A tree that covers a little of the page is worse than no tree: it would
/// order a fragment confidently and leave the rest to be appended, which reads
/// worse than ordering everything geometrically. The threshold is deliberately
/// high — this path is only taken when it is clearly better.
const MIN_COVERAGE: f64 = 0.8;

impl ReadingOrder {
    /// Derive a page's reading order from the structure tree.
    ///
    /// `None` means "no usable opinion" — fall back to geometry. That covers an
    /// untagged document, a tree that names this page not at all, and a tree
    /// that accounts for too little of the page to be trusted.
    pub fn from_structure(
        structure: &RawStructure,
        page: u32,
        spans: &[McidSpan],
        glyph_count: usize,
    ) -> Option<Self> {
        if spans.is_empty() || glyph_count == 0 {
            return None;
        }

        // Where each marked-content id lives on this page. An id may be opened
        // more than once — a paragraph broken around a figure, say — so this
        // maps to a list, not a single span.
        let mut by_mcid: std::collections::HashMap<u32, Vec<(usize, usize)>> =
            std::collections::HashMap::new();
        for span in spans {
            by_mcid
                .entry(span.mcid)
                .or_default()
                .push((span.start, span.end));
        }

        let mut runs: Vec<StructRun> = Vec::new();
        let mut covered = 0usize;

        for element in &structure.elements {
            // Elements belonging to other pages say nothing about this one.
            // A container with no page at all (`/Document`, `/Story`) is a
            // wrapper whose children carry the content.
            if element.page.is_some() && element.page != Some(page) {
                continue;
            }

            let mut ranges = Vec::new();
            for mcid in &element.mcids {
                if let Some(found) = by_mcid.get(mcid) {
                    ranges.extend(found.iter().copied());
                }
            }

            // A block-level element opens a new run even when it carries no
            // content itself — its inline children will fill it.
            if !is_inline(&element.tag) {
                runs.push(StructRun {
                    tag: element.tag.clone(),
                    depth: element.depth,
                    ranges: Vec::new(),
                });
            }

            if ranges.is_empty() {
                // Either a wrapper, or the tree named marked-content ids the
                // content stream never defined — common in a file edited after
                // tagging, and not worth complaining about.
                continue;
            }

            covered += ranges.iter().map(|(start, end)| end - start).sum::<usize>();

            // Inline content joins the block it sits in. With no block open —
            // a stray span before any paragraph — it starts one, so nothing is
            // silently dropped.
            match runs.last_mut() {
                Some(run) => run.ranges.extend(ranges),
                None => runs.push(StructRun {
                    tag: element.tag.clone(),
                    depth: element.depth,
                    ranges,
                }),
            }
        }

        // Drop the wrappers that never received any content.
        runs.retain(|run| !run.ranges.is_empty());

        if runs.is_empty() {
            return None;
        }

        // Refuse a tree that only covers part of the page.
        let coverage = covered as f64 / glyph_count as f64;
        if coverage < MIN_COVERAGE {
            return None;
        }

        Some(ReadingOrder { runs })
    }

    /// How many glyphs the tree accounted for.
    pub fn glyph_count_total(&self) -> usize {
        self.runs.iter().map(StructRun::glyph_count).sum()
    }

    /// Every glyph index in reading order, flattened.
    pub fn glyph_indices(&self) -> Vec<usize> {
        self.runs
            .iter()
            .flat_map(|run| run.ranges.iter().flat_map(|&(start, end)| start..end))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StructElement;

    fn element(tag: &str, page: u32, mcids: &[u32]) -> StructElement {
        StructElement {
            tag: tag.to_string(),
            depth: 1,
            page: Some(page),
            mcids: mcids.to_vec(),
        }
    }

    fn span(mcid: u32, start: usize, end: usize) -> McidSpan {
        McidSpan { mcid, start, end }
    }

    #[test]
    fn the_tree_order_wins_over_the_painting_order() {
        // The whole point. The content stream painted MCID 1 first; the tree
        // says MCID 0 comes first in the document. The tree is authoritative.
        let structure = RawStructure {
            elements: vec![element("P", 1, &[0]), element("H1", 1, &[1])],
            marked: true,
        };
        let spans = [span(1, 0, 5), span(0, 5, 10)];

        let order = ReadingOrder::from_structure(&structure, 1, &spans, 10).expect("usable");
        assert_eq!(order.runs.len(), 2);
        assert_eq!(order.runs[0].tag, "P");
        // The `/P` element's glyphs are 5..10, and they come first.
        assert_eq!(order.glyph_indices(), vec![5, 6, 7, 8, 9, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn inline_spans_join_the_paragraph_they_sit_in() {
        // The bug this rule exists for. `bar_Persons.pdf` wraps every number
        // and every styling change in a `/Span`; treating each as a block put
        // the digits on their own lines and shattered every sentence that
        // mentioned a figure.
        let structure = RawStructure {
            elements: vec![
                StructElement {
                    tag: "P".to_string(),
                    depth: 1,
                    page: Some(1),
                    mcids: vec![],
                },
                element("Span", 1, &[0]),
                element("Span", 1, &[1]),
                element("Span", 1, &[2]),
            ],
            marked: true,
        };
        let spans = [span(0, 0, 4), span(1, 4, 6), span(2, 6, 10)];

        let order = ReadingOrder::from_structure(&structure, 1, &spans, 10).expect("usable");
        assert_eq!(order.runs.len(), 1, "the spans should form one paragraph");
        assert_eq!(order.runs[0].tag, "P");
        assert_eq!(order.glyph_indices().len(), 10);
    }

    #[test]
    fn block_elements_stay_separate() {
        let structure = RawStructure {
            elements: vec![element("P", 1, &[0]), element("H1", 1, &[1])],
            marked: true,
        };
        let spans = [span(0, 0, 5), span(1, 5, 10)];

        let order = ReadingOrder::from_structure(&structure, 1, &spans, 10).expect("usable");
        assert_eq!(order.runs.len(), 2);
    }

    #[test]
    fn an_unrecognised_tag_is_treated_as_a_block() {
        // A custom name `/RoleMap` did not translate is far more likely to be a
        // paragraph style than an inline span, and gluing paragraphs together
        // is worse than splitting them.
        assert!(!is_inline("NormalParagraphStyle"));
        assert!(!is_inline("P"));
        assert!(is_inline("Span"));
        assert!(is_inline("Link"));
    }

    #[test]
    fn an_untagged_page_has_no_opinion() {
        // No spans at all: the caller must fall back to geometry.
        let structure = RawStructure::default();
        assert!(ReadingOrder::from_structure(&structure, 1, &[], 10).is_none());
    }

    #[test]
    fn a_tree_covering_too_little_is_refused() {
        // One tagged paragraph out of a page of a hundred glyphs. Ordering that
        // fragment confidently and appending the rest reads worse than
        // ordering the whole page geometrically.
        let structure = RawStructure {
            elements: vec![element("P", 1, &[0])],
            marked: true,
        };
        let spans = [span(0, 0, 10)];
        assert!(ReadingOrder::from_structure(&structure, 1, &spans, 100).is_none());
    }

    #[test]
    fn elements_for_other_pages_are_ignored() {
        let structure = RawStructure {
            elements: vec![element("P", 2, &[0]), element("P", 1, &[1])],
            marked: true,
        };
        let spans = [span(1, 0, 10)];

        let order = ReadingOrder::from_structure(&structure, 1, &spans, 10).expect("usable");
        assert_eq!(order.runs.len(), 1);
        assert_eq!(order.glyph_count_total(), 10);
    }

    #[test]
    fn ids_the_content_stream_never_defined_are_skipped() {
        // A file edited after tagging routinely leaves the tree pointing at
        // content that is gone. Skip those, and judge the rest on coverage.
        let structure = RawStructure {
            elements: vec![element("P", 1, &[0]), element("P", 1, &[99])],
            marked: true,
        };
        let spans = [span(0, 0, 10)];

        let order = ReadingOrder::from_structure(&structure, 1, &spans, 10).expect("usable");
        assert_eq!(order.runs.len(), 1);
    }

    #[test]
    fn one_element_may_own_several_spans() {
        // A paragraph interrupted by a figure reopens its MCID.
        let structure = RawStructure {
            elements: vec![element("P", 1, &[0])],
            marked: true,
        };
        let spans = [span(0, 0, 4), span(0, 6, 10)];

        let order = ReadingOrder::from_structure(&structure, 1, &spans, 10).expect("usable");
        assert_eq!(order.runs[0].ranges.len(), 2);
        assert_eq!(order.glyph_indices(), vec![0, 1, 2, 3, 6, 7, 8, 9]);
    }
}
