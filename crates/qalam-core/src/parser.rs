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
use crate::types::{
    CodeToUnicode, FontInfo, ImageColorSpace, PageInfo, Palette, RawFont, RawForm, RawImage,
    RawStructure, Rect, Rotation, StructElement,
};

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

        let mut out: Vec<RawFont> = fonts
            .into_iter()
            .map(|(name, dict)| self.raw_font(&name, dict))
            .collect();

        // Fonts a form uses, under names qualified the same way the
        // interpreter will qualify them. A form's `/C2_0` and the page's are
        // different fonts wearing the same name.
        for form in self.collect_forms(page_number)? {
            let Some(font_dict) = self
                .lookup(form.resources, b"Font")
                .and_then(|o| o.as_dict().ok())
            else {
                continue;
            };

            for (name, value) in font_dict.iter() {
                let Ok(dict) = self.resolve(value).and_then(|o| Ok(o.as_dict()?)) else {
                    continue;
                };
                let qualified = format!("{}/{}", form.name, String::from_utf8_lossy(name));
                out.push(self.raw_font(qualified.as_bytes(), dict));
            }
        }
        Ok(out)
    }
    // see 8.10.2 for more information
    /// Every form XObject a page draws, including forms drawn by forms.
    ///
    /// Returned flat, each with a name qualified by its nesting, so the
    /// interpreter can look one up by the name it sees at a `Do`.
    pub fn page_forms(&self, page_number: u32) -> Result<Vec<RawForm>> {
        let mut out = Vec::new();
        for form in self.collect_forms(page_number)? {
            let stream = form.stream;
            let content = stream
                .decompressed_content()
                .unwrap_or_else(|_| stream.content.clone());

            // `/Matrix` defaults to the identity, which most forms use.
            let matrix = self
                .lookup(&stream.dict, b"Matrix")
                .and_then(|o| o.as_array().ok())
                .and_then(|a| {
                    let v: Vec<f64> = a.iter().filter_map(|o| self.number(o)).collect();
                    <[f64; 6]>::try_from(v).ok()
                })
                .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

            out.push(RawForm {
                resource_name: form.name,
                content,
                matrix,
            });
        }
        Ok(out)
    }

    /// Walk the page's form XObjects depth-first, qualifying nested names.
    fn collect_forms(&self, page_number: u32) -> Result<Vec<Form<'_>>> {
        let id = self.page_id(page_number)?;
        let Some(resources) = self
            .inherited(id, b"Resources")
            .and_then(|o| o.as_dict().ok())
        else {
            return Ok(Vec::new());
        };

        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        self.walk_forms(resources, "", 0, &mut seen, &mut out);
        Ok(out)
    }

    /// The recursive half of [`Self::collect_forms`].
    ///
    /// `seen` guards against a form that draws itself, directly or through
    /// another — legal to write and fatal to follow blindly.
    fn walk_forms<'a>(
        &'a self,
        resources: &'a Dictionary,
        prefix: &str,
        depth: usize,
        seen: &mut std::collections::HashSet<ObjectId>,
        out: &mut Vec<Form<'a>>,
    ) {
        const MAX_DEPTH: usize = 8;
        if depth >= MAX_DEPTH {
            return;
        }

        let Some(xobjects) = self
            .lookup(resources, b"XObject")
            .and_then(|o| o.as_dict().ok())
        else {
            return;
        };

        for (name, value) in xobjects.iter() {
            // A form may be reached by two different names; only its *identity*
            // makes a cycle, so that is what is tracked.
            if let Ok(id) = value.as_reference() {
                if !seen.insert(id) {
                    continue;
                }
            }
            let Ok(stream) = self.resolve(value).and_then(|o| Ok(o.as_stream()?)) else {
                continue;
            };
            if name_of(&stream.dict, b"Subtype").as_deref() != Some("Form") {
                continue;
            }

            let qualified = format!("{prefix}{}", String::from_utf8_lossy(name));

            // A form may omit `/Resources`, in which case it uses the ones in
            // force where it was drawn. That inheritance matters: the form
            // holding page 1's title has none of its own, so its `/C2_0` is the
            // *page's* `/C2_0` — and without following that, its text decodes
            // to nothing.
            let inner = self
                .lookup(&stream.dict, b"Resources")
                .and_then(|o| o.as_dict().ok())
                .unwrap_or(resources);

            out.push(Form {
                name: qualified.clone(),
                stream,
                resources: inner,
            });
            self.walk_forms(inner, &format!("{qualified}/"), depth + 1, seen, out);
        }
    }

    /// Read the document's structure tree, if it has one.
    ///
    /// Returns an empty [`RawStructure`] for the overwhelming majority of
    /// files, which are untagged. That is not a failure and must not be
    /// reported as one — it is the normal case, and the geometric path
    /// (L6) exists precisely because of it.
    pub fn structure(&self) -> RawStructure {
        let mut out = RawStructure::default();

        let Ok(catalog) = self.doc.catalog() else {
            return out;
        };

        out.marked = self
            .lookup(catalog, b"MarkInfo")
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Marked").ok())
            .and_then(|o| o.as_bool().ok())
            .unwrap_or(false);

        let Some(root) = self
            .lookup(catalog, b"StructTreeRoot")
            .and_then(|o| o.as_dict().ok())
        else {
            return out;
        };

        // `/RoleMap` translates a producer's own tag names into the standard
        // ones. InDesign emits `/NormalParagraphStyle` and `/Story`; without
        // the map those are meaningless strings, and the reader cannot tell a
        // paragraph from anything else.
        let roles = self.role_map(root);

        // Page object ids, so `/Pg` references can be turned into page numbers.
        let page_numbers: std::collections::HashMap<ObjectId, u32> = self
            .doc
            .get_pages()
            .into_iter()
            .map(|(number, id)| (id, number))
            .collect();

        let mut seen = std::collections::HashSet::new();
        self.walk_structure(
            root,
            0,
            None,
            &page_numbers,
            &roles,
            &mut seen,
            &mut out.elements,
        );
        out
    }

    /// Read `/RoleMap`: the producer's tag names mapped to standard ones.
    fn role_map(&self, root: &Dictionary) -> std::collections::HashMap<String, String> {
        let mut map = std::collections::HashMap::new();
        let Some(dict) = self.lookup(root, b"RoleMap").and_then(|o| o.as_dict().ok()) else {
            return map;
        };

        for (name, value) in dict.iter() {
            if let Ok(target) = self.resolve_or(value).as_name() {
                map.insert(
                    String::from_utf8_lossy(name).into_owned(),
                    String::from_utf8_lossy(target).into_owned(),
                );
            }
        }
        map
    }

    /// Depth-first walk of the structure tree, emitting elements in order.
    ///
    /// `inherited_page` carries `/Pg` down the tree: the spec lets a parent
    /// name the page once and its children omit it, exactly like the page
    /// tree's inheritable attributes.
    ///
    /// `seen` guards against a `/K` cycle. A malformed file can contain one,
    /// and without the guard this recurses until the stack runs out.
    #[allow(clippy::too_many_arguments)]
    fn walk_structure(
        &self,
        element: &Dictionary,
        depth: usize,
        inherited_page: Option<u32>,
        page_numbers: &std::collections::HashMap<ObjectId, u32>,
        roles: &std::collections::HashMap<String, String>,
        seen: &mut std::collections::HashSet<ObjectId>,
        out: &mut Vec<StructElement>,
    ) {
        const MAX_DEPTH: usize = 64;
        if depth > MAX_DEPTH {
            return;
        }

        let page = element
            .get(b"Pg")
            .ok()
            .and_then(|o| o.as_reference().ok())
            .and_then(|id| page_numbers.get(&id).copied())
            .or(inherited_page);

        // `/S` is the element's tag. The tree root has none, and is a container
        // rather than content — so it contributes no element of its own.
        let tag = element
            .get(b"S")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            // A producer's own name resolves to the standard one it stands for;
            // an unmapped name is kept as-is and simply will not be recognised.
            .map(|name| roles.get(&name).cloned().unwrap_or(name));

        // `/K` holds the kids: child elements, integers (marked-content ids),
        // or a marked-content reference dictionary. All three forms occur.
        let mut mcids = Vec::new();
        let mut children: Vec<&Dictionary> = Vec::new();

        if let Some(kids) = element.get(b"K").ok().map(|o| self.resolve_or(o)) {
            // A single kid need not be wrapped in an array.
            let items: Vec<&Object> = match kids.as_array() {
                Ok(array) => array.iter().collect(),
                Err(_) => vec![kids],
            };

            for item in items {
                match item {
                    // A bare integer is a marked-content id on this element's page.
                    Object::Integer(n) => {
                        if let Ok(mcid) = u32::try_from(*n) {
                            mcids.push(mcid);
                        }
                    }
                    _ => {
                        // A reference to a child element, or a `/MCR` /
                        // `/OBJR` dictionary that names an MCID indirectly.
                        if let Ok(id) = item.as_reference() {
                            if !seen.insert(id) {
                                continue;
                            }
                        }
                        if let Ok(dict) = self.resolve_or(item).as_dict() {
                            match dict.get(b"MCID").ok().and_then(|o| o.as_i64().ok()) {
                                Some(n) => {
                                    if let Ok(mcid) = u32::try_from(n) {
                                        mcids.push(mcid);
                                    }
                                }
                                None => children.push(dict),
                            }
                        }
                    }
                }
            }
        }

        // Emit this element before descending, so the flattened order is
        // document order.
        if let Some(tag) = tag {
            out.push(StructElement {
                tag,
                depth,
                page,
                mcids,
            });
        }

        for child in children {
            self.walk_structure(child, depth + 1, page, page_numbers, roles, seen, out);
        }
    }

    /// Pull every image XObject on a page out of the object graph.
    ///
    /// Forms are skipped: a `/Subtype /Form` XObject is a reusable *content
    /// stream*, not a picture, and belongs to a different problem.
    pub fn page_raw_images(&self, page_number: u32) -> Result<Vec<RawImage>> {
        let id = self.page_id(page_number)?;

        // `/XObject` lives in `/Resources`, which is inheritable — a document
        // may declare it once on the page tree root.
        let Some(xobjects) = self
            .inherited(id, b"Resources")
            .and_then(|o| o.as_dict().ok())
            .and_then(|res| self.lookup(res, b"XObject"))
            .and_then(|o| o.as_dict().ok())
        else {
            return Ok(Vec::new());
        };

        let mut images = Vec::new();
        for (name, value) in xobjects.iter() {
            let Ok(stream) = self.resolve(value).and_then(|o| Ok(o.as_stream()?)) else {
                continue;
            };
            if name_of(&stream.dict, b"Subtype").as_deref() != Some("Image") {
                continue;
            }
            if let Some(image) = self.raw_image(name, stream) {
                images.push(image);
            }
        }
        Ok(images)
    }

    /// Read one image stream into plain data.
    ///
    /// Returns `None` for a stream we cannot even establish the dimensions of;
    /// there is nothing useful a later layer could do with it.
    fn raw_image(&self, resource_name: &[u8], stream: &lopdf::Stream) -> Option<RawImage> {
        let dict = &stream.dict;
        let width = self.lookup(dict, b"Width")?.as_i64().ok()?;
        let height = self.lookup(dict, b"Height")?.as_i64().ok()?;
        if width <= 0 || height <= 0 {
            return None;
        }

        let filters = self.filters_of(dict);

        // The image codec, if any, is the last filter in the chain. Everything
        // before it (compression, ASCII armour) has to come off first; the
        // codec's own bytes stay as they are.
        let data = if filters.last().is_some_and(|f| is_image_codec(f)) {
            stream.content.clone()
        } else {
            // `decompressed_content` undoes /FlateDecode and friends. A stream
            // that will not decode is passed through raw rather than dropped —
            // `images.rs` can still report what it is.
            stream
                .decompressed_content()
                .unwrap_or_else(|_| stream.content.clone())
        };

        Some(RawImage {
            resource_name: String::from_utf8_lossy(resource_name).into_owned(),
            width: width as u32,
            height: height as u32,
            bits_per_component: self
                .lookup(dict, b"BitsPerComponent")
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(8)
                .clamp(1, 16) as u8,
            color_space: self.image_color_space(dict),
            palette: self.image_palette(dict),
            filters,
            data,
            has_smask: dict.get(b"SMask").is_ok() || dict.get(b"Mask").is_ok(),
        })
    }

    /// Resolve an `/Indexed` colour space's lookup table.
    ///
    /// The array is `[/Indexed base hival lookup]`, where `lookup` is either a
    /// literal string or a stream — both forms occur, so both are handled.
    fn image_palette(&self, dict: &Dictionary) -> Option<Palette> {
        let array = self.lookup(dict, b"ColorSpace")?.as_array().ok()?;
        let family = array.first()?.as_name().ok()?;
        if !matches!(String::from_utf8_lossy(family).as_ref(), "Indexed" | "I") {
            return None;
        }

        // The palette's own colour space, which says how wide an entry is. It
        // may be a bare name or a nested array such as `[/ICCBased ...]`.
        let base_obj = self.resolve(array.get(1)?).ok()?;
        let base = match base_obj.as_name() {
            Ok(name) => classify_color_space(&String::from_utf8_lossy(name)),
            Err(_) => self.color_space_of_object(base_obj),
        };
        base.components()?;

        let lookup = self.resolve(array.get(3)?).ok()?;
        let entries = match lookup {
            Object::String(bytes, _) => bytes.clone(),
            // A stream's table may be compressed like any other.
            Object::Stream(stream) => stream
                .decompressed_content()
                .unwrap_or_else(|_| stream.content.clone()),
            _ => return None,
        };

        Some(Palette { base, entries })
    }

    /// Classify a colour-space *object* that is an array, e.g. `[/ICCBased s]`.
    fn color_space_of_object(&self, obj: &Object) -> ImageColorSpace {
        let Ok(array) = obj.as_array() else {
            return ImageColorSpace::Other;
        };
        let Some(family) = array.first().and_then(|o| o.as_name().ok()) else {
            return ImageColorSpace::Other;
        };
        match String::from_utf8_lossy(family).as_ref() {
            "ICCBased" => array
                .get(1)
                .and_then(|o| self.resolve(o).ok())
                .and_then(|o| o.as_stream().ok())
                .and_then(|s| self.lookup(&s.dict, b"N"))
                .and_then(|o| o.as_i64().ok())
                .map(|n| match n {
                    1 => ImageColorSpace::Gray,
                    3 => ImageColorSpace::Rgb,
                    4 => ImageColorSpace::Cmyk,
                    _ => ImageColorSpace::Other,
                })
                .unwrap_or(ImageColorSpace::Other),
            other => classify_color_space(other),
        }
    }

    /// The filter chain, whether written as one name or an array of them.
    fn filters_of(&self, dict: &Dictionary) -> Vec<String> {
        let Some(filter) = self.lookup(dict, b"Filter") else {
            return Vec::new();
        };

        if let Ok(name) = filter.as_name() {
            return vec![String::from_utf8_lossy(name).into_owned()];
        }
        filter
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|o| self.resolve(o).ok()?.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Classify `/ColorSpace`, which may be a name or an array.
    fn image_color_space(&self, dict: &Dictionary) -> ImageColorSpace {
        let Some(cs) = self.lookup(dict, b"ColorSpace") else {
            return ImageColorSpace::Other;
        };

        if let Ok(name) = cs.as_name() {
            return classify_color_space(&String::from_utf8_lossy(name));
        }

        // The array forms: `[/ICCBased stream]`, `[/Indexed base hival lookup]`,
        // `[/CalRGB dict]`, and so on.
        let Ok(array) = cs.as_array() else {
            return ImageColorSpace::Other;
        };
        let Some(family) = array.first().and_then(|o| o.as_name().ok()) else {
            return ImageColorSpace::Other;
        };

        match String::from_utf8_lossy(family).as_ref() {
            "Indexed" | "I" => ImageColorSpace::Indexed,
            // An ICC profile we do not interpret; its `/N` says how many
            // components it has, which is all we need to read the samples.
            "ICCBased" => array
                .get(1)
                .and_then(|o| self.resolve(o).ok())
                .and_then(|o| o.as_stream().ok())
                .and_then(|s| self.lookup(&s.dict, b"N"))
                .and_then(|o| o.as_i64().ok())
                .map(|n| match n {
                    1 => ImageColorSpace::Gray,
                    3 => ImageColorSpace::Rgb,
                    4 => ImageColorSpace::Cmyk,
                    _ => ImageColorSpace::Other,
                })
                .unwrap_or(ImageColorSpace::Other),
            other => classify_color_space(other),
        }
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
            self.read_cid_truetype_program(dict, &mut raw);
        } else {
            self.read_simple_widths(dict, &mut raw);
            self.read_encoding(dict, &mut raw);
            raw.font_program = self.read_font_program(dict);
        }
        raw
    }

    /// Pull the embedded CFF font program out of the font descriptor.
    ///
    /// Only `/FontFile3` (Type1C / CFF) is read here. `/FontFile` is Type 1,
    /// whose encoding sits inside an eexec-encrypted section, and is still not
    /// guessed at. `/FontFile2` is TrueType and has its own reader below.
    fn read_font_program(&self, dict: &Dictionary) -> Option<Vec<u8>> {
        let descriptor = self
            .lookup(dict, b"FontDescriptor")
            .and_then(|o| o.as_dict().ok())?;

        let stream = self
            .lookup(descriptor, b"FontFile3")
            .and_then(|o| o.as_stream().ok())?;

        // Font programs are almost always Flate-compressed.
        stream.decompressed_content().ok()
    }

    /// Read `/FontFile2` from a composite font's descendant.
    ///
    /// PDF 32000-1 §9.7.4. A `Type0` font dictionary carries almost nothing
    /// itself: the descriptor, and with it `/FontFile2`, lives one level down
    /// in `/DescendantFonts[0]`.
    ///
    /// # Why the guard on `/Encoding`
    ///
    /// The program is read only to answer "which character is glyph *n*", so
    /// the chain from the painted code to a glyph id has to be one we can
    /// actually follow:
    ///
    /// ```text
    /// code ──(/Encoding CMap)──► CID ──(/CIDToGIDMap)──► glyph id
    /// ```
    ///
    /// The second hop is handled: `/CIDToGIDMap` is either the name `/Identity`
    /// or a stream, and the stream is read here for [`crate::font`] to apply.
    ///
    /// The first hop is not. `Identity-H` means there is nothing to do — the
    /// code *is* the CID — but any other CMap is a translation this crate has
    /// not implemented, and following the chain without it would look up the
    /// wrong glyph and get back a perfectly plausible wrong character. So the
    /// program is left unread and the layers above keep the sources they
    /// already had. Every fixture in the corpus is `Identity-H`; the guard is
    /// there for the file that is not.
    fn read_cid_truetype_program(&self, dict: &Dictionary, raw: &mut RawFont) {
        let identity_encoding = self
            .lookup(dict, b"Encoding")
            .and_then(|o| o.as_name().ok())
            .is_some_and(|name| name == b"Identity-H" || name == b"Identity-V");
        if !identity_encoding {
            return;
        }

        let Some(descendant) = self
            .lookup(dict, b"DescendantFonts")
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| self.resolve(o).ok())
            .and_then(|o| o.as_dict().ok())
        else {
            return;
        };

        // `/CIDToGIDMap` is a name or a stream (PDF 32000-1 §9.7.4.2). Absent
        // means `/Identity`, the spec's default. A name that is neither is
        // malformed, and inventing a reading for it is how a parser starts
        // producing confident nonsense — so nothing is read at all.
        match self.lookup(descendant, b"CIDToGIDMap") {
            None => {}
            Some(obj) if obj.as_name().is_ok_and(|name| name == b"Identity") => {}
            Some(obj) => match obj.as_stream().ok() {
                Some(stream) => raw.cid_to_gid = stream.decompressed_content().ok(),
                None => return,
            },
        }

        let Some(descriptor) = self
            .lookup(descendant, b"FontDescriptor")
            .and_then(|o| o.as_dict().ok())
        else {
            return;
        };

        raw.truetype_program = self
            .lookup(descriptor, b"FontFile2")
            .and_then(|o| o.as_stream().ok())
            .and_then(|stream| stream.decompressed_content().ok());
    }

    /// Read `/Encoding` in either of its two forms.
    ///
    /// It is a bare name (`/WinAnsiEncoding`) or a dictionary carrying a
    /// `/BaseEncoding` and a `/Differences` array. Composite fonts do not use
    /// this path at all — their `/Encoding` names a CMap.
    fn read_encoding(&self, dict: &Dictionary, raw: &mut RawFont) {
        let Some(encoding) = self.lookup(dict, b"Encoding") else {
            return;
        };

        // Form one: a plain name.
        if let Ok(name) = encoding.as_name() {
            raw.base_encoding = Some(String::from_utf8_lossy(name).into_owned());
            return;
        }

        // Form two: a dictionary.
        let Ok(enc_dict) = encoding.as_dict() else {
            return;
        };
        raw.base_encoding = self
            .lookup(enc_dict, b"BaseEncoding")
            .and_then(|o| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned());

        if let Some(array) = self
            .lookup(enc_dict, b"Differences")
            .and_then(|o| o.as_array().ok())
        {
            raw.differences = flatten_differences(array);
        }
    }
    // 9.6.2 read type 1 font.
    /// Read `/FirstChar` and `/Widths` from a simple (1-byte) font.
    fn read_simple_widths(&self, dict: &Dictionary, raw: &mut RawFont) {
        raw.first_char = self
            .lookup(dict, b"FirstChar")
            .and_then(|o| o.as_i64().ok())
            // A negative /FirstChar is nonsense; clamp rather than wrap.
            .map(|n| n.max(0) as u32)
            .unwrap_or(0);
        // look at page 255 from Pdf32000_Iso to learn more.
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

    // for more information about this read 9.7.4.1 and 9.7.4.3 section
    /// Read `/DW` and `/W` from a composite font's descendant.
    /// A `Type0` font is a shell: the widths live in `/DescendantFonts[0]`,
    /// DescendantFonts is one element array and that why we grep the first element.
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
        // se section 9.7.4.3
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
                        let Some(cid) = first.checked_add(offset as u32) else {
                            break;
                        };
                        if let Some(width) = self.number(item) {
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

    // # Rust lesson: lifetimes
    // `&'a self` and `Option<&'a Object>` share the name `'a`, which tells the
    // compiler the returned reference borrows from `self` and may not outlive it.
    // That is what makes returning an interior reference safe without copying.
    // When you omit lifetimes, the compiler applies three mechanical rules:
    //
    // 1. Each elided input lifetime gets its own distinct parameter. fn f(a: &X, b: &Y) becomes fn f<'1, '2>(a: &'1 X, b: &'2 Y).
    // 2. If there is exactly one input lifetime, it's assigned to every elided output lifetime. fn f(a: &X) -> &Y becomes fn f<'1>(a: &'1 X) -> &'1 Y.
    // 3. If one of the inputs is &self or &mut self, the lifetime of self is assigned to every elided output lifetime — regardless of how many other inputs
    // there are.
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

    /// Assemble a [`PageInfo`] for one-page object.
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
    fn inherited(&self, page_id: ObjectId, key: &[u8]) -> Option<&Object> {
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
        // MediaBox is usually written literally:
        // /MediaBox [0 0 595.276 841.89]
        // but the spec permits this:
        // /MediaBox [0 0 12 0 R 13 0 R]
        // where 12 0 R and 13 0 R are separate objects each holding a number.
        // Rare, but legal and a parser that assumes literals will read garbage or fail.
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

/// A form XObject found on a page, with the resources it resolves names in.
struct Form<'a> {
    /// The name qualified by nesting, e.g. `Fm3` or `Fm1/Fm0`.
    name: String,
    /// The form's content stream object.
    stream: &'a lopdf::Stream,
    /// The resource dictionary its names resolve in — its own if it has one,
    /// otherwise the dictionary in force where it was drawn.
    resources: &'a Dictionary,
}

/// Is this filter an image codec rather than a general-purpose one?
///
/// The distinction decides whether the stream's bytes are raw samples or an
/// encoded picture we should hand on untouched.
fn is_image_codec(filter: &str) -> bool {
    matches!(
        filter,
        "DCTDecode" | "DCT" | "JPXDecode" | "CCITTFaxDecode" | "CCF" | "JBIG2Decode"
    )
}

/// Map a colour-space name onto what we model.
fn classify_color_space(name: &str) -> ImageColorSpace {
    match name {
        "DeviceGray" | "CalGray" | "G" => ImageColorSpace::Gray,
        "DeviceRGB" | "CalRGB" | "RGB" => ImageColorSpace::Rgb,
        "DeviceCMYK" | "CMYK" => ImageColorSpace::Cmyk,
        "Indexed" | "I" => ImageColorSpace::Indexed,
        _ => ImageColorSpace::Other,
    }
}

/// Flatten a `/Differences` array into `(code, glyph_name)` pairs.
///
/// The array's format is run-length-ish and easy to misread: a **number** sets
/// the current code, and every **name** after it takes the next code in
/// sequence. So `[ 65 /alpha /beta 200 /gamma ]` means 65→alpha, 66→beta,
/// 200→gamma — not three entries at 65, 200 and nowhere.
fn flatten_differences(array: &[Object]) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    let mut code: u32 = 0;

    for item in array {
        match item {
            Object::Integer(n) => code = (*n).max(0) as u32,
            Object::Real(n) => code = (*n).max(0.0) as u32,
            Object::Name(name) => {
                // Codes above 255 cannot occur in a simple font; skip rather
                // than wrapping the value round.
                if let Ok(byte) = u8::try_from(code) {
                    out.push((byte, String::from_utf8_lossy(name).into_owned()));
                }
                code += 1;
            }
            // Anything else is malformed; ignore it and keep the position.
            _ => {}
        }
    }
    out
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

    /// A font dict whose descendant carries the given `/W` array, with an
    /// empty document behind it. The `/W` entries under test are direct
    /// objects, so nothing needs resolving through the document.
    fn font_with_w(w: Vec<Object>) -> (Pdf, Dictionary, RawFont) {
        let pdf = Pdf {
            doc: Document::with_version("1.7"),
        };
        let mut descendant = Dictionary::new();
        descendant.set("W", Object::Array(w));
        let mut font = Dictionary::new();
        font.set(
            "DescendantFonts",
            Object::Array(vec![Object::Dictionary(descendant)]),
        );
        let raw = RawFont::new(FontInfo {
            resource_name: "F1".to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: None,
            code_to_unicode: CodeToUnicode::None,
        });
        (pdf, font, raw)
    }

    #[test]
    fn cid_width_list_assigns_consecutive_cids() {
        // The ordinary list form: `c [w1 w2]` widths c and c + 1.
        let (pdf, font, mut raw) = font_with_w(vec![
            Object::Integer(10),
            Object::Array(vec![Object::Integer(500), Object::Integer(600)]),
        ]);
        pdf.read_cid_widths(&font, &mut raw);
        assert_eq!(raw.cid_widths, vec![(10, 10, 500.0), (11, 11, 600.0)]);
    }

    #[test]
    fn cid_width_list_stops_at_u32_overflow() {
        // A start CID that saturates to u32::MAX: the first entry still fits,
        // but the next CID cannot be represented, so the list stops instead
        // of panicking (debug) or wrapping to CID 0 (release).
        let (pdf, font, mut raw) = font_with_w(vec![
            Object::Integer(u32::MAX as i64),
            Object::Array(vec![Object::Integer(500), Object::Integer(600)]),
        ]);
        pdf.read_cid_widths(&font, &mut raw);
        assert_eq!(raw.cid_widths, vec![(u32::MAX, u32::MAX, 500.0)]);
    }
}
