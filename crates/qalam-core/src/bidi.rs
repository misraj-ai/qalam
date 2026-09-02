//! **L3 (part 1) — bidirectional reordering.**
//!
//! A PDF paints glyphs left to right in *visual* order. Arabic reads right to
//! left. So the string L2 hands us is the line as the eye scans it, which for
//! Arabic is backwards from how it is read, stored, or searched. Undoing that
//! is failure mode #2 in PLAN.md §2.
//!
//! # We are running the algorithm backwards
//!
//! The Unicode Bidirectional Algorithm (UAX #9) is defined in one direction:
//! **logical → visual**. It is what a text renderer runs. We have the visual
//! order and want the logical order, which is the inverse problem.
//!
//! The practical answer — used by essentially every PDF extractor — is to run
//! the forward algorithm *on the visual string*. For a line that is entirely
//! one direction this is exact, because the reordering is then a simple
//! reversal and applying it twice is the identity:
//!
//! ```text
//!   logical  هذا الدليل        (reads right to left)
//!   visual   ﻞﻴﻟﺪﻟا اﺬﻫ        (painted left to right — what the PDF gives us)
//!   reorder  هذا الدليل        (running UAX #9 on the visual string)
//! ```
//!
//! It is **not** exact for mixed-direction lines. An Arabic sentence containing
//! a Latin phrase or a number has nested embedding levels, and the forward
//! algorithm is not a perfect involution across them: the neutral characters
//! between the runs (spaces, punctuation) can resolve to a different level on
//! the way back. In practice the runs themselves land correctly and only their
//! separators may drift. PLAN.md §8 lists this as an open risk with its own
//! fixtures; this module is where any fix would go.
//!
//! # Why this must run *before* normalisation
//!
//! This is the single ordering rule the whole project hangs on. A lam-alef
//! ligature `ﻻ` (U+FEFB) is **one** character here. Reorder first and it moves
//! as a unit, then NFKC expands it into `ل` + `ا` already in the right order.
//! Normalise first and you get two characters that the reorder then reverses,
//! turning `ولا` into `وال`. See PLAN.md §3 and the regression in §10.1.

use unicode_bidi::{BidiInfo, Level};

/// The base direction of a line of text.
///
/// UAX #9 calls this the *paragraph level*. It decides how neutral characters —
/// spaces, punctuation, digits — line up around the directional runs, so
/// choosing it wrongly leaves a trailing full stop on the wrong end of the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Left-to-right base direction: Latin, digits.
    Ltr,
    /// Right-to-left base direction: Arabic, Hebrew.
    Rtl,
}

impl Direction {
    /// Translate to the `unicode-bidi` paragraph level: 0 is LTR, 1 is RTL.
    fn level(self) -> Level {
        match self {
            Direction::Ltr => Level::ltr(),
            Direction::Rtl => Level::rtl(),
        }
    }
}

/// Guess a line's base direction from the characters in it.
///
/// The rule is deliberately blunt: **any** strong RTL character makes the line
/// RTL. UAX #9's own rule (P2/P3) uses the *first* strong character, but that
/// misfires constantly on extracted PDF text, where a line of Arabic often
/// begins with a bullet, a digit, or a Latin acronym. For a document that is
/// Arabic throughout, "contains Arabic ⇒ RTL" is both simpler and more often
/// right.
pub fn detect_direction(text: &str) -> Direction {
    if text.chars().any(is_rtl_char) {
        Direction::Rtl
    } else {
        Direction::Ltr
    }
}

/// Is this a strong right-to-left character?
///
/// Covers the Arabic and Hebrew blocks plus the two **presentation form**
/// blocks. Including the presentation forms matters enormously here: at this
/// point in the pipeline the text has not been normalised yet, so most Arabic
/// letters are still `U+FExx` shaped glyphs rather than `U+06xx` base letters.
/// Checking only the base ranges would classify a whole page of Arabic as LTR
/// and reorder nothing.
pub fn is_rtl_char(c: char) -> bool {
    matches!(c as u32,
        // Hebrew.
        0x0590..=0x05FF
        // Arabic, Arabic Supplement, Thaana, Arabic Extended-A.
        | 0x0600..=0x07BF
        | 0x0860..=0x08FF
        // Arabic Presentation Forms-A (includes many ligatures).
        | 0xFB50..=0xFDFF
        // Arabic Presentation Forms-B (the shaped letters, and lam-alef).
        | 0xFE70..=0xFEFF
    )
}

/// Convert one line from visual order to logical order.
///
/// The input is what the PDF painted, left to right. The output is what a
/// person reads and what should be stored, searched and copied.
///
/// Characters are **not** changed here, only moved: presentation forms and
/// ligatures come out exactly as they went in. Turning them into base letters
/// is `arabic.rs`'s job, and it must happen after this.
pub fn visual_to_logical(visual: &str, base: Direction) -> String {
    // A line with nothing directional in it cannot be reordered wrongly, and
    // skipping the work avoids allocating for the very common blank line.
    if visual.is_empty() {
        return String::new();
    }

    let info = BidiInfo::new(visual, Some(base.level()));

    // `BidiInfo` splits on paragraph separators. A single extracted line is
    // normally one paragraph, but a stray U+2029 would produce more, so we
    // reorder each and join rather than assuming there is exactly one.
    let mut out = String::with_capacity(visual.len());
    for para in &info.paragraphs {
        // `reorder_line` returns a `Cow<str>`: borrowed when nothing moved,
        // owned when it did. Borrowing in the common LTR case means no
        // allocation at all.
        out.push_str(&info.reorder_line(para, para.range.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_rtl_reordering_is_its_own_inverse() {
        // Three Arabic base letters. Running the algorithm twice must return
        // the original, which is what makes running it forwards on visual
        // input give us logical output.
        let logical = "\u{0627}\u{0628}\u{062A}";
        let once = visual_to_logical(logical, Direction::Rtl);
        let twice = visual_to_logical(&once, Direction::Rtl);
        assert_ne!(once, logical);
        assert_eq!(twice, logical);
    }

    #[test]
    fn ltr_text_is_left_alone() {
        assert_eq!(visual_to_logical("hello", Direction::Ltr), "hello");
    }

    #[test]
    fn presentation_forms_count_as_rtl() {
        // The critical case: before normalisation, Arabic is U+FExx, not
        // U+06xx. Missing this would leave every page classified LTR.
        assert!(is_rtl_char('\u{FE8E}')); // alef final
        assert!(is_rtl_char('\u{FEFB}')); // lam-alef ligature
        assert!(is_rtl_char('\u{0627}')); // alef, base form
        assert!(!is_rtl_char('A'));
        assert!(!is_rtl_char('1'));

        assert_eq!(detect_direction("\u{FE8E}\u{FEDF}"), Direction::Rtl);
        assert_eq!(detect_direction("page 1"), Direction::Ltr);
    }

    #[test]
    fn a_line_that_merely_contains_arabic_is_rtl() {
        // Our blunt rule: a leading digit or bullet must not flip the line to
        // LTR, which UAX #9's own first-strong-character rule would do.
        assert_eq!(detect_direction("1. \u{0627}\u{0628}"), Direction::Rtl);
    }

    #[test]
    fn the_ligature_stays_one_character_through_reordering() {
        // The whole reason this runs before NFKC. U+FEFB must survive the
        // reorder intact so that expanding it afterwards yields `ل` then `ا`.
        let visual = "\u{FEFB}\u{FEEE}"; // lam-alef, then waw-final
        let logical = visual_to_logical(visual, Direction::Rtl);
        assert!(logical.contains('\u{FEFB}'));
        assert_eq!(logical.chars().count(), 2);
        // Reversed, so the waw now comes first — `و` then `لا`.
        assert_eq!(logical.chars().next(), Some('\u{FEEE}'));
    }

    #[test]
    fn empty_input_is_handled() {
        assert_eq!(visual_to_logical("", Direction::Rtl), "");
    }
}
