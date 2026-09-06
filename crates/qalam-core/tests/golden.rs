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

/// The corpus: each fixture and the golden file recording its output.
///
/// Integration tests run with the working directory set to the *package* root
/// (`crates/qalam-core`), while the corpus lives at the repository root, as
/// PLAN.md §5 lays it out — hence the `../..`.
///
/// The two documents exercise genuinely different machinery, which is the
/// point of having both:
///
/// - `test_for_arabic_barser` — `Type0` composite fonts, `Identity-H`, 2-byte
///   CIDs, presentation forms folded by NFKC, multi-column card layouts.
/// - `bar_Persons` — `Type1` simple fonts, 1-byte codes, ligatures whose
///   `/ToUnicode` values are already several base letters, and 18 ruled tables.
const CORPUS: &[(&str, &str)] = &[
    (
        "../../tests/fixtures/test_for_arabic_barser.pdf",
        "../../tests/expected/test_for_arabic_barser.txt",
    ),
    (
        "../../tests/fixtures/bar_Persons.pdf",
        "../../tests/expected/bar_Persons.txt",
    ),
    ("../../tests/fixtures/1.pdf", "../../tests/expected/1.txt"),
    ("../../tests/fixtures/3.pdf", "../../tests/expected/3.txt"),
    ("../../tests/fixtures/8.pdf", "../../tests/expected/8.txt"),
    ("../../tests/fixtures/12.pdf", "../../tests/expected/12.txt"),
];

/// The first fixture, which most of the named regressions below refer to.
const FIXTURE: &str = CORPUS[0].0;

/// The tables fixture.
const TABLES_FIXTURE: &str = CORPUS[1].0;

/// A document that sets `Tc` inside a `q … Q` block and relies on `Q` to
/// restore it.
const SPACING_FIXTURE: &str = CORPUS[2].0;

/// A document that draws some letters as zero-advance overlays.
const OVERLAY_FIXTURE: &str = CORPUS[3].0;

/// A document whose font maps several glyphs to U+FFFD on purpose.
const NO_TEXT_GLYPH_FIXTURE: &str = CORPUS[4].0;

/// A document that tightens its tracking with a large negative `Tc`.
const TIGHT_TRACKING_FIXTURE: &str = CORPUS[5].0;

/// Open a document, or `None` when the fixture is absent.
///
/// The corpus is large and binary, so a checkout without it must still be able
/// to run `cargo test`. Every test returns early rather than failing — with a
/// printed note, so a skipped test never passes silently.
fn open(path: &str) -> Option<Document> {
    if !Path::new(path).exists() {
        eprintln!("note: {path} not present — skipping");
        return None;
    }
    Some(Document::open(path).expect("the fixture should open"))
}

/// Load the main fixture.
fn fixture() -> Option<Document> {
    open(FIXTURE)
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
    for (pdf, golden) in CORPUS {
        let Some(doc) = open(pdf) else { continue };
        compare_against_golden(&doc, golden);
    }
}

/// Diff one document's rendering against its golden file.
fn compare_against_golden(doc: &Document, golden: &str) {
    let actual = render(doc);
    let path = PathBuf::from(golden);

    // `UPDATE_GOLDEN=1` rewrites the file instead of asserting. Gated behind an
    // environment variable so it can never happen by accident in CI.
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("the golden path has a parent"))
            .and_then(|()| std::fs::write(&path, &actual))
            .expect("could not write the golden file");
        eprintln!("note: rewrote {golden}");
        return;
    }

    let Ok(expected) = std::fs::read_to_string(&path) else {
        panic!("{golden} is missing — run with UPDATE_GOLDEN=1 to create it");
    };

    if actual != expected {
        // A whole-document diff is unreadable, so report the first differing
        // line and its neighbours — enough to see what moved.
        let (line, exp, got) = first_difference(&expected, &actual);
        panic!(
            "golden mismatch in {golden} at line {line}\n  expected: {exp}\n  actual:   {got}\n\
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
        assert!(
            text.contains(sentence),
            "card text was broken up: {sentence}"
        );
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
        assert!(
            !page.report.reasons.is_empty(),
            "page {number} gave no reason"
        );
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
            page.report.signals.unresolved, 0,
            "page {} left {} glyph(s) unresolved",
            page.number, page.report.signals.unresolved
        );
    }
}

// ---------------------------------------------------------------------------
// The tables fixture.
// ---------------------------------------------------------------------------

#[test]
fn ruled_tables_are_reconstructed() {
    use qalam_core::Block;

    let Some(doc) = open(TABLES_FIXTURE) else {
        return;
    };

    let tables: Vec<&qalam_core::TableBlock> = doc
        .pages()
        .iter()
        .flat_map(|p| &p.blocks)
        .filter_map(|b| match b {
            Block::Table(t) => Some(t),
            _ => None,
        })
        .collect();

    assert!(
        tables.len() >= 15,
        "expected the document's ruled tables, found {}",
        tables.len()
    );

    // Page 5's table of governorates: seven columns, and the first column in
    // reading order is the rightmost one, because the page is Arabic.
    let page5 = doc.page(5).expect("page 5");
    let table = page5
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Table(t) => Some(&t.table),
            _ => None,
        })
        .expect("page 5 has a table");

    assert_eq!(table.column_count(), 7);
    assert!(table.confidence > 0.8);

    // The header row, right to left.
    let headers: Vec<&str> = table.rows[1].iter().map(|c| c.text.as_str()).collect();
    assert_eq!(headers[0], "المحافظات", "column 0 should be the rightmost");
    assert_eq!(headers[1], "ذكور");

    // A data row, with its numbers in the right cells.
    let muscat = table
        .rows
        .iter()
        .find(|r| r.first().is_some_and(|c| c.text == "مسقط"))
        .expect("Muscat row");
    assert_eq!(muscat[1].text, "3,313");
    assert_eq!(muscat[3].text, "5,263");
}

#[test]
fn no_table_is_invented_in_the_untagged_corpus() {
    use qalam_core::Block;

    // The other fixture has no tables at all — only decorative frames, one of
    // which is a rounded rectangle drawn twice and geometrically identical to
    // a 3x3 grid. Reading it as one shredded a paragraph into empty cells.
    let Some(doc) = fixture() else { return };

    let tables = doc
        .pages()
        .iter()
        .flat_map(|p| &p.blocks)
        .filter(|b| matches!(b, Block::Table(_)))
        .count();

    assert_eq!(tables, 0, "a decorative frame was read as a table");
}

#[test]
fn multi_character_ligatures_keep_their_order() {
    // `bar_Persons.pdf` maps 14 codes to several base letters at once — `لم`,
    // `لج`, `بح`. Those values are already logical, so the line-level reversal
    // must not reach inside them. It used to, turning `المعظم` into `املعظم`.
    let Some(doc) = open(TABLES_FIXTURE) else {
        return;
    };
    // This document is justified with **kashida**: U+0640 tatweel is inserted
    // between letters to stretch a word to the margin, so `المعظم` is stored as
    // `المعظــم`. That is real content — it is in the file and we extract it
    // faithfully — but it is decoration for this assertion, so strip it here
    // rather than making the extractor lossy.
    let text: String = doc
        .page(3)
        .expect("page 3")
        .text()
        .chars()
        .filter(|c| *c != '\u{0640}')
        .collect();

    for (correct, corrupted) in [("الجلال", "اجلالل"), ("المعظم", "املعظم"), ("بحياة", "حبياة")]
    {
        assert!(
            text.contains(correct),
            "expected {correct:?} — the ligature order regressed"
        );
        assert!(
            !text.contains(corrupted),
            "found the swapped form {corrupted:?}"
        );
    }
}

#[test]
fn text_inside_form_xobjects_is_found() {
    // A form XObject is a page within a page: its own content stream, its own
    // resources, its own fonts under its own names. Page 1 of the Arabic
    // corpus hides its title in one, and an interpreter that does not descend
    // into it loses the text with no hint that anything was missed.
    let Some(doc) = fixture() else { return };
    let text = doc.page(1).expect("page 1").text();

    for expected in ["صياغة مقترح تشريعي", "لنظام أو لائحة"] {
        assert!(
            text.contains(expected),
            "form XObject text is missing: {expected}"
        );
    }

    // And it must decode, not arrive as replacement characters. A form's fonts
    // are named `Fm1/C2_0`; a summary the interpreter cannot find is read as a
    // single-byte font, which splits every 2-byte CID in half.
    assert!(
        !text.contains(char::REPLACEMENT_CHARACTER),
        "form text did not decode: {text}"
    );
}

#[test]
fn character_spacing_does_not_leak_past_a_restore() {
    // `1.pdf` sets `-4.02 Tc` inside a `q … Q` block. Leaking it advanced every
    // later glyph 4pt too little, so the computed positions collapsed into each
    // other and text sorted by position came out shuffled:
    //
    //   before:  الشركةع البرو طةين– ات رنساشن وايلن
    //   after:   الشركة عبر الوطنية – ترانس ناشيونال
    //
    // Every character was present and correct; only their order was wrong,
    // which is the kind of failure that reads as a bad extractor rather than a
    // bug (PLAN.md §10.17).
    let Some(doc) = open(SPACING_FIXTURE) else {
        return;
    };
    let text = doc.page(1).expect("page 1").text();

    for expected in [
        "الشركة عبر الوطنية",
        "ترانس ناشيونال",
        "الشركة متعددة الجنسيات",
    ] {
        assert!(text.contains(expected), "expected {expected:?} in: {text}");
    }
}

#[test]
fn letters_drawn_on_top_of_their_neighbour_land_in_the_right_place() {
    // `3.pdf` draws the `ز` of `ميزات` and the `ر` of `المشروع` with **zero
    // advance**, positioned inside the adjacent glyph's ink and nearly 4pt
    // above the baseline. Ordered by their own coordinates they came out as
    // `م زيات` and `المرشوع`, and one was thrown onto a line of its own:
    //
    //   before:  زر
    //            الغرض من هذا المستند هو تحديد مي ات المشوع، …
    //   after:   الغرض من هذا المستند هو تحديد ميزات المشروع، …
    //
    // A glyph that does not move the pen cannot be ordered by where its ink
    // lands; it belongs with the glyph it was drawn over (PLAN.md §10.18).
    let Some(doc) = open(OVERLAY_FIXTURE) else {
        return;
    };
    let text = doc.page(3).expect("page 3").text();

    for expected in ["ميزات", "المشروع"] {
        assert!(text.contains(expected), "expected {expected:?} in: {text}");
    }
    for corrupted in ["مي ات", "المشوع", "م زيات", "المرشوع"] {
        assert!(
            !text.contains(corrupted),
            "found the corrupted form {corrupted:?}"
        );
    }
}

#[test]
fn glyphs_the_font_declares_meaningless_are_not_emitted() {
    // `8.pdf` renders some Arabic letters in two pieces and maps the second to
    // U+FFFD in its own `/ToUnicode`:
    //
    //     <B0> <0634>   ش
    //     <FB> <FFFD>   the rest of it
    //
    // That is the producer stating the glyph carries no text — the opposite of
    // *our* U+FFFD, which means we failed to read something. Emitting it broke
    // words apart: `التشريعية` came out `الت�شريعية` (PLAN.md §10.20).
    let Some(doc) = open(NO_TEXT_GLYPH_FIXTURE) else {
        return;
    };

    let page5 = doc.page(5).expect("page 5").text();
    for expected in ["التشريعية", "المستويات", "المرسوم", "السلطاني", "بإصدار"]
    {
        assert!(page5.contains(expected), "expected {expected:?} in page 5");
    }

    // The whole document should carry only a handful, where the font really
    // does leave a letter unreadable rather than declaring it blank.
    let remaining: usize = doc
        .pages()
        .iter()
        .map(|p| p.text().matches(char::REPLACEMENT_CHARACTER).count())
        .sum();
    assert!(remaining < 20, "{remaining} replacement characters remain");
}

#[test]
fn dropping_a_meaningless_glyph_also_repairs_the_order() {
    // The same glyphs were breaking the *sequence*, not only inserting a
    // character: `يؤكد` extracted as `ي�كؤد`, with the `ؤ` and `ك` swapped
    // around the intruder. Removing it put them back.
    let Some(doc) = open(NO_TEXT_GLYPH_FIXTURE) else {
        return;
    };
    let text = doc.page(10).expect("page 10").text();

    assert!(text.contains("يؤكد"), "expected `يؤكد` in page 10");
    assert!(!text.contains("كؤد"), "the letters are still transposed");
}

#[test]
fn a_hamza_painted_over_its_letter_is_not_a_second_letter() {
    // `8.pdf` draws `إ` as a zero-advance overlay on the glyph carrying the
    // alef — and sometimes on a `لإ` ligature that already includes it. Both
    // carry Unicode, so emitting both doubled the hamza: `والإدارية` came out
    // `والإإدارية`, 277 times across the document (PLAN.md §10.21).
    let Some(doc) = open(NO_TEXT_GLYPH_FIXTURE) else {
        return;
    };

    let doubled: usize = doc
        .pages()
        .iter()
        .map(|p| {
            let t = p.text();
            ["أأ", "إإ", "آآ", "ؤؤ", "ئئ"]
                .iter()
                .map(|pair| t.matches(pair).count())
                .sum::<usize>()
        })
        .sum();
    assert_eq!(doubled, 0, "{doubled} doubled hamzas remain");

    assert!(doc.page(5).expect("page 5").text().contains("والإدارية"));
    assert!(doc.page(6).expect("page 6").text().contains("الأدبية"));
}

#[test]
fn tight_tracking_does_not_split_every_letter() {
    // `12.pdf` sets `-0.75 Tc` against a `Tf` of 1, compensating with large
    // `TJ` kerns. The positions were right all along; the *reported* advance
    // included `Tc` and so went negative, and the word-gap rule then fired
    // between every pair of letters:
    //
    //   before:  ت ق ر ی ر ا ل م ر ا ج ع ا ل م س ت ق ل
    //   after:   تقریر المراجع المستقل
    //
    // `Tc` is spacing between glyphs, not part of one (PLAN.md §10.22).
    let Some(doc) = open(TIGHT_TRACKING_FIXTURE) else {
        return;
    };

    for number in 4..=11 {
        let text = doc.page(number).expect("page exists").text();
        // A page split letter-by-letter is more than half spaces.
        let spaces = text.chars().filter(|c| *c == ' ').count();
        let total = text.chars().filter(|c| !c.is_whitespace()).count().max(1);
        assert!(
            spaces * 2 < total,
            "page {number} is {spaces} spaces to {total} characters — still split"
        );
    }

    assert!(doc
        .page(5)
        .expect("page 5")
        .text()
        .contains("تقریر المراجع المستقل"));
}

#[test]
fn a_table_with_no_ruling_is_still_a_table() {
    // Page 12 of `12.pdf` is a financial statement drawn with no lines at all:
    // six columns held together by alignment alone. The XY-cut separated three
    // of them and merged the rest, so `1,653,281 1,637,299 24` arrived as one
    // line (PLAN.md §10.23).
    let Some(doc) = open(TIGHT_TRACKING_FIXTURE) else {
        return;
    };
    let page = doc.page(12).expect("page 12");

    let table = page
        .blocks
        .iter()
        .find_map(|b| match b {
            qalam_core::Block::Table(t) => Some(&t.table),
            _ => None,
        })
        .expect("page 12 has a table");

    assert_eq!(table.column_count(), 6);
    assert!(table.row_count() > 20);
    // Inferred, so it must not claim a ruled grid's confidence.
    assert!(table.confidence < 1.0);

    // A data row, right to left: label, note, then two currencies of two years.
    let row = table
        .rows
        .iter()
        .find(|r| r.first().is_some_and(|c| c.text == "إيرادات"))
        .expect("the revenue row");
    let cells: Vec<&str> = row.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        cells,
        [
            "إيرادات",
            "24",
            "1,637,299",
            "1,653,281",
            "436,613",
            "440,875"
        ]
    );
}

#[test]
fn the_same_table_is_found_with_and_without_ruling() {
    // `21.pdf` and `22.pdf` are the same document twice: one draws its table
    // borders, the other does not. The ruled one always worked; the borderless
    // one produced nothing at all, because its row numbers sit about 3pt below
    // the rest of their row and split every row in two (PLAN.md §10.24).
    //
    // No goldens for these two — they are large, and what matters is the
    // structure rather than the exact bytes.
    let count_tables = |path: &str| -> Option<(usize, usize, usize)> {
        let doc = open(path)?;
        let tables: Vec<&qalam_core::TableBlock> = doc
            .pages()
            .iter()
            .flat_map(|p| &p.blocks)
            .filter_map(|b| match b {
                qalam_core::Block::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        let widest = tables
            .iter()
            .map(|t| t.table.column_count())
            .max()
            .unwrap_or(0);
        Some((doc.page_count(), tables.len(), widest))
    };

    if let Some((pages, tables, columns)) = count_tables("../../tests/fixtures/21.pdf") {
        assert_eq!(tables, pages, "the ruled document: one table per page");
        assert!(columns >= 8, "the ruled document lost columns");
    }

    if let Some((pages, tables, columns)) = count_tables("../../tests/fixtures/22.pdf") {
        assert_eq!(tables, pages, "the borderless document: one table per page");
        assert!(columns >= 8, "the borderless document lost columns");
    }
}
