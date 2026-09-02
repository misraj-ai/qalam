//! **L0 — document parser.** The only module allowed to know that `lopdf` exists.
//!
//! Its job is to turn a PDF's object graph into the plain types from
//! [`crate::types`], so that every layer above it works with `PageInfo` and
//! `Rect` rather than `Object::Dictionary(...)` lookups by byte string.
//!
//! Isolating the dependency like this is not ceremony: if `lopdf` ever proves too
//! slow or too lenient, only this file changes.
//!
//! # What a PDF actually is (the 60-second version)
//!
//! A PDF is a bag of numbered *objects* (dicts, arrays, numbers, strings, streams)
//! plus an *xref table* at the end saying which byte offset each object lives at.
//! Reading starts from the back: `startxref` → xref → `trailer` → `/Root` (the
//! Catalog) → `/Pages` (a tree) → individual `/Page` dicts. `lopdf` does all of
//! that for us; what remains is knowing which keys to ask for.

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::error::{Error, Result};
use crate::types::{CodeToUnicode, FontInfo, PageInfo, RawFont, Rect, Rotation};

/// An opened PDF document.
///
/// # Rust lesson: the newtype wrapper
///
/// This is a struct with a single private field. That costs nothing at runtime —
/// the compiler lays it out exactly like a bare `Document` — but it lets us
/// publish a small, intentional API and keep `lopdf` types out of our signatures.
pub struct Pdf {
    doc: Document,
}

impl Pdf {
    /// Open and parse a PDF from disk.
    ///
    /// # Rust lesson: generic paths
    ///
    /// `impl AsRef<Path>` means "any type that can be viewed as a path" — so
    /// callers can pass a `&str`, a `String`, a `PathBuf`, or a `&Path` and none
    /// of them has to convert first. It is the idiomatic signature for anything
    /// filesystem-shaped.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();

        // We check readability ourselves first so that a missing file produces our
        // clear `Error::Io { path, .. }` (which names the file) rather than
        // lopdf's more generic parse failure.
        if let Err(source) = std::fs::File::open(path) {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source,
            });
        }

        // `?` = "if this is an Err, convert it with `From` and return it now".
        // The conversion to `Error::Pdf` comes from the `#[from]` in error.rs.
        let doc = Document::load(path)?;
        Ok(Self { doc })
    }

    /// Number of pages in the document.
    pub fn page_count(&self) -> usize {
        self.doc.get_pages().len()
    }

    /// Summarise every page: geometry, rotation, and the fonts it references.
    pub fn pages(&self) -> Vec<PageInfo> {
        // `get_pages` returns a BTreeMap<page_number, ObjectId>, already ordered.
        self.doc
            .get_pages()
            .into_iter()
            .map(|(number, id)| self.page_info(number, id))
            .collect()
    }

    /// The decompressed content stream of one page, as raw bytes.
    ///
    /// This is the sequence of drawing operators (`BT`, `Tf`, `Tj`, `re`, `Do`, ...)
    /// that L1 will interpret. It is bytes, not text: it embeds binary string
    /// literals, so it is *not* valid UTF-8 in general.
    ///
    /// `page_number` is 1-based, matching [`PageInfo::number`].
    pub fn page_content(&self, page_number: u32) -> Result<Vec<u8>> {
        let id = self.page_id(page_number)?;
        Ok(self.doc.get_page_content(id))
    }

    /// Pull every font on a page out of the object graph, ready for L2.
    ///
    /// This is where indirect references get followed and streams get
    /// decompressed, so that `font.rs` never has to touch `lopdf`. See
    /// [`RawFont`] for what comes out.
    pub fn page_raw_fonts(&self, page_number: u32) -> Result<Vec<RawFont>> {
        let id = self.page_id(page_number)?;
        let Ok(fonts) = self.doc.get_page_fonts(id) else {
            return Ok(Vec::new());
        };

        Ok(fonts
            .into_iter()
            .map(|(name, dict)| self.raw_font(&name, dict))
            .collect())
    }

    /// Extract one font dictionary into plain data.
    fn raw_font(&self, resource_name: &[u8], dict: &Dictionary) -> RawFont {
        let mut raw = RawFont::new(self.font_info(resource_name, dict));

        // /ToUnicode is a stream: follow the reference, then undo its filters.
        raw.to_unicode = dict
            .get(b"ToUnicode")
            .ok()
            .and_then(|obj| self.resolve(obj).ok())
            .and_then(|obj| obj.as_stream().ok())
            // `decompressed_content` applies /FlateDecode and friends. A CMap
            // that will not decompress is treated as absent rather than fatal.
            .and_then(|stream| stream.decompressed_content().ok());

        if raw.info.is_two_byte() {
            self.read_cid_widths(dict, &mut raw);
        } else {
            self.read_simple_widths(dict, &mut raw);
        }
        raw
    }

    /// Read `/FirstChar` and `/Widths` from a simple (1-byte) font.
    fn read_simple_widths(&self, dict: &Dictionary, raw: &mut RawFont) {
        raw.first_char = self
            .lookup(dict, b"FirstChar")
            .and_then(|o| o.as_i64().ok())
            // A negative /FirstChar is nonsense; clamp rather than wrap.
            .map(|n| n.max(0) as u32)
            .unwrap_or(0);

        if let Some(array) = self.lookup(dict, b"Widths").and_then(|o| o.as_array().ok()) {
            raw.widths = array.iter().filter_map(|o| self.number(o)).collect();
        }

        // /MissingWidth lives one level down, in the font descriptor.
        raw.missing_width = self
            .lookup(dict, b"FontDescriptor")
            .and_then(|o| o.as_dict().ok())
            .and_then(|fd| self.lookup(fd, b"MissingWidth"))
            .and_then(|o| self.number(o))
            .unwrap_or(0.0);
    }

    /// Read `/DW` and `/W` from a composite font's descendant.
    ///
    /// A `Type0` font is a shell: the widths live in `/DescendantFonts[0]`,
    /// which is the actual CIDFont (PLAN.md §10.1).
    fn read_cid_widths(&self, dict: &Dictionary, raw: &mut RawFont) {
        let Some(descendant) = self
            .lookup(dict, b"DescendantFonts")
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| self.resolve(o).ok())
            .and_then(|o| o.as_dict().ok())
        else {
            return;
        };

        if let Some(dw) = self.lookup(descendant, b"DW").and_then(|o| self.number(o)) {
            raw.default_width = dw;
        }

        let Some(w) = self
            .lookup(descendant, b"W")
            .and_then(|o| o.as_array().ok())
        else {
            return;
        };

        // The /W array interleaves two shapes:
        //   c [w1 w2 ...]      widths for c, c+1, c+2, ...
        //   cfirst clast w     one width for the whole inclusive range
        // Which one is next is decided by whether an array follows the number.
        let mut i = 0;
        while i < w.len() {
            let Some(first) = self.number(&w[i]).map(|n| n.max(0.0) as u32) else {
                // Not a number where one is required: the array is malformed,
                // and guessing where it resynchronises would be worse than
                // stopping with the widths we already have.
                break;
            };

            match w.get(i + 1).map(|o| self.resolve_or(o)) {
                Some(next) if next.as_array().is_ok() => {
                    let list = next.as_array().expect("checked just above");
                    for (offset, item) in list.iter().enumerate() {
                        if let Some(width) = self.number(item) {
                            let cid = first + offset as u32;
                            raw.cid_widths.push((cid, cid, width));
                        }
                    }
                    i += 2;
                }
                Some(_) => {
                    // The `cfirst clast w` form needs a third operand.
                    let last = w.get(i + 1).and_then(|o| self.number(o));
                    let width = w.get(i + 2).and_then(|o| self.number(o));
                    if let (Some(last), Some(width)) = (last, width) {
                        raw.cid_widths.push((first, last.max(0.0) as u32, width));
                    }
                    i += 3;
                }
                None => break,
            }
        }
    }

    /// Get `key` from `dict`, following an indirect reference if there is one.
    fn lookup<'a>(&'a self, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
        dict.get(key).ok().and_then(|obj| self.resolve(obj).ok())
    }

    /// Follow an indirect reference; pass a direct object through unchanged.
    fn resolve<'a>(&'a self, obj: &'a Object) -> Result<&'a Object> {
        match obj.as_reference() {
            Ok(id) => Ok(self.doc.get_object(id)?),
            Err(_) => Ok(obj),
        }
    }

    /// [`Self::resolve`], but a dangling reference yields the reference itself
    /// rather than an error — for the places where we only need to *classify*
    /// the object.
    fn resolve_or<'a>(&'a self, obj: &'a Object) -> &'a Object {
        self.resolve(obj).unwrap_or(obj)
    }

    /// Read an object as a number, following a reference first if needed.
    fn number(&self, obj: &Object) -> Option<f64> {
        self.resolve(obj).ok()?.as_float().ok().map(|f| f as f64)
    }

    /// Look up the object id of a 1-based page number.
    fn page_id(&self, page_number: u32) -> Result<ObjectId> {
        self.doc
            .get_pages()
            .get(&page_number)
            // `.copied()` turns the `Option<&ObjectId>` we borrowed from the map
            // into an owned `Option<ObjectId>` (cheap: ObjectId is Copy).
            .copied()
            // `ok_or` turns `Option` into `Result` by supplying the error case.
            .ok_or(Error::PageNotFound(page_number))
    }

    /// Assemble a [`PageInfo`] for one page object.
    fn page_info(&self, number: u32, id: ObjectId) -> PageInfo {
        // A4 in points, used when a page has no /MediaBox anywhere up its tree.
        // Malformed rather than fatal: better to report a plausible page than to
        // fail the whole document.
        const A4: Rect = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 595.276,
            y1: 841.89,
        };

        let media_box = self
            .inherited(id, b"MediaBox")
            .and_then(|obj| self.rect_from(obj))
            .unwrap_or(A4);

        let rotation = self
            .inherited(id, b"Rotate")
            .and_then(|obj| obj.as_i64().ok())
            .and_then(Rotation::from_degrees)
            .unwrap_or_default();

        PageInfo {
            number,
            media_box,
            rotation,
            fonts: self.page_fonts(id),
        }
    }

    /// Fetch an attribute from a page, walking up `/Parent` if it is not present.
    ///
    /// `/MediaBox`, `/Resources`, `/Rotate` and `/CropBox` are *inheritable*: a
    /// PDF may set `/MediaBox` once on the root `/Pages` node and omit it from
    /// every individual page. Looking only at the page dict is a classic bug.
    ///
    /// # Rust lesson: lifetimes
    ///
    /// `&'a self` and `Option<&'a Object>` share the name `'a`, which tells the
    /// compiler the returned reference borrows from `self` and may not outlive it.
    /// That is what makes returning an interior reference safe without copying.
    fn inherited<'a>(&'a self, page_id: ObjectId, key: &[u8]) -> Option<&'a Object> {
        // Guard against a malformed file whose /Parent chain loops back on itself,
        // which would otherwise spin forever. Page trees are shallow in practice.
        const MAX_DEPTH: usize = 64;

        let mut current = page_id;
        for _ in 0..MAX_DEPTH {
            let dict = self.doc.get_dictionary(current).ok()?;
            if let Ok(value) = dict.get(key) {
                // The value may itself be an indirect reference (`12 0 R`), in
                // which case we follow it; otherwise it is already the object.
                return Some(match value.as_reference() {
                    Ok(target) => self.doc.get_object(target).ok()?,
                    Err(_) => value,
                });
            }
            current = dict.get(b"Parent").ok()?.as_reference().ok()?;
        }
        None
    }

    /// Interpret a PDF array of four numbers as a [`Rect`].
    fn rect_from(&self, obj: &Object) -> Option<Rect> {
        let array = obj.as_array().ok()?;
        if array.len() != 4 {
            return None;
        }
        // A rect's entries are usually literal numbers, but the spec permits
        // indirect references, so resolve each one before reading it.
        let mut n = [0.0f64; 4];
        for (slot, item) in n.iter_mut().zip(array) {
            let resolved = match item.as_reference() {
                Ok(id) => self.doc.get_object(id).ok()?,
                Err(_) => item,
            };
            // `as_float` accepts both PDF integer and real objects.
            *slot = resolved.as_float().ok()? as f64;
        }
        Some(Rect::new(n[0], n[1], n[2], n[3]))
    }

    /// Summarise the fonts reachable from a page's `/Resources /Font` dict.
    fn page_fonts(&self, page_id: ObjectId) -> Vec<FontInfo> {
        // lopdf already walks the inherited /Resources chain for us here.
        let Ok(fonts) = self.doc.get_page_fonts(page_id) else {
            // `let ... else` is Rust's early-exit for a pattern that may not
            // match: if the page has no usable font dict, report none.
            return Vec::new();
        };

        fonts
            .into_iter()
            .map(|(name, dict)| self.font_info(&name, dict))
            .collect()
    }

    /// Read the handful of keys that tell us how a font encodes text.
    fn font_info(&self, resource_name: &[u8], dict: &Dictionary) -> FontInfo {
        let subtype = name_of(dict, b"Subtype").unwrap_or_default();
        let base_font = name_of(dict, b"BaseFont");

        // /Encoding is either a name (`Identity-H`, `WinAnsiEncoding`) or a
        // dictionary with a /BaseEncoding plus a /Differences array. We record
        // only the simple-name case here; L2 handles the dictionary form.
        let encoding = name_of(dict, b"Encoding");

        // The recoverability signal (PLAN.md §2 #4), in priority order.
        let code_to_unicode = if dict.get(b"ToUnicode").is_ok() {
            CodeToUnicode::ToUnicode
        } else if dict.get(b"Encoding").is_ok() {
            CodeToUnicode::EncodingOnly
        } else {
            CodeToUnicode::None
        };

        FontInfo {
            // PDF names are byte strings; `from_utf8_lossy` replaces any invalid
            // byte with U+FFFD rather than failing. Fine for a display label.
            resource_name: String::from_utf8_lossy(resource_name).into_owned(),
            subtype,
            base_font,
            encoding,
            code_to_unicode,
        }
    }
}

/// Read `key` from `dict` as a PDF name, e.g. `/Type0` → `"Type0"`.
///
/// A free function rather than a method: it needs no access to the document, and
/// keeping it out of `impl Pdf` says so.
fn name_of(dict: &Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key)
        .ok()?
        .as_name()
        .ok()
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

// `#[cfg(test)]` compiles this module only under `cargo test`, so test code adds
// nothing to the shipped library. Unit tests living beside the code they test is
// the Rust convention.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_names_the_path() {
        // `Pdf` is not `Debug`, so `unwrap_err()` is unavailable here; the
        // `let ... else` form destructures the `Err` without that requirement.
        let Err(err) = Pdf::open("definitely/not/here.pdf") else {
            panic!("opening a nonexistent file must fail");
        };
        // `matches!` is a one-line `match` that yields a bool.
        assert!(matches!(err, Error::Io { .. }));
        assert!(err.to_string().contains("definitely/not/here.pdf"));
    }

    #[test]
    fn rotation_normalises_negatives_and_rejects_junk() {
        assert_eq!(Rotation::from_degrees(-90), Some(Rotation::Cw270));
        assert_eq!(Rotation::from_degrees(450), Some(Rotation::Cw90));
        assert_eq!(Rotation::from_degrees(37), None);
    }

    #[test]
    fn rect_normalises_swapped_corners() {
        let r = Rect::new(10.0, 20.0, 0.0, 0.0);
        assert_eq!(r.x0, 0.0);
        assert_eq!(r.width(), 10.0);
        assert_eq!(r.height(), 20.0);
    }
}
