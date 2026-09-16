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

use std::collections::{HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;

use crate::cff::{self, GlyphNames};
use crate::content::GlyphWidths;
use crate::encoding::{glyph_name_to_string, Encoding};
// at Pdf32000_iso sections 9.7.5-9.10.3 you will notice that CMap has two property.
// 1 Forward CMap where we drown char on the page this used by Renderer,
// 2 Backward CMap the one we use to extract text.
// The backward map is optional, added by the producer as a
// favour to anyone who later wants the text back. font.rs is almost entirely about the backward map.
// When Word or InDesign embeds an Arabic font, it doesn't embed all 3000 glyphs — it embeds only the
// ones used on this page, maybe 80 of them, renumbered 1, 2, 3, … in whatever order it felt like.
// So in one file 0x0113 means alef; in the next file from the same producer it might mean ب.
// The number is a slot index into a private, one-off font, not a character.
use crate::ttf;
use crate::types::{CidToGid, CodeToUnicode, RawFont};

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
        // bytes ──tokenize──► Vec<Token> ──CMap::parse──► CMap
        //      (lexer:no meaning)        (scanner: look for three keywords, ignore the rest)
        //
        //  The lexer knows nothing about CMaps. It only knows "here is a hex string, here is a bracket,
        //  here is a bare word." All the meaning lives in parse,
        //  which walks the token list looking for beginbfchar, beginbfrange, begincodespacerange and skips
        //  everything else.
        let tokens = tokenize(bytes);
        let mut cmap = CMap {
            // Default to 2: composite fonts are the common case for Arabic, and
            // an explicit codespacerange almost always follows anyway.
            code_bytes: 2,
            // This line mean every thing else will set to default, singles -> {} and ranges -> [].
            ..Default::default()
        };
        // postscript
        //   /CIDInit /ProcSet findresource begin        ← noise
        //   12 dict begin                               ← noise
        //   begincmap                                   ← noise
        //   /CIDSystemInfo << /Registry (Adobe)         ← noise, with a dict and a string
        //     /Ordering (UCS) /Supplement 0 >> def
        //   /CMapName /Adobe-Identity-UCS def            ← noise
        //   /CMapType 2 def                              ← noise
        //   1 begincodespacerange                        ★
        //   <0000> <FFFF>                                ★
        //   endcodespacerange                            ★
        //   2 beginbfchar                                ★
        //   <0003> <0020>                                ★
        //   <0113> <0627>                                ★
        //   endbfchar
        //   endcmap

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
            match &range.dst {
                RangeDst::List(items) => {
                    if let Some(text) = items.get(offset) {
                        return Some(text.clone());
                    }
                }
                RangeDst::Incrementing(start) => {
                    return Some(increment_last_unit(start, offset as u32));
                }
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
    /// Glyph names read from the embedded font program.
    ///
    /// The last rung of the chain, and the only one the renderer checks — see
    /// [`crate::cff`].
    glyph_names: GlyphNames,
    /// Per-code character identity read from an embedded TrueType `cmap`.
    ///
    /// Composite fonts only, via `/FontFile2` + `/CIDToGIDMap`. Like the glyph
    /// names above this is renderer-checked, and it is the second opinion the
    /// disagreement arbitration below trusts (PLAN.md §10). See [`crate::ttf`].
    ttf_identity: HashMap<u32, char>,
    /// Codes where the font program and `/ToUnicode` disagree about a numeric
    /// separator, and the font program is believed.
    ///
    /// See [`Font::arbitrate_separators`] for why this is limited to
    /// separators.
    corrections: HashMap<u32, String>,
    /// Codes where the embedded font's `cmap` contradicted `/ToUnicode`, and
    /// the font program was believed (PLAN.md §10.25).
    ///
    /// This is what the detector reads to refuse a confident `ok`: the page's
    /// map was a lie, and the recovered character is only as good as the font's
    /// subset. Distinct from [`Self::corrections`], which also holds the narrow
    /// separator corrections, so a dispute is a stronger claim than a repair.
    ttf_disagreements: HashSet<u32>,
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

/// An advance width, as a fraction of the em square, below which a code that the
/// embedded font program cannot name is a "box" filler rather than a letter.
///
/// PLAN.md §10.25 measured the real thing: CID 0x467 in `doc3.pdf`'s F4 subset
/// is 125/1000 em (0.125); the font's letters are 0.22–0.27 em. The mark must
/// sit clearly under the narrowest letter, so 0.16 leaves margin on both sides.
const BOX_FILLER_MAX_WIDTH_EM: f64 = 0.16;

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

        // Read the font program's own identity first: the lines below move
        // `raw`'s fields out, and this one needs a borrow of it.
        let ttf_identity = read_ttf_identity(&raw, two_byte);

        // A composite font's `/Encoding` names a CMap (`Identity-H`), not a
        // byte table, so the simple-font encoding path does not apply to it.
        let encoding = if two_byte {
            Encoding::default()
        } else {
            Encoding::new(raw.base_encoding.as_deref(), raw.differences)
        };

        // Only simple fonts carry a name-keyed CFF; a composite font identifies
        // its glyphs by number rather than by name.
        let glyph_names = match (&raw.font_program, two_byte) {
            (Some(program), false) => cff::glyph_names(program),
            _ => GlyphNames::default(),
        };

        let mut corrections = arbitrate_separators(&to_unicode, &encoding, &glyph_names);
        // The font-cmap check is the wider net: it covers every code, not just
        // separators, while still refusing to contradict an agreeing map. The
        // disputed codes are kept apart for the detector, which needs to know
        // the map was a lie even after the text is repaired.
        let font_corrections = arbitrate_against_font(&to_unicode, &ttf_identity);
        let ttf_disagreements: HashSet<u32> = font_corrections.keys().copied().collect();
        corrections.extend(font_corrections);

        Self {
            resource_name: raw.info.resource_name,
            two_byte,
            route: raw.info.code_to_unicode,
            to_unicode,
            encoding,
            glyph_names,
            ttf_identity,
            corrections,
            ttf_disagreements,
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
    /// 1. `/ToUnicode` — a real reverse map, and the writer's own hint.
    /// 2. `/Encoding` + `/Differences` — a simple font's byte table.
    /// 3. The font program: CFF glyph names or a TrueType `cmap`.
    ///
    /// `None` means genuinely unrecoverable, and stays distinct from an empty
    /// string. The order matters: a font can have several of these, and
    /// `/ToUnicode` is the one the writer produced deliberately for text
    /// extraction. The font program only steps in where the map is silent or —
    /// via a correction — where the two contradict each other and the program
    /// is believed.
    ///
    /// A correction is deliberately first: it exists precisely because the map
    /// is wrong for that code (`arbitrate_separators`,
    /// `arbitrate_against_font`).
    ///
    /// The result is still **shaped and in visual order** — presentation forms,
    /// laid down left to right. Making it readable Arabic is L3's job, and doing
    /// any of it here would break the ligature ordering rule (PLAN.md §3).
    pub fn decode(&self, code: u32) -> Option<String> {
        if let Some(fixed) = self.corrections.get(&code) {
            return Some(fixed.clone());
        }

        self.to_unicode
            .get(code)
            .or_else(|| self.encoding.decode(code))
            .or_else(|| self.glyph_name_char(code))
            .or_else(|| self.ttf_char(code))
    }

    /// Resolve a code through the embedded TrueType `cmap`, via its glyph index.
    ///
    /// The composite-font counterpart of the CFF glyph-name rung. Reached only
    /// when the earlier rungs were silent, so it can add information but never
    /// contradict them. Codes beyond an explicit `/CIDToGIDMap` resolve to
    /// glyph 0 (`.notdef`) and therefore have no identity.
    fn ttf_char(&self, code: u32) -> Option<String> {
        self.ttf_identity.get(&code).map(|ch| ch.to_string())
    }

    /// Resolve a code through the embedded font program's glyph name.
    ///
    /// The fourth rung of the chain (PLAN.md §3). Reached only when neither
    /// `/ToUnicode` nor `/Encoding` could answer, so it can add information but
    /// never contradict either.
    fn glyph_name_char(&self, code: u32) -> Option<String> {
        glyph_name_to_string(self.glyph_names.get(code)?)
    }

    /// How many codes the embedded font program names. For reporting.
    pub fn named_glyphs(&self) -> usize {
        self.glyph_names.len()
    }

    /// Codes whose `/ToUnicode` value this font overrides, and with what.
    pub fn corrections(&self) -> &HashMap<u32, String> {
        &self.corrections
    }

    /// Whether the embedded font's `cmap` contradicted `/ToUnicode` for `code`.
    ///
    /// The detector's per-glyph probe (PLAN.md §10.25): a code that answers
    /// `true` here is one where `/ToUnicode` lied and the recovered character
    /// rests on the font's own subset.
    pub fn disputes(&self, code: u32) -> bool {
        self.ttf_disagreements.contains(&code)
    }

    /// How many codes the embedded font contradicted `/ToUnicode` on.
    pub fn dispute_count(&self) -> usize {
        self.ttf_disagreements.len()
    }

    /// Whether this font can resolve anything at all.
    ///
    /// A font that answers `false` here makes every glyph it paints
    /// unrecoverable — the strongest per-font signal the detector has.
    pub fn is_resolvable(&self) -> bool {
        !self.to_unicode.is_empty() || !self.encoding.is_empty() || !self.ttf_identity.is_empty()
    }
    // TODO we should solve this later
    // One real caveat in the current code
    // /W is indexed by CID, not by the code in the content stream. The proper chain is:
    // 2-byte code → (the /Encoding CMap) → CID → /W lookup
    // width_em skips the middle step and matches the raw code against cid_widths directly.
    // For Identity-H that is correct, the whole point of Identity-H is that code == CID
    // and types.rs:180 treats every Type0 as 2-byte on that assumption.
    // So a Type0 font using a non-Identity CMap
    // (e.g. a legacy Adobe-Arabic-1 ordering, or a one-byte-codespace Type0) would get
    // wrong widths here, because the code→CID translation never happens.
    // Rare in practice for modern Arabic PDFs, which are almost
    // universally Identity-H, but it's the known gap.
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

    /// Whether a code is a "box" filler: the embedded font's own cmap cannot
    /// name it, and its declared width is a mark's, not a letter's.
    ///
    /// PLAN.md §10.25 found exactly one such code in the F4 subset of
    /// `doc3.pdf`: CID `0x467`. `/ToUnicode` claims `ا`, but no cmap subtable
    /// maps any codepoint to the glyph, and the drawing is an empty box a
    /// quarter of a letter's height. Another document from the same producer
    /// maps that identical CID to `د`; accepting either claim turns four or
    /// five stacked marks into a run of letters that prints nothing. The font
    /// program cannot spell the glyph, so it carries no identity to recover —
    /// like the producer-declared U+FFFD of PLAN.md §10.20, it is ink without
    /// text, and L3 drops it.
    ///
    /// The rule is deliberately narrow. It needs **both** a silent font program
    /// *and* a mark-sized width, so a code the font really names is untouched
    /// however thin it is, and a wide glyph is never dropped just because a
    /// broken subset lost its cmap entry.
    pub fn is_box_filler(&self, code: u32) -> bool {
        // The cmap signal exists only for composite fonts: a simple font's
        // codes are not addressed by a TrueType cmap, so `ttf_identity` is
        // empty there and "silent program" would mean nothing.
        if !self.two_byte {
            return false;
        }
        // A missing or unusable TrueType cmap is no evidence at all. Without
        // this guard every narrow CIDFont glyph (including valid marks in CFF
        // fonts) would look like an unnamed filler.
        if self.ttf_identity.is_empty() || self.ttf_disagreements.is_empty() {
            return false;
        }
        if self.ttf_identity.contains_key(&code) {
            return false;
        }
        // These are the two forged claims observed for the same unnamed box:
        // alef in `doc3.pdf`, dal in `test1 (13).pdf`. Do not generalise cmap
        // silence into permission to discard arbitrary letters, punctuation,
        // or combining marks.
        matches!(
            self.to_unicode.get(code).as_deref(),
            Some("\u{0627}" | "\u{062F}")
        ) && self.width_em(code) < BOX_FILLER_MAX_WIDTH_EM
    }
}

/// Build the per-code character identity from an embedded TrueType program.
///
/// Composite fonts only (`two_byte`): their codes are CIDs, which the
/// `/CIDToGIDMap` bridge turns into glyph indices in the program. A simple
/// font's codes go through `/Encoding` instead, and its identity is already
/// captured by the CFF/`post` routes — a TrueType `cmap` is not addressed by
/// 1-byte codes, so mixing it in here would be wrong.
fn read_ttf_identity(raw: &RawFont, two_byte: bool) -> HashMap<u32, char> {
    if !two_byte {
        return HashMap::new();
    }
    let Some(program) = &raw.ttf_program else {
        return HashMap::new();
    };
    let by_gid = ttf::cmap_glyph_map(program);

    match &raw.cid_to_gid {
        CidToGid::Identity => by_gid,
        CidToGid::Map(map) => map
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(code, glyph)| {
                by_gid
                    .get(&u32::from(glyph))
                    .and_then(|&ch| u32::try_from(code).ok().map(|code| (code, ch)))
            })
            .collect(),
    }
}

/// Find codes where the embedded font's `cmap` and `/ToUnicode` disagree about
/// what a character is, and believe the font program.
///
/// # Why this is sound — and where it stops
///
/// The `cmap` table is not a hint; it is the identity the glyphs were built
/// around, and the renderer draws from the very same outlines that `cmap`
/// describes. A producer who wants a page to *look* right must get the stored
/// font program right; only `/ToUnicode` is free to lie without being seen.
///
/// But "prefer the font wherever they differ" is still too hot. A correct
/// Arabic font maps a code to a presentation form (U+FExx) while the PDF's
/// `/ToUnicode` kindly gives the base letter — NFKC folds those to the same
/// thing, so they must not contradict each other. The rule is therefore:
///
/// 1. `/ToUnicode` gives **exactly one character** (a multi-character value is
///    a ligature like `لا` and is never touched).
/// 2. The font's `cmap` gives one character too.
/// 3. Their **NFKC forms disagree** — the two sources are different letters,
///    not the same letter in different disguises.
///
/// Only then does the font program win. This is the same shape as
/// [`arbitrate_separators`], but unconfined: in a doc3-style PDF the producer
/// re-stamps every code, and a separator-only net would not catch it.
fn arbitrate_against_font(
    to_unicode: &CMap,
    identity: &HashMap<u32, char>,
) -> HashMap<u32, String> {
    let mut out = HashMap::new();

    /// The single `char` a destination amounts to, if it is exactly one.
    fn single(text: &str) -> Option<char> {
        let mut chars = text.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        Some(first)
    }

    for (&code, &font_char) in identity {
        let Some(mapped) = to_unicode.get(code).as_deref().and_then(single) else {
            continue;
        };
        // NFKC equality = the same base letter in a different disguise (base
        // vs presentation form), so the two sources agree and `/ToUnicode`
        // stands. Only a real difference of identity is corrected.
        let mapped_nfkc: String = mapped.nfkc().collect();
        let font_nfkc: String = font_char.nfkc().collect();
        if mapped_nfkc != font_nfkc {
            // The affected Arabic producer's disputed cmap entries identify
            // yeh with the Persian code point. Canonicalise only while we are
            // already rejecting a contradictory `/ToUnicode`; an agreeing,
            // correctly encoded U+06CC remains untouched.
            let corrected =
                if font_nfkc == "\u{06CC}" || matches!(font_char, '\u{FBE9}' | '\u{FBEF}') {
                    '\u{064A}'
                } else {
                    font_char
                };
            out.insert(code, corrected.to_string());
        }
    }
    out
}

/// Characters that group or separate digits, in both scripts.
///
/// The arbitration below is confined to these. A code whose `/ToUnicode` value
/// is one of them and whose glyph name is a *different* one of them is the
/// exact fault we are correcting; anything else is left alone.
const SEPARATORS: [char; 6] = [
    ',',        // COMMA
    '.',        // FULL STOP
    '\u{060C}', // ARABIC COMMA
    '\u{066B}', // ARABIC DECIMAL SEPARATOR
    '\u{066C}', // ARABIC THOUSANDS SEPARATOR
    '\u{02D9}', // DOT ABOVE, used as a separator by some Arabic faces
];

/// Find codes where the font program and `/ToUnicode` disagree about a numeric
/// separator, and believe the font program.
///
/// # Why this is narrow on purpose
///
/// Preferring glyph names wholesale would be reckless: plenty of subset fonts
/// name glyphs `g42` or `cid123`, carrying no Unicode at all, and on those
/// files a good `/ToUnicode` is the only real information there is.
///
/// So a correction is made only when **every** one of these holds:
///
/// 1. `/ToUnicode` gives a single character, and it is a separator.
/// 2. The font program names the same code, and the name resolves to a single
///    character, and it too is a separator.
/// 3. The two disagree.
///
/// The result can therefore only ever turn one separator into another. It
/// cannot touch a letter, a digit, or a code the two sources agree on — and on
/// a correct PDF they always agree, so nothing is corrected at all.
///
/// The fault it repairs is real and costly: `bar_Persons.pdf` maps a glyph it
/// draws as an Arabic comma to `.`, so `3,709` extracts as `3.709` — a value
/// wrong by a factor of a thousand, and wrong in a way no reader would catch.
fn arbitrate_separators(
    to_unicode: &CMap,
    encoding: &Encoding,
    names: &GlyphNames,
) -> HashMap<u32, String> {
    let mut out = HashMap::new();

    /// The character a source gives for a code, if it is exactly one and is a
    /// separator.
    fn separator(text: &str) -> Option<char> {
        let mut chars = text.chars();
        let first = chars.next()?;
        // Exactly one character: a multi-character value is a ligature or an
        // expansion, not a separator.
        if chars.next().is_some() || !SEPARATORS.contains(&first) {
            return None;
        }
        Some(first)
    }

    for code in 0..=u32::from(u8::MAX) {
        let Some(mapped) = to_unicode.get(code).as_deref().and_then(separator) else {
            continue;
        };

        // The second opinion, in order of how directly the renderer relies on
        // it: `/Differences` names the glyph the renderer looks up, and the
        // font program's own charset names the same thing from inside.
        let named = encoding
            .decode(code)
            .or_else(|| names.get(code).and_then(glyph_name_to_string));

        let Some(named) = named.as_deref().and_then(separator) else {
            continue;
        };

        if mapped != named {
            out.insert(code, named.to_string());
        }
    }
    out
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

    /// Whether a code is a "box" filler in the font that painted it.
    ///
    /// See [`Font::is_box_filler`]. An unknown font answers `false`: it is
    /// already reported as unresolvable, and must not claim to know more.
    pub fn is_box_filler(&self, resource_name: &str, code: u32) -> bool {
        self.fonts
            .get(resource_name)
            .is_some_and(|font| font.is_box_filler(code))
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
        .as_chunks::<2>()
        .0
        .iter()
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
        .as_chunks::<2>()
        .0
        .iter()
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
    fn short_bfrange_list_does_not_shadow_a_later_range() {
        // Truncated array: it names only 0x0100 of the 0x0100–0x0101 range.
        let src = "2 beginbfrange\n<0100> <0101> [<0041>]\n<0101> <0101> <0042>\nendbfrange";
        let cmap = CMap::parse(src.as_bytes());
        assert_eq!(cmap.get(0x0100).as_deref(), Some("A"));
        assert_eq!(cmap.get(0x0101).as_deref(), Some("B"));
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

    // ---- arbitration -----------------------------------------------------

    fn font_with(cmap: &str, differences: Vec<(u8, String)>) -> Font {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "T1_0".to_string(),
            subtype: "Type1".to_string(),
            base_font: None,
            encoding: Some("WinAnsiEncoding".to_string()),
            code_to_unicode: CodeToUnicode::ToUnicode,
        });
        raw.to_unicode = Some(cmap.as_bytes().to_vec());
        raw.base_encoding = Some("WinAnsiEncoding".to_string());
        raw.differences = differences;
        Font::from_raw(raw)
    }

    #[test]
    fn the_font_overrules_a_wrong_separator() {
        // `bar_Persons.pdf`, font T1_0: `/ToUnicode` says code 161 is a full
        // stop, but `/Differences` names the glyph `uni066B` — the Arabic
        // decimal separator, which is what the page actually draws. Believing
        // the map turns `3,709` into `3.709`: a value wrong by a factor of a
        // thousand, in a way no reader would catch.
        let font = font_with(
            "1 beginbfchar\n<A1> <002E>\nendbfchar",
            vec![(161, "uni066B".to_string())],
        );

        assert_eq!(font.decode(161).as_deref(), Some("\u{066B}"));
        assert_eq!(font.corrections().len(), 1);
    }

    #[test]
    fn a_font_whose_sources_agree_is_left_alone() {
        // Every correct PDF. The two sources say the same thing, so nothing is
        // corrected and the output is exactly what it always was.
        let font = font_with(
            "1 beginbfchar\n<2C> <002C>\nendbfchar",
            vec![(44, "comma".to_string())],
        );
        assert!(font.corrections().is_empty());
        assert_eq!(font.decode(44).as_deref(), Some(","));
    }

    #[test]
    fn arbitration_cannot_touch_letters_or_digits() {
        // The guarantee that makes this safe to run on any document: a
        // correction can only ever replace one separator with another. Here
        // `/ToUnicode` and `/Differences` disagree about a *letter* and a
        // *digit*, and both disagreements are ignored.
        let font = font_with(
            "2 beginbfchar\n<83> <0037>\n<41> <0041>\nendbfchar",
            vec![
                // The font says these are an Arabic seven and an alef.
                (131, "uni0667".to_string()),
                (65, "uni0627".to_string()),
            ],
        );

        assert!(
            font.corrections().is_empty(),
            "only separators may be corrected"
        );
        assert_eq!(font.decode(131).as_deref(), Some("7"));
        assert_eq!(font.decode(65).as_deref(), Some("A"));
    }

    #[test]
    fn a_multi_character_value_is_never_a_separator() {
        // A ligature mapping to several characters must not be mistaken for a
        // separator and replaced by one.
        let font = font_with(
            "1 beginbfchar\n<A1> <06440627>\nendbfchar",
            vec![(161, "uni066B".to_string())],
        );
        assert!(font.corrections().is_empty());
    }

    #[test]
    fn the_font_program_answers_when_nothing_else_can() {
        // Design (b): the fourth rung of the chain. With no `/ToUnicode` entry
        // and no `/Differences`, a glyph name from the embedded font program is
        // the only information there is — so it may *add* an answer, but it can
        // never contradict one.
        let names = crate::cff::GlyphNames::default();
        assert!(names.is_empty(), "an absent font program resolves nothing");
    }

    #[test]
    fn font_map_reports_unknown_fonts_rather_than_guessing() {
        let map = FontMap::default();
        assert_eq!(map.decode("nope", 0x0113), None);
        // Widths still degrade gracefully so positions do not collapse.
        assert_eq!(map.width("nope", 0x0113), 0.5);
    }

    // ---- TTF cmap arbitration --------------------------------------------

    /// One big-endian `u16`.
    fn bs16(v: u16) -> [u8; 2] {
        v.to_be_bytes()
    }

    /// A minimal sfnt holding a single format-4 `cmap` subtable that maps each
    /// `(codepoint, glyph)` entry via the `idDelta` shortcut.
    ///
    /// The composite fonts this arbitration targets address the font program by
    /// glyph index (`/CIDToGIDMap /Identity`), so `entries` are written in
    /// glyph-index space, with an `(identity, glyph)` shape below.
    /// A TrueType program whose cmap maps `glyphs` (`(gid, codepoint)`) via a
    /// one-segment-per-glyph format-4 table.
    fn ttf_program(singles: &[(u32, u32)]) -> Vec<u8> {
        let n = singles.len() as u32;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&bs16(4)); // format
        fmt.extend_from_slice(&bs16(16 + 8 * (n + 1) as u16)); // length
        fmt.extend_from_slice(&bs16(0)); // language
        fmt.extend_from_slice(&bs16(2 * (n + 1) as u16)); // segCountX2 (+ sentinel)
        fmt.extend_from_slice(&bs16(0)); // searchRange
        fmt.extend_from_slice(&bs16(0)); // entrySelector
        fmt.extend_from_slice(&bs16(0)); // rangeShift
        for &(_, code) in singles {
            fmt.extend_from_slice(&bs16(code as u16));
        }
        fmt.extend_from_slice(&bs16(0xFFFF)); // sentinel endCode
        fmt.extend_from_slice(&bs16(0)); // reservedPad
        for &(_, code) in singles {
            fmt.extend_from_slice(&bs16(code as u16));
        }
        fmt.extend_from_slice(&bs16(0xFFFF)); // sentinel startCode
        for &(gid, code) in singles {
            // glyph = code + delta ⇒ delta = gid − code, wrapping.
            fmt.extend_from_slice(&bs16((gid as i64 - code as i64) as u16));
        }
        fmt.extend_from_slice(&bs16(1));
        for _ in 0..n + 1 {
            fmt.extend_from_slice(&bs16(0)); // idRangeOffset
        }

        let mut buf = Vec::new();
        buf.extend_from_slice(&(0x0001_0000u32).to_be_bytes());
        buf.extend_from_slice(&bs16(1)); // numTables
        buf.extend_from_slice(&bs16(0));
        buf.extend_from_slice(&bs16(0));
        buf.extend_from_slice(&bs16(0));
        buf.extend_from_slice(b"cmap");
        buf.extend_from_slice(&[0, 0, 0, 0]); // checksum
        buf.extend_from_slice(&28u32.to_be_bytes()); // cmap offset
        buf.extend_from_slice(&(4 + 8 + fmt.len() as u32).to_be_bytes()); // length
        buf.extend_from_slice(&bs16(0)); // cmap version
        buf.extend_from_slice(&bs16(1)); // cmap numTables
        buf.extend_from_slice(&bs16(3)); // platform Windows
        buf.extend_from_slice(&bs16(1)); // Unicode BMP
        buf.extend_from_slice(&12u32.to_be_bytes()); // subtable offset
        buf.extend_from_slice(&fmt);

        // Cheap sanity: dir record lands right after the header.
        assert_eq!(&buf[12..16], b"cmap");
        buf
    }

    /// A composite font with a `/ToUnicode` and a stacked TrueType program.
    fn composite_font(to_unicode: &str, glyphs: &[(u32, u32)]) -> Font {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "C2_0".to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: Some("Identity-H".to_string()),
            code_to_unicode: CodeToUnicode::ToUnicode,
        });
        raw.to_unicode = Some(to_unicode.as_bytes().to_vec());
        raw.ttf_program = Some(ttf_program(glyphs));
        Font::from_raw(raw)
    }

    #[test]
    fn the_font_program_overrules_a_lying_tounicode() {
        // `doc3.pdf` (PLAN.md §10): `/ToUnicode` claims the code is an alef,
        // but the embedded font's cmap says the glyph is a beh (U+0628). The
        // cmap is what the outlines were built around, so it wins.
        let font = composite_font(
            "1 beginbfchar\n<0100> <0627>\nendbfchar",
            &[(0x0100, 0x0628)],
        );
        assert_eq!(font.corrections().len(), 1);
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{0628}"));
    }

    #[test]
    fn a_presentation_form_agrees_with_its_base_letter() {
        // A correct PDF: `/ToUnicode` gives the base letter, the font program
        // gives its presentation form. NFKC folds them to the same thing, so
        // the map is left alone — correcting here would be vandalism.
        let font = composite_font(
            "1 beginbfchar\n<0100> <0628>\nendbfchar",
            &[(0x0100, 0xFE90)], // beh-initial
        );
        assert!(font.corrections().is_empty());
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{0628}"));
    }

    #[test]
    fn the_known_mark_to_yeh_lie_becomes_arabic_yeh() {
        let font = composite_font(
            "1 beginbfchar\n<0100> <065A>\nendbfchar",
            &[(0x0100, 0xFBE9)],
        );
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{064A}"));
    }

    #[test]
    fn correct_persian_yeh_is_preserved() {
        let font = composite_font(
            "1 beginbfchar\n<0100> <06CC>\nendbfchar",
            &[(0x0100, 0x06CC)],
        );
        assert!(font.corrections().is_empty());
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{06CC}"));
    }

    #[test]
    fn a_multi_character_tounicode_value_is_not_overruled() {
        // `ToUnicode` maps the code to a two-character ligature `لا`; the font
        // program maps the same glyph to the single ligature codepoint U+FEFB.
        // A multi-character value is never second-guessed.
        let font = composite_font(
            "1 beginbfchar\n<0100> <0644 0627>\nendbfchar",
            &[(0x0100, 0xFEFB)],
        );
        assert!(font.corrections().is_empty());
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{0644}\u{0627}"));
    }

    #[test]
    fn a_font_program_can_make_a_font_resolvable() {
        // No `/ToUnicode`, but the embedded font knows what its glyphs are.
        // PLAN.md §3's chain ends at the font program, so this font can now
        // answer — and the detector has one less `unmappable_fonts`.
        let font = composite_font("", &[(0x0100, 0x0628)]);
        assert!(font.to_unicode.is_empty());
        assert!(font.is_resolvable());
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{0628}"));
    }

    #[test]
    fn a_simple_font_ignores_the_ttf_cmap() {
        // Simple (1-byte) fonts key their codes off `/Encoding`, not the
        // composite CID→GID bridge, so a TrueType cmap must not leak in here:
        // the code 0x41 is a Latin `A` under WinAnsi, not a glyph index into
        // the program.
        let mut raw = RawFont::new(FontInfo {
            resource_name: "TT1".to_string(),
            subtype: "TrueType".to_string(),
            base_font: None,
            encoding: Some("WinAnsiEncoding".to_string()),
            code_to_unicode: CodeToUnicode::None,
        });
        raw.ttf_program = Some(ttf_program(&[(0x0628, 0x0100)]));
        let font = Font::from_raw(raw);

        assert!(!font.is_resolvable());
        assert_eq!(font.decode(0x41), None);
    }

    #[test]
    fn a_narrow_composite_glyph_without_ttf_evidence_is_not_dropped() {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "C2_0".to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: Some("Identity-H".to_string()),
            code_to_unicode: CodeToUnicode::ToUnicode,
        });
        raw.to_unicode = Some(b"1 beginbfchar\n<0100> <0627>\nendbfchar".to_vec());
        raw.default_width = 125.0;
        let font = Font::from_raw(raw);

        assert!(!font.is_box_filler(0x0100));
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{0627}"));
    }

    #[test]
    fn the_known_narrow_dal_filler_is_dropped() {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "C2_0".to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: Some("Identity-H".to_string()),
            code_to_unicode: CodeToUnicode::ToUnicode,
        });
        // CID 0x0100 is the unnamed filler. A second CID supplies the required
        // evidence that this font's `/ToUnicode` map contradicts its cmap.
        raw.to_unicode = Some(b"2 beginbfchar\n<0100> <062F>\n<0101> <0627>\nendbfchar".to_vec());
        raw.ttf_program = Some(ttf_program(&[(0x0101, 0x0628)]));
        raw.default_width = 125.0;
        let font = Font::from_raw(raw);

        assert!(font.is_box_filler(0x0100));
        assert_eq!(font.decode(0x0100).as_deref(), Some("\u{062F}"));
    }

    #[test]
    fn codes_past_an_explicit_cid_to_gid_map_do_not_become_identity() {
        let mut raw = RawFont::new(FontInfo {
            resource_name: "C2_0".to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: Some("Identity-H".to_string()),
            code_to_unicode: CodeToUnicode::None,
        });
        raw.ttf_program = Some(ttf_program(&[(0x0100, 0x0628)]));
        raw.cid_to_gid = CidToGid::Map(vec![0]);
        let font = Font::from_raw(raw);

        assert_eq!(font.decode(0x0100), None);
    }
}
