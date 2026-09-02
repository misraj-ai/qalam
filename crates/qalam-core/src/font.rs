//! **L2 — fonts: turning glyph codes into characters.**
//!
//! L1 handed us numbers. `0x0113` is not a letter; it is an index into a
//! particular subset font, and the *only* thing that can say what it means is
//! that font's `/ToUnicode` map. This module parses that map.
//!
//! It is also where the honest failure lives. If a font has no `/ToUnicode` and
//! no usable `/Encoding`, its codes are unrecoverable by parsing alone — that is
//! failure mode #4 in PLAN.md §2, and the boundary where "no OCR" genuinely
//! cannot win. We report it rather than inventing characters.
//!
//! # The CMap grammar
//!
//! `/ToUnicode` is a stream in a PostScript-flavoured language, but only a few
//! constructs matter (PLAN.md §10.2):
//!
//! ```text
//! 1 begincodespacerange
//! <0000> <FFFF>              ← 2-byte codes
//! endcodespacerange
//! 100 beginbfchar
//! <0003> <0020>              ← CID 3 is a space
//! <0113> <0627>              ← alef, a base letter
//! <0114> <FE8E>              ← alef-final, a *presentation form*
//! endbfchar
//! 2 beginbfrange
//! <0100> <0105> <0041>       ← a run: 0100→A, 0101→B, ...
//! <0200> <0201> [<0041> <0042>]
//! endbfrange
//! ```
//!
//! Two properties of real maps drive the design:
//!
//! - **A destination is a string, not a character.** One CID can map to several
//!   codepoints — PLAN.md §10.1 found `01AE → U+0651 U+064B` (shadda +
//!   fathatan). A `HashMap<u32, char>` would silently truncate it.
//! - **The map is a mixture.** The same font maps `0113` to a base letter and
//!   `0114` to a presentation form. That is exactly why NFKC is mandatory later,
//!   and why this layer must not try to normalise anything itself.
//!
//! Destinations are UTF-16BE, so a codepoint above U+FFFF arrives as a surrogate
//! pair that has to be recombined.

use std::collections::HashMap;

use crate::content::GlyphWidths;
use crate::encoding::Encoding;
use crate::types::{CodeToUnicode, RawFont};

/// A parsed `/ToUnicode` CMap: glyph code → the text it stands for.
///
/// # Rust lesson: why two containers
///
/// Single mappings go in a hash map, but ranges are kept as ranges. A CMap may
/// legally contain `<0000> <FFFF>`, and expanding that into a map would
/// materialise 65,536 entries for a font that uses a dozen. Keeping ranges
/// intact costs one short linear scan on lookup and bounds the memory.
#[derive(Debug, Default, Clone)]
pub struct CMap {
    /// How many bytes make up one code, from `begincodespacerange`.
    /// 2 for `Identity-H`, 1 for a simple font.
    code_bytes: usize,
    /// `beginbfchar` entries: one code, one destination string.
    single: HashMap<u32, String>,
    /// `beginbfrange` entries, unexpanded.
    ranges: Vec<BfRange>,
}

/// One `beginbfrange` entry, covering the inclusive code range `lo..=hi`.
#[derive(Debug, Clone)]
struct BfRange {
    lo: u32,
    hi: u32,
    dst: RangeDst,
}

/// What a range maps to.
#[derive(Debug, Clone)]
enum RangeDst {
    /// `<lo> <hi> <dstStart>` — consecutive codes get consecutive destinations,
    /// incrementing the *last* UTF-16 unit.
    Incrementing(String),
    /// `<lo> <hi> [<d0> <d1> ...]` — one explicit destination per code.
    List(Vec<String>),
}

impl CMap {
    /// Parse a `/ToUnicode` stream.
    ///
    /// Never fails: a CMap we cannot make sense of yields whatever mappings we
    /// did understand. A partial map is strictly better than none, and the
    /// recoverability detector (L4) will notice the gaps by counting how many
    /// codes resolve.
    pub fn parse(bytes: &[u8]) -> Self {
        let tokens = tokenize(bytes);
        let mut cmap = CMap {
            // Default to 2: composite fonts are the common case for Arabic, and
            // an explicit codespacerange almost always follows anyway.
            code_bytes: 2,
            ..Default::default()
        };

        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Word(w) if w == "begincodespacerange" => {
                    i = cmap.read_codespace(&tokens, i + 1);
                }
                Token::Word(w) if w == "beginbfchar" => {
                    i = cmap.read_bfchar(&tokens, i + 1);
                }
                Token::Word(w) if w == "beginbfrange" => {
                    i = cmap.read_bfrange(&tokens, i + 1);
                }
                // Everything else — `/CMapName`, `def`, `begin`, the counts
                // before each section — is scaffolding we do not need.
                _ => i += 1,
            }
        }
        cmap
    }

    /// The code width in bytes, as declared by `begincodespacerange`.
    pub fn code_bytes(&self) -> usize {
        self.code_bytes
    }

    /// How many codes this map can resolve. A recoverability signal for L4.
    pub fn len(&self) -> usize {
        self.single.len()
            + self
                .ranges
                .iter()
                // `saturating_sub` and `+ 1` because the range is inclusive;
                // saturating guards against a malformed `hi < lo`.
                .map(|r| (r.hi.saturating_sub(r.lo) as usize) + 1)
                .sum::<usize>()
    }

    /// `true` if the map resolves nothing at all.
    pub fn is_empty(&self) -> bool {
        self.single.is_empty() && self.ranges.is_empty()
    }

    /// Look up one code.
    ///
    /// Returns the destination *string*, which may be several characters — the
    /// `01AE → U+0651 U+064B` case from PLAN.md §10.1.
    pub fn get(&self, code: u32) -> Option<String> {
        // Single mappings win: a `bfchar` entry is more specific than a range,
        // and writers do use both for the same code.
        if let Some(text) = self.single.get(&code) {
            return Some(text.clone());
        }

        for range in &self.ranges {
            if code < range.lo || code > range.hi {
                continue;
            }
            let offset = (code - range.lo) as usize;
            return match &range.dst {
                RangeDst::List(items) => items.get(offset).cloned(),
                RangeDst::Incrementing(start) => Some(increment_last_unit(start, offset as u32)),
            };
        }
        None
    }

    /// `<lo> <hi>` pairs; we only want the byte width of `lo`.
    fn read_codespace(&mut self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() {
            match &tokens[i] {
                Token::Word(w) if w == "endcodespacerange" => return i + 1,
                Token::Hex(bytes) => {
                    // A code is 1 byte or 2 in practice; anything else is a
                    // file we would misread, so leave the default in place.
                    if (1..=2).contains(&bytes.len()) {
                        self.code_bytes = bytes.len();
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        i
    }

    /// `<src> <dst>` pairs until `endbfchar`.
    fn read_bfchar(&mut self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() {
            match (&tokens[i], tokens.get(i + 1)) {
                (Token::Word(w), _) if w == "endbfchar" => return i + 1,
                (Token::Hex(src), Some(Token::Hex(dst))) => {
                    self.single.insert(code_of(src), utf16be_to_string(dst));
                    i += 2;
                }
                // A malformed pair: skip one token and try to resynchronise
                // rather than abandoning the rest of the section.
                _ => i += 1,
            }
        }
        i
    }

    /// `<lo> <hi> <dst>` or `<lo> <hi> [ ... ]` triples until `endbfrange`.
    fn read_bfrange(&mut self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() {
            if matches!(&tokens[i], Token::Word(w) if w == "endbfrange") {
                return i + 1;
            }

            let (Token::Hex(lo), Some(Token::Hex(hi))) = (&tokens[i], tokens.get(i + 1)) else {
                i += 1;
                continue;
            };
            let (lo, hi) = (code_of(lo), code_of(hi));

            match tokens.get(i + 2) {
                Some(Token::Hex(dst)) => {
                    self.ranges.push(BfRange {
                        lo,
                        hi,
                        dst: RangeDst::Incrementing(utf16be_to_string(dst)),
                    });
                    i += 3;
                }
                Some(Token::ArrayOpen) => {
                    let mut items = Vec::new();
                    let mut j = i + 3;
                    while let Some(token) = tokens.get(j) {
                        match token {
                            Token::Hex(dst) => items.push(utf16be_to_string(dst)),
                            Token::ArrayClose => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    self.ranges.push(BfRange {
                        lo,
                        hi,
                        dst: RangeDst::List(items),
                    });
                    // +1 to step past the closing bracket.
                    i = j + 1;
                }
                _ => i += 2,
            }
        }
        i
    }
}

/// One font, ready to answer the two questions L1 and L3 ask of it:
/// what does this code mean, and how wide is it?
#[derive(Debug, Clone)]
pub struct Font {
    /// The resource name the content stream uses, e.g. `C2_0`.
    pub resource_name: String,
    /// The parsed `/ToUnicode` map, empty when the font has none.
    pub to_unicode: CMap,
    /// The simple-font `/Encoding`, used when `/ToUnicode` cannot answer.
    pub encoding: Encoding,
    /// Whether codes are two bytes wide.
    pub two_byte: bool,
    /// Which reverse-mapping route this font offers (from L0).
    pub route: CodeToUnicode,
    /// `/FirstChar`, the code `widths[0]` describes.
    first_char: u32,
    /// Simple-font `/Widths`, in thousandths of an em.
    widths: Vec<f64>,
    /// Width for codes a simple font's `/Widths` does not cover.
    missing_width: f64,
    /// Composite-font `/DW`.
    default_width: f64,
    /// Composite-font `/W`, as inclusive `(first, last, width)` triples.
    cid_widths: Vec<(u32, u32, f64)>,
}

impl Font {
    /// Build a usable font from L0's raw extraction.
    pub fn from_raw(raw: RawFont) -> Self {
        let two_byte = raw.info.is_two_byte();

        // Parse the CMap if there is one; an absent map is an empty map, so
        // lookups just return `None` without a special case anywhere.
        let to_unicode = raw
            .to_unicode
            .as_deref()
            .map(CMap::parse)
            .unwrap_or_default();

        // A composite font's `/Encoding` names a CMap (`Identity-H`), not a
        // byte table, so the simple-font encoding path does not apply to it.
        let encoding = if two_byte {
            Encoding::default()
        } else {
            Encoding::new(raw.base_encoding.as_deref(), raw.differences)
        };

        Self {
            resource_name: raw.info.resource_name,
            two_byte,
            route: raw.info.code_to_unicode,
            to_unicode,
            encoding,
            first_char: raw.first_char,
            widths: raw.widths,
            missing_width: raw.missing_width,
            default_width: raw.default_width,
            cid_widths: raw.cid_widths,
        }
    }

    /// Resolve one glyph code to the text it stands for.
    ///
    /// This is the fallback chain from PLAN.md §3, minus `/ActualText` (which
    /// lives at the content-stream level, not the font level):
    ///
    /// 1. `/ToUnicode` — a real reverse map, and always right when present.
    /// 2. `/Encoding` + `/Differences` — a simple font's byte table.
    ///
    /// `None` means genuinely unrecoverable, and stays distinct from an empty
    /// string. The order matters: a font can have both, and `/ToUnicode` is the
    /// one the writer produced deliberately for text extraction.
    ///
    /// The result is still **shaped and in visual order** — presentation forms,
    /// laid down left to right. Making it readable Arabic is L3's job, and doing
    /// any of it here would break the ligature ordering rule (PLAN.md §3).
    pub fn decode(&self, code: u32) -> Option<String> {
        // `or_else` and not `or`: the second branch is only evaluated when the
        // first returned `None`, so we never build the fallback needlessly.
        self.to_unicode
            .get(code)
            .or_else(|| self.encoding.decode(code))
    }

    /// Whether this font can resolve anything at all.
    ///
    /// A font that answers `false` here makes every glyph it paints
    /// unrecoverable — the strongest per-font signal the detector has.
    pub fn is_resolvable(&self) -> bool {
        !self.to_unicode.is_empty() || !self.encoding.is_empty()
    }

    /// Advance width of a code, as a fraction of the em square.
    fn width_em(&self, code: u32) -> f64 {
        let thousandths = if self.two_byte {
            self.cid_widths
                .iter()
                .find(|(first, last, _)| code >= *first && code <= *last)
                .map(|(_, _, w)| *w)
                .unwrap_or(self.default_width)
        } else {
            // `checked_sub` returns None when `code < first_char`, which is the
            // out-of-range case — no underflow, no wrapped index.
            code.checked_sub(self.first_char)
                .and_then(|i| self.widths.get(i as usize))
                .copied()
                .unwrap_or(self.missing_width)
        };
        thousandths / 1000.0
    }
}

/// Every font on one page, keyed by the resource name the stream uses.
///
/// Implements [`GlyphWidths`], so handing it to the L1 interpreter replaces the
/// `AssumedWidths` placeholder with real metrics.
#[derive(Debug, Default, Clone)]
pub struct FontMap {
    fonts: HashMap<String, Font>,
}

impl FontMap {
    /// Build from L0's raw font extraction for a page.
    pub fn from_raw(raws: Vec<RawFont>) -> Self {
        Self {
            fonts: raws
                .into_iter()
                .map(Font::from_raw)
                .map(|f| (f.resource_name.clone(), f))
                .collect(),
        }
    }

    /// Look up a font by the name a `Tf` operator used.
    pub fn get(&self, resource_name: &str) -> Option<&Font> {
        self.fonts.get(resource_name)
    }

    /// Resolve a glyph code through the font that painted it.
    ///
    /// `None` means *unrecoverable*: either the font is unknown or its map has
    /// no entry for this code. Not an empty string — the difference between
    /// "no text here" and "text we cannot read" is the whole point of the
    /// project, and collapsing them would hide it.
    pub fn decode(&self, resource_name: &str, code: u32) -> Option<String> {
        self.fonts.get(resource_name)?.decode(code)
    }

    /// Iterate over the fonts, for reporting.
    pub fn iter(&self) -> impl Iterator<Item = &Font> {
        self.fonts.values()
    }
}

impl GlyphWidths for FontMap {
    fn width(&self, font: &str, code: u32) -> f64 {
        self.fonts
            .get(font)
            .map(|f| f.width_em(code))
            // An unknown font gets the same half-em guess as before, so a
            // missing resource degrades positions rather than collapsing them.
            .unwrap_or(0.5)
    }
}

// ---------------------------------------------------------------------------
// CMap tokenizer
//
// PLAN.md §10.2: this is emphatically *not* a PostScript interpreter. Four
// token kinds cover the whole grammar we care about.
// ---------------------------------------------------------------------------

/// A token from a CMap stream.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// `<0113>` — a hex string, stored as the bytes it denotes.
    Hex(Vec<u8>),
    /// `[`
    ArrayOpen,
    /// `]`
    ArrayClose,
    /// A bare keyword such as `beginbfchar`, or a `/Name`, or a number.
    Word(String),
}

/// Split a CMap stream into tokens.
fn tokenize(bytes: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            // Whitespace between tokens.
            b' ' | b'\t' | b'\r' | b'\n' | b'\x0c' | b'\0' => i += 1,

            // A comment runs to end of line.
            b'%' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }

            b'[' => {
                tokens.push(Token::ArrayOpen);
                i += 1;
            }
            b']' => {
                tokens.push(Token::ArrayClose);
                i += 1;
            }

            b'<' => {
                // `<<` opens a dictionary, which we skip past as a word so it
                // is never mistaken for a hex string.
                if bytes.get(i + 1) == Some(&b'<') {
                    tokens.push(Token::Word("<<".to_string()));
                    i += 2;
                    continue;
                }
                let start = i + 1;
                let end = find(bytes, start, b'>');
                tokens.push(Token::Hex(hex_bytes(&bytes[start..end])));
                // +1 to step past the '>' itself, if it was found.
                i = (end + 1).min(bytes.len());
            }
            b'>' => {
                // A stray '>' or the '>>' closing a dictionary.
                i += if bytes.get(i + 1) == Some(&b'>') {
                    2
                } else {
                    1
                };
            }

            // A literal string `(Adobe)` — appears in /CIDSystemInfo. Skipped
            // wholesale, honouring backslash escapes so `\)` does not end it.
            b'(' => {
                i += 1;
                let mut depth = 1;
                while i < bytes.len() && depth > 0 {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
            }

            // Anything else runs until the next delimiter or whitespace.
            _ => {
                let start = i;
                while i < bytes.len() && !is_delimiter(bytes[i]) {
                    i += 1;
                }
                tokens.push(Token::Word(
                    String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                ));
            }
        }
    }
    tokens
}

/// Is this byte a token boundary?
fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t'
            | b'\r'
            | b'\n'
            | b'\x0c'
            | b'\0'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'('
            | b')'
            | b'%'
    )
}

/// Index of the next `needle` at or after `from`, or the end of the slice.
fn find(bytes: &[u8], from: usize, needle: u8) -> usize {
    bytes[from..]
        .iter()
        .position(|&b| b == needle)
        .map(|p| from + p)
        // No terminator: treat the rest of the input as the token.
        .unwrap_or(bytes.len())
}

/// Decode hex digits into bytes, ignoring any whitespace between them.
///
/// An odd number of digits is padded with a trailing zero, which is what the
/// PDF spec prescribes for hex strings.
fn hex_bytes(digits: &[u8]) -> Vec<u8> {
    let mut nibbles: Vec<u8> = digits
        .iter()
        .filter_map(|b| (*b as char).to_digit(16))
        .map(|d| d as u8)
        .collect();

    if nibbles.len() % 2 == 1 {
        nibbles.push(0);
    }
    nibbles
        .chunks_exact(2)
        .map(|p| (p[0] << 4) | p[1])
        .collect()
}

/// Interpret code bytes as a big-endian integer.
///
/// Truncated to the low 4 bytes: a code wider than `u32` is malformed, and this
/// is a saturating misread rather than a panic.
fn code_of(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .take(4)
        .fold(0u32, |acc, &b| (acc << 8) | u32::from(b))
}

/// Decode a CMap destination — UTF-16 big-endian — into a `String`.
///
/// # Rust lesson: `char` is not a code unit
///
/// A Rust `char` is a full Unicode scalar value, so a codepoint above U+FFFF
/// arriving as a UTF-16 surrogate pair must be recombined into one `char`.
/// `char::decode_utf16` does exactly that; unpaired surrogates (which occur in
/// broken fonts) become U+FFFD rather than an error.
fn utf16be_to_string(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));

    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Advance a `bfrange` destination by `offset`, incrementing its last UTF-16
/// unit — the rule the spec gives for `<lo> <hi> <dstStart>`.
///
/// So `<0041>` at offset 2 is `C`. For a multi-character destination only the
/// final character moves, which is how ranges of accented forms are written.
fn increment_last_unit(start: &str, offset: u32) -> String {
    if offset == 0 {
        return start.to_string();
    }

    let mut chars: Vec<char> = start.chars().collect();
    if let Some(last) = chars.last_mut() {
        let bumped = u32::from(*last).saturating_add(offset);
        // A value that is not a valid scalar (a surrogate, or beyond U+10FFFF)
        // means the range overran; leave the character alone rather than
        // fabricating one.
        if let Some(c) = char::from_u32(bumped) {
            *last = c;
        }
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FontInfo;

    /// The opening of a real `/ToUnicode` stream from our fixture, trimmed.
    const REAL_CMAP: &str = "\
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo
<< /Registry (Adobe)
/Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
4 beginbfchar
<0003> <0020>
<0113> <0627>
<0114> <FE8E>
<01AE> <0651064B>
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end";

    #[test]
    fn parses_a_real_cmap() {
        let cmap = CMap::parse(REAL_CMAP.as_bytes());

        // `<0000> <FFFF>` declares two-byte codes.
        assert_eq!(cmap.code_bytes(), 2);
        assert_eq!(cmap.len(), 4);

        // CID 3 is the space — read from the map, never assumed (PLAN.md §10.1).
        assert_eq!(cmap.get(0x0003).as_deref(), Some(" "));
        // A base letter and a presentation form, from the same font. This
        // mixture is why NFKC is mandatory later.
        assert_eq!(cmap.get(0x0113).as_deref(), Some("\u{0627}"));
        assert_eq!(cmap.get(0x0114).as_deref(), Some("\u{FE8E}"));
    }

    #[test]
    fn one_code_can_map_to_several_codepoints() {
        // The `01AE → U+0651 U+064B` case (shadda + fathatan) from PLAN.md
        // §10.1 — the reason a destination is a String and not a char.
        let cmap = CMap::parse(REAL_CMAP.as_bytes());
        let decoded = cmap.get(0x01AE).expect("01AE is mapped");
        assert_eq!(decoded.chars().count(), 2);
        assert_eq!(decoded, "\u{0651}\u{064B}");
    }

    #[test]
    fn unmapped_codes_return_none_not_empty() {
        // "Text we cannot read" must stay distinguishable from "no text".
        let cmap = CMap::parse(REAL_CMAP.as_bytes());
        assert_eq!(cmap.get(0xFFFE), None);
    }

    #[test]
    fn bfrange_increments_the_last_unit() {
        let src = "2 beginbfrange\n<0100> <0102> <0041>\nendbfrange";
        let cmap = CMap::parse(src.as_bytes());
        assert_eq!(cmap.get(0x0100).as_deref(), Some("A"));
        assert_eq!(cmap.get(0x0101).as_deref(), Some("B"));
        assert_eq!(cmap.get(0x0102).as_deref(), Some("C"));
        // Outside the declared range.
        assert_eq!(cmap.get(0x0103), None);
    }

    #[test]
    fn bfrange_with_an_explicit_list() {
        let src = "1 beginbfrange\n<0200> <0202> [<0627> <0628> <0629>]\nendbfrange";
        let cmap = CMap::parse(src.as_bytes());
        assert_eq!(cmap.get(0x0200).as_deref(), Some("\u{0627}"));
        assert_eq!(cmap.get(0x0202).as_deref(), Some("\u{0629}"));
    }

    #[test]
    fn a_huge_bfrange_is_not_expanded_into_memory() {
        // `<0000> <FFFF>` is legal. Storing it as a range means 65,536 codes
        // are answerable from one small struct.
        let src = "1 beginbfrange\n<0000> <FFFF> <0041>\nendbfrange";
        let cmap = CMap::parse(src.as_bytes());
        assert_eq!(cmap.len(), 65_536);
        assert_eq!(cmap.get(0x0000).as_deref(), Some("A"));
        // Incrementing past the end of the Unicode range must not fabricate a
        // character or panic.
        assert!(cmap.get(0xFFFF).is_some());
    }

    #[test]
    fn bfchar_wins_over_an_overlapping_bfrange() {
        let src = "1 beginbfrange\n<0100> <0110> <0041>\nendbfrange\n\
                   1 beginbfchar\n<0105> <0627>\nendbfchar";
        let cmap = CMap::parse(src.as_bytes());
        assert_eq!(cmap.get(0x0105).as_deref(), Some("\u{0627}"));
        // Neighbours still come from the range.
        assert_eq!(cmap.get(0x0104).as_deref(), Some("E"));
    }

    #[test]
    fn surrogate_pairs_become_one_char() {
        // U+1F600 is written as the pair D83D DE00 in UTF-16BE.
        let src = "1 beginbfchar\n<0001> <D83DDE00>\nendbfchar";
        let cmap = CMap::parse(src.as_bytes());
        let decoded = cmap.get(1).unwrap();
        assert_eq!(decoded.chars().count(), 1);
        assert_eq!(decoded.chars().next(), Some('\u{1F600}'));
    }

    #[test]
    fn one_byte_codespace_is_honoured() {
        let src = "1 begincodespacerange\n<00> <FF>\nendcodespacerange";
        assert_eq!(CMap::parse(src.as_bytes()).code_bytes(), 1);
    }

    #[test]
    fn truncated_cmap_keeps_what_it_understood() {
        // A stream cut off mid-section must not lose the entries before it.
        let src = "2 beginbfchar\n<0003> <0020>\n<0113> <06";
        let cmap = CMap::parse(src.as_bytes());
        assert_eq!(cmap.get(0x0003).as_deref(), Some(" "));
    }

    #[test]
    fn a_font_with_no_tounicode_resolves_nothing() {
        // The `/TT0` case in our fixture: WinAnsiEncoding, no /ToUnicode.
        let info = FontInfo {
            resource_name: "TT0".to_string(),
            subtype: "TrueType".to_string(),
            base_font: None,
            encoding: Some("WinAnsiEncoding".to_string()),
            code_to_unicode: CodeToUnicode::EncodingOnly,
        };
        let font = Font::from_raw(RawFont::new(info));
        assert!(font.to_unicode.is_empty());
        assert_eq!(font.decode(0x31), None);
    }

    #[test]
    fn simple_font_widths_are_indexed_from_first_char() {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "F1".to_string(),
            subtype: "TrueType".to_string(),
            base_font: None,
            encoding: None,
            code_to_unicode: CodeToUnicode::None,
        });
        raw.first_char = 65;
        raw.widths = vec![500.0, 750.0];
        raw.missing_width = 250.0;
        let font = Font::from_raw(raw);

        assert_eq!(font.width_em(65), 0.5);
        assert_eq!(font.width_em(66), 0.75);
        // Past the end of /Widths.
        assert_eq!(font.width_em(67), 0.25);
        // Below /FirstChar: `checked_sub` prevents an underflowing index.
        assert_eq!(font.width_em(10), 0.25);
    }

    #[test]
    fn cid_widths_use_ranges_then_fall_back_to_dw() {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "C2_0".to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: Some("Identity-H".to_string()),
            code_to_unicode: CodeToUnicode::ToUnicode,
        });
        raw.default_width = 1000.0;
        raw.cid_widths = vec![(3, 3, 260.0), (100, 200, 600.0)];
        let font = Font::from_raw(raw);

        assert_eq!(font.width_em(3), 0.26);
        assert_eq!(font.width_em(150), 0.6);
        // Outside every range → /DW.
        assert_eq!(font.width_em(9999), 1.0);
    }

    #[test]
    fn font_map_reports_unknown_fonts_rather_than_guessing() {
        let map = FontMap::default();
        assert_eq!(map.decode("nope", 0x0113), None);
        // Widths still degrade gracefully so positions do not collapse.
        assert_eq!(map.width("nope", 0x0113), 0.5);
    }
}
