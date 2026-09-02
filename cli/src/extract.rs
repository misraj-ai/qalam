//! The `extract` command — the end of Tier A.
//!
//! Deliberately thin. Every step of the pipeline lives in
//! [`qalam_core::Document`], so this file only decides what to *print*. The
//! Python bindings call the same API, which is what keeps the two front ends
//! from drifting apart.

use qalam_core::{Document, Recoverability};

/// Extract one page, or the whole document when `page` is `None`.
pub fn extract(path: &str, page: Option<u32>) -> qalam_core::Result<()> {
    let doc = Document::open(path)?;

    // `match` on the Option to pick what to walk; one page or all of them
    // takes the same code path from here on.
    let pages: Vec<&qalam_core::Page> = match page {
        Some(n) => vec![doc.page(n).ok_or(qalam_core::Error::PageNotFound(n))?],
        None => doc.pages().iter().collect(),
    };

    for page in &pages {
        println!(
            "── page {} [{}]──",
            page.number,
            page.report.verdict.as_str()
        );
        for reason in &page.report.reasons {
            println!("   ! {reason}");
        }

        // The whole point: do not print text we have judged untrustworthy as
        // though it were a result.
        if page.needs_ocr() {
            println!("   (no trustworthy text — this page needs OCR)");
        } else {
            println!("{}", page.text());
        }
        println!();
    }

    if pages.len() > 1 {
        summarise(&doc);
    }
    Ok(())
}

/// Print a tally of verdicts across the document.
fn summarise(doc: &Document) {
    let count = |v: Recoverability| doc.pages().iter().filter(|p| p.report.verdict == v).count();

    println!(
        "{} page(s): {} ok, {} degraded, {} need OCR — mean confidence {:.2}",
        doc.page_count(),
        count(Recoverability::Ok),
        count(Recoverability::Degraded),
        count(Recoverability::NeedsOcr),
        doc.confidence(),
    );

    let needs = doc.pages_needing_ocr();
    if !needs.is_empty() {
        let list: Vec<String> = needs.iter().map(u32::to_string).collect();
        println!("pages needing OCR: {}", list.join(", "));
    }
}
