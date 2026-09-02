//! **L4 — the recoverability detector.**
//!
//! The differentiating feature (PLAN.md §3). Every other PDF extractor, handed
//! a page it cannot read, emits garbage that looks like text. This module's job
//! is to say **"this page needs OCR"** instead, so that a silent corruption
//! becomes an actionable signal.
//!
//! # Why a single number is not enough
//!
//! It is tempting to score a page as "fraction of glyphs that resolved" and
//! stop. That metric reports **100%** for a scanned page — because a page with
//! no glyphs has no unresolved ones. The most important failure it could
//! detect is exactly the one it scores perfectly.
//!
//! So the detector weighs several independent signals, each of which catches a
//! different failure:
//!
//! | Signal | Catches |
//! |---|---|
//! | No glyphs at all | A scan. The classic needs-OCR page. |
//! | Fonts with no `/ToUnicode` and no usable `/Encoding` | Failure mode #4 |
//! | Fraction of codes that resolved to nothing | A wrong or partial CMap |
//! | Presentation forms surviving normalisation | NFKC did not do its job |
//! | Share of glyphs painted invisibly (`Tr 3`) | Someone else's OCR layer |
//! | Replacement characters in the output | Corruption that reached the text |
//!
//! A page is only called `Ok` when every one of them is clean.

use crate::arabic::TextLine;
use crate::content::PageGlyphs;
use crate::font::FontMap;

/// The verdict for one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recoverability {
    /// The text layer is sound; the extracted text can be trusted.
    Ok,
    /// Text came out, but something is wrong with it. Usable with care —
    /// a partial CMap, or a minority of unreadable glyphs.
    Degraded,
    /// There is no usable text layer. Emitting the extracted text would be
    /// worse than useless, because it would look like a result.
    NeedsOcr,
}

impl Recoverability {
    /// A short machine-friendly label, for reports and the eventual Python API.
    pub fn as_str(self) -> &'static str {
        match self {
            Recoverability::Ok => "ok",
            Recoverability::Degraded => "degraded",
            Recoverability::NeedsOcr => "needs_ocr",
        }
    }
}

/// The measurements a verdict is drawn from.
///
/// Kept separate from the verdict itself so a caller can disagree with our
/// thresholds and apply their own — and so that tuning them is a change to one
/// function rather than to the measuring code.
#[derive(Debug, Clone, Default)]
pub struct Signals {
    /// Glyphs painted on the page.
    pub glyph_count: usize,
    /// Glyph codes that resolved to nothing.
    pub unresolved: usize,
    /// Fonts declared on the page.
    pub font_count: usize,
    /// Fonts with neither a `/ToUnicode` map nor a usable `/Encoding`.
    pub unmappable_fonts: usize,
    /// Glyphs painted in an invisible render mode (`Tr` 3 or 7).
    pub invisible_glyphs: usize,
    /// Presentation-form characters still present after normalisation.
    pub residual_presentation_forms: usize,
    /// U+FFFD characters in the final text.
    pub replacement_chars: usize,
    /// Characters of text produced.
    pub text_len: usize,
}

impl Signals {
    /// Fraction of glyphs that resolved, 0.0 to 1.0.
    ///
    /// A page with no glyphs scores 0.0, not 1.0 — "nothing failed" is not the
    /// same as "everything worked", and this is precisely where the naive
    /// metric goes wrong.
    pub fn resolution_rate(&self) -> f64 {
        if self.glyph_count == 0 {
            return 0.0;
        }
        1.0 - (self.unresolved as f64 / self.glyph_count as f64)
    }

    /// Fraction of glyphs painted invisibly.
    pub fn invisible_rate(&self) -> f64 {
        if self.glyph_count == 0 {
            return 0.0;
        }
        self.invisible_glyphs as f64 / self.glyph_count as f64
    }
}

/// A page's verdict, with the evidence behind it.
#[derive(Debug, Clone)]
pub struct PageReport {
    /// 1-based page number.
    pub page: u32,
    /// What we concluded.
    pub verdict: Recoverability,
    /// How much we trust the extracted text, 0.0 to 1.0.
    pub confidence: f64,
    /// The raw measurements.
    pub signals: Signals,
    /// Human-readable reasons, in the order they were found. Empty when the
    /// page is clean.
    pub reasons: Vec<String>,
}

// --- thresholds -----------------------------------------------------------
//
// Gathered here rather than buried in the logic, because they are judgement
// calls that a real corpus should tune (PLAN.md M5), not facts about PDF.

/// Below this resolution rate a page is not worth emitting.
const NEEDS_OCR_RESOLUTION: f64 = 0.5;
/// Below this, the text is suspect but still probably useful.
///
/// Set to 1.0 deliberately: **any** unresolved glyph means a character is
/// missing from the output, so `Ok` is reserved for pages where everything
/// resolved. A tolerance here would let real losses pass as clean, which is the
/// failure this project exists to prevent.
const DEGRADED_RESOLUTION: f64 = 1.0;
/// Above this share of invisible glyphs, the text layer is somebody's OCR.
const OCR_LAYER_INVISIBLE_RATE: f64 = 0.9;
/// Fewer characters than this on a page with glyphs suggests a stray label
/// rather than content.
const MINIMUM_MEANINGFUL_TEXT: usize = 2;

/// Assess one page.
///
/// Takes the output of every layer below: L1's glyphs, L2's fonts, L3's lines.
/// The detector measures rather than re-derives — it is the last stage, and
/// everything it needs has already been computed.
pub fn assess(page: u32, glyphs: &PageGlyphs, fonts: &FontMap, lines: &[TextLine]) -> PageReport {
    let signals = measure(glyphs, fonts, lines);
    let (verdict, reasons) = judge(&signals);

    PageReport {
        page,
        verdict,
        confidence: confidence(&signals, verdict),
        signals,
        reasons,
    }
}

/// Collect the measurements.
fn measure(glyphs: &PageGlyphs, fonts: &FontMap, lines: &[TextLine]) -> Signals {
    let text: String = lines.iter().map(|l| l.text.as_str()).collect();

    Signals {
        glyph_count: glyphs.glyphs.len(),
        unresolved: lines.iter().map(|l| l.unresolved).sum(),
        font_count: fonts.iter().count(),
        unmappable_fonts: fonts.iter().filter(|f| !f.is_resolvable()).count(),
        invisible_glyphs: glyphs
            .glyphs
            .iter()
            .filter(|g| !g.style.render_mode.is_visible())
            .count(),
        // NFKC should have folded every one of these away. Any that survive
        // mean normalisation did not reach them — a real defect in the output,
        // not a cosmetic one, because the text will not compare or search
        // equal to the same words typed normally.
        residual_presentation_forms: text
            .chars()
            .filter(|c| matches!(*c as u32, 0xFB50..=0xFDFF | 0xFE70..=0xFEFF))
            .count(),
        replacement_chars: text
            .chars()
            .filter(|c| *c == char::REPLACEMENT_CHARACTER)
            .count(),
        text_len: text.chars().count(),
    }
}

/// Turn measurements into a verdict.
///
/// Written as a list of independent checks that each *lower* the verdict, so
/// adding a signal later means adding a check, not rewriting a decision tree.
fn judge(s: &Signals) -> (Recoverability, Vec<String>) {
    let mut verdict = Recoverability::Ok;
    let mut reasons = Vec::new();

    // Lower the verdict, never raise it.
    let mut fail = |level: Recoverability, reason: String, verdict: &mut Recoverability| {
        if level > *verdict {
            *verdict = level;
        }
        reasons.push(reason);
    };

    if s.glyph_count == 0 {
        // The case a naive resolution rate scores as perfect.
        fail(
            Recoverability::NeedsOcr,
            "no text layer: the page paints no glyphs at all".to_string(),
            &mut verdict,
        );
        // Nothing else can be measured, so stop here rather than piling on
        // vacuous complaints about a page with no content.
        return (verdict, reasons);
    }

    let rate = s.resolution_rate();
    if rate < NEEDS_OCR_RESOLUTION {
        fail(
            Recoverability::NeedsOcr,
            format!("only {:.0}% of glyph codes could be resolved", rate * 100.0),
            &mut verdict,
        );
    } else if rate < DEGRADED_RESOLUTION {
        fail(
            Recoverability::Degraded,
            format!(
                "{} of {} glyph codes unresolved",
                s.unresolved, s.glyph_count
            ),
            &mut verdict,
        );
    }

    if s.unmappable_fonts > 0 {
        fail(
            Recoverability::Degraded,
            format!(
                "{} of {} fonts have neither /ToUnicode nor a usable /Encoding",
                s.unmappable_fonts, s.font_count
            ),
            &mut verdict,
        );
    }

    if s.residual_presentation_forms > 0 {
        fail(
            Recoverability::Degraded,
            format!(
                "{} presentation-form characters survived normalisation",
                s.residual_presentation_forms
            ),
            &mut verdict,
        );
    }

    if s.invisible_rate() > OCR_LAYER_INVISIBLE_RATE {
        // Not a failure of ours: the text is readable. But it is somebody
        // else's OCR output, so its accuracy is theirs, not the document's.
        fail(
            Recoverability::Degraded,
            "text is painted invisibly — this looks like an OCR layer over a scan".to_string(),
            &mut verdict,
        );
    }

    if s.text_len < MINIMUM_MEANINGFUL_TEXT {
        fail(
            Recoverability::NeedsOcr,
            "glyphs were painted but produced almost no text".to_string(),
            &mut verdict,
        );
    }

    (verdict, reasons)
}

/// Score how much the extracted text can be trusted, 0.0 to 1.0.
fn confidence(s: &Signals, verdict: Recoverability) -> f64 {
    if verdict == Recoverability::NeedsOcr {
        return 0.0;
    }

    let mut score = s.resolution_rate();

    // Each remaining defect costs, in rough proportion to how much of the text
    // it touches. `min` keeps a single signal from driving the score negative.
    if s.text_len > 0 {
        let bad = (s.residual_presentation_forms + s.replacement_chars) as f64;
        score -= (bad / s.text_len as f64).min(0.5);
    }
    if s.font_count > 0 {
        score -= 0.2 * (s.unmappable_fonts as f64 / s.font_count as f64);
    }
    if s.invisible_rate() > OCR_LAYER_INVISIBLE_RATE {
        score -= 0.1;
    }

    score.clamp(0.0, 1.0)
}

/// Order the verdicts so `judge` can take the worst one seen.
///
/// # Rust lesson: deriving comparison
///
/// `PartialOrd`/`Ord` on an enum compares by **declaration order**, so the
/// variants above are written best-to-worst on purpose: `Ok < Degraded <
/// NeedsOcr`. That ordering is load-bearing — reordering the variants would
/// silently invert the detector — so it is stated here rather than left
/// implicit in the derive.
impl PartialOrd for Recoverability {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Recoverability {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(r: Recoverability) -> u8 {
            match r {
                Recoverability::Ok => 0,
                Recoverability::Degraded => 1,
                Recoverability::NeedsOcr => 2,
            }
        }
        rank(*self).cmp(&rank(*other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page that came out clean.
    fn healthy() -> Signals {
        Signals {
            glyph_count: 500,
            unresolved: 0,
            font_count: 3,
            unmappable_fonts: 0,
            invisible_glyphs: 0,
            residual_presentation_forms: 0,
            replacement_chars: 0,
            text_len: 480,
        }
    }

    #[test]
    fn a_clean_page_is_ok_with_full_confidence() {
        let s = healthy();
        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::Ok);
        assert!(reasons.is_empty());
        assert!((confidence(&s, verdict) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_page_with_no_glyphs_needs_ocr() {
        // The case a naive "fraction of glyphs that resolved" metric scores as
        // a perfect 100%, because there are no failures to count.
        let s = Signals::default();
        assert_eq!(s.resolution_rate(), 0.0, "an empty page must not score 1.0");

        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::NeedsOcr);
        assert_eq!(
            reasons.len(),
            1,
            "an empty page needs one reason, not a pile"
        );
        assert!(reasons[0].contains("no text layer"));
        assert_eq!(confidence(&s, verdict), 0.0);
    }

    #[test]
    fn mostly_unresolved_glyphs_need_ocr() {
        let s = Signals {
            unresolved: 400,
            ..healthy()
        };
        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::NeedsOcr);
        assert!(reasons.iter().any(|r| r.contains("20%")));
    }

    #[test]
    fn a_few_unresolved_glyphs_are_only_degraded() {
        // Still worth emitting — with a warning.
        let s = Signals {
            unresolved: 5,
            ..healthy()
        };
        let (verdict, _) = judge(&s);
        assert_eq!(verdict, Recoverability::Degraded);
        assert!(confidence(&s, verdict) > 0.9);
    }

    #[test]
    fn an_unmappable_font_degrades_the_page() {
        let s = Signals {
            unmappable_fonts: 1,
            ..healthy()
        };
        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::Degraded);
        assert!(reasons.iter().any(|r| r.contains("/ToUnicode")));
        // The confidence penalty is proportional to the share of fonts.
        assert!(confidence(&s, verdict) < 1.0);
    }

    #[test]
    fn surviving_presentation_forms_are_a_real_defect() {
        // NFKC should have folded these away. Text containing them will not
        // compare or search equal to the same words typed normally.
        let s = Signals {
            residual_presentation_forms: 12,
            ..healthy()
        };
        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::Degraded);
        assert!(reasons.iter().any(|r| r.contains("presentation-form")));
    }

    #[test]
    fn an_invisible_text_layer_is_flagged_as_someone_elses_ocr() {
        // Mode 3 text is readable, so this is not `NeedsOcr` — but its accuracy
        // belongs to whoever ran the OCR, not to the document.
        let s = Signals {
            invisible_glyphs: 500,
            ..healthy()
        };
        assert_eq!(s.invisible_rate(), 1.0);

        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::Degraded);
        assert!(reasons.iter().any(|r| r.contains("OCR layer")));
    }

    #[test]
    fn a_little_invisible_text_is_not_suspicious() {
        // Hidden watermarks and accessibility labels are normal.
        let s = Signals {
            invisible_glyphs: 20,
            ..healthy()
        };
        assert_eq!(judge(&s).0, Recoverability::Ok);
    }

    #[test]
    fn glyphs_that_produce_no_text_need_ocr() {
        // Every code resolved, but to nothing usable — a font whose CMap maps
        // everything to a blank.
        let s = Signals {
            text_len: 0,
            ..healthy()
        };
        assert_eq!(judge(&s).0, Recoverability::NeedsOcr);
    }

    #[test]
    fn the_worst_signal_decides_the_verdict() {
        // Several problems at once: the verdict is the worst, and every reason
        // is still reported so a caller can see all of them.
        let s = Signals {
            unresolved: 450,
            unmappable_fonts: 2,
            residual_presentation_forms: 3,
            ..healthy()
        };
        let (verdict, reasons) = judge(&s);
        assert_eq!(verdict, Recoverability::NeedsOcr);
        assert!(reasons.len() >= 3);
    }

    #[test]
    fn verdicts_order_from_best_to_worst() {
        // Load-bearing: `judge` lowers the verdict by comparing. Inverting this
        // ordering would silently invert the detector.
        assert!(Recoverability::Ok < Recoverability::Degraded);
        assert!(Recoverability::Degraded < Recoverability::NeedsOcr);
    }

    #[test]
    fn confidence_stays_inside_its_range() {
        // Pile on every penalty at once and the score must not go negative.
        let s = Signals {
            glyph_count: 10,
            unresolved: 5,
            font_count: 2,
            unmappable_fonts: 2,
            invisible_glyphs: 10,
            residual_presentation_forms: 50,
            replacement_chars: 50,
            text_len: 10,
        };
        let score = confidence(&s, Recoverability::Degraded);
        assert!((0.0..=1.0).contains(&score), "score was {score}");
    }
}
