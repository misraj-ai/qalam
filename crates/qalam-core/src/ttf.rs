//! **L2 (part 3) — the TrueType `cmap` table.**
//!
//! A composite PDF font embeds its glyphs as a TrueType program (`/FontFile2`).
//! Inside that program, the `cmap` table says what character each glyph stands
//! for. That identity is what the original font was *built* around, and no PDF
//! writer can change it without also rebuilding the outlines — which is exactly
//! why it is a trustworthy second opinion when a `/ToUnicode` map lies.
//!
//! The table is the mirror image of the map we want: it is written
//! **Unicode → glyph index**, and we need **glyph index → character**. So this
//! module parses it and inverts it.
//!
//! Only the two formats that Arabic fonts actually use are supported in full:
//!
//! - **format 4** — BMP segment mapping (`segCount` + ranges), the Windows
//!   Unicode table every shaper-ready font carries;
//! - **format 12** — full-repertoire groups, for faces that reach past the BMP.
//!
//! Formats 0 and 6 (tiny fixed tables) are read defensively; every other format
//! is skipped rather than guessed at. The whole parser is bounds-checked: a font
//! program is attacker-controlled bytes, and a malformed table must yield a
//! partial map, never a panic.

use std::collections::HashMap;

/// Read a big-endian `u16` at `offset`, or `None` past the end of `bytes`.
fn u16be(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

/// Read a big-endian `u32` at `offset`, or `None` past the end of `bytes`.
fn u32be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
        *bytes.get(offset + 2)?,
        *bytes.get(offset + 3)?,
    ]))
}

/// Whether a character is an Arabic **presentation form** (a shaped variant).
///
/// When several codepoints share one glyph, the presentation form is the one
/// that records the glyph's actual shape; the base letter is its logical
/// summary. We prefer the former so the font's own geometry reaches L3, whose
/// NFKC step folds it back to the base letter anyway.
fn is_presentation(c: char) -> bool {
    let u = c as u32;
    (0xFB50..=0xFDFF).contains(&u) || (0xFE70..=0xFEFF).contains(&u)
}

/// Insert one (glyph, character) pair, preferring Arabic presentation forms.
fn insert(map: &mut HashMap<u32, char>, glyph: u32, ch: char) {
    if glyph == 0 {
        return;
    }
    match map.get(&glyph) {
        Some(&existing) if is_presentation(ch) && !is_presentation(existing) => {
            map.insert(glyph, ch);
        }
        None => {
            map.insert(glyph, ch);
        }
        _ => {}
    }
}

/// Parse every usable `cmap` subtable of an embedded font program.
///
/// Returns glyph index → character. `None`-ish returns: a program with no
/// parseable `cmap` yields an empty map, which callers treat as "no font
/// identity known" rather than as an error — a partial map is strictly better
/// than none, and the recoverability detector sees the gaps by counting how
/// many codes resolve.
///
/// Subtables on the Macintosh platform are skipped: their encodings are not
/// Unicode and mapping them through here would quietly misname glyphs.
pub fn cmap_glyph_map(program: &[u8]) -> HashMap<u32, char> {
    let mut out = HashMap::new();

    let Some(cm_start) = cmap_table_offset(program) else {
        return out;
    };
    let Some(num_tables) = u16be(program, cm_start + 2) else {
        return out;
    };

    for i in 0..num_tables {
        let rec = cm_start + 4 + usize::from(i) * 8;
        let (Some(platform), Some(encoding)) = (u16be(program, rec), u16be(program, rec + 2))
        else {
            break;
        };

        // Only Unicode-bearing subtables: the Unicode platform itself, and
        // Windows' Unicode BMP / full-repertoire encodings.
        let unicode = platform == 0 || (platform == 3 && matches!(encoding, 1 | 10));
        if !unicode {
            continue;
        }

        let Some(sub_rel) = u32be(program, rec + 4) else {
            break;
        };
        let Some(sub) = cm_start.checked_add(sub_rel as usize) else {
            break;
        };

        match u16be(program, sub).unwrap_or(0) {
            0 => read_format_0(program, sub, &mut out),
            4 => read_format_4(program, sub, &mut out),
            6 => read_format_6(program, sub, &mut out),
            12 => read_format_12(program, sub, &mut out),
            _ => {} // Unknown format: skip rather than misinterpret.
        }
    }
    out
}

/// Locate the `cmap` table in the sfnt table directory.
fn cmap_table_offset(program: &[u8]) -> Option<usize> {
    let num_tables = u16be(program, 4)? as usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = program.get(rec..rec + 4)?;
        if tag == b"cmap" {
            return u32be(program, rec + 8).map(|p| p as usize);
        }
    }
    None
}

/// Format 0: one glyph index per byte, in codepoint order.
fn read_format_0(bytes: &[u8], sub: usize, out: &mut HashMap<u32, char>) {
    // header (6 bytes) + 256 entries
    if bytes.len().saturating_sub(sub) < 262 {
        return;
    }
    for code in 0..256u16 {
        if let Some(&g) = bytes.get(sub + 6 + usize::from(code)) {
            insert(
                out,
                u32::from(g),
                char::from_u32(u32::from(code)).unwrap_or('\u{FFFD}'),
            );
        }
    }
}

/// Format 6: trimmed array — `firstCode` + one glyph index per codepoint.
fn read_format_6(bytes: &[u8], sub: usize, out: &mut HashMap<u32, char>) {
    let (Some(first), Some(count)) = (u16be(bytes, sub + 6), u16be(bytes, sub + 8)) else {
        return;
    };
    for i in 0..count {
        let Some(g) = u16be(bytes, sub + 10 + usize::from(i) * 2) else {
            break;
        };
        let code = u32::from(first) + u32::from(i);
        insert(
            out,
            u32::from(g),
            char::from_u32(code).unwrap_or('\u{FFFD}'),
        );
    }
}

/// Format 4: the common BMP table — codepoint segments with delta/offset pairs.
fn read_format_4(bytes: &[u8], sub: usize, out: &mut HashMap<u32, char>) {
    let Some(seg_count_x2) = u16be(bytes, sub + 6) else {
        return;
    };
    let seg_count = usize::from(seg_count_x2 / 2);
    if seg_count == 0 {
        return;
    }

    // Array layout, all relative to the subtable start:
    //   endCode[seg]  startCode[seg]  idDelta[seg]  idRangeOffset[seg]
    // The glyph index addressed by `idRangeOffset` is relative to that very
    // entry, so `glyphIdArray` needs no explicit base of its own.
    let end_base = sub + 14;
    let start_base = end_base + seg_count * 2 + 2; // + reservedPad
    let delta_base = start_base + seg_count * 2;
    let range_base = delta_base + seg_count * 2;

    for seg in 0..seg_count {
        let (Some(end), Some(start)) = (
            u16be(bytes, end_base + seg * 2),
            u16be(bytes, start_base + seg * 2),
        ) else {
            break;
        };
        // The sentinel segment (endCode 0xFFFF) terminates the display list.
        if start > end || end == 0xFFFF {
            continue;
        }

        let delta = u16be(bytes, delta_base + seg * 2).unwrap_or(0);
        let range_offset = u16be(bytes, range_base + seg * 2).unwrap_or(0);

        for code in u32::from(start)..=u32::from(end) {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            // `idRangeOffset == 0`: glyph = (code + delta), wrapping in 16 bits.
            // Otherwise the glyph index is stored inline in `glyphIdArray`,
            // addressed relative to *this* range-offset entry.
            let glyph = if range_offset == 0 {
                let c = match u16::try_from(code) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                u32::from(c.wrapping_add(delta))
            } else {
                let Some(addr) = (range_base + seg * 2)
                    .checked_add(usize::from(range_offset))
                    .and_then(|a| a.checked_add((code as usize - start as usize) * 2))
                else {
                    break;
                };
                match u16be(bytes, addr) {
                    // A zero entry remains the missing glyph. Every other
                    // glyph-array value is adjusted by idDelta just like the
                    // format-4 specification requires.
                    Some(0) => 0,
                    Some(g) => u32::from(g.wrapping_add(delta)),
                    None => break,
                }
            };
            insert(out, glyph, ch);
        }
    }
}

/// Format 12: full-repertoire groups — (start char, end char, start glyph).
fn read_format_12(bytes: &[u8], sub: usize, out: &mut HashMap<u32, char>) {
    let Some(num_groups) = u32be(bytes, sub + 12) else {
        return;
    };
    let groups_base = sub + 16;
    for group in 0..num_groups {
        let g = groups_base + group as usize * 12;
        let (Some(start), Some(end), Some(start_glyph)) =
            (u32be(bytes, g), u32be(bytes, g + 4), u32be(bytes, g + 8))
        else {
            break;
        };
        if end < start {
            continue;
        }
        // A plausible run is one codepoint per char; anything beyond a 65k
        // run is a malformed table we decline to expand.
        let Some(run) = end.checked_sub(start).and_then(|n| n.checked_add(1)) else {
            continue;
        };
        if run > 0x1_0000 {
            continue;
        }
        for (i, code) in (start..=end).enumerate() {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            let glyph = start_glyph.saturating_add(i as u32);
            insert(out, glyph, ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn push_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_be_bytes());
    }

    #[track_caller]
    fn push_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Assemble a minimal sfnt whose only table is `cmap`, holding the given
    /// subtable blobs. Subtable 0 is (3,1) Windows BMP; the rest are (0,3).
    fn build_program(subtables: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = Vec::new();
        push_u32(&mut buf, 0x0001_0000);
        assert!(!subtables.is_empty());
        push_u16(&mut buf, 1); // numTables
        push_u16(&mut buf, 0); // searchRange
        push_u16(&mut buf, 0); // entrySelector
        push_u16(&mut buf, 0); // rangeShift
        buf.extend_from_slice(b"cmap");
        push_u32(&mut buf, 0); // checksum (unused)

        let cmap_off = 12u32 + 16;
        push_u32(&mut buf, cmap_off);

        // The directory record is 16 bytes (tag, checksum, offset, length);
        // the offset above pins the table right behind it.
        let mut cmap_len = 4u32 + 8 * subtables.len() as u32;
        for st in subtables {
            cmap_len += st.len() as u32;
        }
        push_u32(&mut buf, cmap_len);
        assert_eq!(buf.len(), 28);
        push_u16(&mut buf, 0); // version
        push_u16(&mut buf, subtables.len() as u16);

        // Offsets are relative to the cmap table start: forward-computed.
        let mut offs = Vec::new();
        let mut cur = 4u32 + 8 * subtables.len() as u32;
        for st in subtables {
            offs.push(cur);
            cur += st.len() as u32;
        }
        for (i, _st) in subtables.iter().enumerate() {
            let (platform, encoding) = if i == 0 { (3u16, 1u16) } else { (0, 3) };
            push_u16(&mut buf, platform);
            push_u16(&mut buf, encoding);
            push_u32(&mut buf, offs[i]);
        }
        for st in subtables {
            buf.extend_from_slice(st);
        }
        buf
    }

    /// A format-4 subtable: `A..C` → glyphs 1..3 and `ا..ب` → glyphs 5..6 via
    /// the `idDelta` shortcut.
    fn format_4() -> Vec<u8> {
        let mut buf = Vec::new();
        push_u16(&mut buf, 4); // format
        push_u16(&mut buf, 40); // length
        push_u16(&mut buf, 0); // language
        push_u16(&mut buf, 6); // segCountX2: three segments
        push_u16(&mut buf, 0); // searchRange
        push_u16(&mut buf, 0); // entrySelector
        push_u16(&mut buf, 0); // rangeShift
        for end in [0x0043u16, 0x0629, 0xFFFF] {
            push_u16(&mut buf, end);
        }
        push_u16(&mut buf, 0); // reservedPad
        for start in [0x0041u16, 0x0627, 0xFFFF] {
            push_u16(&mut buf, start);
        }
        // glyph = code + delta, wrapping: A..C → 1..3, ا..ب → 5, 6.
        push_u16(&mut buf, (1i32 - 0x0041i32) as u16);
        push_u16(&mut buf, (5i32 - 0x0627i32) as u16);
        push_u16(&mut buf, 1); // sentinel delta
        for _ in 0..3 {
            push_u16(&mut buf, 0); // idRangeOffset
        }
        buf
    }

    /// A format-12 subtable mapping neither base but the *presentation forms*
    /// alef-final and beh-initial onto the same glyphs 5 and 6.
    fn format_12() -> Vec<u8> {
        let mut buf = Vec::new();
        push_u16(&mut buf, 12);
        push_u16(&mut buf, 0); // reserved
        push_u32(&mut buf, 28); // length
        push_u32(&mut buf, 0); // language
        push_u32(&mut buf, 2); // numGroups
        push_u32(&mut buf, 0xFE8D); // alef-final
        push_u32(&mut buf, 0xFE8D);
        push_u32(&mut buf, 5);
        push_u32(&mut buf, 0xFE90); // beh-initial
        push_u32(&mut buf, 0xFE90);
        push_u32(&mut buf, 6);
        buf
    }

    /// A format-4 subtable using glyphIdArray and a non-zero idDelta.
    fn format_4_with_range_offset() -> Vec<u8> {
        let mut buf = Vec::new();
        push_u16(&mut buf, 4);
        push_u16(&mut buf, 34); // header + two segments + one glyph entry
        push_u16(&mut buf, 0);
        push_u16(&mut buf, 4); // two segments, including the sentinel
        push_u16(&mut buf, 0);
        push_u16(&mut buf, 0);
        push_u16(&mut buf, 0);
        push_u16(&mut buf, 0x0041); // endCode
        push_u16(&mut buf, 0xFFFF);
        push_u16(&mut buf, 0); // reservedPad
        push_u16(&mut buf, 0x0041); // startCode
        push_u16(&mut buf, 0xFFFF);
        push_u16(&mut buf, 5); // idDelta: stored glyph 2 becomes glyph 7
        push_u16(&mut buf, 1);
        push_u16(&mut buf, 4); // from this word to the glyph-array entry
        push_u16(&mut buf, 0);
        push_u16(&mut buf, 2); // glyphIdArray
        buf
    }

    #[test]
    fn format4_and_12_merge_with_presentation_preference() {
        let program = build_program(&[format_4(), format_12()]);
        let map = cmap_glyph_map(&program);

        // Latin, decoded simply.
        assert_eq!(map.get(&1), Some(&'A'));
        assert_eq!(map.get(&2), Some(&'B'));
        assert_eq!(map.get(&3), Some(&'C'));
        // ا and ب arrive as presentation forms once two subtables reach the
        // same glyph: the shaped form is what the glyph actually stands for.
        assert_eq!(map.get(&5), Some(&'\u{FE8D}'));
        assert_eq!(map.get(&6), Some(&'\u{FE90}'));
        // No glyph was invented for an unmapped index.
        assert_eq!(map.get(&4), None);
    }

    #[test]
    fn format4_applies_delta_to_glyph_array_entries() {
        let program = build_program(&[format_4_with_range_offset()]);
        let map = cmap_glyph_map(&program);

        assert_eq!(map.get(&7), Some(&'A'));
        assert_eq!(map.get(&2), None);
    }

    #[test]
    fn empty_or_truncated_program_yields_empty_map() {
        assert!(cmap_glyph_map(b"").is_empty());
        assert!(cmap_glyph_map(&[0, 1]).is_empty());
        // A valid directory with no tables.
        assert!(cmap_glyph_map(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]).is_empty());
    }

    #[test]
    fn truncated_format4_does_not_panic() {
        // A directory pointing at a cmap whose subtable is chopped off.
        let mut st = Vec::new();
        push_u16(&mut st, 4); // format
        push_u16(&mut st, 40); // advertises more than is written
        push_u16(&mut st, 0);
        st.truncate(6);
        let program = build_program(&[st]);
        let map = cmap_glyph_map(&program);
        assert!(map.is_empty());
    }

    #[test]
    fn overflowing_format12_group_is_skipped() {
        let mut st = Vec::new();
        push_u16(&mut st, 12);
        push_u16(&mut st, 0);
        push_u32(&mut st, 28);
        push_u32(&mut st, 0);
        push_u32(&mut st, 1);
        push_u32(&mut st, 0);
        push_u32(&mut st, u32::MAX);
        push_u32(&mut st, 1);

        let program = build_program(&[st]);
        assert!(cmap_glyph_map(&program).is_empty());
    }
}
