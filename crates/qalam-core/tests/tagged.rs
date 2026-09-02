//! Tagged-PDF tests, driven by PDFs this file builds from scratch.
//!
//! # Why synthesise the fixture
//!
//! The corpus has no tagged document — `/StructTreeRoot` appears zero times in
//! it. Rather than write a `/StructTreeRoot` reader that has never seen a byte
//! of real input, these tests construct genuine PDFs: real objects, a real xref
//! table, parsed by the same `lopdf` the library uses. That is weaker evidence
//! than a document produced by Word or InDesign — a real writer will do things
//! this builder does not — but it is far stronger than asserting against
//! hand-made structs, because everything from byte offsets upward is exercised.
//!
//! Replace these with a real tagged PDF when one is available; the assertions
//! should carry over unchanged.

use std::fmt::Write as _;

/// A minimal PDF writer: enough to make a valid file, and no more.
///
/// # Rust lesson: building bytes with an offset table
///
/// A PDF's trailer has to name the byte offset of every object, so the writer
/// records where each one started as it goes. That is the whole reason this is
/// a struct with state rather than one big `format!`.
struct MiniPdf {
    bytes: Vec<u8>,
    /// Byte offset of each object, indexed by object number minus one.
    offsets: Vec<usize>,
}

impl MiniPdf {
    fn new() -> Self {
        Self {
            bytes: b"%PDF-1.7\n".to_vec(),
            offsets: Vec::new(),
        }
    }

    /// Append one indirect object and return its number.
    fn object(&mut self, body: &str) -> usize {
        let number = self.offsets.len() + 1;
        self.offsets.push(self.bytes.len());
        self.bytes
            .extend_from_slice(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
        number
    }

    /// Append a stream object, filling in `/Length` from the content itself.
    fn stream(&mut self, content: &str) -> usize {
        let body = format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len() + 1
        );
        self.object(&body)
    }

    /// Close the file: xref table, trailer, `startxref`.
    fn finish(mut self, root: usize) -> Vec<u8> {
        let xref_at = self.bytes.len();
        let count = self.offsets.len() + 1;

        let mut xref = format!("xref\n0 {count}\n0000000000 65535 f \n");
        for offset in &self.offsets {
            // Exactly 20 bytes per entry, as the spec requires.
            let _ = writeln!(xref, "{offset:010} 00000 n ");
        }
        let _ = write!(
            xref,
            "trailer\n<< /Size {count} /Root {root} 0 R >>\nstartxref\n{xref_at}\n%%EOF\n"
        );

        self.bytes.extend_from_slice(xref.as_bytes());
        self.bytes
    }
}

/// Build a tagged one-page PDF whose **structure order disagrees with its
/// geometry** — the case that makes a tagged reader worth having.
///
/// Two lines are painted: `Alpha` low on the page, `Beta` above it. Read
/// geometrically, top to bottom, that is `Beta` then `Alpha`. The structure
/// tree says the opposite: `Alpha` (a `/P`) comes first, `Beta` (an `/H1`)
/// second. A reader that honours the tags must return `Alpha`, `Beta`.
fn tagged_pdf() -> Vec<u8> {
    let mut pdf = MiniPdf::new();

    let font = pdf.object(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );

    // `Alpha` is lower on the page than `Beta`, so geometry and structure
    // disagree about which comes first.
    let content = pdf.stream(
        "BT /F1 12 Tf 20 100 Td /P << /MCID 0 >> BDC (Alpha) Tj EMC ET\n\
         BT /F1 12 Tf 20 150 Td /H1 << /MCID 1 >> BDC (Beta) Tj EMC ET",
    );

    // Objects that refer to each other by number have to be numbered before
    // they are written, so the page and tree root are reserved by hand.
    let page = pdf.offsets.len() + 1;
    let pages = page + 1;
    let struct_root = pages + 1;
    let doc_elem = struct_root + 1;
    let alpha_elem = doc_elem + 1;
    let beta_elem = alpha_elem + 1;
    let catalog = beta_elem + 1;

    pdf.object(&format!(
        "<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 200 200] \
         /Resources << /Font << /F1 {font} 0 R >> >> \
         /Contents {content} 0 R /StructParents 0 >>"
    ));
    pdf.object(&format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"));
    pdf.object(&format!("<< /Type /StructTreeRoot /K [{doc_elem} 0 R] >>"));
    pdf.object(&format!(
        "<< /Type /StructElem /S /Document /P {struct_root} 0 R \
         /K [{alpha_elem} 0 R {beta_elem} 0 R] >>"
    ));
    // `/K [0]` attaches this element to the marked-content sequence with
    // MCID 0 on the page named by `/Pg`.
    pdf.object(&format!(
        "<< /Type /StructElem /S /P /P {doc_elem} 0 R /Pg {page} 0 R /K [0] >>"
    ));
    pdf.object(&format!(
        "<< /Type /StructElem /S /H1 /P {doc_elem} 0 R /Pg {page} 0 R /K [1] >>"
    ));
    pdf.object(&format!(
        "<< /Type /Catalog /Pages {pages} 0 R /StructTreeRoot {struct_root} 0 R \
         /MarkInfo << /Marked true >> >>"
    ));

    pdf.finish(catalog)
}

/// Write a PDF to a temporary file, since the API opens by path.
///
/// # Rust lesson: `cargo test` runs tests in parallel
///
/// Every test gets its own file name. Sharing one path looks harmless and is
/// not: two tests writing and reading it at once produced a truncated file and
/// a bewildering `InvalidFileHeader`, which reads exactly like a bug in the
/// parser. The counter makes the tests independent, which is what lets them run
/// concurrently at all.
fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "qalam-test-{name}-{}-{unique}.pdf",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("could not write the test PDF");
    path
}

#[test]
fn the_synthetic_pdf_is_a_valid_document() {
    // Before trusting any assertion about structure, prove the file parses and
    // the ordinary pipeline reads it. A builder bug would otherwise look like
    // a library bug.
    let path = write_temp("tagged", &tagged_pdf());
    let doc = qalam_core::Document::open(&path).expect("the synthetic PDF should open");

    assert_eq!(doc.page_count(), 1);
    let text = doc.page(1).expect("page 1").text();
    assert!(text.contains("Alpha"), "got: {text:?}");
    assert!(text.contains("Beta"), "got: {text:?}");
}

#[test]
fn geometry_alone_reads_the_page_in_the_wrong_order() {
    // The premise every tagged test below rests on, asserted rather than
    // assumed: read by position, `Beta` (higher up the page) comes first. If
    // this ever stopped being true, those tests would pass for the wrong
    // reason and nobody would notice.
    //
    // This must call the geometric path *directly*. Going through `Document`
    // would now honour the structure tree and return the opposite order —
    // which is the whole point of the wiring, and would make this assertion
    // fail against perfectly correct code.
    let path = write_temp("tagged", &tagged_pdf());
    let pdf = qalam_core::Pdf::open(&path).expect("opens");
    let info = pdf.pages().into_iter().next().expect("one page");
    let fonts = qalam_core::FontMap::from_raw(pdf.page_raw_fonts(1).expect("fonts"));
    let glyphs = qalam_core::interpret(&pdf.page_content(1).expect("content"), &info.fonts, &fonts);

    let text = qalam_core::lines_to_text(&qalam_core::reconstruct(&glyphs, &fonts));

    let beta = text.find("Beta").expect("Beta present");
    let alpha = text.find("Alpha").expect("Alpha present");
    assert!(
        beta < alpha,
        "geometry should put Beta first, got: {text:?}"
    );
}

// ---------------------------------------------------------------------------
// The structure tree, read from the file rather than from a hand-made struct.
// ---------------------------------------------------------------------------

#[test]
fn the_structure_tree_is_read_in_document_order() {
    let path = write_temp("tagged", &tagged_pdf());
    let pdf = qalam_core::Pdf::open(&path).expect("opens");
    let structure = pdf.structure();

    assert!(structure.marked, "/MarkInfo /Marked should be honoured");
    assert!(structure.is_usable());

    // `/Document` wraps the two content elements, and is emitted before them.
    let tags: Vec<&str> = structure.elements.iter().map(|e| e.tag.as_str()).collect();
    assert_eq!(tags, ["Document", "P", "H1"]);

    // `/Pg` is inherited down the tree; the wrapper names no page of its own.
    assert_eq!(structure.elements[0].page, None);
    assert_eq!(structure.elements[1].page, Some(1));
    assert_eq!(structure.elements[1].mcids, vec![0]);
    assert_eq!(structure.elements[2].mcids, vec![1]);
}

#[test]
fn the_content_stream_reports_its_marked_content_ids() {
    let path = write_temp("tagged", &tagged_pdf());
    let pdf = qalam_core::Pdf::open(&path).expect("opens");
    let info = pdf.pages().into_iter().next().expect("one page");
    let fonts = qalam_core::FontMap::from_raw(pdf.page_raw_fonts(1).expect("fonts"));
    let glyphs = qalam_core::interpret(&pdf.page_content(1).expect("content"), &info.fonts, &fonts);

    // `Alpha` is five glyphs and painted first; `Beta` is four.
    assert_eq!(glyphs.mcid_spans.len(), 2);
    assert_eq!(glyphs.mcid_spans[0].mcid, 0);
    assert_eq!(glyphs.mcid_spans[0].end - glyphs.mcid_spans[0].start, 5);
    assert_eq!(glyphs.mcid_spans[1].mcid, 1);
}

#[test]
fn the_tags_override_the_geometry() {
    // The test this whole file exists for. Geometry reads `Beta` first because
    // it sits higher on the page; the structure tree says `Alpha` comes first,
    // and the tree is the document's own statement of its reading order.
    let path = write_temp("tagged", &tagged_pdf());
    let pdf = qalam_core::Pdf::open(&path).expect("opens");
    let info = pdf.pages().into_iter().next().expect("one page");
    let fonts = qalam_core::FontMap::from_raw(pdf.page_raw_fonts(1).expect("fonts"));
    let glyphs = qalam_core::interpret(&pdf.page_content(1).expect("content"), &info.fonts, &fonts);

    let order = qalam_core::ReadingOrder::from_structure(
        &pdf.structure(),
        1,
        &glyphs.mcid_spans,
        glyphs.glyphs.len(),
    )
    .expect("the tree should be usable");

    assert_eq!(order.runs.len(), 2);
    assert_eq!(order.runs[0].tag, "P");
    assert_eq!(order.runs[1].tag, "H1");

    // Resolve the ordered glyph indices back into text and check the result.
    let text: String = order
        .glyph_indices()
        .into_iter()
        .filter_map(|i| {
            let glyph = &glyphs.glyphs[i];
            fonts.decode(&glyph.style.font, glyph.code)
        })
        .collect();

    assert_eq!(text, "AlphaBeta", "the tree order was not honoured");
}

#[test]
fn an_untagged_document_yields_no_structure() {
    // The normal case, and it must be quiet: no tree is not an error, it is
    // what nearly every PDF looks like.
    let mut pdf = MiniPdf::new();
    let font = pdf.object(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );
    let content = pdf.stream("BT /F1 12 Tf 20 100 Td (Plain) Tj ET");
    let page = pdf.offsets.len() + 1;
    let pages = page + 1;
    let catalog = pages + 1;
    pdf.object(&format!(
        "<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 200 200] \
         /Resources << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
    ));
    pdf.object(&format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"));
    pdf.object(&format!("<< /Type /Catalog /Pages {pages} 0 R >>"));

    let path = write_temp("untagged", &pdf.finish(catalog));
    let opened = qalam_core::Pdf::open(&path).expect("opens");
    let structure = opened.structure();

    assert!(!structure.marked);
    assert!(structure.elements.is_empty());
    assert!(!structure.is_usable());

    // And the ordinary path still reads it.
    let doc = qalam_core::Document::open(&path).expect("opens");
    assert!(doc.page(1).expect("page 1").text().contains("Plain"));
}

// ---------------------------------------------------------------------------
// The high-level API, which is what actually has to honour the tags.
// ---------------------------------------------------------------------------

#[test]
fn document_honours_the_structure_tree() {
    // Everything above proves the pieces work. This proves they are wired
    // together: the ordinary entry point, on a tagged file, returns the
    // document's stated order and not the geometric one.
    let path = write_temp("tagged", &tagged_pdf());
    let doc = qalam_core::Document::open(&path).expect("opens");
    let page = doc.page(1).expect("page 1");

    assert!(page.tagged, "the page should have used its structure tree");
    assert_eq!(doc.tagged_pages(), vec![1]);

    let text = page.text();
    let alpha = text.find("Alpha").expect("Alpha present");
    let beta = text.find("Beta").expect("Beta present");
    assert!(alpha < beta, "the tree order was not honoured: {text:?}");
}

#[test]
fn each_structure_element_becomes_its_own_block() {
    // The tree's elements are the document's own idea of a block, so they map
    // straight onto ours — one region per element, in tree order.
    let path = write_temp("tagged", &tagged_pdf());
    let doc = qalam_core::Document::open(&path).expect("opens");
    let page = doc.page(1).expect("page 1");

    let texts: Vec<String> = page.text_blocks().map(|b| b.text()).collect();
    assert_eq!(texts, ["Alpha", "Beta"]);
}

#[test]
fn an_untagged_document_still_uses_geometry() {
    // The fallback must be invisible: no tree, no complaint, same result as
    // before any of this existed.
    let doc = qalam_core::Document::open("../../tests/fixtures/test_for_arabic_barser.pdf");
    let Ok(doc) = doc else {
        eprintln!("note: corpus fixture not present — skipping");
        return;
    };

    assert!(doc.tagged_pages().is_empty(), "the corpus is untagged");
    assert!(doc.pages().iter().all(|p| !p.tagged));
    // And the ligature regression still holds through the geometric path.
    assert!(doc
        .page(4)
        .expect("page 4")
        .text()
        .contains("إرشادي ولا يغني"));
}
