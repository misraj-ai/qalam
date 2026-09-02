//! The `extract` command — the end of Tier A.
//!
//! Runs the whole pipeline for real: L0 opens the file, L1 interprets the
//! content stream, L2 resolves codes through `/ActualText` and `/ToUnicode`,
//! L3 groups lines, reorders them and normalises them, and L4 judges whether
//! the result can be trusted.

use qalam_core::{FontMap, Pdf, Recoverability};

/// Extract one page, or the whole document when `page` is `None`.
pub fn extract(path: &str, page: Option<u32>) -> qalam_core::Result<()> {
    let pdf = Pdf::open(path)?;

    // Read the page summaries once. `pages()` walks the object graph, so
    // calling it per page would make the whole run quadratic.
    let all_pages = pdf.pages();

    let numbers: Vec<u32> = match page {
        Some(n) => vec![n],
        None => all_pages.iter().map(|p| p.number).collect(),
    };

    let mut reports = Vec::new();

    for number in numbers {
        let Some(info) = all_pages.iter().find(|p| p.number == number) else {
            return Err(qalam_core::Error::PageNotFound(number));
        };

        // L2 first: the font map supplies both the code→text mapping and the
        // real advance widths that L1 needs.
        let fonts = FontMap::from_raw(pdf.page_raw_fonts(number)?);
        let content = pdf.page_content(number)?;
        let glyphs = qalam_core::interpret(&content, &info.fonts, &fonts);

        // L3: geometry → bidi reorder → NFKC.
        let lines = qalam_core::reconstruct(&glyphs, &fonts);

        // L4: is any of this trustworthy?
        let report = qalam_core::assess(number, &glyphs, &fonts, &lines);

        println!("── page {number} [{}]──", report.verdict.as_str());
        for reason in &report.reasons {
            println!("   ! {reason}");
        }

        // The whole point: do not print text we have judged untrustworthy as
        // though it were a result.
        if report.verdict == Recoverability::NeedsOcr {
            println!("   (no trustworthy text — this page needs OCR)");
        } else {
            for line in &lines {
                println!("{}", line.text);
            }
        }
        println!();

        reports.push(report);
    }

    summarise(&reports);
    Ok(())
}

/// Print a one-line-per-verdict tally across the pages processed.
fn summarise(reports: &[qalam_core::PageReport]) {
    if reports.len() <= 1 {
        return;
    }

    let count = |v: Recoverability| reports.iter().filter(|r| r.verdict == v).count();

    // The mean confidence across pages, which is more informative than a
    // glyph-level rate because a page with no glyphs scores 0, not 100%.
    let mean: f64 = reports.iter().map(|r| r.confidence).sum::<f64>() / reports.len() as f64;

    println!(
        "{} page(s): {} ok, {} degraded, {} need OCR — mean confidence {:.2}",
        reports.len(),
        count(Recoverability::Ok),
        count(Recoverability::Degraded),
        count(Recoverability::NeedsOcr),
        mean,
    );

    let needs: Vec<String> = reports
        .iter()
        .filter(|r| r.verdict == Recoverability::NeedsOcr)
        .map(|r| r.page.to_string())
        .collect();
    if !needs.is_empty() {
        println!("pages needing OCR: {}", needs.join(", "));
    }
}
