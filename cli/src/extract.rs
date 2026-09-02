//! The `extract` command — the end of Tier A.
//!
//! Deliberately thin. Every step of the pipeline lives in
//! [`qalam_core::Document`], so this file only decides what to *print*. The
//! Python bindings call the same API, which is what keeps the two front ends
//! from drifting apart.

use qalam_core::{Block, Document, Recoverability};

/// Extract one page, or the whole document when `page` is `None`.
///
/// With `blocks`, prints the structured model — every block with its type,
/// position and confidence — instead of running the text together.
pub fn extract(path: &str, page: Option<u32>) -> qalam_core::Result<()> {
    run(path, page, false)
}

/// Print the page's typed blocks in reading order.
pub fn blocks(path: &str, page: Option<u32>) -> qalam_core::Result<()> {
    run(path, page, true)
}

fn run(path: &str, page: Option<u32>, show_blocks: bool) -> qalam_core::Result<()> {
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
        if show_blocks {
            print_blocks(page);
        } else if page.needs_ocr() {
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

/// Print one page's blocks in reading order.
fn print_blocks(page: &qalam_core::Page) {
    if page.blocks.is_empty() {
        println!("   (no blocks)");
        return;
    }

    for block in &page.blocks {
        // A block's position is what makes the reading order checkable by eye:
        // for an RTL page the x values should march right to left within a row.
        let position = match block.bbox() {
            Some(b) => format!(
                "({:>5.0},{:>5.0}) {:>4.0}x{:<4.0}",
                b.x0,
                b.y0,
                b.width(),
                b.height()
            ),
            None => "        position unknown".to_string(),
        };

        match block {
            Block::Text(text) => {
                println!(
                    "[{:>2}] text  {position}  confidence {:.2}  {} line(s)",
                    block.reading_index(),
                    text.confidence,
                    text.lines.len()
                );
                for line in &text.lines {
                    println!("       {}", line.text);
                }
            }
            Block::Image(image) => {
                let kind = if image.is_background {
                    "image (background)"
                } else {
                    "image"
                };
                match &image.image {
                    qalam_core::ExtractedImage::Ready(ready) => println!(
                        "[{:>2}] {kind}  {position}  {}  {}x{}px",
                        block.reading_index(),
                        ready.file_name(),
                        ready.width,
                        ready.height
                    ),
                    qalam_core::ExtractedImage::Unsupported { reason, .. } => println!(
                        "[{:>2}] {kind}  {position}  not decoded: {reason}",
                        block.reading_index()
                    ),
                }
            }
        }
    }
}
