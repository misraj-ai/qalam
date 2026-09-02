//! The `extract` command — the end of Tier A.
//!
//! Runs the whole pipeline for real: L0 opens the file, L1 interprets the
//! content stream, L2 resolves codes through `/ToUnicode`, and L3 groups lines,
//! reorders them and normalises them. What comes out should be readable Arabic.

use qalam_core::{FontMap, Pdf};

/// Extract one page, or the whole document when `page` is `None`.
pub fn extract(path: &str, page: Option<u32>) -> qalam_core::Result<()> {
    let pdf = Pdf::open(path)?;

    // `match` on the Option to build the list of pages to walk. One page or all
    // of them takes the same code path from here on.
    let pages: Vec<u32> = match page {
        Some(n) => vec![n],
        None => pdf.pages().iter().map(|p| p.number).collect(),
    };

    let mut total_glyphs = 0usize;
    let mut total_unresolved = 0usize;

    for number in pages {
        let Some(info) = pdf.pages().into_iter().find(|p| p.number == number) else {
            return Err(qalam_core::Error::PageNotFound(number));
        };

        // L2 first: the font map supplies both the code→text mapping and the
        // real advance widths that L1 needs.
        let fonts = FontMap::from_raw(pdf.page_raw_fonts(number)?);
        let content = pdf.page_content(number)?;
        let glyphs = qalam_core::interpret(&content, &info.fonts, &fonts);

        // L3: geometry → bidi reorder → NFKC.
        let lines = qalam_core::reconstruct(&glyphs.glyphs, &fonts);

        let unresolved: usize = lines.iter().map(|l| l.unresolved).sum();
        total_glyphs += glyphs.glyphs.len();
        total_unresolved += unresolved;

        println!("── page {number} ──");
        for line in &lines {
            println!("{}", line.text);
        }
        if lines.is_empty() {
            println!("(no text — this page may need OCR)");
        }
        println!();
    }

    // A first, crude recoverability figure. L4 will replace it with a real
    // score that also weighs missing ToUnicode maps and presentation-form
    // ratios (PLAN.md §3, L4).
    if total_glyphs > 0 {
        let rate = 100.0 * (1.0 - total_unresolved as f64 / total_glyphs as f64);
        println!("{total_unresolved} of {total_glyphs} glyphs unresolved ({rate:.1}% readable)");
    }
    Ok(())
}
