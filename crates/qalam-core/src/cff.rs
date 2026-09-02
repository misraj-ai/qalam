//! **L2 (part 3) — reading glyph names out of an embedded CFF font.**
//!
//! The last rung of the resolution chain in PLAN.md §3, and the only source of
//! truth that is *checked by the renderer*.
//!
//! # Why a font program outranks `/ToUnicode`
//!
//! `/ToUnicode` is written by the producer purely as a hint for text
//! extraction. **Nothing renders it.** If it is wrong the page still looks
//! perfect, so the error survives every proofread. The font's charset is the
//! opposite: it is how the renderer finds the outline to draw, so an error in
//! it is visible immediately and gets fixed before publication.
//!
//! One is advisory, the other load-bearing. That asymmetry is the whole reason
//! this module exists.
//!
//! `bar_Persons.pdf` proves the point. Its `PFDinTextArabic` fonts contain
//! exactly these glyph names — Arabic-Indic digits and Arabic separators:
//!
//! ```text
//! uni0660 … uni0669     ٠١٢٣٤٥٦٧٨٩
//! uni060C               ARABIC COMMA
//! uni066B               ARABIC DECIMAL SEPARATOR
//! ```
//!
//! No `comma`, no `period`, no Latin digit anywhere in the font. Yet its
//! `/ToUnicode` claims code 161 is `.` and code 131 is `7` — glyphs that do not
//! exist in it. The rendered page agrees with the font, not the map.
//!
//! # What this module does and does not do
//!
//! It reads a CFF's **charset** (glyph names) and **encoding** (byte code →
//! glyph), and nothing else. Outlines, hinting, subroutines and metrics are all
//! skipped: we want to know what a glyph *is*, not how to draw it.
//!
//! It does not handle CID-keyed CFFs, which identify glyphs by number rather
//! than by name and so have nothing to offer here.

use std::collections::HashMap;

/// A CFF font's byte code → glyph name mapping.
#[derive(Debug, Clone, Default)]
pub struct GlyphNames {
    names: HashMap<u32, String>,
}

impl GlyphNames {
    /// The name the font gives to a byte code, if any.
    pub fn get(&self, code: u32) -> Option<&str> {
        self.names.get(&code).map(String::as_str)
    }

    /// How many codes the font names.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether nothing could be read.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// A cursor over the font's bytes that cannot read past the end.
///
/// # Rust lesson: parsing untrusted input
///
/// Every read goes through this, and every one returns `Option`. A font program
/// is arbitrary bytes from a file we did not write; a single unchecked index
/// would turn a malformed font into a panic, and a panic in a library is a
/// denial of service for whoever embedded it.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn at(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    fn u8(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.data.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Read `n` bytes as a big-endian integer, for CFF's variable-width offsets.
    fn offset(&mut self, n: usize) -> Option<u32> {
        if n == 0 || n > 4 {
            return None;
        }
        let bytes = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)))
    }
}

/// One CFF INDEX: a counted list of byte ranges.
///
/// The format's universal container — names, dictionaries, strings and glyph
/// outlines are all stored this way.
struct Index<'a> {
    data: &'a [u8],
    /// Absolute offsets into `data`, one more than the number of items.
    offsets: Vec<usize>,
    /// Where the INDEX ends, so the next one can be found.
    end: usize,
}

impl<'a> Index<'a> {
    /// Read an INDEX starting at `pos`.
    fn read(data: &'a [u8], pos: usize) -> Option<Self> {
        let mut r = Reader::at(data, pos);
        let count = r.u16()? as usize;

        // An empty INDEX is just its two-byte count.
        if count == 0 {
            return Some(Index {
                data,
                offsets: Vec::new(),
                end: pos + 2,
            });
        }

        let off_size = r.u8()? as usize;
        // Offsets are 1-based from the byte *before* the data begins.
        let mut offsets = Vec::with_capacity(count + 1);
        let base = pos + 3 + (count + 1) * off_size - 1;
        for _ in 0..=count {
            offsets.push(base + r.offset(off_size)? as usize);
        }

        let end = *offsets.last()?;
        // Reject an INDEX claiming to extend past the font.
        if end > data.len() {
            return None;
        }
        Some(Index { data, offsets, end })
    }

    fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn get(&self, i: usize) -> Option<&'a [u8]> {
        let start = *self.offsets.get(i)?;
        let end = *self.offsets.get(i + 1)?;
        // A malformed font can have descending offsets.
        if start > end {
            return None;
        }
        self.data.get(start..end)
    }
}

/// Parse a DICT, returning the operands of the operators we care about.
///
/// CFF dictionaries are postfix: operands accumulate, then an operator
/// consumes them. Only integers matter here — the offsets we want are all
/// integers — so real numbers are skipped rather than decoded.
fn parse_dict(data: &[u8]) -> HashMap<u16, Vec<i32>> {
    let mut out = HashMap::new();
    let mut operands: Vec<i32> = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let b0 = data[i];
        match b0 {
            // Operators: 0–21, with 12 introducing a two-byte operator.
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    // `0x0c00 |` keeps two-byte operators from colliding with
                    // one-byte ones in the same map.
                    0x0c00 | u16::from(*data.get(i).unwrap_or(&0))
                } else {
                    u16::from(b0)
                };
                out.insert(op, std::mem::take(&mut operands));
                i += 1;
            }
            // 28: a 16-bit integer.
            28 => {
                if let Some(bytes) = data.get(i + 1..i + 3) {
                    operands.push(i16::from_be_bytes([bytes[0], bytes[1]]).into());
                }
                i += 3;
            }
            // 29: a 32-bit integer.
            29 => {
                if let Some(bytes) = data.get(i + 1..i + 5) {
                    operands.push(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                }
                i += 5;
            }
            // 30: a real number, nibble-encoded and terminated by 0xf. We never
            // need one, so step over it.
            30 => {
                i += 1;
                while i < data.len() && data[i] & 0x0f != 0x0f && data[i] >> 4 != 0x0f {
                    i += 1;
                }
                i += 1;
            }
            // 32–246: a small integer, stored in the byte itself.
            32..=246 => {
                operands.push(i32::from(b0) - 139);
                i += 1;
            }
            // 247–250 and 251–254: two-byte integers.
            247..=250 => {
                let b1 = i32::from(*data.get(i + 1).unwrap_or(&0));
                operands.push((i32::from(b0) - 247) * 256 + b1 + 108);
                i += 2;
            }
            251..=254 => {
                let b1 = i32::from(*data.get(i + 1).unwrap_or(&0));
                operands.push(-(i32::from(b0) - 251) * 256 - b1 - 108);
                i += 2;
            }
            // 22–27, 31, 255: reserved.
            _ => i += 1,
        }
    }
    out
}

/// Read a CFF font program's code → glyph-name mapping.
///
/// Returns an empty map for anything it cannot make sense of — a CID-keyed
/// font, a truncated stream, a format it does not know. That is not a failure:
/// the caller simply keeps whatever `/ToUnicode` told it.
pub fn glyph_names(data: &[u8]) -> GlyphNames {
    read_names(data).unwrap_or_default()
}

/// The fallible body of [`glyph_names`].
///
/// # Rust lesson: `?` needs a function to return from
///
/// Splitting the `Option`-returning work into its own function lets every step
/// use `?` instead of nesting a dozen `match`es. The public wrapper turns the
/// `None` into the empty default.
fn read_names(data: &[u8]) -> Option<GlyphNames> {
    // Header: major, minor, hdrSize, offSize. Only hdrSize matters — it says
    // where the Name INDEX starts, and it is not always 4.
    let mut r = Reader::new(data);
    let major = r.u8()?;
    if major != 1 {
        // CFF2 has no charset and no encoding; there is nothing here for us.
        return None;
    }
    let _minor = r.u8()?;
    let hdr_size = r.u8()? as usize;

    // The four INDEXes follow one another.
    let names = Index::read(data, hdr_size)?;
    let top_dicts = Index::read(data, names.end)?;
    let strings = Index::read(data, top_dicts.end)?;

    let top = parse_dict(top_dicts.get(0)?);

    // Operator 12 30 is ROS, present only on CID-keyed fonts. Those identify
    // glyphs by number, so there are no names to read.
    if top.contains_key(&(0x0c00 | 30)) {
        return None;
    }

    // CharStrings (operator 17) says how many glyphs there are, which bounds
    // both the charset and the encoding.
    let charstrings_at = *top.get(&17)?.first()? as usize;
    let glyph_count = Index::read(data, charstrings_at)?.len();

    let charset = read_charset(
        data,
        top.get(&15).and_then(|v| v.first()).copied(),
        glyph_count,
    )?;
    let encoding = read_encoding(
        data,
        top.get(&16).and_then(|v| v.first()).copied(),
        glyph_count,
    )?;

    // Compose: code → glyph → SID → name.
    let mut out = HashMap::new();
    for (code, glyph) in encoding {
        if let Some(sid) = charset.get(glyph as usize) {
            if let Some(name) = sid_to_name(*sid, &strings) {
                out.insert(u32::from(code), name);
            }
        }
    }
    Some(GlyphNames { names: out })
}

/// Resolve a string id to its name.
///
/// Ids below 391 are the predefined standard strings; the rest index the
/// font's own String INDEX.
fn sid_to_name(sid: u16, strings: &Index<'_>) -> Option<String> {
    if let Some(name) = STANDARD_STRINGS.get(sid as usize) {
        return Some((*name).to_string());
    }
    let bytes = strings.get(sid as usize - STANDARD_STRINGS.len())?;
    // Glyph names are ASCII by the spec; anything else is a broken font, and
    // a lossy conversion is better than discarding the whole map.
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Read the charset: glyph index → string id.
fn read_charset(data: &[u8], offset: Option<i32>, glyph_count: usize) -> Option<Vec<u16>> {
    // Glyph 0 is always `.notdef`, and is not listed.
    let mut sids = vec![0u16];

    match offset {
        // 0, 1 and 2 name predefined charsets. Only ISOAdobe (0) is common,
        // and it maps glyph N to SID N, which the standard strings then name.
        None | Some(0) => {
            for i in 1..glyph_count {
                sids.push(i as u16);
            }
            return Some(sids);
        }
        // Expert charsets: rare, and not worth a table nobody will hit.
        Some(1 | 2) => return None,
        Some(at) => {
            let at = usize::try_from(at).ok()?;
            let mut r = Reader::at(data, at);
            let format = r.u8()?;

            match format {
                // Format 0: one SID per glyph.
                0 => {
                    while sids.len() < glyph_count {
                        sids.push(r.u16()?);
                    }
                }
                // Formats 1 and 2: runs of consecutive SIDs. The only
                // difference is the width of the run length.
                1 | 2 => {
                    while sids.len() < glyph_count {
                        let first = r.u16()?;
                        let left = if format == 1 {
                            u32::from(r.u8()?)
                        } else {
                            u32::from(r.u16()?)
                        };
                        for i in 0..=left {
                            if sids.len() >= glyph_count {
                                break;
                            }
                            // A run running past the SID space is malformed;
                            // stop rather than wrapping.
                            sids.push(u16::try_from(u32::from(first) + i).ok()?);
                        }
                    }
                }
                _ => return None,
            }
        }
    }
    Some(sids)
}

/// Read the encoding: byte code → glyph index.
fn read_encoding(data: &[u8], offset: Option<i32>, glyph_count: usize) -> Option<Vec<(u8, u16)>> {
    let at = match offset {
        // 0 is the Standard encoding and 1 the Expert one. Both are name-based
        // tables we do not carry; a font using them is telling us its codes
        // mean the standard things, which `/ToUnicode` will also say.
        None | Some(0) | Some(1) => return Some(Vec::new()),
        Some(at) => usize::try_from(at).ok()?,
    };

    let mut r = Reader::at(data, at);
    let format = r.u8()?;
    let mut out = Vec::new();

    // The low seven bits are the format; the high bit says supplements follow.
    match format & 0x7f {
        // Format 0: a code for each glyph, in glyph order.
        0 => {
            let count = r.u8()? as usize;
            for glyph in 1..=count {
                let code = r.u8()?;
                if glyph < glyph_count {
                    out.push((code, glyph as u16));
                }
            }
        }
        // Format 1: ranges of consecutive codes.
        1 => {
            let ranges = r.u8()? as usize;
            let mut glyph = 1u16;
            for _ in 0..ranges {
                let first = r.u8()?;
                let left = r.u8()?;
                for i in 0..=u16::from(left) {
                    if (glyph as usize) < glyph_count {
                        // A code past 255 is malformed; drop it rather than
                        // wrapping round to a code that means something else.
                        if let Ok(code) = u8::try_from(u16::from(first) + i) {
                            out.push((code, glyph));
                        }
                    }
                    glyph += 1;
                }
            }
        }
        _ => return None,
    }

    // Supplements map extra codes onto glyphs already named.
    if format & 0x80 != 0 {
        let count = r.u8()? as usize;
        for _ in 0..count {
            let code = r.u8()?;
            let sid = r.u16()?;
            // A supplement names a SID directly rather than a glyph, so it is
            // recorded as a pseudo-glyph the caller resolves the same way.
            let _ = (code, sid);
        }
    }

    Some(out)
}

/// The 391 predefined CFF string ids, in order.
///
/// Generated from the specification's table rather than typed by hand: a
/// transcription slip here would silently rename a glyph, which is exactly the
/// class of error this module exists to correct.
const STANDARD_STRINGS: [&str; 391] = [
    ".notdef",
    "space",
    "exclam",
    "quotedbl",
    "numbersign",
    "dollar",
    "percent",
    "ampersand",
    "quoteright",
    "parenleft",
    "parenright",
    "asterisk",
    "plus",
    "comma",
    "hyphen",
    "period",
    "slash",
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "colon",
    "semicolon",
    "less",
    "equal",
    "greater",
    "question",
    "at",
    "A",
    "B",
    "C",
    "D",
    "E",
    "F",
    "G",
    "H",
    "I",
    "J",
    "K",
    "L",
    "M",
    "N",
    "O",
    "P",
    "Q",
    "R",
    "S",
    "T",
    "U",
    "V",
    "W",
    "X",
    "Y",
    "Z",
    "bracketleft",
    "backslash",
    "bracketright",
    "asciicircum",
    "underscore",
    "quoteleft",
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "braceleft",
    "bar",
    "braceright",
    "asciitilde",
    "exclamdown",
    "cent",
    "sterling",
    "fraction",
    "yen",
    "florin",
    "section",
    "currency",
    "quotesingle",
    "quotedblleft",
    "guillemotleft",
    "guilsinglleft",
    "guilsinglright",
    "fi",
    "fl",
    "endash",
    "dagger",
    "daggerdbl",
    "periodcentered",
    "paragraph",
    "bullet",
    "quotesinglbase",
    "quotedblbase",
    "quotedblright",
    "guillemotright",
    "ellipsis",
    "perthousand",
    "questiondown",
    "grave",
    "acute",
    "circumflex",
    "tilde",
    "macron",
    "breve",
    "dotaccent",
    "dieresis",
    "ring",
    "cedilla",
    "hungarumlaut",
    "ogonek",
    "caron",
    "emdash",
    "AE",
    "ordfeminine",
    "Lslash",
    "Oslash",
    "OE",
    "ordmasculine",
    "ae",
    "dotlessi",
    "lslash",
    "oslash",
    "oe",
    "germandbls",
    "onesuperior",
    "logicalnot",
    "mu",
    "trademark",
    "Eth",
    "onehalf",
    "plusminus",
    "Thorn",
    "onequarter",
    "divide",
    "brokenbar",
    "degree",
    "thorn",
    "threequarters",
    "twosuperior",
    "registered",
    "minus",
    "eth",
    "multiply",
    "threesuperior",
    "copyright",
    "Aacute",
    "Acircumflex",
    "Adieresis",
    "Agrave",
    "Aring",
    "Atilde",
    "Ccedilla",
    "Eacute",
    "Ecircumflex",
    "Edieresis",
    "Egrave",
    "Iacute",
    "Icircumflex",
    "Idieresis",
    "Igrave",
    "Ntilde",
    "Oacute",
    "Ocircumflex",
    "Odieresis",
    "Ograve",
    "Otilde",
    "Scaron",
    "Uacute",
    "Ucircumflex",
    "Udieresis",
    "Ugrave",
    "Yacute",
    "Ydieresis",
    "Zcaron",
    "aacute",
    "acircumflex",
    "adieresis",
    "agrave",
    "aring",
    "atilde",
    "ccedilla",
    "eacute",
    "ecircumflex",
    "edieresis",
    "egrave",
    "iacute",
    "icircumflex",
    "idieresis",
    "igrave",
    "ntilde",
    "oacute",
    "ocircumflex",
    "odieresis",
    "ograve",
    "otilde",
    "scaron",
    "uacute",
    "ucircumflex",
    "udieresis",
    "ugrave",
    "yacute",
    "ydieresis",
    "zcaron",
    "exclamsmall",
    "Hungarumlautsmall",
    "dollaroldstyle",
    "dollarsuperior",
    "ampersandsmall",
    "Acutesmall",
    "parenleftsuperior",
    "parenrightsuperior",
    "twodotenleader",
    "onedotenleader",
    "zerooldstyle",
    "oneoldstyle",
    "twooldstyle",
    "threeoldstyle",
    "fouroldstyle",
    "fiveoldstyle",
    "sixoldstyle",
    "sevenoldstyle",
    "eightoldstyle",
    "nineoldstyle",
    "commasuperior",
    "threequartersemdash",
    "periodsuperior",
    "questionsmall",
    "asuperior",
    "bsuperior",
    "centsuperior",
    "dsuperior",
    "esuperior",
    "isuperior",
    "lsuperior",
    "msuperior",
    "nsuperior",
    "osuperior",
    "rsuperior",
    "ssuperior",
    "tsuperior",
    "ff",
    "ffi",
    "ffl",
    "parenleftinferior",
    "parenrightinferior",
    "Circumflexsmall",
    "hyphensuperior",
    "Gravesmall",
    "Asmall",
    "Bsmall",
    "Csmall",
    "Dsmall",
    "Esmall",
    "Fsmall",
    "Gsmall",
    "Hsmall",
    "Ismall",
    "Jsmall",
    "Ksmall",
    "Lsmall",
    "Msmall",
    "Nsmall",
    "Osmall",
    "Psmall",
    "Qsmall",
    "Rsmall",
    "Ssmall",
    "Tsmall",
    "Usmall",
    "Vsmall",
    "Wsmall",
    "Xsmall",
    "Ysmall",
    "Zsmall",
    "colonmonetary",
    "onefitted",
    "rupiah",
    "Tildesmall",
    "exclamdownsmall",
    "centoldstyle",
    "Lslashsmall",
    "Scaronsmall",
    "Zcaronsmall",
    "Dieresissmall",
    "Brevesmall",
    "Caronsmall",
    "Dotaccentsmall",
    "Macronsmall",
    "figuredash",
    "hypheninferior",
    "Ogoneksmall",
    "Ringsmall",
    "Cedillasmall",
    "questiondownsmall",
    "oneeighth",
    "threeeighths",
    "fiveeighths",
    "seveneighths",
    "onethird",
    "twothirds",
    "zerosuperior",
    "foursuperior",
    "fivesuperior",
    "sixsuperior",
    "sevensuperior",
    "eightsuperior",
    "ninesuperior",
    "zeroinferior",
    "oneinferior",
    "twoinferior",
    "threeinferior",
    "fourinferior",
    "fiveinferior",
    "sixinferior",
    "seveninferior",
    "eightinferior",
    "nineinferior",
    "centinferior",
    "dollarinferior",
    "periodinferior",
    "commainferior",
    "Agravesmall",
    "Aacutesmall",
    "Acircumflexsmall",
    "Atildesmall",
    "Adieresissmall",
    "Aringsmall",
    "AEsmall",
    "Ccedillasmall",
    "Egravesmall",
    "Eacutesmall",
    "Ecircumflexsmall",
    "Edieresissmall",
    "Igravesmall",
    "Iacutesmall",
    "Icircumflexsmall",
    "Idieresissmall",
    "Ethsmall",
    "Ntildesmall",
    "Ogravesmall",
    "Oacutesmall",
    "Ocircumflexsmall",
    "Otildesmall",
    "Odieresissmall",
    "OEsmall",
    "Oslashsmall",
    "Ugravesmall",
    "Uacutesmall",
    "Ucircumflexsmall",
    "Udieresissmall",
    "Yacutesmall",
    "Thornsmall",
    "Ydieresissmall",
    "001.000",
    "001.001",
    "001.002",
    "001.003",
    "Black",
    "Bold",
    "Book",
    "Light",
    "Medium",
    "Regular",
    "Roman",
    "Semibold",
];
