//! **L3 (part 2) — Arabic reconstruction.**
//!
//! This is where a bag of positioned glyph codes becomes readable text. Three
//! steps, and the order of the last two is the single most important decision
//! in the project:
//!
//! 1. **Group into lines by geometry.** Stream order is not reading order;
//!    positions are (PLAN.md §3). Glyphs sharing a baseline form a line.
//! 2. **Reorder each line from visual to logical** (`bidi.rs`), while ligatures
//!    are still single presentation glyphs.
//! 3. **Then normalise** with NFKC, folding presentation forms back to base
//!    letters and expanding ligatures.
//!
//! Swapping 2 and 3 corrupts every ligature — NFKC turns `ﻻ` into `ل` + `ا`,
//! and a later reorder reverses those two into `ال`. PLAN.md §10.1 caught this
//! on a real file: `ولا` became `وال`.

use unicode_normalization::UnicodeNormalization;

use crate::bidi::{self, Direction};
use crate::content::PageGlyphs;
use crate::font::FontMap;
use crate::types::{Glyph, Rect, Style};

/// One reconstructed line of text.
#[derive(Debug, Clone)]
pub struct TextLine {
    /// The text in **logical order, normalised** — base letters, ready to be
    /// stored, searched or displayed.
    pub text: String,
    /// The y coordinate the glyphs shared, in PDF space (larger is higher).
    pub baseline: f64,
    /// The area the line's glyphs covered.
    pub bbox: Rect,
    /// The base direction this line was reordered with.
    pub direction: Direction,
    /// The styling of most of the line's glyphs — colour, font, size.
    ///
    /// A single style per line, not per character. A line is overwhelmingly
    /// uniform in real documents, and carrying per-character styling through a
    /// bidi reorder *and* an NFKC expansion (which changes the character count)
    /// would need index tracking that Tier A has no use for. Full `Span`
    /// fidelity belongs with the HTML export that actually consumes it —
    /// PLAN.md §3, Tier D.
    pub style: Style,
    /// How many glyphs on this line no font could resolve.
    ///
    /// The raw material for the recoverability detector (L4): a line that is
    /// mostly unresolved is not text we should be emitting.
    pub unresolved: usize,
    /// Total glyphs that went into this line.
    pub glyph_count: usize,
}

impl TextLine {
    /// The fraction of this line's glyphs that resolved to characters.
    ///
    /// 1.0 is perfect, 0.0 means nothing was readable. Returns 1.0 for an empty
    /// line so that a blank line never drags a page's score down.
    pub fn resolution_rate(&self) -> f64 {
        if self.glyph_count == 0 {
            return 1.0;
        }
        1.0 - (self.unresolved as f64 / self.glyph_count as f64)
    }
}

/// A glyph paired with the `/ActualText` that overrides it, if any.
///
/// `Some("")` is meaningful and distinct from `None`: it marks a glyph that a
/// span covers but whose text was already emitted by the span's first glyph.
/// Without that distinction an override's text would repeat once per glyph.
#[derive(Debug, Clone)]
struct Placed {
    glyph: Glyph,
    actual: Option<String>,
}

/// Turn one page's glyphs into lines of correct, logical-order text.
///
/// This is the whole of Tier A in one call: L1 gave us the glyphs, L2 gave us
/// `fonts`, and what comes back is readable.
pub fn reconstruct(page: &PageGlyphs, fonts: &FontMap) -> Vec<TextLine> {
    let placed = apply_actual_text(page);
    group_into_lines(&placed)
        .into_iter()
        .filter_map(|line| build_line(&line, fonts))
        .collect()
}

/// Attach `/ActualText` overrides to the glyphs they cover.
///
/// The whole span's text goes on its first glyph and the rest are blanked, so
/// the text appears exactly once, positioned where the span began.
fn apply_actual_text(page: &PageGlyphs) -> Vec<Placed> {
    let mut placed: Vec<Placed> = page
        .glyphs
        .iter()
        .map(|glyph| Placed {
            glyph: glyph.clone(),
            actual: None,
        })
        .collect();

    for span in &page.actual_text {
        // Clamp against a malformed range rather than panicking on the slice.
        let end = span.end.min(placed.len());
        let Some(start) = (span.start < end).then_some(span.start) else {
            continue;
        };

        placed[start].actual = Some(span.text.clone());
        for slot in &mut placed[start + 1..end] {
            slot.actual = Some(String::new());
        }
    }
    placed
}

/// Join a page's lines into a single string, top to bottom.
///
/// The plain-text view of the model — `extract_text()` in the eventual Python
/// API. Note it ignores styling entirely: styling never complicates this path.
pub fn lines_to_text(lines: &[TextLine]) -> String {
    lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Partition glyphs into lines by their baselines.
///
/// # Why geometry and not stream order
///
/// A PDF may paint a page's text in any order at all — a writer might emit
/// every heading first, or interleave two columns. The only reliable statement
/// about a line is that its glyphs sit at the same height. PLAN.md §3 makes
/// this a design rule: *positions are ground truth for order*.
fn group_into_lines(glyphs: &[Placed]) -> Vec<Vec<Placed>> {
    if glyphs.is_empty() {
        return Vec::new();
    }

    // Sort top-to-bottom. PDF's y grows upwards, so descending y is reading
    // order down the page.
    let mut sorted: Vec<Placed> = glyphs.to_vec();
    sorted.sort_by(|a, b| {
        let (a, b) = (&a.glyph, &b.glyph);
        // `f64` is only `PartialOrd` — NaN has no place in an ordering — so
        // `sort_by` needs a total order. `total_cmp` provides one, and a NaN
        // coordinate from a malformed file sorts to one end instead of
        // corrupting the sort or panicking.
        b.y.total_cmp(&a.y).then(a.x.total_cmp(&b.x))
    });

    let mut lines: Vec<Vec<Placed>> = Vec::new();
    let mut current: Vec<Placed> = Vec::new();
    let mut current_y = sorted[0].glyph.y;

    for placed in sorted {
        // Tolerance scales with the type size: 2pt of drift is a different
        // line in 6pt footnotes but the same line in a 40pt heading. Subscripts
        // and diacritics sit slightly off the baseline and must not split it.
        let tolerance = (placed.glyph.style.size * 0.3).max(0.5);

        if (placed.glyph.y - current_y).abs() > tolerance && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_y = placed.glyph.y;
        }
        current.push(placed);
    }

    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Build one [`TextLine`] from the glyphs sharing a baseline.
///
/// Returns `None` for a line that produced no text at all, so blank lines do
/// not clutter the output.
fn build_line(placed: &[Placed], fonts: &FontMap) -> Option<TextLine> {
    let first = placed.first()?;

    // Left to right, which is the order the glyphs were painted in — the
    // *visual* order we are about to undo.
    let mut ordered: Vec<&Placed> = placed.iter().collect();
    ordered.sort_by(|a, b| a.glyph.x.total_cmp(&b.glyph.x));

    // Build the line as pieces rather than one string, because decoded glyphs
    // and `/ActualText` need opposite treatment by the reorder below.
    let mut pieces: Vec<Piece> = Vec::new();
    let mut unresolved = 0;
    let mut previous_end: Option<f64> = None;

    for item in &ordered {
        let glyph = &item.glyph;

        // Some PDFs separate words by moving the pen rather than painting a
        // space glyph. Detect that as a gap wider than a fraction of the type
        // size, and only when a space is not already there.
        if let Some(end) = previous_end {
            let gap = glyph.x - end;
            let already_spaced = matches!(pieces.last(), Some(Piece::Decoded(t)) if t == " ");
            if gap > glyph.style.size * WORD_GAP_FRACTION && !already_spaced {
                pieces.push(Piece::Decoded(" ".to_string()));
            }
        }
        previous_end = Some(glyph.x + glyph.advance);

        // Rung one of the chain: the writer told us outright what this run
        // says, so nothing else is consulted.
        if let Some(text) = &item.actual {
            if !text.is_empty() {
                pieces.push(Piece::Actual(text.clone()));
            }
            continue;
        }

        match fonts.decode(&glyph.style.font, glyph.code) {
            Some(text) => pieces.push(Piece::Decoded(text)),
            None => {
                // An unresolvable code becomes U+FFFD, never nothing. Silently
                // dropping it would turn "we cannot read this" into "there was
                // nothing here" — the exact deception this project exists to
                // avoid.
                unresolved += 1;
                pieces.push(Piece::Decoded(char::REPLACEMENT_CHARACTER.to_string()));
            }
        }
    }

    let direction = bidi::detect_direction(&pieces_text(&pieces));

    // ---- the order-of-operations rule -----------------------------------
    // Step 1: reorder, while ligatures are still single glyphs.
    let logical = bidi::visual_to_logical(&assemble_visual(&pieces, direction), direction);

    // Step 2: only now normalise. NFKC folds U+FExx presentation forms to base
    // letters and expands `ﻻ` into `ل` + `ا` — in the order the reorder left
    // them, which is the correct one.
    //
    // `.nfkc()` is an iterator adaptor from `unicode-normalization`; it streams
    // characters rather than building an intermediate string.
    let normalised: String = logical.nfkc().collect();

    let text = tidy_whitespace(&normalised);
    if text.is_empty() {
        return None;
    }

    Some(TextLine {
        text,
        baseline: first.glyph.y,
        bbox: line_bbox(&ordered),
        direction,
        style: dominant_style(placed),
        unresolved,
        glyph_count: placed.len(),
    })
}

/// A fragment of a line, tagged by where its text came from.
#[derive(Debug, Clone)]
enum Piece {
    /// Text decoded from glyph codes: still in **visual** order.
    Decoded(String),
    /// Text taken from `/ActualText`: already in **logical** order.
    Actual(String),
}

/// The pieces' text concatenated, for direction detection only.
fn pieces_text(pieces: &[Piece]) -> String {
    pieces
        .iter()
        .map(|p| match p {
            Piece::Decoded(t) | Piece::Actual(t) => t.as_str(),
        })
        .collect()
}

/// Assemble the pieces into the visual-order string the reorder expects.
///
/// # The `/ActualText` problem
///
/// Decoded glyphs arrive in visual order and the reorder below turns them into
/// logical order. `/ActualText` is *already* logical — the writer wrote it for
/// a human. Passing it through unchanged would leave the reorder to reverse it,
/// producing a backwards word inside an otherwise correct line.
///
/// So an RTL override is reversed on the way in, and the line's reorder undoes
/// that. This is exact for a span of a single direction, which is what
/// `/ActualText` is used for in practice — a ligature, a hyphenated word, a
/// logo's name. It carries the same caveat as the whole visual→logical
/// inversion (see `bidi.rs`): a span mixing directions internally may not round
/// trip perfectly. PLAN.md §8 tracks that.
fn assemble_visual(pieces: &[Piece], direction: Direction) -> String {
    let mut visual = String::new();
    for piece in pieces {
        match piece {
            Piece::Decoded(text) => visual.push_str(text),
            Piece::Actual(text) if direction == Direction::Rtl => {
                visual.extend(text.chars().rev());
            }
            Piece::Actual(text) => visual.push_str(text),
        }
    }
    visual
}

/// How wide a gap, as a fraction of the type size, means a word break.
///
/// Tuned low: a missing space is harder to notice and harder to fix than an
/// extra one, and inter-letter spacing within an Arabic word is very small.
const WORD_GAP_FRACTION: f64 = 0.25;

/// Collapse runs of whitespace and trim the ends.
///
/// Extracted lines routinely carry leading indent spaces and doubled spaces
/// from kerning gaps. Normalising them makes golden-file comparison meaningful.
fn tidy_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The bounding box of a line's glyphs.
///
/// Heights are approximated from the type size, because a glyph's real ink
/// extent needs the font program's per-glyph bounding boxes — more work than
/// any current consumer justifies.
fn line_bbox(glyphs: &[&Placed]) -> Rect {
    let mut x0 = f64::INFINITY;
    let mut x1 = f64::NEG_INFINITY;
    let mut y0 = f64::INFINITY;
    let mut y1 = f64::NEG_INFINITY;

    for placed in glyphs {
        let g = &placed.glyph;
        x0 = x0.min(g.x);
        x1 = x1.max(g.x + g.advance);
        // Descenders drop below the baseline, ascenders rise above it. These
        // fractions are the usual rough proportions of a Latin/Arabic face.
        y0 = y0.min(g.y - g.style.size * 0.25);
        y1 = y1.max(g.y + g.style.size * 0.75);
    }

    // An empty slice would leave the infinities in place; the caller only ever
    // passes a non-empty line, but returning a degenerate rect is safer than
    // asserting.
    if x0.is_infinite() {
        return Rect::new(0.0, 0.0, 0.0, 0.0);
    }
    Rect::new(x0, y0, x1, y1)
}

/// The style shared by most of a line's glyphs.
///
/// A plain count rather than anything cleverer: lines are near-uniform, and the
/// majority style is what a reader would call "the style of this line".
fn dominant_style(glyphs: &[Placed]) -> Style {
    let mut best: Option<(&Style, usize)> = None;

    for candidate in glyphs {
        let count = glyphs
            .iter()
            .filter(|g| g.glyph.style.merges_with(&candidate.glyph.style))
            .count();

        // `is_none_or` keeps the first style seen when counts tie, which makes
        // the result deterministic rather than dependent on iteration order.
        if best.is_none_or(|(_, best_count)| count > best_count) {
            best = Some((&candidate.glyph.style, count));
        }
    }

    best.map(|(style, _)| style.clone())
        .unwrap_or_else(|| Style {
            color: crate::types::Color::BLACK,
            font: String::new(),
            size: 0.0,
            render_mode: crate::types::TextRenderMode::Fill,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Color, TextRenderMode};

    /// Build a glyph carrying a literal character, bypassing font lookup.
    ///
    /// L3's logic is about order and normalisation, not about fonts, so the
    /// tests below drive it with text directly. `code` is unused by the
    /// reconstruction path once a decoder is supplied.
    fn glyph(x: f64, y: f64, size: f64) -> Glyph {
        Glyph {
            code: 0,
            x,
            y,
            advance: size * 0.5,
            style: Style {
                color: Color::BLACK,
                font: "F".to_string(),
                size,
                render_mode: TextRenderMode::Fill,
            },
        }
    }

    /// Wrap glyphs as [`Placed`] with no `/ActualText` override.
    fn placed(glyphs: Vec<Glyph>) -> Vec<Placed> {
        glyphs
            .into_iter()
            .map(|glyph| Placed {
                glyph,
                actual: None,
            })
            .collect()
    }

    /// Run steps 2 and 3 — reorder then normalise — on a visual-order string.
    ///
    /// This is the heart of M2 isolated from geometry and fonts.
    fn visual_to_text(visual: &str) -> String {
        let direction = bidi::detect_direction(visual);
        let logical = bidi::visual_to_logical(visual, direction);
        tidy_whitespace(&logical.nfkc().collect::<String>())
    }

    #[test]
    fn the_frozen_ligature_regression() {
        // PLAN.md §10.1 and §7. This is the exact line our fixture's page 4
        // paints, as presentation forms in visual (left-to-right) order —
        // the string L2 hands to L3.
        //
        // Logical answer: هذا الدليل إرشادي ولا يغني عن الرجوع إلى
        // The test must fail if `ولا` ever comes out as `وال`.
        let visual = "\u{FE90}\u{FEDF}\u{0625} \u{0639}\u{FEEE}\u{062C}\u{FEA3}\u{FEDF}\u{0627} \
                      \u{FEE6}\u{0639} \u{FEF2}\u{FEE7}\u{FEE0}\u{FEA9} \u{FEFB}\u{FEEE} \
                      \u{FEF3}\u{062F}\u{FE8E}\u{FEB7}\u{0631}\u{0625} \
                      \u{FEDE}\u{FEF4}\u{FEDF}\u{FEAA}\u{FEDF}\u{0627} \u{0627}\u{FEAC}\u{FEEB}";

        let text = visual_to_text(visual);

        // The assertion the whole project turns on.
        assert!(
            text.contains("\u{0648}\u{0644}\u{0627}"),
            "expected `ولا` in: {text}"
        );
        assert!(
            !text.contains("\u{0648}\u{0627}\u{0644}"),
            "produced the `وال` corruption: {text}"
        );

        // The first word logically is `هذا`, which is painted *last* (rightmost).
        assert!(text.starts_with("\u{0647}\u{0630}\u{0627}"), "got: {text}");
    }

    #[test]
    fn normalising_before_reordering_would_corrupt_the_ligature() {
        // Proof by demonstration that the order is not interchangeable. This
        // test does it the WRONG way round on purpose and asserts the damage,
        // so that anyone tempted to swap the steps sees exactly what breaks.
        let visual = "\u{FEFB}\u{FEEE}"; // lam-alef ligature, then waw

        // Wrong order: NFKC first splits `ﻻ` into two characters, and the
        // reorder then reverses them.
        let normalised_first: String = visual.nfkc().collect();
        let then_reordered = bidi::visual_to_logical(&normalised_first, Direction::Rtl);
        assert!(
            then_reordered.contains("\u{0627}\u{0644}"),
            "the wrong order should produce alef-lam (`ال`), got: {then_reordered}"
        );

        // Right order: reorder while it is still one glyph, then expand.
        let right = visual_to_text(visual);
        assert!(
            right.contains("\u{0644}\u{0627}"),
            "the right order should produce lam-alef (`لا`), got: {right}"
        );
        assert_ne!(then_reordered, right);
    }

    #[test]
    fn nfkc_folds_presentation_forms_to_base_letters() {
        // U+FE8E is alef-final; it must become plain alef U+0627.
        let text = visual_to_text("\u{FE8E}");
        assert_eq!(text, "\u{0627}");

        // Nothing in the U+FBxx–U+FExx presentation ranges should survive.
        let mixed = visual_to_text("\u{FEDF}\u{FEE0}\u{FE94}");
        assert!(
            !mixed.chars().any(|c| matches!(c as u32,
                0xFB50..=0xFDFF | 0xFE70..=0xFEFF)),
            "presentation forms survived: {mixed:?}"
        );
    }

    #[test]
    fn a_multi_codepoint_mapping_survives_the_pipeline() {
        // The `01AE → U+0651 U+064B` case: shadda plus fathatan. Both marks
        // must still be there after reordering and normalisation.
        let text = visual_to_text("\u{0628}\u{0651}\u{064B}");
        assert!(text.contains('\u{0651}'));
        assert!(text.contains('\u{064B}'));
    }

    #[test]
    fn latin_text_passes_through_untouched() {
        assert_eq!(visual_to_text("Section 3.1"), "Section 3.1");
    }

    #[test]
    fn glyphs_are_grouped_by_baseline_not_stream_order() {
        // Painted out of order — second line first — as a PDF is free to do.
        let glyphs = vec![
            glyph(10.0, 100.0, 12.0),
            glyph(20.0, 100.0, 12.0),
            glyph(10.0, 200.0, 12.0),
        ];
        let lines = group_into_lines(&placed(glyphs));
        assert_eq!(lines.len(), 2);
        // Top of the page first: y=200 before y=100.
        assert_eq!(lines[0][0].glyph.y, 200.0);
        assert_eq!(lines[1].len(), 2);
    }

    #[test]
    fn small_baseline_drift_does_not_split_a_line() {
        // Diacritics and subscripts sit slightly off the baseline.
        let glyphs = vec![glyph(10.0, 100.0, 12.0), glyph(20.0, 101.5, 12.0)];
        assert_eq!(group_into_lines(&placed(glyphs)).len(), 1);

        // The same 1.5pt drift in 4pt type *is* a different line, because the
        // tolerance scales with the type size.
        let small = vec![glyph(10.0, 100.0, 4.0), glyph(20.0, 101.5, 4.0)];
        assert_eq!(group_into_lines(&placed(small)).len(), 2);
    }

    #[test]
    fn a_nan_coordinate_does_not_panic_the_sort() {
        // `sort_by` requires a total order; `f64` alone does not provide one.
        let mut glyphs = vec![glyph(10.0, 100.0, 12.0), glyph(20.0, 100.0, 12.0)];
        glyphs[1].y = f64::NAN;
        let lines = group_into_lines(&placed(glyphs));
        assert_eq!(lines.iter().map(|l| l.len()).sum::<usize>(), 2);
    }

    #[test]
    fn unresolved_glyphs_are_counted_for_the_detector() {
        let line = TextLine {
            text: "x".to_string(),
            baseline: 0.0,
            bbox: Rect::new(0.0, 0.0, 1.0, 1.0),
            direction: Direction::Ltr,
            style: dominant_style(&placed(vec![glyph(0.0, 0.0, 12.0)])),
            unresolved: 3,
            glyph_count: 12,
        };
        assert!((line.resolution_rate() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn dominant_style_is_the_majority_not_the_first() {
        let mut a = glyph(0.0, 0.0, 24.0);
        a.style.color = Color::Rgb(1.0, 0.0, 0.0);
        let b = glyph(10.0, 0.0, 12.0);
        let c = glyph(20.0, 0.0, 12.0);

        // One red 24pt glyph, two black 12pt ones.
        let style = dominant_style(&placed(vec![a, b, c]));
        assert!((style.size - 12.0).abs() < 1e-9);
        assert!(style.color.is_black());
    }

    #[test]
    fn whitespace_is_tidied() {
        assert_eq!(tidy_whitespace("  a   b  "), "a b");
        assert_eq!(tidy_whitespace("   "), "");
    }

    // ---- /ActualText -----------------------------------------------------

    #[test]
    fn actual_text_lands_on_the_first_glyph_and_blanks_the_rest() {
        let page = PageGlyphs {
            glyphs: vec![
                glyph(0.0, 0.0, 12.0),
                glyph(10.0, 0.0, 12.0),
                glyph(20.0, 0.0, 12.0),
            ],
            actual_text: vec![crate::content::ActualText {
                start: 0,
                end: 2,
                text: "ffi".to_string(),
            }],
            ..Default::default()
        };

        let placed = apply_actual_text(&page);
        assert_eq!(placed[0].actual.as_deref(), Some("ffi"));
        // Covered but already accounted for: `Some("")`, not `None`, so the
        // text is emitted once rather than once per glyph.
        assert_eq!(placed[1].actual.as_deref(), Some(""));
        // Outside the span.
        assert_eq!(placed[2].actual, None);
    }

    #[test]
    fn a_malformed_actual_text_range_is_clamped_not_panicked() {
        let page = PageGlyphs {
            glyphs: vec![glyph(0.0, 0.0, 12.0)],
            actual_text: vec![
                // Past the end of the glyph list.
                crate::content::ActualText {
                    start: 0,
                    end: 99,
                    text: "x".to_string(),
                },
                // Backwards, and entirely out of bounds.
                crate::content::ActualText {
                    start: 50,
                    end: 10,
                    text: "y".to_string(),
                },
            ],
            ..Default::default()
        };

        let placed = apply_actual_text(&page);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].actual.as_deref(), Some("x"));
    }

    #[test]
    fn an_rtl_override_is_not_reversed_by_the_line_reorder() {
        // `/ActualText` is already logical; decoded glyphs are visual. Without
        // compensation the reorder would reverse the override, producing a
        // backwards word inside an otherwise correct line.
        let arabic = "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}"; // مرحبا
        let pieces = vec![Piece::Actual(arabic.to_string())];

        let visual = assemble_visual(&pieces, Direction::Rtl);
        let logical = bidi::visual_to_logical(&visual, Direction::Rtl);
        assert_eq!(logical, arabic, "the override came back reversed");
    }

    #[test]
    fn an_ltr_override_passes_through_unchanged() {
        let pieces = vec![Piece::Actual("ffi".to_string())];
        assert_eq!(assemble_visual(&pieces, Direction::Ltr), "ffi");
    }

    #[test]
    fn actual_text_beats_the_glyph_codes_it_covers() {
        // The point of the top rung of the chain: whatever the glyphs decode
        // to, the writer's declared text wins.
        let page = PageGlyphs {
            glyphs: vec![glyph(0.0, 100.0, 12.0), glyph(10.0, 100.0, 12.0)],
            actual_text: vec![crate::content::ActualText {
                start: 0,
                end: 2,
                text: "fi".to_string(),
            }],
            ..Default::default()
        };

        // An empty font map: every code is otherwise unresolvable, so any text
        // at all can only have come from the override.
        let lines = reconstruct(&page, &FontMap::default());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "fi");
        // The glyphs never went through font decoding, so nothing is unresolved.
        assert_eq!(lines[0].unresolved, 0);
    }
}
