//! **L2 (part 2) — simple-font encodings.**
//!
//! Not every font carries a `/ToUnicode` map. A *simple* font — one byte per
//! code — can instead declare an `/Encoding`, which says what each of its 256
//! codes means. That is the second rung of the fallback chain in PLAN.md §3:
//!
//! ```text
//!   /ActualText  →  /ToUnicode  →  /Encoding (+ /Differences)  →  font cmap
//! ```
//!
//! This module is that third rung. It matters more than it looks: in our test
//! document 79 of 237 font instances have no `/ToUnicode` at all, and every one
//! of them declares `WinAnsiEncoding`. Without this they emit U+FFFD; with it
//! they read correctly.
//!
//! # An encoding is two layers
//!
//! `/Encoding` is either a bare name (`/WinAnsiEncoding`) or a dictionary:
//!
//! ```text
//! /Encoding << /BaseEncoding /WinAnsiEncoding
//!              /Differences [ 65 /alpha /beta  200 /gamma ] >>
//! ```
//!
//! `/Differences` overrides individual codes with **glyph names**, and the
//! array's format is idiosyncratic: a number resets the current code, and each
//! name that follows takes the next code up. So the example above means
//! 65→`alpha`, 66→`beta`, 200→`gamma`.
//!
//! # Resolving glyph names
//!
//! A glyph name is resolved the way the Adobe Glyph List specification says:
//! through the **full Adobe Glyph List** (4,281 names, embedded from
//! `resources/glyphlist.txt`), then the algorithmic `uniXXXX` and `uXXXX`
//! forms. A name nothing resolves returns `None` — unresolvable — rather than a
//! guess. That is the whole ethic of the project applied one level down.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One of PDF's predefined simple-font encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseEncoding {
    /// `/WinAnsiEncoding` — Windows code page 1252. By far the most common.
    WinAnsi,
    /// `/MacRomanEncoding` — the classic Mac OS Roman character set.
    MacRoman,
    /// `/StandardEncoding` — Adobe's original. ASCII-like, but note that codes
    /// 0x27 and 0x60 are the *typographic* quotes, not the ASCII ones.
    Standard,
}

impl BaseEncoding {
    /// Recognise an encoding name as it appears in a font dictionary.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "WinAnsiEncoding" => Some(BaseEncoding::WinAnsi),
            "MacRomanEncoding" => Some(BaseEncoding::MacRoman),
            // `PDFDocEncoding` agrees with Standard over the ASCII range, which
            // is the part that carries text.
            "StandardEncoding" | "PDFDocEncoding" => Some(BaseEncoding::Standard),
            // `Identity-H`/`Identity-V` are composite-font CMap names, not
            // simple-font encodings; they belong to a different code path.
            _ => None,
        }
    }

    /// Map one byte code to a character.
    pub fn to_char(self, code: u8) -> Option<char> {
        match self {
            BaseEncoding::WinAnsi => winansi(code),
            BaseEncoding::MacRoman => macroman(code),
            BaseEncoding::Standard => standard(code),
        }
    }
}

/// `WinAnsiEncoding` is Windows-1252.
///
/// Below 0x80 it is ASCII; from 0xA0 up it is Latin-1, so both ranges are
/// computed rather than tabulated. Only 0x80–0x9F, where Windows-1252 diverges
/// from Latin-1, needs a table.
fn winansi(code: u8) -> Option<char> {
    match code {
        // The control range carries no text.
        0x00..=0x1F => None,
        0x20..=0x7E => Some(code as char),
        0x7F => None,
        0x80..=0x9F => WINANSI_HIGH[(code - 0x80) as usize],
        // Latin-1: the codepoint equals the byte value.
        0xA0..=0xFF => char::from_u32(code as u32),
    }
}

/// `MacRomanEncoding`: ASCII below 0x80, a full table above it.
fn macroman(code: u8) -> Option<char> {
    match code {
        0x00..=0x1F => None,
        0x20..=0x7E => Some(code as char),
        0x7F => None,
        0x80..=0xFF => MACROMAN_HIGH[(code - 0x80) as usize],
    }
}

/// `StandardEncoding`, over the range that matters.
///
/// The ASCII range with two documented substitutions. Above 0x7F, Standard
/// diverges substantially from both Latin-1 and Windows-1252, and those codes
/// are rare in practice — they return `None` rather than a Latin-1 misreading.
fn standard(code: u8) -> Option<char> {
    match code {
        // Standard puts the typographic quotes where ASCII has the straight
        // apostrophe and backtick. Reading these as ASCII is a real, if minor,
        // corruption — so we honour the encoding.
        0x27 => Some('\u{2019}'),
        0x60 => Some('\u{2018}'),
        0x20..=0x7E => Some(code as char),
        _ => None,
    }
}

/// Windows-1252's divergence from Latin-1, codes 0x80–0x9F.
/// Generated from the published code page; `None` marks the undefined slots.
const WINANSI_HIGH: [Option<char>; 32] = [
    Some('\u{20AC}'),
    None,
    Some('\u{201A}'),
    Some('\u{0192}'),
    Some('\u{201E}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02C6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017D}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02DC}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203A}'),
    Some('\u{0153}'),
    None,
    Some('\u{017E}'),
    Some('\u{0178}'),
];

/// Mac OS Roman, codes 0x80–0xFF.
const MACROMAN_HIGH: [Option<char>; 128] = [
    Some('\u{00C4}'),
    Some('\u{00C5}'),
    Some('\u{00C7}'),
    Some('\u{00C9}'),
    Some('\u{00D1}'),
    Some('\u{00D6}'),
    Some('\u{00DC}'),
    Some('\u{00E1}'),
    Some('\u{00E0}'),
    Some('\u{00E2}'),
    Some('\u{00E4}'),
    Some('\u{00E3}'),
    Some('\u{00E5}'),
    Some('\u{00E7}'),
    Some('\u{00E9}'),
    Some('\u{00E8}'),
    Some('\u{00EA}'),
    Some('\u{00EB}'),
    Some('\u{00ED}'),
    Some('\u{00EC}'),
    Some('\u{00EE}'),
    Some('\u{00EF}'),
    Some('\u{00F1}'),
    Some('\u{00F3}'),
    Some('\u{00F2}'),
    Some('\u{00F4}'),
    Some('\u{00F6}'),
    Some('\u{00F5}'),
    Some('\u{00FA}'),
    Some('\u{00F9}'),
    Some('\u{00FB}'),
    Some('\u{00FC}'),
    Some('\u{2020}'),
    Some('\u{00B0}'),
    Some('\u{00A2}'),
    Some('\u{00A3}'),
    Some('\u{00A7}'),
    Some('\u{2022}'),
    Some('\u{00B6}'),
    Some('\u{00DF}'),
    Some('\u{00AE}'),
    Some('\u{00A9}'),
    Some('\u{2122}'),
    Some('\u{00B4}'),
    Some('\u{00A8}'),
    Some('\u{2260}'),
    Some('\u{00C6}'),
    Some('\u{00D8}'),
    Some('\u{221E}'),
    Some('\u{00B1}'),
    Some('\u{2264}'),
    Some('\u{2265}'),
    Some('\u{00A5}'),
    Some('\u{00B5}'),
    Some('\u{2202}'),
    Some('\u{2211}'),
    Some('\u{220F}'),
    Some('\u{03C0}'),
    Some('\u{222B}'),
    Some('\u{00AA}'),
    Some('\u{00BA}'),
    Some('\u{03A9}'),
    Some('\u{00E6}'),
    Some('\u{00F8}'),
    Some('\u{00BF}'),
    Some('\u{00A1}'),
    Some('\u{00AC}'),
    Some('\u{221A}'),
    Some('\u{0192}'),
    Some('\u{2248}'),
    Some('\u{2206}'),
    Some('\u{00AB}'),
    Some('\u{00BB}'),
    Some('\u{2026}'),
    Some('\u{00A0}'),
    Some('\u{00C0}'),
    Some('\u{00C3}'),
    Some('\u{00D5}'),
    Some('\u{0152}'),
    Some('\u{0153}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{201C}'),
    Some('\u{201D}'),
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{00F7}'),
    Some('\u{25CA}'),
    Some('\u{00FF}'),
    Some('\u{0178}'),
    Some('\u{2044}'),
    Some('\u{20AC}'),
    Some('\u{2039}'),
    Some('\u{203A}'),
    Some('\u{FB01}'),
    Some('\u{FB02}'),
    Some('\u{2021}'),
    Some('\u{00B7}'),
    Some('\u{201A}'),
    Some('\u{201E}'),
    Some('\u{2030}'),
    Some('\u{00C2}'),
    Some('\u{00CA}'),
    Some('\u{00C1}'),
    Some('\u{00CB}'),
    Some('\u{00C8}'),
    Some('\u{00CD}'),
    Some('\u{00CE}'),
    Some('\u{00CF}'),
    Some('\u{00CC}'),
    Some('\u{00D3}'),
    Some('\u{00D4}'),
    Some('\u{F8FF}'),
    Some('\u{00D2}'),
    Some('\u{00DA}'),
    Some('\u{00DB}'),
    Some('\u{00D9}'),
    Some('\u{0131}'),
    Some('\u{02C6}'),
    Some('\u{02DC}'),
    Some('\u{00AF}'),
    Some('\u{02D8}'),
    Some('\u{02D9}'),
    Some('\u{02DA}'),
    Some('\u{00B8}'),
    Some('\u{02DD}'),
    Some('\u{02DB}'),
    Some('\u{02C7}'),
];

/// A simple font's complete encoding: a base table plus per-code overrides.
#[derive(Debug, Clone, Default)]
pub struct Encoding {
    /// The `/BaseEncoding`, or the bare `/Encoding` name.
    ///
    /// `None` means the font declared no encoding we recognise, in which case
    /// only `/Differences` can resolve anything.
    base: Option<BaseEncoding>,
    /// `/Differences`: code → glyph name.
    differences: HashMap<u8, String>,
}

impl Encoding {
    /// Build from the pieces L0 extracted.
    pub fn new(base_name: Option<&str>, differences: Vec<(u8, String)>) -> Self {
        Self {
            base: base_name.and_then(BaseEncoding::from_name),
            differences: differences.into_iter().collect(),
        }
    }

    /// `true` if this encoding can resolve nothing at all.
    pub fn is_empty(&self) -> bool {
        self.base.is_none() && self.differences.is_empty()
    }

    /// Resolve one code to text.
    ///
    /// `/Differences` wins over the base encoding — that is the entire point of
    /// the array. A code the differences name but whose glyph name we cannot
    /// resolve returns `None`, *without* falling back to the base table: the
    /// font explicitly said that code is something else, so the base table's
    /// answer would be wrong rather than merely unknown.
    pub fn decode(&self, code: u32) -> Option<String> {
        // Simple fonts are single-byte by definition; a wider code cannot be
        // meant for this path.
        let code = u8::try_from(code).ok()?;

        if let Some(name) = self.differences.get(&code) {
            return glyph_name_to_string(name);
        }
        self.base?.to_char(code).map(String::from)
    }
}

/// Resolve a PostScript glyph name to the text it denotes.
///
/// Follows the Adobe Glyph List specification:
///
/// 1. Drop any suffix after the first dot: `one.oldstyle` means `one`.
/// 2. Split the rest on underscores into components, so a ligature named
///    `f_i` means `f` followed by `i`.
/// 3. Resolve each component, trying in order:
///    - the Adobe Glyph List, e.g. `space`, `Aacute`, `afii57415` (alef);
///    - `uniXXXX`: groups of four hex UTF-16 code units, so `uni0627` is alef
///      and `uni06440627` is lam followed by alef;
///    - `uXXXX` to `uXXXXXX`: a single hex scalar value.
///
/// **If any component cannot be resolved, the whole name returns `None`.** The
/// specification would map that component to nothing and keep the rest, but
/// that silently drops a character: `f_g42` would read as `f`. Anything
/// unresolvable — `g42`, `cid1234`, a subsetter's invention — is reported, not
/// guessed.
pub fn glyph_name_to_string(name: &str) -> Option<String> {
    let base = name.split('.').next().unwrap_or(name);

    let mut text = String::new();
    for component in base.split('_') {
        text.push_str(&component_to_string(component)?);
    }
    (!text.is_empty()).then_some(text)
}

/// Resolve one underscore-separated component of a glyph name.
fn component_to_string(component: &str) -> Option<String> {
    if component.is_empty() {
        return None;
    }

    if let Some(text) = agl_lookup(component) {
        return Some(text);
    }

    if let Some(rest) = component.strip_prefix("uni") {
        // The AGL spells several units as one run of hex digits —
        // `uni06440627` is lam followed by alef. Some tools instead repeat the
        // prefix, `uni0644uni0627`, so strip any further occurrences and accept
        // both spellings.
        let rest = rest.replace("uni", "");
        // Must be whole groups of four hex digits, and at least one.
        if !rest.is_empty() && rest.len() % 4 == 0 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            let units: Vec<u16> = rest
                .as_bytes()
                .as_chunks::<4>()
                .0
                .iter()
                .filter_map(|chunk| {
                    // `from_utf8` cannot fail here: we checked every byte is a
                    // hex digit, which is ASCII.
                    u16::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()
                })
                .collect();

            // UTF-16, so a surrogate pair recombines into one char — the same
            // rule the CMap destinations follow.
            let decoded: String = char::decode_utf16(units)
                .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect();
            return (!decoded.is_empty()).then_some(decoded);
        }
    }

    if let Some(rest) = component.strip_prefix('u') {
        if (4..=6).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return u32::from_str_radix(rest, 16)
                .ok()
                .and_then(char::from_u32)
                .map(String::from);
        }
    }

    None
}

/// The Adobe Glyph List, embedded in the library when it is compiled.
///
/// # Rust lesson: `include_str!`
///
/// `include_str!` reads a file **at compile time** and bakes its contents into
/// the binary as a `&'static str`. Nothing is read from disk when the program
/// runs, so the list travels inside the library — into the CLI and every Python
/// wheel — with no path to get wrong and no file to lose.
///
/// The path is relative to this source file. That is why the list lives inside
/// the `qalam-core` crate: when a crate is packaged, only files inside its own
/// directory go with it, and a file elsewhere in the repository would be
/// missing from a build made from that package.
///
/// The list is © Adobe, under the BSD-style licence reproduced at the top of
/// the file, which must stay with it.
const GLYPH_LIST: &str = include_str!("../resources/glyphlist.txt");

/// Names some producers use that the Adobe Glyph List does not define.
///
/// Kept deliberately tiny, and only for names seen in real fonts. `hyphenminus`
/// was already resolved before the full list was embedded, so dropping it
/// would have made those PDFs worse.
const EXTRA_NAMES: &[(&str, &str)] = &[("hyphenminus", "-")];

/// Look a name up in the Adobe Glyph List, or the few extra names above.
fn agl_lookup(name: &str) -> Option<String> {
    glyph_list().get(name).cloned().or_else(|| {
        EXTRA_NAMES
            .iter()
            .find(|(extra, _)| *extra == name)
            .map(|(_, text)| (*text).to_string())
    })
}

/// The parsed glyph list, built the first time it is needed.
///
/// # Rust lesson: `OnceLock`
///
/// Parsing 4,281 lines on every lookup would waste time, and parsing them when
/// the program starts would slow down programs that never read a glyph name.
/// `OnceLock` runs the closure passed to `get_or_init` exactly once, the first
/// time anyone asks, and every later call gets the same table. It is safe to
/// share between threads, which matters because the Python binding extracts
/// with the GIL released.
///
/// The keys are `&'static str` slices of `GLYPH_LIST` itself, so the names are
/// never copied.
fn glyph_list() -> &'static HashMap<&'static str, String> {
    static TABLE: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
    TABLE.get_or_init(|| parse_glyph_list(GLYPH_LIST))
}

/// Parse the `name;XXXX` lines of a glyph list.
///
/// A value may hold several code points separated by spaces
/// (`name;05D3 05B2`), so it becomes a `String`, not a `char`. Comment lines
/// start with `#`. A malformed line is skipped rather than half-read; the test
/// `the_embedded_glyph_list_parses_completely` makes sure the real file has
/// none.
fn parse_glyph_list(source: &str) -> HashMap<&str, String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (name, codes) = line.split_once(';')?;
            // `collect` into `Option<String>` stops at the first code point
            // that fails to parse, so a bad value never yields partial text.
            let text: Option<String> = codes
                .split_whitespace()
                .map(|hex| u32::from_str_radix(hex, 16).ok().and_then(char::from_u32))
                .collect();
            let text = text.filter(|t| !t.is_empty())?;
            Some((name, text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winansi_covers_ascii_and_the_1252_range() {
        assert_eq!(BaseEncoding::WinAnsi.to_char(b'1'), Some('1'));
        assert_eq!(BaseEncoding::WinAnsi.to_char(b'.'), Some('.'));
        // 0x92 is the right single quote in 1252, not a control character.
        assert_eq!(BaseEncoding::WinAnsi.to_char(0x92), Some('\u{2019}'));
        // 0x80 is the euro sign.
        assert_eq!(BaseEncoding::WinAnsi.to_char(0x80), Some('\u{20AC}'));
        // Latin-1 above 0xA0.
        assert_eq!(BaseEncoding::WinAnsi.to_char(0xE9), Some('é'));
        // Undefined slots and controls stay unresolved.
        assert_eq!(BaseEncoding::WinAnsi.to_char(0x81), None);
        assert_eq!(BaseEncoding::WinAnsi.to_char(0x07), None);
    }

    #[test]
    fn macroman_differs_from_winansi_above_ascii() {
        // 0x80 is A-diaeresis on the Mac, the euro sign on Windows. Picking the
        // wrong table silently corrupts every accented character.
        assert_eq!(BaseEncoding::MacRoman.to_char(0x80), Some('Ä'));
        assert_eq!(BaseEncoding::WinAnsi.to_char(0x80), Some('\u{20AC}'));
        assert_eq!(BaseEncoding::MacRoman.to_char(b'A'), Some('A'));
    }

    #[test]
    fn standard_encoding_uses_typographic_quotes() {
        assert_eq!(BaseEncoding::Standard.to_char(0x27), Some('\u{2019}'));
        assert_eq!(BaseEncoding::Standard.to_char(0x60), Some('\u{2018}'));
        assert_eq!(BaseEncoding::Standard.to_char(b'A'), Some('A'));
    }

    #[test]
    fn identity_h_is_not_a_simple_encoding() {
        // It names a composite-font CMap; treating it as a byte table would be
        // a category error.
        assert_eq!(BaseEncoding::from_name("Identity-H"), None);
        assert_eq!(
            BaseEncoding::from_name("WinAnsiEncoding"),
            Some(BaseEncoding::WinAnsi)
        );
    }

    #[test]
    fn differences_override_the_base_table() {
        let enc = Encoding::new(
            Some("WinAnsiEncoding"),
            vec![
                (65, "alpha".to_string()),
                (66, "uni0627".to_string()),
                (68, "g42".to_string()),
            ],
        );
        // 65 would be 'A' in WinAnsi, but /Differences renamed it to `alpha`,
        // which the Adobe Glyph List resolves to Greek alpha.
        assert_eq!(enc.decode(65).as_deref(), Some("\u{03B1}"));
        // 66 resolves through the algorithmic uniXXXX form.
        assert_eq!(enc.decode(66).as_deref(), Some("\u{0627}"));
        // Untouched codes still come from the base table.
        assert_eq!(enc.decode(67).as_deref(), Some("C"));
        // A renamed code whose name resolves to nothing is unresolvable, and
        // must NOT silently fall back to the base table's 'D'.
        assert_eq!(enc.decode(68), None);
    }

    #[test]
    fn glyph_names_resolve_three_ways() {
        // uniXXXX, including several units in one name.
        assert_eq!(glyph_name_to_string("uni0627").as_deref(), Some("\u{0627}"));
        // The AGL spelling: one run of hex digits.
        assert_eq!(
            glyph_name_to_string("uni06440627").as_deref(),
            Some("\u{0644}\u{0627}")
        );
        // The repeated-prefix spelling some tools emit, same meaning.
        assert_eq!(
            glyph_name_to_string("uni0644uni0627").as_deref(),
            Some("\u{0644}\u{0627}")
        );
        // uXXXX with a scalar beyond the basic plane.
        assert_eq!(glyph_name_to_string("u1F600").as_deref(), Some("\u{1F600}"));
        // AGL names.
        assert_eq!(glyph_name_to_string("period").as_deref(), Some("."));
        assert_eq!(glyph_name_to_string("A").as_deref(), Some("A"));
        // A suffix names a variant of the same character.
        assert_eq!(glyph_name_to_string("one.oldstyle").as_deref(), Some("1"));
    }

    #[test]
    fn a_surrogate_pair_in_a_uni_name_becomes_one_char() {
        let s = glyph_name_to_string("uniD83DuniDE00").expect("valid pair");
        assert_eq!(s.chars().count(), 1);
        assert_eq!(s.chars().next(), Some('\u{1F600}'));
    }

    #[test]
    fn subsetter_names_are_unresolvable_not_guessed() {
        // These carry no Unicode information at all. Inventing one would be
        // exactly the silent corruption this project exists to prevent.
        assert_eq!(glyph_name_to_string("g42"), None);
        assert_eq!(glyph_name_to_string("cid1234"), None);
        assert_eq!(glyph_name_to_string("uni06"), None);
        assert_eq!(glyph_name_to_string(""), None);
    }

    #[test]
    fn the_embedded_glyph_list_parses_completely() {
        // Every non-comment line of the file must become an entry. A mismatch
        // means the file was truncated or corrupted, or the parser skipped
        // lines it should have read.
        let data_lines = GLYPH_LIST
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .count();
        assert_eq!(data_lines, 4281, "Adobe Glyph List 2.0 has 4,281 names");
        assert_eq!(glyph_list().len(), data_lines);
    }

    #[test]
    fn the_full_list_resolves_names_the_old_subset_could_not() {
        assert_eq!(glyph_name_to_string("alpha").as_deref(), Some("\u{03B1}"));
        assert_eq!(glyph_name_to_string("Aacute").as_deref(), Some("\u{00C1}"));
        // Arabic, under both of the AGL's naming schemes.
        assert_eq!(
            glyph_name_to_string("afii57415").as_deref(),
            Some("\u{0627}")
        );
        assert_eq!(
            glyph_name_to_string("alefarabic").as_deref(),
            Some("\u{0627}")
        );
        assert_eq!(
            glyph_name_to_string("lamarabic").as_deref(),
            Some("\u{0644}")
        );
    }

    #[test]
    fn a_list_entry_may_hold_several_code_points() {
        assert_eq!(
            glyph_name_to_string("hamzadammaarabic").as_deref(),
            Some("\u{0621}\u{064F}")
        );
    }

    #[test]
    fn names_the_old_subset_resolved_still_resolve() {
        assert_eq!(glyph_name_to_string("space").as_deref(), Some(" "));
        assert_eq!(
            glyph_name_to_string("quoteright").as_deref(),
            Some("\u{2019}")
        );
        assert_eq!(glyph_name_to_string("nbspace").as_deref(), Some("\u{00A0}"));
        // Not an AGL name, but resolved before the full list, so kept.
        assert_eq!(glyph_name_to_string("hyphenminus").as_deref(), Some("-"));
    }

    #[test]
    fn a_ligature_name_resolves_each_component() {
        assert_eq!(glyph_name_to_string("f_i").as_deref(), Some("fi"));
        assert_eq!(
            glyph_name_to_string("uni0644_uni0627").as_deref(),
            Some("\u{0644}\u{0627}")
        );
        assert_eq!(glyph_name_to_string("f_f_i.alt").as_deref(), Some("ffi"));
    }

    #[test]
    fn one_unresolvable_component_makes_the_whole_name_unresolvable() {
        // The spec would keep the `f` and drop `g42`, silently losing a
        // character. Reporting the name as unresolvable is the honest answer.
        assert_eq!(glyph_name_to_string("f_g42"), None);
        assert_eq!(glyph_name_to_string("f__i"), None);
        assert_eq!(glyph_name_to_string("_"), None);
    }

    #[test]
    fn a_malformed_glyph_list_line_is_skipped_not_half_read() {
        let table =
            parse_glyph_list("# comment\nA;0041\nbroken\nX;ZZZZ\nY;0059 QQ\nBC;0042 0043\n");
        assert_eq!(table.len(), 2);
        assert_eq!(table.get("A").map(String::as_str), Some("A"));
        assert_eq!(table.get("BC").map(String::as_str), Some("BC"));
        assert!(
            !table.contains_key("Y"),
            "a partly valid value must not be kept"
        );
    }

    #[test]
    fn an_empty_encoding_resolves_nothing() {
        let enc = Encoding::default();
        assert!(enc.is_empty());
        assert_eq!(enc.decode(65), None);
    }

    #[test]
    fn two_byte_codes_are_not_this_paths_business() {
        let enc = Encoding::new(Some("WinAnsiEncoding"), Vec::new());
        assert_eq!(enc.decode(0x0113), None);
    }
}
