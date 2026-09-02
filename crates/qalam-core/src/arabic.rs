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
use crate::layout::{self, Item};
use crate::structure::ReadingOrder;
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
    reconstruct_regions(page, fonts, &[], None)
        .into_iter()
        .flat_map(|region| region.lines)
        .collect()
}

/// One region of a page: a run of text, and any extra boxes that fell inside it.
///
/// A region is what the XY-cut decided is a self-contained piece of the page —
/// one card, one column, one heading band. Keeping them apart is what stops
/// three side-by-side cards from being read as one interleaved paragraph.
#[derive(Debug, Clone)]
pub struct Region {
    /// The text in this region, in reading order.
    pub lines: Vec<TextLine>,
    /// Indices into the `extra_boxes` passed to [`reconstruct_regions`], for
    /// whatever landed in this region. Used to place images among the text.
    pub extras: Vec<usize>,
    /// The area the region covers.
    pub bbox: Rect,
}

/// Split a page into regions of text, in reading order.
///
/// `extra_boxes` are non-text rectangles — image placements — that should take
/// part in the reading-order pass. They are plain [`Rect`]s on purpose: reading
/// order is a property of *where things are*, so this layer needs nothing about
/// what they contain, and stays free of any dependency on L7.
///
/// Including them matters. Ordering images by vertical position alone would put
/// a figure that sits beside a column of text in the wrong place; feeding its
/// box through the same cut puts it in the column it actually belongs to.
pub fn reconstruct_regions(
    page: &PageGlyphs,
    fonts: &FontMap,
    extra_boxes: &[Rect],
    order: Option<&ReadingOrder>,
) -> Vec<Region> {
    let placed = apply_actual_text(page);
    if placed.is_empty() && extra_boxes.is_empty() {
        return Vec::new();
    }

    // A tagged document states its own reading order, and a statement beats an
    // inference. Fall through to geometry whenever there is no usable tree —
    // which is nearly always (PLAN.md §10.3).
    if let Some(order) = order {
        return regions_from_structure(&placed, fonts, order);
    }

    // L6 first. Grouping by baseline across a whole page interleaves columns:
    // three cards side by side share every baseline, so a line-by-line reading
    // takes one fragment from each and shuffles three paragraphs together.
    // Splitting the page into regions first keeps each column's prose intact.
    //
    // Glyphs come first in the item list and the extras after, so an index
    // below `placed.len()` is a glyph and anything at or above it is an extra.
    let mut items: Vec<Item> = placed.iter().map(item_for).collect();
    items.extend(extra_boxes.iter().map(|b| Item {
        x0: b.x0,
        x1: b.x1,
        y0: b.y0,
        y1: b.y1,
        // An image has no type size. Using the page's median text size keeps
        // the gutter thresholds meaningful rather than letting a size of zero
        // collapse them.
        size: median_text_size(&placed),
    }));

    let rtl = page_direction(&placed, fonts) == Direction::Rtl;
    let split = placed.len();

    layout::segment(&items, rtl)
        .into_iter()
        .filter_map(|region| {
            // `partition` separates the glyph indices from the extras.
            let (glyph_ids, extra_ids): (Vec<usize>, Vec<usize>) =
                region.into_iter().partition(|&i| i < split);

            let glyphs: Vec<Placed> = glyph_ids.iter().map(|&i| placed[i].clone()).collect();
            let lines: Vec<TextLine> = group_into_lines(&glyphs)
                .into_iter()
                .filter_map(|line| build_line(&line, fonts))
                .collect();

            let extras: Vec<usize> = extra_ids.iter().map(|&i| i - split).collect();
            if lines.is_empty() && extras.is_empty() {
                return None;
            }

            Some(Region {
                bbox: region_bbox(&lines, &extras, extra_boxes),
                lines,
                extras,
            })
        })
        .collect()
}

/// Build regions from a tagged document's structure tree.
///
/// Each structure element becomes one region, in the tree's document order.
/// Lines *within* a region are still grouped by baseline: the tree says which
/// glyphs belong together and in what order, not where the line breaks fall,
/// so geometry remains the right tool for that.
///
/// Images are not placed here. A tagged file usually tags its figures too, but
/// this reader only follows text marked-content ids, so the caller appends any
/// images afterwards rather than guessing where they belong.
fn regions_from_structure(placed: &[Placed], fonts: &FontMap, order: &ReadingOrder) -> Vec<Region> {
    order
        .runs
        .iter()
        .filter_map(|run| {
            let glyphs: Vec<Placed> = run
                .ranges
                .iter()
                // `get` rather than indexing: the tree names glyph ranges we
                // computed separately, and a malformed file could put them out
                // of step. A missing glyph should drop, not panic.
                .flat_map(|&(start, end)| (start..end).filter_map(|i| placed.get(i).cloned()))
                .collect();

            let lines: Vec<TextLine> = group_into_lines(&glyphs)
                .into_iter()
                .filter_map(|line| build_line(&line, fonts))
                .collect();

            if lines.is_empty() {
                return None;
            }
            Some(Region {
                bbox: region_bbox(&lines, &[], &[]),
                lines,
                extras: Vec::new(),
            })
        })
        .collect()
}

/// The median type size on a page, or a plausible default when there is no text.
fn median_text_size(placed: &[Placed]) -> f64 {
    let mut sizes: Vec<f64> = placed.iter().map(|p| p.glyph.style.size).collect();
    if sizes.is_empty() {
        // A page of images only; any positive size keeps the thresholds sane.
        return 10.0;
    }
    sizes.sort_by(f64::total_cmp);
    sizes[sizes.len() / 2]
}

/// The area covered by a region's lines and extras together.
fn region_bbox(lines: &[TextLine], extras: &[usize], extra_boxes: &[Rect]) -> Rect {
    let boxes = lines
        .iter()
        .map(|l| l.bbox)
        .chain(extras.iter().filter_map(|&i| extra_boxes.get(i).copied()));

    boxes
        .fold(None::<Rect>, |acc, b| {
            Some(match acc {
                None => b,
                Some(a) => Rect::new(
                    a.x0.min(b.x0),
                    a.y0.min(b.y0),
                    a.x1.max(b.x1),
                    a.y1.max(b.y1),
                ),
            })
        })
        .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0))
}

/// The box a glyph occupies, for the layout pass.
///
/// Heights are approximated from the type size, the same way [`line_bbox`] does
/// it — a glyph's true ink extent needs per-glyph bounding boxes from the font
/// program, which is far more work than choosing a column boundary justifies.
fn item_for(placed: &Placed) -> Item {
    let g = &placed.glyph;
    Item {
        x0: g.x,
        x1: g.x + g.advance,
        y0: g.y - g.style.size * 0.25,
        y1: g.y + g.style.size * 0.75,
        size: g.style.size,
    }
}

/// The dominant direction of a whole page, which decides column ordering.
///
/// Decided across the page rather than per line, because a single column of
/// Latin figures inside an Arabic document must not reverse that page's column
/// order. Individual lines still get their own direction in [`build_line`].
fn page_direction(placed: &[Placed], fonts: &FontMap) -> Direction {
    // Stop at the first strong RTL character rather than decoding the whole
    // page: one is all the answer needs.
    for item in placed {
        let text = match &item.actual {
            Some(text) => Some(text.clone()),
            None => fonts.decode(&item.glyph.style.font, item.glyph.code),
        };
        if text.is_some_and(|t| t.chars().any(bidi::is_rtl_char)) {
            return Direction::Rtl;
        }
    }
    Direction::Ltr
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

    // Decode every glyph up front. The base direction decides how combining
    // marks are placed, and that cannot be known until the text exists.
    let decoded: Vec<(&Placed, Option<Piece>)> = ordered
        .iter()
        .map(|item| {
            let piece = match &item.actual {
                // Rung one of the chain: the writer told us outright what this
                // run says, so nothing else is consulted.
                Some(text) => Some(Piece::Actual(text.clone())),
                None => Some(
                    match fonts.decode(&item.glyph.style.font, item.glyph.code) {
                        Some(text) => Piece::Decoded(text),
                        // An unresolvable code becomes U+FFFD, never nothing.
                        // Silently dropping it would turn "we cannot read this"
                        // into "there was nothing here" — the exact deception this
                        // project exists to avoid.
                        None => Piece::Decoded(char::REPLACEMENT_CHARACTER.to_string()),
                    },
                ),
            };
            (*item, piece)
        })
        .collect();

    let direction = bidi::detect_direction(
        &decoded
            .iter()
            .filter_map(|(_, p)| p.as_ref().map(piece_str))
            .collect::<String>(),
    );

    // Build the line as pieces rather than one string, because decoded glyphs
    // and `/ActualText` need opposite treatment by the reorder below.
    let mut pieces: Vec<Piece> = Vec::new();
    let mut unresolved = 0;

    // The rightmost point any glyph has reached so far — a running *maximum*,
    // not simply the previous glyph's end, so a mark tucked back over its base
    // letter cannot drag the edge leftwards and fake a word gap.
    let mut right_edge: Option<f64> = None;

    // Where combining marks for the current base letter go. See below.
    let mut mark_slot: Option<usize> = None;

    for (item, piece) in decoded {
        let glyph = &item.glyph;
        let Some(piece) = piece else { continue };
        let text = piece_str(&piece);

        if text == "\u{FFFD}" {
            unresolved += 1;
        }

        let is_mark = is_mark_glyph(glyph, text);

        // Some PDFs separate words by moving the pen rather than painting a
        // space glyph. Detect that as a gap wider than a fraction of the type
        // size, and only when a space is not already there. A mark is drawn on
        // top of the letter before it, so it can never open a word.
        if !is_mark {
            if let Some(edge) = right_edge {
                let gap = glyph.x - edge;
                let already_spaced = matches!(pieces.last(), Some(Piece::Decoded(t)) if t == " ");
                if gap > glyph.style.size * WORD_GAP_FRACTION && !already_spaced {
                    pieces.push(Piece::Decoded(" ".to_string()));
                    mark_slot = None;
                }
            }
        }

        let end = glyph.x + glyph.advance;
        right_edge = Some(right_edge.map_or(end, |e| e.max(end)));

        // Drop the blank placeholders an `/ActualText` span leaves behind.
        if matches!(&piece, Piece::Actual(t) if t.is_empty()) {
            continue;
        }

        match (is_mark, direction, mark_slot) {
            // An RTL mark is emitted *before* its base, so that the reorder
            // below — which reverses the whole run — lands it *after* the base,
            // where logical order requires it. Inserting each further mark at
            // the same slot keeps their relative order correct through the
            // reversal too.
            (true, Direction::Rtl, Some(slot)) => pieces.insert(slot, piece),
            // An LTR mark already follows its base and stays put; so does a
            // mark with no base to attach to, at the start of a line.
            (true, _, _) => pieces.push(piece),
            (false, _, _) => {
                pieces.push(piece);
                // This base owns any marks that follow it.
                mark_slot = Some(pieces.len() - 1);
            }
        }
    }

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

    // Step 3: undo one NFKC artefact. The *isolated* presentation forms of the
    // tashkeel decompose with a SPACE as their base — NFKC(U+FC60) is
    // `SPACE + FATHA + SHADDA` — because a mark shown alone needs something to
    // sit on. Here the mark is not alone: it belongs to the letter beside it,
    // and that space would split a word in half. Observed on page 5 of the
    // fixture, where `تتضمّن` came out as `تتض َّمن`.
    let text = tidy_whitespace(&strip_mark_bases(&normalised));
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

/// Remove the placeholder space that NFKC puts before an isolated mark.
///
/// A space directly followed by a combining mark is not real text: it is the
/// base that Unicode's compatibility decomposition supplies so an isolated mark
/// has something to render on. In extracted PDF text the mark always belongs to
/// a neighbouring letter, so the space is spurious.
fn strip_mark_bases(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            // Keep everything except a space whose next character is a mark.
            !(**c == ' ' && chars.get(i + 1).copied().is_some_and(is_combining_mark))
        })
        .map(|(_, c)| *c)
        .collect()
}

/// The text of a piece, whatever its kind.
fn piece_str(piece: &Piece) -> &str {
    match piece {
        Piece::Decoded(t) | Piece::Actual(t) => t.as_str(),
    }
}

/// Is this glyph a combining mark rather than a letter?
///
/// # Why this matters: the tashkeel bug
///
/// Arabic vowel marks (tashkeel) — shadda, fatha, the tanween — are painted as
/// separate glyphs positioned *over* the letter they modify. In a real file
/// they arrive with an advance of zero and an `x` that sits **inside** the
/// preceding letter's span, because that is where the mark belongs visually.
///
/// Tracking "where the last glyph ended" therefore walks backwards at every
/// mark, and the following letter then looks far away — so the word-gap rule
/// fires and splits a word in half. Observed on page 5 of the test fixture:
/// `تتضمّن` came out as `تتض َّمن`.
///
/// Two independent guards, either of which alone fixes it:
/// the running maximum in `build_line`, and this test, which stops a mark from
/// opening a word at all.
fn is_mark_glyph(glyph: &Glyph, text: &str) -> bool {
    if text.is_empty() {
        // An `/ActualText` placeholder, not a mark.
        return false;
    }

    // Ask what the character *becomes*, not what it is. A code may decode to a
    // presentation form such as U+FC60 whose normalised value is a pair of
    // marks; testing the raw character would miss it. The leading space is the
    // decomposition's placeholder base (see `strip_mark_bases`).
    let normalised: String = text.nfkc().collect();
    let stripped = normalised.trim_start_matches(' ');
    if !stripped.is_empty() && stripped.chars().all(is_combining_mark) {
        return true;
    }

    // A glyph that does not move the pen cannot separate two words either.
    const NEGLIGIBLE_ADVANCE: f64 = 0.05;
    glyph.advance.abs() < glyph.style.size * NEGLIGIBLE_ADVANCE
}

/// Is this character a combining mark that renders on top of another?
///
/// Covers Arabic tashkeel in both their base and presentation-form encodings,
/// plus the generic combining-diacritical block. This is not a full Unicode
/// category lookup — that would need a property table this crate does not carry
/// — but it covers every mark an Arabic document produces.
fn is_combining_mark(c: char) -> bool {
    matches!(c as u32,
        // Combining Diacritical Marks (Latin, but they appear in mixed text).
        0x0300..=0x036F
        // Arabic tashkeel: fatha, damma, kasra, shadda, sukun, the tanween.
        | 0x064B..=0x065F
        // Superscript alef.
        | 0x0670
        // Quranic annotation and Arabic Extended-A marks.
        | 0x06D6..=0x06ED
        | 0x08D3..=0x08FF
        // Presentation forms of the tashkeel (U+FE70–FE7F are shadda pairs and
        // isolated marks; they normalise to the U+064x forms above).
        | 0xFE70..=0xFE7F
    )
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

    // ---- combining marks (tashkeel) --------------------------------------

    #[test]
    fn the_tashkeel_word_split_regression() {
        // Page 5 of the fixture, the word `تتضمّن`, exactly as painted: visual
        // (left-to-right) order, with U+FC60 — the *isolated* shadda-with-fatha
        // ligature — drawn on top of the meem.
        //
        // It used to come out as `تتض َّمن`: a space in the middle of the word,
        // and the marks landing before their base letter instead of after.
        let glyphs = vec![
            // noon, meem, then the mark sitting inside the meem's span,
            // then dad, teh, teh.
            (366.517, 7.546, "\u{FEE6}"),
            (374.063, 7.040, "\u{FEE4}"),
            (377.418, 1.353, "\u{FC60}"),
            (381.103, 9.526, "\u{FEC0}"),
            (390.629, 4.345, "\u{FE98}"),
            (394.974, 3.883, "\u{FE97}"),
        ];

        let mut pieces = Vec::new();
        let mut slot: Option<usize> = None;
        for (x, adv, text) in &glyphs {
            let mut g = glyph(*x, 610.16, 11.0);
            g.advance = *adv;
            let piece = Piece::Decoded((*text).to_string());
            if is_mark_glyph(&g, text) {
                match slot {
                    Some(i) => pieces.insert(i, piece),
                    None => pieces.push(piece),
                }
            } else {
                pieces.push(piece);
                slot = Some(pieces.len() - 1);
            }
        }

        let visual = assemble_visual(&pieces, Direction::Rtl);
        let logical = bidi::visual_to_logical(&visual, Direction::Rtl);
        let text = tidy_whitespace(&strip_mark_bases(&logical.nfkc().collect::<String>()));

        assert_eq!(
            text,
            "\u{062A}\u{062A}\u{0636}\u{0645}\u{064E}\u{0651}\u{0646}"
        );
        assert!(!text.contains(' '), "a space split the word: {text:?}");
    }

    #[test]
    fn an_isolated_mark_form_is_recognised_through_normalisation() {
        // U+FC60 is a letter by category and has a non-zero advance, so neither
        // a raw character test nor an advance test alone would spot it. What
        // gives it away is that it *normalises* to nothing but marks.
        let mut g = glyph(0.0, 0.0, 11.0);
        g.advance = 1.353;
        assert!(is_mark_glyph(&g, "\u{FC60}"));

        // A zero-advance mark in its base form.
        let mut zero = glyph(0.0, 0.0, 11.0);
        zero.advance = 0.0;
        assert!(is_mark_glyph(&zero, "\u{064B}"));

        // An ordinary letter is not a mark.
        let mut letter = glyph(0.0, 0.0, 11.0);
        letter.advance = 7.0;
        assert!(!is_mark_glyph(&letter, "\u{FEE4}"));
    }

    #[test]
    fn nfkc_supplies_a_space_base_that_we_remove() {
        // The behaviour that caused the bug, asserted so the fix is not
        // mistaken for arbitrary whitespace munging.
        let expanded: String = "\u{FC60}".nfkc().collect();
        assert!(
            expanded.starts_with(' '),
            "NFKC no longer prefixes a space; strip_mark_bases may be obsolete"
        );

        assert_eq!(strip_mark_bases(" \u{064E}"), "\u{064E}");
        // A space before an ordinary letter is real text and must survive.
        assert_eq!(strip_mark_bases(" \u{0645}"), " \u{0645}");
        assert_eq!(strip_mark_bases("a b"), "a b");
    }

    #[test]
    fn a_mark_never_opens_a_word() {
        // A mark is painted over the letter before it, at an x *inside* that
        // letter's span. Treating it as a normal glyph made the running edge
        // walk backwards and faked a word gap for the next letter.
        let mut base = glyph(100.0, 0.0, 11.0);
        base.advance = 7.0;
        let mut mark = glyph(103.0, 0.0, 11.0);
        mark.advance = 0.0;
        let next = glyph(107.0, 0.0, 11.0);

        let page = PageGlyphs {
            glyphs: vec![base, mark, next],
            ..Default::default()
        };
        // With an empty font map every glyph is U+FFFD, so no space glyph can
        // come from decoding — any space would be one we invented.
        let lines = reconstruct(&page, &FontMap::default());
        assert_eq!(lines.len(), 1);
        assert!(
            !lines[0].text.contains(' '),
            "invented a gap: {:?}",
            lines[0].text
        );
    }

    #[test]
    fn ltr_marks_stay_after_their_base() {
        // Latin combining marks already follow their base and must not be moved.
        let pieces = vec![
            Piece::Decoded("e".to_string()),
            Piece::Decoded("\u{0301}".to_string()),
        ];
        let visual = assemble_visual(&pieces, Direction::Ltr);
        let logical = bidi::visual_to_logical(&visual, Direction::Ltr);
        let text: String = logical.nfkc().collect();
        assert_eq!(text, "é");
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
