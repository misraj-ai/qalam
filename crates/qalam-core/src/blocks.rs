//! The page's output model: typed blocks in reading order.
//!
//! PLAN.md §3 puts this at the top of the pipeline's output. A page is not a
//! string; it is an ordered list of *things* — paragraphs, pictures, and
//! eventually tables — each of which knows where it is and how much we trust
//! it. Plain text is then the trivial reduction: walk the blocks in order and
//! join the text ones.
//!
//! That direction matters. Building blocks by re-parsing extracted text would
//! be guesswork; building text by flattening blocks throws nothing away.
//!
//! # Reading order is a property of the page, not of the text
//!
//! Images take part in the same XY-cut that orders the text (L6), rather than
//! being slotted in afterwards by vertical position. A figure beside a column
//! of text belongs *in that column*; ordering it by height alone would put it
//! between two unrelated paragraphs.
//!
//! The one thing held out of that pass is a **full-page background**. A cover
//! photograph covers every gutter on the page, so including it would leave the
//! cut with nowhere to split and collapse the whole page into a single region.
//! Those are recognised by area and ordered first, which is also where they are
//! painted.

use crate::arabic::{self, TextLine};
use crate::content::PageGlyphs;
use crate::font::FontMap;
use crate::images::{ExtractedImage, PlacedImage};
use crate::structure::ReadingOrder;
use crate::types::Rect;

/// One piece of a page.
#[derive(Debug, Clone)]
pub enum Block {
    /// A run of text: a paragraph, a heading, a card's contents.
    Text(TextBlock),
    /// A picture.
    Image(ImageBlock),
    // `Table` joins these at M8. The enum is the reason that will be an
    // additive change: every `match` on a `Block` will stop compiling until it
    // handles the new case, which is exactly the reminder we want.
}

impl Block {
    /// Where this block sits in the page's reading order, counting from 0.
    pub fn reading_index(&self) -> usize {
        match self {
            Block::Text(b) => b.reading_index,
            Block::Image(b) => b.reading_index,
        }
    }

    /// The area the block covers, when known.
    ///
    /// `None` only for an image we never saw drawn — see [`ImageBlock::bbox`].
    pub fn bbox(&self) -> Option<Rect> {
        match self {
            Block::Text(b) => Some(b.bbox),
            Block::Image(b) => b.bbox,
        }
    }

    /// The block's text, empty for anything that is not text.
    pub fn text(&self) -> String {
        match self {
            Block::Text(b) => b.text(),
            Block::Image(_) => String::new(),
        }
    }
}

/// A run of text occupying one region of the page.
#[derive(Debug, Clone)]
pub struct TextBlock {
    /// The lines, in reading order, each already logical and normalised.
    pub lines: Vec<TextLine>,
    /// The area the block covers.
    pub bbox: Rect,
    /// Position in the page's reading order.
    pub reading_index: usize,
    /// How much of this block resolved, 0.0 to 1.0.
    ///
    /// Weighted by glyph count, so one unreadable word in a long paragraph
    /// costs little and a block that is mostly unresolvable scores near zero.
    /// Reconstructed blocks are always best-effort (PLAN.md §1), and this is
    /// where that shows.
    pub confidence: f64,
}

impl TextBlock {
    /// The block's lines joined with newlines.
    pub fn text(&self) -> String {
        arabic::lines_to_text(&self.lines)
    }
}

/// A picture on the page.
#[derive(Debug, Clone)]
pub struct ImageBlock {
    /// The decoded image, or the reason we declined to decode it.
    pub image: ExtractedImage,
    /// Where it was painted, in PDF points.
    ///
    /// `None` means we never saw a `Do` for it — almost certainly because it is
    /// drawn inside a form XObject we do not enter. Such blocks are ordered
    /// last, since there is nothing to order them by.
    pub bbox: Option<Rect>,
    /// Position in the page's reading order.
    pub reading_index: usize,
    /// Whether this image covers most of the page.
    ///
    /// A background, not a figure. Held out of the reading-order pass (see the
    /// module docs) and ordered first.
    pub is_background: bool,
}

/// An image covering at least this fraction of the page is a background.
///
/// Generous on purpose: a cover photograph is often placed *larger* than the
/// page and clipped, so the test has to pass comfortably for those, while a
/// half-page figure must still be treated as content.
const BACKGROUND_AREA_FRACTION: f64 = 0.7;

/// Assemble one page's blocks, in reading order.
pub fn assemble(
    glyphs: &PageGlyphs,
    fonts: &FontMap,
    images: &[PlacedImage],
    page_box: Rect,
    order: Option<&ReadingOrder>,
) -> Vec<Block> {
    // Split the images into the ones that can take part in the reading-order
    // pass and the ones that cannot.
    let mut backgrounds = Vec::new();
    let mut unplaced = Vec::new();
    let mut positioned = Vec::new();

    for (index, placed) in images.iter().enumerate() {
        match placed.bbox {
            Some(bbox) if covers_page(bbox, page_box) => backgrounds.push(index),
            Some(bbox) => positioned.push((index, bbox)),
            None => unplaced.push(index),
        }
    }

    let boxes: Vec<Rect> = positioned.iter().map(|(_, b)| *b).collect();
    let regions = arabic::reconstruct_regions(glyphs, fonts, &boxes, order);

    // Which positioned images a region claimed. The tagged path does not place
    // images at all, so anything left over is appended rather than lost.
    let mut claimed = vec![false; positioned.len()];
    let mut blocks = Vec::new();

    // Backgrounds first: that is where they are painted, and a reader
    // encounters them before anything on top of them.
    for index in backgrounds {
        blocks.push(image_block(&images[index], blocks.len(), true));
    }

    for region in regions {
        // Within a region, text comes before the images that fell inside it.
        // Regions are small by construction, so a finer rule would be inventing
        // precision we do not have.
        if !region.lines.is_empty() {
            blocks.push(Block::Text(TextBlock {
                confidence: block_confidence(&region.lines),
                bbox: region.bbox,
                reading_index: blocks.len(),
                lines: region.lines,
            }));
        }
        for extra in region.extras {
            // `extras` indexes the `boxes` slice, which is parallel to
            // `positioned`, which carries the original image index.
            if let Some((index, _)) = positioned.get(extra) {
                claimed[extra] = true;
                blocks.push(image_block(&images[*index], blocks.len(), false));
            }
        }
    }

    // Positioned images no region claimed — the tagged path never places them.
    for (slot, (index, _)) in positioned.iter().enumerate() {
        if !claimed[slot] {
            blocks.push(image_block(&images[*index], blocks.len(), false));
        }
    }

    // Anything we could not locate goes last, rather than being dropped or
    // given an invented position.
    for index in unplaced {
        blocks.push(image_block(&images[index], blocks.len(), false));
    }

    blocks
}

/// Build one image block.
fn image_block(placed: &PlacedImage, reading_index: usize, is_background: bool) -> Block {
    Block::Image(ImageBlock {
        image: placed.image.clone(),
        bbox: placed.bbox,
        reading_index,
        is_background,
    })
}

/// Does this rectangle cover enough of the page to be a background?
fn covers_page(bbox: Rect, page: Rect) -> bool {
    let page_area = page.width() * page.height();
    if page_area <= 0.0 {
        return false;
    }
    // Clamp to the page: a cover image is often placed larger than the page and
    // clipped, and its *drawn* area should not count the part nobody sees.
    let visible_w = (bbox.x1.min(page.x1) - bbox.x0.max(page.x0)).max(0.0);
    let visible_h = (bbox.y1.min(page.y1) - bbox.y0.max(page.y0)).max(0.0);

    (visible_w * visible_h) / page_area >= BACKGROUND_AREA_FRACTION
}

/// A block's confidence, weighted by how many glyphs each line contributed.
///
/// A plain mean over lines would let a two-character line count as much as a
/// full one, so a single stray unreadable mark could halve a paragraph's score.
fn block_confidence(lines: &[TextLine]) -> f64 {
    let total: usize = lines.iter().map(|l| l.glyph_count).sum();
    if total == 0 {
        return 1.0;
    }
    let unresolved: usize = lines.iter().map(|l| l.unresolved).sum();
    1.0 - (unresolved as f64 / total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::{Image, ImageFormat};

    const A4: Rect = Rect {
        x0: 0.0,
        y0: 0.0,
        x1: 595.0,
        y1: 842.0,
    };

    fn placed(bbox: Option<Rect>) -> PlacedImage {
        PlacedImage {
            image: ExtractedImage::Ready(Image {
                resource_name: "Im0".to_string(),
                width: 10,
                height: 10,
                format: ImageFormat::Png,
                data: Vec::new(),
                dropped_transparency: false,
            }),
            bbox,
        }
    }

    #[test]
    fn a_cover_photo_is_recognised_as_a_background() {
        // Page 1 of the fixture: 1373x916pt placed at (-303, -40) on an A4
        // page. Larger than the page and hanging off both edges, so only the
        // clipped area may be counted.
        let cover = Rect::new(-303.0, -40.0, 1070.0, 876.0);
        assert!(covers_page(cover, A4));
    }

    #[test]
    fn a_figure_is_not_a_background() {
        // A half-page figure is content and must stay in the reading order.
        assert!(!covers_page(Rect::new(50.0, 400.0, 545.0, 700.0), A4));
        // A logo, most certainly not.
        assert!(!covers_page(Rect::new(303.0, 581.0, 553.0, 651.0), A4));
    }

    #[test]
    fn an_image_entirely_off_the_page_covers_nothing() {
        // Clamping to the page means a huge rectangle drawn outside it scores
        // zero rather than a large number.
        assert!(!covers_page(Rect::new(2000.0, 2000.0, 5000.0, 5000.0), A4));
    }

    #[test]
    fn backgrounds_come_first_and_unplaced_images_come_last() {
        let images = [
            placed(Some(Rect::new(100.0, 100.0, 200.0, 200.0))),
            placed(None),
            placed(Some(Rect::new(-303.0, -40.0, 1070.0, 876.0))),
        ];
        let blocks = assemble(
            &PageGlyphs::default(),
            &FontMap::default(),
            &images,
            A4,
            None,
        );

        assert_eq!(blocks.len(), 3);
        // The background, then the positioned figure, then the one we could
        // not locate.
        let Block::Image(first) = &blocks[0] else {
            panic!("expected an image block");
        };
        assert!(first.is_background);

        let Block::Image(last) = &blocks[2] else {
            panic!("expected an image block");
        };
        assert!(last.bbox.is_none());
    }

    #[test]
    fn reading_indices_are_dense_and_in_order() {
        let images = [
            placed(Some(Rect::new(10.0, 10.0, 20.0, 20.0))),
            placed(None),
        ];
        let blocks = assemble(
            &PageGlyphs::default(),
            &FontMap::default(),
            &images,
            A4,
            None,
        );

        for (position, block) in blocks.iter().enumerate() {
            assert_eq!(block.reading_index(), position);
        }
    }

    #[test]
    fn block_confidence_weights_by_glyph_count() {
        use crate::bidi::Direction;
        use crate::types::{Color, Style, TextRenderMode};

        let line = |glyphs: usize, unresolved: usize| TextLine {
            text: "x".to_string(),
            baseline: 0.0,
            bbox: Rect::new(0.0, 0.0, 1.0, 1.0),
            direction: Direction::Rtl,
            style: Style {
                color: Color::BLACK,
                font: String::new(),
                size: 10.0,
                render_mode: TextRenderMode::Fill,
            },
            unresolved,
            glyph_count: glyphs,
        };

        // One bad glyph in a 99-glyph line, beside a perfect 1-glyph line.
        // A plain mean over lines would score 0.5; weighting gives 0.99.
        let score = block_confidence(&[line(99, 1), line(1, 0)]);
        assert!((score - 0.99).abs() < 1e-9, "got {score}");

        // No glyphs at all is not a failure.
        assert_eq!(block_confidence(&[]), 1.0);
    }
}
