//! Golden-file tests: the whole pipeline, through the public API only.
//!
//! # Rust lesson: integration tests
//!
//! Files under `tests/` are compiled as **separate crates** that link against
//! the library the way a real user would. That is the point: they can only
//! reach `pub` items, so they prove the public API is usable, and they would
//! catch a refactor that quietly broke it while every unit test still passed.
//!
//! The unit tests inside `src/` check pieces in isolation. This file checks
//! that the pieces still add up, on a real 44-page Arabic document.
//!
//! # Why golden files
//!
//! Extraction output is too big to assert inline and too detailed to eyeball.
//! A golden file records what the pipeline produced when a human last checked
//! it; the test re-runs the pipeline and diffs. Any change to any layer shows
//! up as a diff, so an "unrelated" tweak to bidi or layout cannot silently
//! alter page 30.
//!
//! Regenerate after an intended change, then **read the diff** before
//! committing it:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p qalam-core --test golden
//! git diff tests/expected
//! ```
//!
//! Alongside the golden file are named assertions for the specific bugs this
//! project has already fixed. A golden diff says "something changed"; those say
//! *what* broke, which is worth a great deal at 3am.

use std::path::{Path, PathBuf};

use qalam_core::Document;

/// The fixture and its golden output.
///
/// Integration tests run with the working directory set to the *package* root
/// (`crates/qalam-core`), while the corpus lives at the repository root, as
/// PLAN.md §5 lays it out — hence the `../..`.
const FIXTURE: &str = "../../tests/fixtures/test_for_arabic_barser.pdf";
const GOLDEN: &str = "../../tests/expected/test_for_arabic_barser.txt";

/// Load the fixture, or `None` when it is absent.
///
/// The corpus is large and binary, so a checkout without it must still be able
/// to run `cargo test`. Every test below returns early rather than failing —
/// with a printed note, so a skipped test never passes silently.
fn fixture() -> Option<Document> {
    if !Path::new(FIXTURE).exists() {
        eprintln!("note: {FIXTURE} not present — skipping");
        return None;
    }
    Some(Document::open(FIXTURE).expect("the fixture should open"))
}

/// Render a document to the golden format: one block per page, with the
/// verdict and confidence in the header so a change in *judgement* shows up as
/// a diff too, not only a change in text.
fn render(doc: &Document) -> String {
    let mut out = String::new();
    for page in doc.pages() {
        out.push_str(&format!(
            "# page {:>2} [{}] confidence={:.2}\n",
            page.number,
            page.report.verdict.as_str(),
            page.confidence(),
        ));
        for reason in &page.report.reasons {
            out.push_str(&format!("# ! {reason}\n"));
        }
        let text = page.text();
        if !text.is_empty() {
            out.push_str(&text);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

#[test]
fn output_matches_the_golden_file() {
    let Some(doc) = fixture() else { return };
    let actual = render(&doc);
    let path = PathBuf::from(GOLDEN);

    // `UPDATE_GOLDEN=1` rewrites the file instead of asserting. Gated behind an
    // environment variable so it can never happen by accident in CI.
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("GOLDEN has a parent"))
            .and_then(|()| std::fs::write(&path, &actual))
            .expect("could not write the golden file");
        eprintln!("note: rewrote {GOLDEN}");
        return;
    }

    let Ok(expected) = std::fs::read_to_string(&path) else {
        panic!("{GOLDEN} is missing — run with UPDATE_GOLDEN=1 to create it");
    };

    if actual != expected {
        // A whole-document diff is unreadable, so report the first differing
        // line and its neighbours — enough to see what moved.
        let (line, exp, got) = first_difference(&expected, &actual);
        panic!(
            "golden mismatch at line {line}\n  expected: {exp}\n  actual:   {got}\n\
             \nRun `UPDATE_GOLDEN=1 cargo test -p qalam-core --test golden` \
             and read the diff if this change was intended."
        );
    }
}

/// The 1-based line number of the first difference, with both versions.
fn first_difference(expected: &str, actual: &str) -> (usize, String, String) {
    let mut exp = expected.lines();
    let mut act = actual.lines();
    let mut line = 0;

    loop {
        line += 1;
        match (exp.next(), act.next()) {
            (None, None) => return (line, "<end>".into(), "<end>".into()),
            (a, b) if a != b => {
                return (
                    line,
                    a.unwrap_or("<end of file>").to_string(),
                    b.unwrap_or("<end of file>").to_string(),
                )
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Named regressions.
//
// Each of these is a bug that actually happened, recorded in PLAN.md §10. The
// golden file would also catch them, but only as "line 47 changed"; these say
// which invariant broke.
// ---------------------------------------------------------------------------

#[test]
fn ligatures_survive_the_reorder() {
    // PLAN.md §10.1. Reordering must happen while `ﻻ` is still one glyph;
    // normalising first turns `ولا` into `وال`.
    let Some(doc) = fixture() else { return };
    let text = doc.page(4).expect("page 4").text();

    assert!(
        text.contains("إرشادي ولا يغني"),
        "expected `إرشادي ولا يغني` in: {text}"
    );
    // Match the phrase, not the bare word: `وال` occurs legitimately on this
    // same page inside `والإجراءات`.
    assert!(
        !text.contains("إرشادي وال يغني"),
        "the `وال` corruption is back: {text}"
    );
}

#[test]
fn no_presentation_forms_survive_normalisation() {
    // NFKC must fold every U+FBxx–U+FExx shaped glyph back to a base letter.
    // Text that keeps them will not compare or search equal to the same words
    // typed normally — a silent defect, which is the worst kind.
    let Some(doc) = fixture() else { return };
    let leftovers: Vec<char> = doc
        .text()
        .chars()
        .filter(|c| matches!(*c as u32, 0xFB50..=0xFDFF | 0xFE70..=0xFEFF))
        .collect();

    assert!(
        leftovers.is_empty(),
        "{} presentation forms survived, e.g. {:?}",
        leftovers.len(),
        &leftovers[..leftovers.len().min(5)]
    );
}

#[test]
fn tashkeel_stay_attached_to_their_letter() {
    // PLAN.md §10.5. Two bugs met here: NFKC supplying a space as an isolated
    // mark's base, and marks landing before their base after the reorder.
    let Some(doc) = fixture() else { return };
    let text = doc.page(5).expect("page 5").text();

    assert!(text.contains("تتضمَّن"), "expected `تتضمَّن` in page 5");
    assert!(!text.contains("تتض "), "a space split the word again");

    // A space immediately followed by a combining mark is the NFKC artefact,
    // and must never reach the output anywhere in the document.
    let whole = doc.text();
    let chars: Vec<char> = whole.chars().collect();
    let orphaned = chars
        .windows(2)
        .filter(|w| w[0] == ' ' && matches!(w[1] as u32, 0x064B..=0x065F | 0x0670))
        .count();
    assert_eq!(orphaned, 0, "found {orphaned} marks stranded after a space");
}

#[test]
fn columns_are_not_interleaved() {
    // PLAN.md §10.4. Page 6 has three cards side by side under a full-width
    // intro. Grouping by baseline alone takes one fragment from each card per
    // line and shuffles three paragraphs together.
    let Some(doc) = fixture() else { return };
    let text = doc.page(6).expect("page 6").text();

    // Each card's opening sentence must be contiguous.
    for sentence in [
        "يخاطب هذا الدليل موظفي",
        "يهدف هذا الدليل إلى تطوير دور",
        "يتضمن الدليل الإرشادات وقائمة",
    ] {
        assert!(text.contains(sentence), "card text was broken up: {sentence}");
    }

    // And the cards must appear right-to-left, in that order.
    let position = |needle: &str| text.find(needle).expect("card heading present");
    assert!(
        position("المستفيد من الدليل") < position("الهدف من الدليل"),
        "cards are not in right-to-left order"
    );
    assert!(position("الهدف من الدليل") < position("محتوى الدليل"));
}

#[test]
fn scanned_pages_are_flagged_rather_than_returned_empty() {
    // The project's whole thesis: a page with no text layer must announce
    // itself, not quietly contribute nothing to a successful-looking result.
    let Some(doc) = fixture() else { return };
    assert_eq!(doc.pages_needing_ocr(), vec![2, 3, 42, 43]);

    for number in doc.pages_needing_ocr() {
        let page = doc.page(number).expect("flagged page exists");
        assert!(!page.report.reasons.is_empty(), "page {number} gave no reason");
        assert_eq!(page.confidence(), 0.0);
    }
}

#[test]
fn every_readable_page_resolves_every_glyph() {
    // After the /Encoding fallback chain landed, no glyph on a readable page
    // should be unresolvable. A regression here means a font route was lost.
    let Some(doc) = fixture() else { return };
    for page in doc.pages().iter().filter(|p| !p.needs_ocr()) {
        assert_eq!(
            page.report.signals.unresolved,
            0,
            "page {} left {} glyph(s) unresolved",
            page.number,
            page.report.signals.unresolved
        );
    }
}
