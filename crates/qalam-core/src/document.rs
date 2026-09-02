//! The high-level API: open a file, get text.
//!
//! Everything below this module is a layer of the pipeline; this is the one
//! thing a caller actually wants. It exists so that the orchestration —
//! interpret, resolve, reconstruct, assess, in that order, with the font map
//! built before the interpreter runs — is written **once**, here, rather than
//! copied into the CLI and again into the Python bindings. A binding that
//! reimplements the sequence is a binding that will drift out of step with it.
//!
//! ```no_run
//! let doc = qalam_core::Document::open("guide.pdf")?;
//! println!("{}", doc.text());
//!
//! for page in doc.pages() {
//!     if page.needs_ocr() {
//!         println!("page {} needs OCR", page.number);
//!     }
//! }
//! # Ok::<(), qalam_core::Error>(())
//! ```

use crate::arabic::{self, TextLine};
use crate::detect::{self, PageReport, Recoverability};
use crate::font::FontMap;
use crate::images::{self, PlacedImage};
use crate::parser::Pdf;
use crate::types::Rotation;
use crate::Result;

/// A fully extracted document.
///
/// Extraction happens eagerly in [`Document::open`]: by the time you hold one
/// of these, every page has been read, resolved and judged. That costs memory
/// proportional to the document, and buys an API with no failure modes after
/// construction — which matters a great deal for a Python binding, where a
/// lazily-failing accessor is far harder to use well.
#[derive(Debug, Clone)]
pub struct Document {
    pages: Vec<Page>,
}

/// One extracted page.
#[derive(Debug, Clone)]
pub struct Page {
    /// 1-based page number, as a human would say it.
    pub number: u32,
    /// Page width in points.
    pub width: f64,
    /// Page height in points.
    pub height: f64,
    /// The page's `/Rotate`.
    pub rotation: Rotation,
    /// The text, in reading order: columns already resolved (L6), each line in
    /// logical order and normalised (L3).
    pub lines: Vec<TextLine>,
    /// What the detector concluded about this page (L4).
    pub report: PageReport,
    /// The images drawn on this page, each with where it landed (L7).
    ///
    /// Independent of the text: an image needs no font, no reading order and no
    /// recoverability judgement, so it is extracted whatever the page's verdict
    /// — including on a page that needs OCR, where the image *is* the content.
    pub images: Vec<PlacedImage>,
}

impl Page {
    /// This page's text, lines joined with newlines.
    ///
    /// Returned regardless of the verdict — a caller who has checked
    /// [`Page::report`] may well want to look at degraded text. It is
    /// [`Document::text`] that declines to hand back the untrustworthy ones.
    pub fn text(&self) -> String {
        arabic::lines_to_text(&self.lines)
    }

    /// Whether this page has no usable text layer.
    pub fn needs_ocr(&self) -> bool {
        self.report.verdict == Recoverability::NeedsOcr
    }

    /// How much the extracted text can be trusted, 0.0 to 1.0.
    pub fn confidence(&self) -> f64 {
        self.report.confidence
    }
}

impl Document {
    /// Open a PDF and extract every page.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let pdf = Pdf::open(path)?;

        // Read the page summaries once. `Pdf::pages` walks the object graph, so
        // calling it per page would make the whole run quadratic.
        let summaries = pdf.pages();
        let mut pages = Vec::with_capacity(summaries.len());

        for info in &summaries {
            let number = info.number;

            // L2 before L1: the font map supplies both the code→text mapping
            // and the advance widths the interpreter needs to place glyphs.
            let fonts = FontMap::from_raw(pdf.page_raw_fonts(number)?);
            let content = pdf.page_content(number)?;

            // L1 → L3 → L4.
            let glyphs = crate::content::interpret(&content, &info.fonts, &fonts);
            let lines = arabic::reconstruct(&glyphs, &fonts);
            let report = detect::assess(number, &glyphs, &fonts, &lines);

            // L7, off the critical path for text: images come from
            // `/Resources`, and their positions from the `Do` operators L1 saw.
            let raw_images = pdf.page_raw_images(number)?;
            let images = images::extract_placed(&raw_images, &glyphs.xobjects);

            pages.push(Page {
                number,
                width: info.media_box.width(),
                height: info.media_box.height(),
                rotation: info.rotation,
                lines,
                report,
                images,
            });
        }

        Ok(Self { pages })
    }

    /// Every page, in document order.
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// Look up a page by its 1-based number.
    pub fn page(&self, number: u32) -> Option<&Page> {
        self.pages.iter().find(|p| p.number == number)
    }

    /// How many pages the document has.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Every image in the document, paired with the page it appears on.
    pub fn images(&self) -> impl Iterator<Item = (u32, &PlacedImage)> {
        self.pages
            .iter()
            .flat_map(|page| page.images.iter().map(move |img| (page.number, img)))
    }

    /// The whole document's text, pages separated by a blank line.
    ///
    /// **Pages judged [`Recoverability::NeedsOcr`] contribute nothing.** That
    /// is the entire point of the project: a page whose text layer is missing
    /// or unreadable must not quietly hand back plausible-looking garbage that
    /// a caller will mistake for a result. Ask [`Document::pages_needing_ocr`]
    /// which pages were left out, or read [`Page::text`] directly to see them
    /// anyway.
    pub fn text(&self) -> String {
        self.pages
            .iter()
            .filter(|p| !p.needs_ocr())
            .map(|p| p.text())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The numbers of the pages that [`Document::text`] omitted.
    ///
    /// The actionable half of the signal: hand these to an OCR engine.
    pub fn pages_needing_ocr(&self) -> Vec<u32> {
        self.pages
            .iter()
            .filter(|p| p.needs_ocr())
            .map(|p| p.number)
            .collect()
    }

    /// Mean per-page confidence across the document, 0.0 to 1.0.
    ///
    /// Pages needing OCR score 0 and are *included* in the mean, so a document
    /// that is half scans reports about 0.5 rather than a misleading 1.0.
    pub fn confidence(&self) -> f64 {
        if self.pages.is_empty() {
            return 0.0;
        }
        self.pages.iter().map(Page::confidence).sum::<f64>() / self.pages.len() as f64
    }
}

/// Extract a document's text in one call.
///
/// The convenience form of [`Document::open`] followed by [`Document::text`],
/// and the exact behaviour the Python `qalam.extract_text()` exposes.
pub fn extract_text(path: impl AsRef<std::path::Path>) -> Result<String> {
    Ok(Document::open(path)?.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "../../tests/fixtures/test_for_arabic_barser.pdf";

    /// Skip rather than fail when the fixture is absent, so a fresh clone
    /// without the (large, binary) test file still passes `cargo test`.
    fn fixture() -> Option<Document> {
        if !std::path::Path::new(FIXTURE).exists() {
            return None;
        }
        Some(Document::open(FIXTURE).expect("the fixture should open"))
    }

    #[test]
    fn opens_the_fixture_and_finds_every_page() {
        let Some(doc) = fixture() else { return };
        assert_eq!(doc.page_count(), 44);
        assert_eq!(doc.page(1).map(|p| p.number), Some(1));
        assert!(doc.page(999).is_none());
        // A4 in points.
        assert!((doc.page(1).unwrap().width - 595.276).abs() < 0.01);
    }

    #[test]
    fn the_ligature_regression_survives_the_high_level_api() {
        // PLAN.md §10.1, checked through the API a caller actually uses.
        let Some(doc) = fixture() else { return };
        let text = doc.page(4).expect("page 4").text();

        // Match the whole phrase, not the bare word. `وال` on its own is a
        // false alarm: the same page legitimately contains `والإجراءات`, which
        // begins waw-alef-lam. Only the corrupted phrase is the bug.
        assert!(
            text.contains("إرشادي ولا يغني"),
            "expected `إرشادي ولا يغني` in: {text}"
        );
        assert!(
            !text.contains("إرشادي وال يغني"),
            "produced the `وال` corruption: {text}"
        );
    }

    #[test]
    fn scanned_pages_are_reported_not_silently_dropped() {
        let Some(doc) = fixture() else { return };
        assert_eq!(doc.pages_needing_ocr(), vec![2, 3, 42, 43]);
        assert!(doc.page(2).unwrap().needs_ocr());
        assert!(!doc.page(4).unwrap().needs_ocr());
    }

    #[test]
    fn document_text_omits_the_unreadable_pages() {
        let Some(doc) = fixture() else { return };
        let whole = doc.text();
        // Page 4's text is in there.
        assert!(whole.contains("\u{0647}\u{0630}\u{0627} \u{0627}\u{0644}\u{062F}"));
        // And the mean confidence reflects the four scanned pages rather than
        // rounding up to a perfect score.
        assert!(doc.confidence() > 0.85 && doc.confidence() < 1.0);
    }

    #[test]
    fn extract_text_matches_the_long_form() {
        let Some(doc) = fixture() else { return };
        assert_eq!(extract_text(FIXTURE).unwrap(), doc.text());
    }
}
