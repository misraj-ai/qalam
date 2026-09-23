//! **L2 (part 4) — reading the character map out of an embedded TrueType font.**
//!
//! The sibling of [`crate::cff`], for the other font format. Same argument,
//! different table: a font program is what the *renderer* consults, so it is
//! checked by every reader of the document, while `/ToUnicode` is an advisory
//! note that nothing renders and nobody proofreads.
//!
//! # What this reads, and which way round
//!
//! A TrueType `cmap` table answers the question a renderer asks: *"which glyph
//! do I draw for this character?"* — Unicode → glyph id. Text extraction needs
//! the opposite direction, so this module builds the map and then inverts it.
//!
//! Inversion is where the care goes, because the forward map is not injective.
//! A single glyph is routinely reachable from several codepoints:
//!
//! ```text
//! U+0622 ARABIC LETTER ALEF WITH MADDA ABOVE  ─┐
//!                                              ├─► glyph 897
//! U+FE81 ARABIC LETTER ALEF WITH MADDA, ISOLATED ┘
//! ```
//!
//! Both are true, but only one is useful here. L3 reorders text *while it is
//! still in presentation forms* and normalises afterwards (PLAN.md §3), so
//! handing it the base letter throws away the very distinction the reordering
//! depends on. When a glyph is reachable from both, this module keeps the
//! presentation form.
//!
//! # Why only some subtables
//!
//! A `cmap` holds several subtables for different platforms. Only the Windows
//! Unicode ones are read: format 4 (the Basic Multilingual Plane, which is
//! every Arabic font in existence) and format 12 (the full range).
//!
//! The Windows *Symbol* subtable `(3, 0)` is deliberately skipped. It maps
//! codes into the private-use area at `U+F000`, so inverting it yields `U+F0xx`
//! — not text, just a different way of saying "glyph 3". A wrong answer is
//! worse than no answer, because the layer above cannot tell it is wrong.
//!
//! # What this module does not do
//!
//! Outlines, hinting, metrics, `GSUB`, `post` glyph names: all skipped. The
//! only question asked is which character a glyph stands for.

use std::collections::HashMap;

/// An embedded TrueType font's glyph id → Unicode mapping.
///
/// The inverse of the font's own `cmap`, which is the direction text extraction
/// needs. Empty when the font carries no usable Unicode subtable — a normal
/// outcome, not an error, and the caller simply gets no second opinion.
#[derive(Debug, Clone, Default)]
pub struct GlyphUnicode {
    map: HashMap<u16, char>,
}

impl GlyphUnicode {
    /// The character this glyph stands for, if the font's `cmap` reaches it.
    pub fn get(&self, glyph: u16) -> Option<char> {
        self.map.get(&glyph).copied()
    }

    /// How many glyphs the font gives a character for.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing could be read.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every `(glyph, character)` pair, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (u16, char)> + '_ {
        self.map.iter().map(|(g, c)| (*g, *c))
    }
}

/// Read a TrueType font program and invert its `cmap`.
///
/// Never fails. A font we cannot parse — truncated, a format we do not read, a
/// bare CFF wearing a `.ttf` extension — yields an empty map, and the caller
/// carries on with the sources it already had.
pub fn glyph_unicode(program: &[u8]) -> GlyphUnicode {
    let Some(cmap) = table(program, b"cmap") else {
        return GlyphUnicode::default();
    };

    let Some(subtable) = best_subtable(cmap) else {
        return GlyphUnicode::default();
    };

    // Forward direction first — Unicode → glyph — because that is the shape the
    // font stores and the shape the parsers below produce.
    let mut forward: Vec<(u32, u16)> = Vec::new();
    match be16(subtable, 0) {
        Some(4) => read_format4(subtable, &mut forward),
        Some(12) => read_format12(subtable, &mut forward),
        _ => {}
    }

    let mut map: HashMap<u16, char> = HashMap::new();
    for (code, glyph) in forward {
        // Glyph 0 is `.notdef` by definition: the box a renderer draws when it
        // has nothing. It stands for no character.
        if glyph == 0 {
            continue;
        }
        let Some(ch) = char::from_u32(code) else {
            continue;
        };
        // A private-use codepoint is a font's internal label, not text. Letting
        // one in would turn "we do not know" into a confident wrong answer.
        if is_private_use(code) {
            continue;
        }

        match map.entry(glyph) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(ch);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if prefer(ch, *slot.get()) {
                    slot.insert(ch);
                }
            }
        }
    }

    add_kashida_variants(program, &mut map);

    GlyphUnicode { map }
}

/// The tatweel, U+0640 — the bar that stretches a word to justify a line.
pub const TATWEEL: char = '\u{0640}';

/// Find the stretched kashida glyphs and label them as tatweel.
///
/// # The problem
///
/// Justifying Arabic does not stretch the spaces, it stretches the *letters*:
/// the joining stroke between them grows. A font does that with alternate
/// glyphs — the same connecting bar at several widths — and picks between them
/// through `GSUB`, which is a table no `cmap` describes. So these glyphs are
/// unreachable from the character side, and a producer writing `/ToUnicode` has
/// to invent an entry for them. `30_doc3.pdf` calls them `ا`, `32_doc5.pdf`
/// calls them `د`, and a word stretched across four of them comes back as
/// `تاااااریخ` instead of `تاریخ`.
///
/// # The evidence
///
/// The variants are the *same drawing* as the font's own tatweel, at a
/// different width. In `30_doc3.pdf`'s Times New Roman Bold, the tatweel is
/// glyph `0x2F0` with bounding box `(-70, 293, 406, 565)`, and glyphs `0x31B`,
/// `0x467`, `0x468` and `0x469` are a one-contour outline with the identical
/// left edge and the identical top and bottom — only the right edge moves, from
/// 256 units to 2048. Out of 4,685 glyphs in that font, those are the only
/// four that match.
///
/// So the test is: same contour count, same left edge, same vertical extent as
/// the tatweel, and *not already named by the `cmap`* — that last condition
/// keeps this strictly additive, unable to overwrite a character the font
/// itself vouches for.
fn add_kashida_variants(program: &[u8], map: &mut HashMap<u16, char>) {
    // Which glyph is the tatweel? Only the font can say, and if it has no
    // tatweel at all then it has no stretched variants of one either.
    let Some((&tatweel_glyph, _)) = map.iter().find(|(_, ch)| **ch == TATWEEL) else {
        return;
    };

    let Some(reference) = glyph_outline(program, tatweel_glyph) else {
        return;
    };
    // A composite or empty glyph is no template to match against.
    if reference.contours <= 0 {
        return;
    }

    let Some(count) = table(program, b"maxp").and_then(|maxp| be16(maxp, 4)) else {
        return;
    };

    for glyph in 0..count {
        if map.contains_key(&glyph) {
            continue;
        }
        if glyph_outline(program, glyph).is_some_and(|o| o.is_stretch_of(&reference)) {
            map.insert(glyph, TATWEEL);
        }
    }
}

/// The header of one glyph outline: how many contours, and its bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outline {
    contours: i16,
    x_min: i16,
    y_min: i16,
    y_max: i16,
}

impl Outline {
    /// Is this the same drawing as `other`, widened or narrowed?
    ///
    /// Everything but the right edge has to match. A letter that merely happens
    /// to be short and wide still starts and ends at its own heights, so the
    /// vertical extent is what does the real work here.
    fn is_stretch_of(&self, other: &Outline) -> bool {
        self.contours == other.contours
            && self.x_min == other.x_min
            && self.y_min == other.y_min
            && self.y_max == other.y_max
    }
}

/// Read one glyph's header out of `glyf`, via the `loca` index.
///
/// `loca` is an array of offsets into `glyf`, one per glyph plus a final
/// sentinel, so glyph *n* occupies `loca[n]..loca[n + 1]`. Its entries are
/// 16-bit (halved, to stretch the range) or 32-bit, and `head` says which —
/// the one piece of cross-table trivia this module cannot avoid.
fn glyph_outline(program: &[u8], glyph: u16) -> Option<Outline> {
    let head = table(program, b"head")?;
    let long_offsets = match be16(head, 50)? {
        0 => false,
        1 => true,
        _ => return None,
    };

    let loca = table(program, b"loca")?;
    let at = |index: u32| -> Option<u32> {
        if long_offsets {
            be32(loca, (index as usize).checked_mul(4)?)
        } else {
            // Halved on the way in, so doubled on the way out.
            be16(loca, (index as usize).checked_mul(2)?).map(|o| u32::from(o) * 2)
        }
    };

    let start = at(u32::from(glyph))?;
    let end = at(u32::from(glyph) + 1)?;
    // Equal offsets mean a glyph with no outline — a space, most often.
    if end <= start {
        return None;
    }

    let glyf = table(program, b"glyf")?;
    let outline = glyf.get(start as usize..end as usize)?;

    Some(Outline {
        contours: be16(outline, 0)? as i16,
        x_min: be16(outline, 2)? as i16,
        y_min: be16(outline, 4)? as i16,
        y_max: be16(outline, 8)? as i16,
    })
}

/// Which of two characters for the same glyph to keep.
///
/// Presentation forms win, for the reason in the module header: L3 reorders
/// before it normalises, so the shaped form carries information the base letter
/// has already lost. Between two characters of the same kind the lower
/// codepoint wins — an arbitrary rule, but a *stable* one, so the same font
/// always produces the same map.
fn prefer(candidate: char, current: char) -> bool {
    match (
        is_presentation_form(candidate),
        is_presentation_form(current),
    ) {
        (true, false) => true,
        (false, true) => false,
        _ => candidate < current,
    }
}

/// Is this an Arabic presentation form — a shaped glyph rather than a letter?
///
/// The two Unicode blocks that hold them: Presentation Forms-A (ligatures and
/// the Quranic marks) and Presentation Forms-B (the four joining shapes of each
/// letter, plus lam-alef).
fn is_presentation_form(c: char) -> bool {
    matches!(c as u32, 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

/// The three private-use areas, whose codepoints mean whatever a font says.
fn is_private_use(code: u32) -> bool {
    matches!(code, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

/// Is this a character whose glyph is *mirrored* when it sits in RTL text?
///
/// These are the characters the font program cannot testify about. UAX#9 L4
/// says a renderer draws `(` with a `)` shape inside an RTL run, so a producer
/// laying out Arabic emits the glyph for the *mirrored* character. Reading that
/// glyph back gives the shape that was painted, not the character that was
/// meant, and the two are opposites — exactly the wrong answer rather than an
/// approximate one.
///
/// So the program abstains on these, and the layers above fall back to a source
/// that records logical order. `/ToUnicode` does, even in a file whose map is
/// otherwise untrustworthy: mirroring is a *layout* decision, applied after the
/// producer already knew which bracket it meant.
///
/// The list is the paired punctuation that appears in real documents, not the
/// whole `Bidi_Mirrored` property — which runs to hundreds of mathematical
/// symbols and would need a Unicode table this crate does not carry.
pub fn is_mirrored(c: char) -> bool {
    matches!(
        c,
        '(' | ')'
            | '[' | ']'
            | '{' | '}'
            | '<' | '>'
            // Guillemets, the Arabic quotation marks of choice.
            | '\u{00AB}' | '\u{00BB}'
            | '\u{2039}' | '\u{203A}'
            // Ornate parentheses, used around Quranic quotations.
            | '\u{FD3E}' | '\u{FD3F}'
    )
}

/// Locate one table in the font's directory.
///
/// A TrueType file opens with a fixed header and then a table of contents: a
/// four-byte tag, a checksum, an offset and a length for each table.
fn table<'a>(program: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    let count = be16(program, 4)? as usize;
    for i in 0..count {
        // 12 bytes of header, then 16 per directory entry.
        let entry = 12 + 16 * i;
        let found = program.get(entry..entry + 4)?;
        if found == tag {
            let offset = be32(program, entry + 8)? as usize;
            let length = be32(program, entry + 12)? as usize;
            // `get` rather than indexing: the offsets come from the file, so a
            // corrupt one must yield `None`, never a panic.
            return program.get(offset..offset.checked_add(length)?);
        }
    }
    None
}

/// Choose which `cmap` subtable to read.
///
/// The preference is the widest Windows Unicode table available: `(3, 10)`
/// covers the supplementary planes, `(3, 1)` the Basic Multilingual Plane.
/// Anything else — Macintosh tables, the Symbol table — is left alone, for the
/// reason in the module header.
fn best_subtable(cmap: &[u8]) -> Option<&[u8]> {
    let count = be16(cmap, 2)? as usize;
    let mut best: Option<(u16, &[u8])> = None;

    for i in 0..count {
        let record = 4 + 8 * i;
        let platform = be16(cmap, record)?;
        let encoding = be16(cmap, record + 2)?;
        let offset = be32(cmap, record + 4)? as usize;

        let rank = match (platform, encoding) {
            (3, 10) => 2,
            (3, 1) => 1,
            _ => continue,
        };

        let Some(subtable) = cmap.get(offset..) else {
            continue;
        };
        if best.is_none_or(|(best_rank, _)| rank > best_rank) {
            best = Some((rank, subtable));
        }
    }

    best.map(|(_, subtable)| subtable)
}

/// Format 4: segmented coverage of the Basic Multilingual Plane.
///
/// The workhorse format, and the fiddliest. Characters are grouped into
/// segments, and each segment resolves one of two ways: add a constant delta to
/// the character, or index into a shared glyph array. The second form is what
/// `idRangeOffset` encodes, and it is a byte offset *from its own position* —
/// a 1990s trick for keeping the table compact that every parser since has had
/// to reproduce.
fn read_format4(t: &[u8], out: &mut Vec<(u32, u16)>) {
    let Some(seg_x2) = be16(t, 6) else { return };
    let segments = (seg_x2 / 2) as usize;

    // The four parallel arrays, each `segments` entries long.
    let ends = 14;
    let starts = ends + seg_x2 as usize + 2; // +2 for the reserved padding word
    let deltas = starts + seg_x2 as usize;
    let offsets = deltas + seg_x2 as usize;

    for i in 0..segments {
        let (Some(end), Some(start), Some(delta), Some(range_offset)) = (
            be16(t, ends + 2 * i),
            be16(t, starts + 2 * i),
            be16(t, deltas + 2 * i),
            be16(t, offsets + 2 * i),
        ) else {
            return;
        };

        // The final segment is a sentinel mapping 0xFFFF; nothing to record.
        if start > end {
            continue;
        }

        for code in start..=end {
            let glyph = if range_offset == 0 {
                // `delta` is added modulo 65536 — wrapping is the specified
                // behaviour here, not an accident to be guarded against.
                code.wrapping_add(delta)
            } else {
                let at = offsets + 2 * i + range_offset as usize + 2 * (code - start) as usize;
                match be16(t, at) {
                    Some(0) | None => continue,
                    Some(glyph) => glyph.wrapping_add(delta),
                }
            };

            if glyph != 0 {
                out.push((u32::from(code), glyph));
            }

            // `start..=end` with `end == u16::MAX` would never terminate, since
            // the loop counter cannot go past it.
            if code == u16::MAX {
                break;
            }
        }
    }
}

/// Format 12: grouped coverage of the whole codepoint range.
///
/// Far simpler than format 4 — a flat list of `(first, last, first glyph)`
/// groups — because it was designed once the compactness tricks stopped being
/// worth their complexity.
fn read_format12(t: &[u8], out: &mut Vec<(u32, u16)>) {
    let Some(groups) = be32(t, 12) else { return };

    for i in 0..groups as usize {
        let at = 16 + 12 * i;
        let (Some(start), Some(end), Some(first_glyph)) =
            (be32(t, at), be32(t, at + 4), be32(t, at + 8))
        else {
            return;
        };
        if start > end {
            continue;
        }
        // A malformed group could claim millions of codepoints; the font has at
        // most 65,535 glyphs, so anything beyond that is noise.
        let span = (end - start).min(u16::MAX as u32);

        for k in 0..=span {
            let Ok(glyph) = u16::try_from(first_glyph + k) else {
                break;
            };
            if glyph != 0 {
                out.push((start + k, glyph));
            }
        }
    }
}

/// A big-endian `u16` at a byte offset, or `None` if it runs off the end.
fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    let pair = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([pair[0], pair[1]]))
}

/// A big-endian `u32` at a byte offset, or `None` if it runs off the end.
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let quad = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]]))
}

/// Byte-level builders for the tests, here rather than in the test module
/// so that `font.rs` can drive a whole `Font` from a synthetic program.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Assemble a minimal TrueType file holding one `cmap` table.
    ///
    /// Building the bytes by hand keeps the test honest: a fixture file could
    /// drift, and a real font would drag in tables this module never reads.
    pub fn font_with_cmap(subtable: &[u8], platform: u16, encoding: u16) -> Vec<u8> {
        let mut cmap = Vec::new();
        cmap.extend_from_slice(&0u16.to_be_bytes()); // version
        cmap.extend_from_slice(&1u16.to_be_bytes()); // one subtable
        cmap.extend_from_slice(&platform.to_be_bytes());
        cmap.extend_from_slice(&encoding.to_be_bytes());
        cmap.extend_from_slice(&12u32.to_be_bytes()); // offset to the subtable
        cmap.extend_from_slice(subtable);

        let mut font = Vec::new();
        font.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version
        font.extend_from_slice(&1u16.to_be_bytes()); // one table
        font.extend_from_slice(&[0; 6]); // searchRange, entrySelector, rangeShift
        font.extend_from_slice(b"cmap");
        font.extend_from_slice(&0u32.to_be_bytes()); // checksum
        font.extend_from_slice(&28u32.to_be_bytes()); // offset
        font.extend_from_slice(&(cmap.len() as u32).to_be_bytes());
        font.extend_from_slice(&cmap);
        font
    }

    /// A format 4 subtable in its simple form: one delta segment per range,
    /// plus the mandatory 0xFFFF sentinel.
    pub fn format4(segments: &[(u16, u16, u16)]) -> Vec<u8> {
        let count = segments.len() + 1;
        let seg_x2 = (count * 2) as u16;

        let mut t = Vec::new();
        t.extend_from_slice(&4u16.to_be_bytes()); // format
        t.extend_from_slice(&0u16.to_be_bytes()); // length, unread
        t.extend_from_slice(&0u16.to_be_bytes()); // language
        t.extend_from_slice(&seg_x2.to_be_bytes());
        t.extend_from_slice(&[0; 6]); // searchRange, entrySelector, rangeShift

        for (_, end, _) in segments {
            t.extend_from_slice(&end.to_be_bytes());
        }
        t.extend_from_slice(&u16::MAX.to_be_bytes()); // sentinel end
        t.extend_from_slice(&0u16.to_be_bytes()); // reserved padding

        for (start, _, _) in segments {
            t.extend_from_slice(&start.to_be_bytes());
        }
        t.extend_from_slice(&u16::MAX.to_be_bytes()); // sentinel start

        for (start, _, glyph) in segments {
            // `code + delta == glyph` for the first code in the segment.
            t.extend_from_slice(&glyph.wrapping_sub(*start).to_be_bytes());
        }
        t.extend_from_slice(&1u16.to_be_bytes()); // sentinel delta: 0xFFFF → 0

        for _ in 0..count {
            t.extend_from_slice(&0u16.to_be_bytes()); // idRangeOffset
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn a_windows_unicode_cmap_is_inverted() {
        // Two single-character segments: alef isolated and beh initial.
        let font = font_with_cmap(
            &format4(&[(0xFE8D, 0xFE8D, 10), (0xFE91, 0xFE91, 11)]),
            3,
            1,
        );
        let map = glyph_unicode(&font);

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(10), Some('\u{FE8D}'));
        assert_eq!(map.get(11), Some('\u{FE91}'));
        assert_eq!(map.get(12), None);
    }

    #[test]
    fn a_glyph_reachable_from_both_keeps_the_presentation_form() {
        // The case from the module header, and the reason `prefer` exists:
        // glyph 897 is reachable from the base letter *and* from its isolated
        // form. L3 reorders before it normalises, so the base letter would
        // throw away the distinction the reordering runs on.
        let font = font_with_cmap(
            &format4(&[(0x0622, 0x0622, 897), (0xFE81, 0xFE81, 897)]),
            3,
            1,
        );
        let map = glyph_unicode(&font);

        assert_eq!(map.len(), 1);
        assert_eq!(map.get(897), Some('\u{FE81}'));

        // And the choice does not depend on which segment came first.
        let flipped = font_with_cmap(
            &format4(&[(0xFE81, 0xFE81, 897), (0x0622, 0x0622, 897)]),
            3,
            1,
        );
        assert_eq!(glyph_unicode(&flipped).get(897), Some('\u{FE81}'));
    }

    #[test]
    fn the_symbol_subtable_is_not_read() {
        // `(3, 0)` maps into the private-use area, so inverting it would answer
        // `U+F041` — a glyph number wearing a codepoint's clothes. Refusing to
        // answer is the honest outcome.
        let font = font_with_cmap(&format4(&[(0xF041, 0xF041, 5)]), 3, 0);
        assert!(glyph_unicode(&font).is_empty());

        // The same codepoints in a table we *do* read are still refused, since
        // a private-use character is not text whichever table it came from.
        let windows = font_with_cmap(&format4(&[(0xF041, 0xF041, 5)]), 3, 1);
        assert!(glyph_unicode(&windows).is_empty());
    }

    #[test]
    fn mirrored_characters_are_recognised() {
        // Not a parser test: this is the list the layers above consult before
        // believing anything the font program says about a bracket.
        assert!(is_mirrored('('));
        assert!(is_mirrored(')'));
        assert!(is_mirrored('\u{00BB}'));
        // A letter or a digit is painted as itself whichever way the line runs.
        assert!(!is_mirrored('\u{0627}'));
        assert!(!is_mirrored('7'));
        assert!(!is_mirrored('.'));
    }

    #[test]
    fn a_truncated_font_yields_an_empty_map() {
        // Every offset in the file comes from the file, so a corrupt one must
        // produce nothing rather than a panic. L4 reports the silence.
        let font = font_with_cmap(&format4(&[(0xFE8D, 0xFE8D, 10)]), 3, 1);
        for cut in 0..font.len() {
            let _ = glyph_unicode(&font[..cut]);
        }
        assert!(glyph_unicode(&[]).is_empty());
        assert!(glyph_unicode(b"not a font at all").is_empty());
    }
}
