//! **L1 (part 2) — the content-stream interpreter.**
//!
//! A page's content stream is a flat list of operators in postfix order:
//! operands first, then the operator name. `18 0 0 18 394 334 Tm` sets the text
//! matrix; `<011A0158>Tj` paints four bytes' worth of glyph codes with it.
//!
//! Our job is the inverse of rendering. A renderer walks these operators to put
//! ink on a page; we walk them to recover **where each glyph was painted, which
//! code it was, and what it looked like** — and nothing more. Every operator
//! that only affects appearance-we-don't-model (paths, clipping, shading) is
//! skipped, but the *state* operators around them are still tracked, because
//! `q`/`Q`/`cm` change where later text lands.
//!
//! What comes out is deliberately **not text yet**. A [`Glyph`] holds a font
//! code, not a character: turning `0x0113` into `ا` needs the font's
//! `/ToUnicode` map, which is L2's job (`font.rs`). Keeping the two apart means
//! this module can be tested purely on geometry, with no fonts involved.
//!
//! # The one formula that matters
//!
//! After painting a glyph, the text matrix advances horizontally by
//!
//! ```text
//!   tx = ((w0 - Tj/1000) x Tfs + Tc + Tw) x Th
//! ```
//!
//! where `w0` is the glyph's width in text space, `Tfs` the font size, `Tc`
//! character spacing, `Tw` word spacing (single-byte code 32 only), `Th` the
//! horizontal scale, and `Tj` the kerning number from a `TJ` array. Everything
//! else in here is bookkeeping around that line.

use lopdf::content::{Content, Operation};
use lopdf::Object;

use crate::graphics::{ColorSpaceKind, GraphicsStack, Matrix};
use crate::types::{FontInfo, Glyph, Style, TextOrientation, TextRenderMode};

/// Supplies the advance width of a glyph, in fractions of an em.
///
/// # Rust lesson: traits as seams
///
/// A trait is a set of methods a type promises to provide — an interface. It is
/// used here to keep a *layering* promise: real widths live in the font's
/// `/Widths` or `/W` arrays, which is L2's territory, so this module names what
/// it needs and lets someone else supply it. When `font.rs` lands it implements
/// this trait and the interpreter is unchanged.
pub trait GlyphWidths {
    /// Width of `code` in `font`, as a fraction of the em square.
    ///
    /// PDF stores these as thousandths (a 500-wide glyph is half an em), so
    /// implementations divide by 1000 before returning.
    fn width(&self, font: &str, code: u32) -> f64;
}

/// Placeholder metrics for before `font.rs` exists: every glyph is half an em.
///
/// Positions of each *show* operation stay exact — those come from the text
/// matrix — but positions *within* a string are approximate until real widths
/// arrive. Line grouping (L3) only needs the former, so this is enough to get
/// the pipeline running end to end.
pub struct AssumedWidths;

impl GlyphWidths for AssumedWidths {
    fn width(&self, _font: &str, _code: u32) -> f64 {
        0.5
    }
}

/// A run of glyphs whose true text the PDF states outright.
///
/// A marked-content section may carry `/ActualText`, giving the text a run of
/// glyphs *really* represents — regardless of what the glyphs decode to. It is
/// used for ligatures, for hyphenated words split across lines, and for glyphs
/// that are not text at all (a logo drawn from a custom font). It is the
/// **top** rung of the resolution chain in PLAN.md §3: when present, it wins.
#[derive(Debug, Clone, PartialEq)]
pub struct ActualText {
    /// Index of the first glyph covered, into [`PageGlyphs::glyphs`].
    pub start: usize,
    /// One past the last glyph covered — a half-open range, as Rust ranges are.
    pub end: usize,
    /// The text the writer says this run represents, already decoded from
    /// PDF's text-string format.
    pub text: String,
}

/// One `Do` operator: an XObject painted somewhere on the page.
///
/// The interpreter cannot tell an image from a form here — that needs the
/// page's `/Resources /XObject` dictionary, which this module deliberately does
/// not have. It records *every* invocation with the transform in effect, and
/// leaves the classifying to whoever holds the resources.
#[derive(Debug, Clone, PartialEq)]
pub struct XObjectUse {
    /// The resource name, e.g. `Im0` in `/Im0 Do`.
    pub name: String,
    /// The current transformation matrix at the moment of the `Do`.
    ///
    /// This is the whole of an image's geometry. PDF paints an image into the
    /// **unit square** — (0,0) to (1,1) — and lets the CTM scale, rotate and
    /// move it into place. The pixel dimensions say nothing about where it
    /// lands or how big it is; this matrix says both.
    pub ctm: Matrix,
    /// How many glyphs had been painted when this happened.
    ///
    /// A cheap ordering key: it says whether the object was drawn before or
    /// after the text around it, without needing a full reading-order pass.
    pub glyph_index: usize,
}

/// A run of glyphs belonging to one marked-content sequence.
///
/// In a *tagged* PDF, `/P << /MCID 3 >> BDC … EMC` says "these glyphs are
/// marked-content item 3", and the structure tree elsewhere in the file says
/// where item 3 sits in the document's logical order. That is the whole
/// mechanism: the content stream numbers its pieces, and the tree orders them.
///
/// Without this the tree is unusable — it would name pieces we could not find.
#[derive(Debug, Clone, PartialEq)]
pub struct McidSpan {
    /// The `/MCID` value.
    pub mcid: u32,
    /// Index of the first glyph covered.
    pub start: usize,
    /// One past the last glyph covered.
    pub end: usize,
}

/// A straight, axis-aligned line painted on the page.
///
/// Table borders are drawn one of two ways, and both arrive here: as a
/// **stroked** segment (`m`/`l`/`S`), or as a **filled rectangle so thin it
/// reads as a line** (`re`/`f`). The second is at least as common as the first,
/// and an implementation that only looks for strokes misses half the tables in
/// the world.
///
/// Coordinates are in device space, with the CTM already applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuledLine {
    /// Left or bottom end.
    pub x0: f64,
    /// Left or bottom end.
    pub y0: f64,
    /// Right or top end.
    pub x1: f64,
    /// Right or top end.
    pub y1: f64,
}

impl RuledLine {
    /// How far the line runs horizontally.
    pub fn width(&self) -> f64 {
        (self.x1 - self.x0).abs()
    }

    /// How far the line runs vertically.
    pub fn height(&self) -> f64 {
        (self.y1 - self.y0).abs()
    }

    /// A line that runs left to right.
    pub fn is_horizontal(&self) -> bool {
        self.width() > self.height()
    }

    /// A line that runs bottom to top.
    pub fn is_vertical(&self) -> bool {
        !self.is_horizontal()
    }

    /// The longer of the two extents.
    pub fn length(&self) -> f64 {
        self.width().max(self.height())
    }
}

/// Everything the interpreter recovered from one page.
#[derive(Debug, Default)]
pub struct PageGlyphs {
    /// Every glyph painted, in the order the stream painted them — which for
    /// Arabic is **visual** order, not reading order. Fixing that is L3's job.
    pub glyphs: Vec<Glyph>,
    /// Every `Do` on the page, in the order they were painted.
    ///
    /// Images are matched against these to find where they landed. Forms are in
    /// here too: a form is a reusable sub-stream with its own resources, which
    /// we do not enter, so it may hide text and images from us. Recording the
    /// invocation keeps that omission visible rather than silent.
    pub xobjects: Vec<XObjectUse>,
    /// Operators we recognised as text-affecting but chose not to implement.
    /// Useful while building; expected to stay empty on ordinary files.
    pub unsupported: Vec<String>,
    /// `/ActualText` overrides, as glyph ranges. Usually empty.
    pub actual_text: Vec<ActualText>,
    /// Marked-content spans carrying an `/MCID`, for tagged documents.
    ///
    /// Empty for the overwhelming majority of files, which are untagged.
    pub mcid_spans: Vec<McidSpan>,
    /// Thin axis-aligned lines painted on the page — candidate table borders.
    ///
    /// Only *painted* geometry is here. A rectangle used as a clipping path
    /// (`re W n`) draws nothing, and recording it would put a spurious border
    /// around every page: our own corpus opens each page with a full-page clip.
    pub ruled_lines: Vec<RuledLine>,
}

/// The text-object state, reset at every `BT`.
///
/// PDF splits text state in two: *parameters* (font, spacing, mode) persist
/// across `BT`/`ET` in the graphics state, while the two *matrices* are reset
/// to identity by `BT`. We keep both here and only reset the matrices, which
/// matches the spec and is a classic place to introduce a bug.
#[derive(Debug, Clone)]
struct TextState {
    /// Resource name from `Tf`, e.g. `C2_0`.
    font: String,
    /// The `Tf` size operand — often a meaningless `1`, with the real scale in
    /// the text matrix. Never report this as the font size (PLAN.md §10.1).
    font_size: f64,
    /// `Tc`, extra space after every glyph, in unscaled text units.
    char_spacing: f64,
    /// `Tw`, extra space after single-byte code 32 only.
    word_spacing: f64,
    /// `Tz` as a fraction: the operator takes a percentage, we store 1.0 for 100.
    horizontal_scale: f64,
    /// `TL`, the line height used by `T*`, `'` and `"`.
    leading: f64,
    /// `Ts`, superscript/subscript offset.
    rise: f64,
    /// `Tr`, how glyphs are painted — including mode 3, invisible.
    render_mode: TextRenderMode,
    /// `Tm`, the text matrix: where the next glyph goes.
    tm: Matrix,
    /// `Tlm`, the text *line* matrix: where the current line started, so `T*`
    /// knows what to drop down from.
    tlm: Matrix,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            font: String::new(),
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            // 100%, stored as a fraction so it can be multiplied directly.
            horizontal_scale: 1.0,
            leading: 0.0,
            rise: 0.0,
            render_mode: TextRenderMode::Fill,
            tm: Matrix::IDENTITY,
            tlm: Matrix::IDENTITY,
        }
    }
}

/// Walks a page's operators and collects positioned, styled glyphs.
struct Interpreter<'a> {
    /// Graphics state stack: transforms and colours (`q`, `Q`, `cm`, `rg`, ...).
    graphics: GraphicsStack,
    /// Text state. Lives outside `BT`/`ET` because most of it persists.
    text: TextState,
    /// Fonts declared on this page, for deciding 1-byte vs 2-byte codes.
    fonts: &'a [FontInfo],
    /// Where real advance widths come from.
    widths: &'a dyn GlyphWidths,
    /// What we have found so far.
    out: PageGlyphs,
    /// The path being built by `m`, `l` and `re`, not yet painted.
    ///
    /// PDF separates *constructing* a path from *painting* it, and only the
    /// painting operators say whether anything was actually drawn.
    path: Vec<PathSegment>,
    /// Where the current subpath began, for `h` (closepath).
    path_start: Option<(f64, f64)>,
    /// The pen's current position in device space.
    path_cursor: Option<(f64, f64)>,
    /// The open marked-content sections, innermost last.
    ///
    /// Each entry remembers the glyph index where the section began and its
    /// `/ActualText`, if any. A stack because `BDC`/`BMC` sections nest, and a
    /// nested one must not close its parent.
    marked_content: Vec<MarkedSection>,
}

/// A piece of the path under construction, in device space.
#[derive(Debug, Clone, Copy)]
enum PathSegment {
    /// A straight run between two points, from `m` then `l`.
    Line { x0: f64, y0: f64, x1: f64, y1: f64 },
    /// A rectangle from `re`, kept whole because a thin one is a rule.
    Rect { x0: f64, y0: f64, x1: f64, y1: f64 },
}

/// One open marked-content section.
struct MarkedSection {
    /// Glyph count at the moment the section opened.
    start: usize,
    /// The section's `/ActualText`, if it declared one.
    actual_text: Option<String>,
    /// The section's `/MCID`, if it declared one.
    mcid: Option<u32>,
}

/// Interpret one page's content stream.
///
/// `fonts` comes from [`crate::PageInfo::fonts`]; it decides how many bytes each
/// glyph code occupies, which is the difference between reading a string and
/// reading noise.
///
/// # Rust lesson: `&dyn Trait`
///
/// `widths: &dyn GlyphWidths` accepts *any* type implementing the trait,
/// resolved at run time through a vtable. The alternative, `impl GlyphWidths`,
/// compiles a separate copy of the function per type — faster, but this is
/// called once per page, so the flexibility is worth more than the nanoseconds.
pub fn interpret(content: &[u8], fonts: &[FontInfo], widths: &dyn GlyphWidths) -> PageGlyphs {
    // lopdf tokenises the stream for us. A stream we cannot even tokenise yields
    // no glyphs rather than an error: other pages may still be fine.
    let Ok(parsed) = Content::decode(content) else {
        return PageGlyphs::default();
    };

    let mut interp = Interpreter {
        graphics: GraphicsStack::new(),
        text: TextState::default(),
        fonts,
        widths,
        out: PageGlyphs::default(),
        path: Vec::new(),
        path_start: None,
        path_cursor: None,
        marked_content: Vec::new(),
    };

    for op in &parsed.operations {
        interp.run(op);
    }
    interp.out
}

impl Interpreter<'_> {
    /// Dispatch one operator.
    fn run(&mut self, op: &Operation) {
        // Operands are PDF objects; most operators want them as numbers.
        let nums = numbers(&op.operands);

        match op.operator.as_str() {
            // ---- graphics state ------------------------------------------
            "q" => self.graphics.save(),
            "Q" => self.graphics.restore(),
            "cm" => {
                if let [a, b, c, d, e, f] = nums[..] {
                    let m = Matrix::new(a, b, c, d, e, f);
                    // `cm` *pre*-multiplies: the new transform applies before
                    // the existing one. Reversing this is the classic bug.
                    let ctm = self.graphics.current().ctm;
                    self.graphics.current_mut().ctm = m.then(ctm);
                }
            }

            // ---- colour --------------------------------------------------
            // Lower case sets the non-stroking (fill) colour, upper case the
            // stroking one. Each of these sets the colour space too.
            "g" | "G" => self.set_color(op, ColorSpaceKind::Gray, &nums),
            "rg" | "RG" => self.set_color(op, ColorSpaceKind::Rgb, &nums),
            "k" | "K" => self.set_color(op, ColorSpaceKind::Cmyk, &nums),
            // `cs`/`CS` select a space without giving a colour; a later
            // `sc`/`scn` supplies the components.
            "cs" | "CS" => {
                if let Some(name) = op.operands.first().and_then(object_name) {
                    let kind = ColorSpaceKind::from_name(&name);
                    if op.operator == "cs" {
                        self.graphics.current_mut().fill_space = kind;
                    } else {
                        self.graphics.current_mut().stroke_space = kind;
                    }
                }
            }
            "sc" | "scn" | "SC" | "SCN" => {
                let stroking = op.operator.starts_with('S');
                let state = self.graphics.current();
                let space = if stroking {
                    state.stroke_space
                } else {
                    state.fill_space
                };
                // `scn` may end with a pattern name; `numbers` already dropped
                // any non-numeric operand, and a count mismatch then yields
                // `Color::Unknown` rather than a wrong colour.
                let color = space.color_from_components(&nums);
                let state = self.graphics.current_mut();
                if stroking {
                    state.stroke_color = color;
                } else {
                    state.fill_color = color;
                }
            }

            // ---- text objects --------------------------------------------
            "BT" => {
                // Only the matrices reset; font, spacing and mode persist.
                self.text.tm = Matrix::IDENTITY;
                self.text.tlm = Matrix::IDENTITY;
            }
            "ET" => {}

            // ---- text state ----------------------------------------------
            "Tf" => {
                if let Some(name) = op.operands.first().and_then(object_name) {
                    self.text.font = name;
                }
                if let Some(size) = nums.first() {
                    self.text.font_size = *size;
                }
            }
            "Tc" => self.text.char_spacing = nums.first().copied().unwrap_or(0.0),
            "Tw" => self.text.word_spacing = nums.first().copied().unwrap_or(0.0),
            // The operator's operand is a percentage; store it as a fraction.
            "Tz" => self.text.horizontal_scale = nums.first().copied().unwrap_or(100.0) / 100.0,
            "TL" => self.text.leading = nums.first().copied().unwrap_or(0.0),
            "Ts" => self.text.rise = nums.first().copied().unwrap_or(0.0),
            "Tr" => {
                let mode = nums.first().copied().unwrap_or(0.0) as i64;
                self.text.render_mode = TextRenderMode::from_operand(mode);
            }

            // ---- text positioning ----------------------------------------
            "Td" => {
                if let [tx, ty] = nums[..] {
                    self.next_line(tx, ty);
                }
            }
            "TD" => {
                if let [tx, ty] = nums[..] {
                    // `TD` is `Td` plus "set leading to -ty" — one operator
                    // doing two jobs, a common source of wrong line spacing.
                    self.text.leading = -ty;
                    self.next_line(tx, ty);
                }
            }
            "Tm" => {
                if let [a, b, c, d, e, f] = nums[..] {
                    // `Tm` *replaces* both matrices; it does not compose.
                    let m = Matrix::new(a, b, c, d, e, f);
                    self.text.tm = m;
                    self.text.tlm = m;
                }
            }
            "T*" => {
                let leading = self.text.leading;
                self.next_line(0.0, -leading);
            }

            // ---- showing text --------------------------------------------
            "Tj" => {
                if let Some(bytes) = op.operands.first().and_then(object_string) {
                    self.show(bytes);
                }
            }
            "'" => {
                // Move to the next line, then show.
                let leading = self.text.leading;
                self.next_line(0.0, -leading);
                if let Some(bytes) = op.operands.first().and_then(object_string) {
                    self.show(bytes);
                }
            }
            "\"" => {
                // `aw ac string "` — set word and char spacing, then behave as `'`.
                if let [aw, ac] = nums[..] {
                    self.text.word_spacing = aw;
                    self.text.char_spacing = ac;
                }
                let leading = self.text.leading;
                self.next_line(0.0, -leading);
                if let Some(bytes) = op.operands.get(2).and_then(object_string) {
                    self.show(bytes);
                }
            }
            "TJ" => {
                // An array mixing strings to paint and numbers to kern by.
                let Some(items) = op.operands.first().and_then(|o| o.as_array().ok()) else {
                    return;
                };
                for item in items {
                    if let Some(bytes) = object_string(item) {
                        self.show(bytes);
                    } else if let Some(kern) = object_number(item) {
                        // A positive number moves *left* (closes up the text),
                        // hence the negation. Units are thousandths of an em.
                        let tx = -kern / 1000.0 * self.text.font_size * self.text.horizontal_scale;
                        self.text.tm = Matrix::translation(tx, 0.0).then(self.text.tm);
                    }
                }
            }

            // ---- path construction ---------------------------------------
            // These build a path; nothing is drawn until a painting operator
            // says so. Coordinates are transformed by the CTM as they arrive,
            // so a later `Q` cannot change where an already-built segment is.
            "m" => {
                if let [x, y] = nums[..] {
                    self.path_start = Some(self.graphics.current().ctm.apply(x, y));
                    self.path_cursor = self.path_start;
                }
            }
            "l" => {
                if let [x, y] = nums[..] {
                    let to = self.graphics.current().ctm.apply(x, y);
                    if let Some((x0, y0)) = self.path_cursor {
                        self.push_segment(PathSegment::Line {
                            x0,
                            y0,
                            x1: to.0,
                            y1: to.1,
                        });
                    }
                    self.path_cursor = Some(to);
                }
            }
            "re" => {
                if let [x, y, w, h] = nums[..] {
                    // The CTM may flip or rotate, so transform two opposite
                    // corners and let `Rect`-style normalisation sort them out.
                    let ctm = self.graphics.current().ctm;
                    let (x0, y0) = ctm.apply(x, y);
                    let (x1, y1) = ctm.apply(x + w, y + h);
                    self.push_segment(PathSegment::Rect { x0, y0, x1, y1 });
                    // `re` leaves the current point at the rectangle's origin.
                    self.path_cursor = Some((x0, y0));
                    self.path_start = self.path_cursor;
                }
            }
            "h" => {
                // Close the subpath: a line back to where it started.
                if let (Some((x0, y0)), Some((x1, y1))) = (self.path_cursor, self.path_start) {
                    self.push_segment(PathSegment::Line { x0, y0, x1, y1 });
                }
                self.path_cursor = self.path_start;
            }
            // Curves. Their control points are not tracked — a table border is
            // never a Bézier — but the current point must still follow, or a
            // later `l` would draw a line from the wrong place.
            "c" | "v" | "y" => {
                if nums.len() >= 2 {
                    let (x, y) = (nums[nums.len() - 2], nums[nums.len() - 1]);
                    self.path_cursor = Some(self.graphics.current().ctm.apply(x, y));
                }
            }

            // ---- path painting -------------------------------------------
            // Only these actually put ink on the page. `n` is the important
            // exception: it ends a path *without* painting, and is what every
            // clipping rectangle uses. Treating it as a paint would draw a
            // border around every page in our own corpus.
            "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" => {
                self.paint_path();
            }
            "n" => self.clear_path(),

            // ---- marked content ------------------------------------------
            // `BDC` opens a section with a property list, which is where
            // `/ActualText` lives. `BMC` opens one without properties. Both are
            // closed by `EMC`.
            "BDC" | "BMC" => {
                // Both properties live in the same place, so read the operand
                // once and pull each out of it.
                let properties = op.operands.get(1).and_then(|o| o.as_dict().ok());

                let mcid = properties
                    .and_then(|d| d.get(b"MCID").ok())
                    .and_then(|o| o.as_i64().ok())
                    .and_then(|n| u32::try_from(n).ok());

                let actual_text = if op.operator == "BDC" {
                    // Operands are `/Tag /PropertyName` or `/Tag << ... >>`.
                    // Only the inline-dictionary form is readable here: the
                    // named form points into `/Resources /Properties`, which
                    // this module deliberately does not have.
                    properties
                        .and_then(|d| d.get(b"ActualText").ok())
                        .and_then(object_string)
                        .map(pdf_text_string)
                } else {
                    None
                };

                // Bound the nesting so a pathological stream cannot grow this
                // without limit, matching the graphics stack's guard.
                const MAX_NESTING: usize = 64;
                if self.marked_content.len() < MAX_NESTING {
                    self.marked_content.push(MarkedSection {
                        start: self.out.glyphs.len(),
                        actual_text,
                        mcid,
                    });
                }
            }
            "EMC" => {
                if let Some(section) = self.marked_content.pop() {
                    let end = self.out.glyphs.len();
                    // Record only sections that actually painted something.
                    if end > section.start {
                        if let Some(text) = section.actual_text {
                            self.out.actual_text.push(ActualText {
                                start: section.start,
                                end,
                                text,
                            });
                        }
                        if let Some(mcid) = section.mcid {
                            self.out.mcid_spans.push(McidSpan {
                                mcid,
                                start: section.start,
                                end,
                            });
                        }
                    }
                }
            }

            // ---- everything else -----------------------------------------
            "Do" => {
                // Record the name and the transform; deciding what the object
                // *is* belongs to a layer that can see `/Resources`.
                if let Some(name) = op.operands.first().and_then(object_name) {
                    self.out.xobjects.push(XObjectUse {
                        name,
                        ctm: self.graphics.current().ctm,
                        glyph_index: self.out.glyphs.len(),
                    });
                }
            }
            _ => {
                // Paths, clipping, shading, marked content, inline images: all
                // irrelevant to where text lands, so silently ignored.
            }
        }
    }

    /// Add a segment to the path under construction.
    ///
    /// Bounded, because a page of dense vector artwork can hold hundreds of
    /// thousands of segments and none of them is a table border.
    fn push_segment(&mut self, segment: PathSegment) {
        const MAX_SEGMENTS: usize = 8192;
        if self.path.len() < MAX_SEGMENTS {
            self.path.push(segment);
        }
    }

    /// A painting operator ran: keep whatever in the path looks like a rule.
    fn paint_path(&mut self) {
        // A filled rectangle thinner than this reads as a line rather than a
        // block of colour. Table rules are hairlines to a couple of points;
        // anything thicker is a band or a background.
        const MAX_RULE_THICKNESS: f64 = 3.0;
        // Shorter than this and it is a tick, a bullet or a dash, not a border.
        const MIN_RULE_LENGTH: f64 = 4.0;
        // How far from axis-aligned a segment may be. Table borders are
        // straight; a diagonal is artwork.
        const AXIS_TOLERANCE: f64 = 0.5;

        for segment in std::mem::take(&mut self.path) {
            let line = match segment {
                PathSegment::Line { x0, y0, x1, y1 } => {
                    // Keep only near-axis-aligned segments.
                    let (dx, dy) = ((x1 - x0).abs(), (y1 - y0).abs());
                    if dx > AXIS_TOLERANCE && dy > AXIS_TOLERANCE {
                        continue;
                    }
                    RuledLine { x0, y0, x1, y1 }
                }
                PathSegment::Rect { x0, y0, x1, y1 } => {
                    let (left, right) = (x0.min(x1), x0.max(x1));
                    let (bottom, top) = (y0.min(y1), y0.max(y1));
                    let (w, h) = (right - left, top - bottom);

                    // A thin rectangle *is* a rule: collapse it to its centre
                    // line so the two ways of drawing a border become one
                    // representation downstream.
                    if h <= MAX_RULE_THICKNESS && w > h {
                        let mid = (bottom + top) / 2.0;
                        RuledLine {
                            x0: left,
                            y0: mid,
                            x1: right,
                            y1: mid,
                        }
                    } else if w <= MAX_RULE_THICKNESS && h > w {
                        let mid = (left + right) / 2.0;
                        RuledLine {
                            x0: mid,
                            y0: bottom,
                            x1: mid,
                            y1: top,
                        }
                    } else {
                        // A filled area, not a border. Its *edges* could be
                        // read as rules, but a table cell shaded grey would
                        // then invent four borders it does not have.
                        continue;
                    }
                }
            };

            if line.length() >= MIN_RULE_LENGTH {
                self.out.ruled_lines.push(line);
            }
        }
    }

    /// End the path without painting: `n`, and what every clip uses.
    fn clear_path(&mut self) {
        self.path.clear();
        self.path_cursor = None;
        self.path_start = None;
    }

    /// Set a fill or stroke colour from a `g`/`rg`/`k` family operator.
    ///
    /// Case decides which: PDF's convention throughout is lower case for
    /// non-stroking operations and upper case for stroking ones.
    fn set_color(&mut self, op: &Operation, space: ColorSpaceKind, nums: &[f64]) {
        let color = space.color_from_components(nums);
        let stroking = op
            .operator
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase());

        let state = self.graphics.current_mut();
        if stroking {
            state.stroke_space = space;
            state.stroke_color = color;
        } else {
            state.fill_space = space;
            state.fill_color = color;
        }
    }

    /// Start a new line offset `(tx, ty)` from the current *line* matrix.
    ///
    /// The offset is from `tlm`, not `tm` — otherwise each line would drift by
    /// the width of the previous one.
    fn next_line(&mut self, tx: f64, ty: f64) {
        self.text.tlm = Matrix::translation(tx, ty).then(self.text.tlm);
        self.text.tm = self.text.tlm;
    }

    /// Paint one string: split it into glyph codes and emit each one.
    fn show(&mut self, bytes: &[u8]) {
        // How wide is a code in this font? `Identity-H` composite fonts use two
        // bytes, simple fonts one. Guessing wrong turns text into noise.
        let two_byte = self
            .fonts
            .iter()
            .find(|f| f.resource_name == self.text.font)
            .is_some_and(FontInfo::is_two_byte);

        if two_byte {
            // `chunks_exact(2)` yields only whole pairs, dropping a trailing odd
            // byte — which would be a malformed string anyway.
            for pair in bytes.chunks_exact(2) {
                // Big-endian: PDF writes the high byte first.
                let code = u32::from(pair[0]) << 8 | u32::from(pair[1]);
                self.emit(code, false);
            }
        } else {
            for &byte in bytes {
                // Word spacing applies to single-byte code 32 only — never to a
                // two-byte code that happens to equal 32.
                self.emit(u32::from(byte), byte == 32);
            }
        }
    }

    /// Emit one glyph at the current position and advance the text matrix.
    fn emit(&mut self, code: u32, is_space: bool) {
        // Copy the scalars we need out of `self.text` first. Beyond avoiding
        // borrow-checker friction when we mutate `self.out` below, it keeps the
        // formula readable.
        let font_size = self.text.font_size;
        let h_scale = self.text.horizontal_scale;
        let ctm = self.graphics.current().ctm;

        // The text rendering matrix: font size and rise, then the text matrix,
        // then the page transform. This composition is what makes
        // `/C2_0 1 Tf` + `20.5559 ... Tm` come out as 20.56pt (PLAN.md §10.1).
        let scaling = Matrix::new(
            font_size * h_scale,
            0.0,
            0.0,
            font_size,
            0.0,
            self.text.rise,
        );
        let trm = scaling.then(self.text.tm).then(ctm);

        // The glyph origin is the transformed text-space origin, which for this
        // matrix is simply its translation part.
        let (x, y) = trm.apply(0.0, 0.0);

        // Which colour a reader actually sees depends on the render mode.
        let state = self.graphics.current();
        let color = if self.text.render_mode.paints_with_stroke_color() {
            state.stroke_color
        } else {
            state.fill_color
        };

        // The advance, in unscaled text space. Word spacing applies to the
        // single-byte code 32 only — never to a 2-byte code that equals 32.
        let word = if is_space {
            self.text.word_spacing
        } else {
            0.0
        };
        let width = self.widths.width(&self.text.font, code);
        let tx = (width * font_size + self.text.char_spacing + word) * h_scale;

        // Report the advance in device space, so it is directly comparable with
        // the `x` above. `tm x ctm` (without the font-size scaling, which `tx`
        // already includes) is what converts text space to device space.
        let to_device = self.text.tm.then(ctm);

        self.out.glyphs.push(Glyph {
            code,
            x,
            y,
            // Where the matrix sends the unit x vector is the direction the
            // text advances, and so which way it is meant to be read.
            orientation: TextOrientation::from_advance(trm.a, trm.b),
            advance: tx * to_device.horizontal_scale(),
            style: Style {
                color,
                font: self.text.font.clone(),
                // The *effective* size, not the `Tf` operand.
                size: trm.vertical_scale(),
                render_mode: self.text.render_mode,
            },
        });

        self.text.tm = Matrix::translation(tx, 0.0).then(self.text.tm);
    }
}

// ---------------------------------------------------------------------------
// Operand helpers
//
// lopdf hands us `Object`s; these three turn them into the shapes we want,
// returning `Option` so a malformed operand is skipped rather than fatal.
// ---------------------------------------------------------------------------

/// Read a PDF integer or real as `f64`.
fn object_number(obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// Keep only the numeric operands, in order.
///
/// # Rust lesson: iterator chains
///
/// `filter_map` combines "test each item" and "transform it" in one pass: the
/// closure returns `Option`, and `None` items are dropped. No intermediate
/// vector is built — the whole chain fuses into a single loop.
fn numbers(operands: &[Object]) -> Vec<f64> {
    operands.iter().filter_map(object_number).collect()
}

/// Read a PDF name operand as a `String`, e.g. `/C2_0` → `"C2_0"`.
fn object_name(obj: &Object) -> Option<String> {
    obj.as_name()
        .ok()
        .map(|n| String::from_utf8_lossy(n).into_owned())
}

/// Decode a PDF *text string* into Rust text.
///
/// PDF stores these in one of two ways, distinguished by a byte-order mark:
/// UTF-16BE when the string starts with `FE FF`, and PDFDocEncoding otherwise.
/// PDFDocEncoding agrees with Latin-1 across the range that carries text, which
/// is what the fallback below assumes.
fn pdf_text_string(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
        return char::decode_utf16(units)
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
    }
    // Latin-1: each byte is its own codepoint.
    bytes.iter().map(|&b| b as char).collect()
}

/// Borrow a PDF string operand's raw bytes.
///
/// These are *not* text: in a composite font they are big-endian glyph codes,
/// so they must never be treated as UTF-8.
fn object_string(obj: &Object) -> Option<&[u8]> {
    match obj {
        Object::String(bytes, _) => Some(bytes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CodeToUnicode, Color};

    /// A one-byte font, like a plain `TrueType` resource.
    fn simple_font(name: &str) -> FontInfo {
        FontInfo {
            resource_name: name.to_string(),
            subtype: "TrueType".to_string(),
            base_font: None,
            encoding: Some("WinAnsiEncoding".to_string()),
            code_to_unicode: CodeToUnicode::EncodingOnly,
        }
    }

    /// A two-byte composite font, like the `Identity-H` fonts in our fixture.
    fn composite_font(name: &str) -> FontInfo {
        FontInfo {
            resource_name: name.to_string(),
            subtype: "Type0".to_string(),
            base_font: None,
            encoding: Some("Identity-H".to_string()),
            code_to_unicode: CodeToUnicode::ToUnicode,
        }
    }

    /// Run a snippet of content-stream source through the interpreter.
    fn run(source: &str, fonts: &[FontInfo]) -> PageGlyphs {
        interpret(source.as_bytes(), fonts, &AssumedWidths)
    }

    #[test]
    fn td_positions_glyphs_and_tf_sets_the_size() {
        let out = run("BT /F1 12 Tf 100 200 Td (AB) Tj ET", &[simple_font("F1")]);
        assert_eq!(out.glyphs.len(), 2);
        assert_eq!(out.glyphs[0].code, u32::from(b'A'));
        assert_eq!(out.glyphs[1].code, u32::from(b'B'));
        assert_eq!((out.glyphs[0].x, out.glyphs[0].y), (100.0, 200.0));
        assert!((out.glyphs[0].style.size - 12.0).abs() < 1e-9);
        // The second glyph sits one advance to the right: 0.5 em of 12pt.
        assert!((out.glyphs[1].x - 106.0).abs() < 1e-9);
    }

    #[test]
    fn the_size_one_tf_idiom_reports_the_real_size() {
        // PLAN.md §10.1: `/C2_0 1 Tf` with the real scale in the text matrix.
        // Reading the `Tf` operand alone would report 1pt.
        let out = run(
            "BT /C2_0 1 Tf 20.5559 0 0 20.5559 118.66 476.77 Tm <0113>Tj ET",
            &[composite_font("C2_0")],
        );
        assert_eq!(out.glyphs.len(), 1);

        // Tolerance note: lopdf stores a PDF real as `f32`, so `118.66` from
        // the stream reaches us as 118.660004. That is the file's precision,
        // not our error — hence 1e-3 rather than the 1e-9 used where the
        // arithmetic is exact.
        assert!((out.glyphs[0].style.size - 20.5559).abs() < 1e-3);
        assert!((out.glyphs[0].x - 118.66).abs() < 1e-3);
        assert!((out.glyphs[0].y - 476.77).abs() < 1e-3);
    }

    #[test]
    fn composite_fonts_read_two_byte_codes() {
        // Four bytes are two CIDs, not four. Getting this wrong is the
        // difference between reading text and reading noise.
        let out = run("BT /C2_0 1 Tf <011A0158>Tj ET", &[composite_font("C2_0")]);
        assert_eq!(out.glyphs.len(), 2);
        assert_eq!(out.glyphs[0].code, 0x011A);
        assert_eq!(out.glyphs[1].code, 0x0158);
    }

    #[test]
    fn the_same_bytes_in_a_simple_font_are_four_codes() {
        let out = run("BT /F1 1 Tf <011A0158>Tj ET", &[simple_font("F1")]);
        assert_eq!(out.glyphs.len(), 4);
        assert_eq!(out.glyphs[0].code, 0x01);
    }

    #[test]
    fn fill_colour_reaches_the_glyph() {
        let out = run(
            "0.016 0.408 0.294 rg BT /F1 12 Tf (x) Tj ET",
            &[simple_font("F1")],
        );
        // The green used for page numbers in our fixture.
        assert_eq!(out.glyphs[0].style.color, Color::Rgb(0.016, 0.408, 0.294));
        assert_eq!(out.glyphs[0].style.color.to_css_hex(), "#04684b");
    }

    #[test]
    fn gray_operator_gives_white_headings() {
        // `1 g` is how the fixture paints white text over a coloured banner.
        let out = run("1 g BT /F1 12 Tf (x) Tj ET", &[simple_font("F1")]);
        assert_eq!(out.glyphs[0].style.color, Color::Gray(1.0));
        assert_eq!(out.glyphs[0].style.color.to_css_hex(), "#ffffff");
    }

    #[test]
    fn q_restores_the_colour_for_later_text() {
        let out = run(
            "1 0 0 rg q 0 0 1 rg BT /F1 12 Tf (a) Tj ET Q BT /F1 12 Tf (b) Tj ET",
            &[simple_font("F1")],
        );
        assert_eq!(out.glyphs[0].style.color, Color::Rgb(0.0, 0.0, 1.0));
        // After `Q` the red set before `q` is back in effect.
        assert_eq!(out.glyphs[1].style.color, Color::Rgb(1.0, 0.0, 0.0));
    }

    #[test]
    fn cm_moves_and_scales_later_text() {
        // A `cm` outside the text object still positions the text inside it.
        let out = run(
            "2 0 0 2 10 20 cm BT /F1 12 Tf 5 5 Td (x) Tj ET",
            &[simple_font("F1")],
        );
        // (5,5) scaled by 2 then translated by (10,20).
        assert_eq!((out.glyphs[0].x, out.glyphs[0].y), (20.0, 30.0));
        // The size doubles with the transform too.
        assert!((out.glyphs[0].style.size - 24.0).abs() < 1e-9);
    }

    #[test]
    fn tj_array_numbers_kern_the_pen_leftwards() {
        // A positive TJ number closes up the text, so it moves *left*.
        let plain = run("BT /F1 10 Tf [(a)(b)] TJ ET", &[simple_font("F1")]);
        let kerned = run("BT /F1 10 Tf [(a) 100 (b)] TJ ET", &[simple_font("F1")]);
        let gap = plain.glyphs[1].x - kerned.glyphs[1].x;
        // 100/1000 of an em at 10pt = 1pt.
        assert!((gap - 1.0).abs() < 1e-9, "kern moved by {gap}");
    }

    #[test]
    fn td_offsets_from_the_line_start_not_the_pen() {
        // Two `Td`s compose from the *line* matrix. If `Td` offset from the
        // text matrix instead, the second line would drift right by the width
        // of the first.
        let out = run(
            "BT /F1 10 Tf 100 700 Td (abc) Tj 0 -12 Td (d) Tj ET",
            &[simple_font("F1")],
        );
        let last = out.glyphs.last().unwrap();
        assert_eq!((last.x, last.y), (100.0, 688.0));
    }

    #[test]
    fn t_star_uses_the_leading() {
        let out = run(
            "BT /F1 10 Tf 14 TL 50 500 Td (a) Tj T* (b) Tj ET",
            &[simple_font("F1")],
        );
        assert_eq!((out.glyphs[1].x, out.glyphs[1].y), (50.0, 486.0));
    }

    #[test]
    fn invisible_text_is_still_extracted_but_flagged() {
        // Mode 3 is the signature of an OCR layer over a scan: we keep the
        // glyphs — they may be the only text there is — but record the mode.
        let out = run("BT /F1 12 Tf 3 Tr (x) Tj ET", &[simple_font("F1")]);
        assert_eq!(out.glyphs.len(), 1);
        assert!(!out.glyphs[0].style.render_mode.is_visible());
    }

    #[test]
    fn stroke_only_text_takes_the_stroke_colour() {
        let out = run(
            "1 0 0 rg 0 0 1 RG BT /F1 12 Tf 1 Tr (x) Tj ET",
            &[simple_font("F1")],
        );
        // Render mode 1 outlines the glyph, so blue is what a reader sees.
        assert_eq!(out.glyphs[0].style.color, Color::Rgb(0.0, 0.0, 1.0));
    }

    #[test]
    fn word_spacing_applies_to_single_byte_spaces_only() {
        let with_tw = run("BT /F1 10 Tf 5 Tw (a b) Tj ET", &[simple_font("F1")]);
        let without = run("BT /F1 10 Tf (a b) Tj ET", &[simple_font("F1")]);
        // The glyph after the space is pushed right by the word spacing.
        let shift = with_tw.glyphs[2].x - without.glyphs[2].x;
        assert!((shift - 5.0).abs() < 1e-9, "shift was {shift}");
    }

    #[test]
    fn xobjects_are_recorded_with_the_transform_that_places_them() {
        // A `cm` before the `Do` is the whole of an image's geometry: PDF
        // paints into the unit square and lets the matrix scale and move it.
        let out = run("q 200 0 0 100 50 600 cm /Im0 Do Q", &[]);
        assert_eq!(out.xobjects.len(), 1);
        assert_eq!(out.xobjects[0].name, "Im0");

        let ctm = out.xobjects[0].ctm;
        // The unit square's corners map to the image's placed rectangle.
        assert_eq!(ctm.apply(0.0, 0.0), (50.0, 600.0));
        assert_eq!(ctm.apply(1.0, 1.0), (250.0, 700.0));
    }

    #[test]
    fn q_restores_the_transform_between_two_placements() {
        // Two images placed from the same saved state must not accumulate each
        // other's transforms.
        let out = run(
            "q 10 0 0 10 0 0 cm /Im0 Do Q q 20 0 0 20 100 100 cm /Im1 Do Q",
            &[],
        );
        assert_eq!(out.xobjects.len(), 2);
        assert_eq!(out.xobjects[0].ctm.apply(1.0, 1.0), (10.0, 10.0));
        assert_eq!(out.xobjects[1].ctm.apply(1.0, 1.0), (120.0, 120.0));
    }

    #[test]
    fn a_form_invocation_is_recorded_even_though_we_do_not_enter_it() {
        // We cannot tell a form from an image here, and must not pretend the
        // object was not there.
        let out = run("q /Fm0 Do Q", &[]);
        assert_eq!(out.xobjects.len(), 1);
        assert_eq!(out.xobjects[0].name, "Fm0");
        assert!(out.glyphs.is_empty());
    }

    #[test]
    fn an_xobject_drawn_after_text_records_the_glyph_count() {
        let out = run(
            "BT /F1 12 Tf (ab) Tj ET q 1 0 0 1 0 0 cm /Im0 Do Q",
            &[simple_font("F1")],
        );
        assert_eq!(out.xobjects[0].glyph_index, 2);
    }

    #[test]
    fn graphics_noise_before_bt_is_ignored_without_breaking_state() {
        // The fixture's page 1 opens with a rounded-rectangle banner. None of
        // it should produce glyphs, and none of it should disturb the text.
        let out = run(
            "q 0 0 595 841 re W n 0.016 0.69 0.533 rg 1 0 0 1 531 355 cm \
             0 0 m 0 8.9 l h f* Q BT /F1 12 Tf 100 100 Td (x) Tj ET",
            &[simple_font("F1")],
        );
        assert_eq!(out.glyphs.len(), 1);
        // The `cm` and `rg` were inside `q`/`Q`, so the text is unaffected:
        // still at (100,100), still black.
        assert_eq!((out.glyphs[0].x, out.glyphs[0].y), (100.0, 100.0));
        assert!(out.glyphs[0].style.color.is_black());
    }

    // ---- /ActualText -----------------------------------------------------

    #[test]
    fn bdc_with_actual_text_records_the_glyph_range() {
        let out = run(
            "BT /F1 12 Tf /Span <</ActualText (fi)>> BDC (\\001\\002) Tj EMC ET",
            &[simple_font("F1")],
        );
        assert_eq!(out.glyphs.len(), 2);
        assert_eq!(out.actual_text.len(), 1);
        assert_eq!(out.actual_text[0].text, "fi");
        assert_eq!((out.actual_text[0].start, out.actual_text[0].end), (0, 2));
    }

    #[test]
    fn utf16_actual_text_is_decoded_via_its_byte_order_mark() {
        // A PDF text string beginning FE FF is UTF-16BE. Here: U+0645 U+0631.
        let out = run(
            "BT /F1 12 Tf /Span <</ActualText <FEFF06450631>>> BDC (x) Tj EMC ET",
            &[simple_font("F1")],
        );
        assert_eq!(out.actual_text.len(), 1);
        assert_eq!(out.actual_text[0].text, "\u{0645}\u{0631}");
    }

    #[test]
    fn marked_content_without_actual_text_records_nothing() {
        // `/OC` optional-content layers wrap the text in our fixture and carry
        // no override — the common case, which must not produce a phantom span.
        let out = run(
            "/OC /MC0 BDC BT /F1 12 Tf (x) Tj ET EMC",
            &[simple_font("F1")],
        );
        assert_eq!(out.glyphs.len(), 1);
        assert!(out.actual_text.is_empty());
    }

    #[test]
    fn an_empty_marked_section_records_no_override() {
        // The section declared text but painted nothing, so there is no glyph
        // range to attach it to.
        let out = run("/Span <</ActualText (x)>> BDC EMC", &[simple_font("F1")]);
        assert!(out.actual_text.is_empty());
    }

    #[test]
    fn nested_sections_close_in_the_right_order() {
        // The inner `EMC` must close the inner section, not the outer one.
        let out = run(
            "BT /F1 12 Tf \
             /Span <</ActualText (outer)>> BDC (a) Tj \
             /Span <</ActualText (inner)>> BDC (b) Tj EMC \
             (c) Tj EMC ET",
            &[simple_font("F1")],
        );
        assert_eq!(out.glyphs.len(), 3);
        assert_eq!(out.actual_text.len(), 2);

        // The inner section closes first, covering only glyph 1.
        assert_eq!(out.actual_text[0].text, "inner");
        assert_eq!((out.actual_text[0].start, out.actual_text[0].end), (1, 2));
        // The outer covers all three.
        assert_eq!(out.actual_text[1].text, "outer");
        assert_eq!((out.actual_text[1].start, out.actual_text[1].end), (0, 3));
    }

    #[test]
    fn mcids_are_recorded_as_glyph_ranges() {
        let out = run(
            "BT /F1 12 Tf /P <</MCID 0>> BDC (ab) Tj EMC /P <</MCID 7>> BDC (c) Tj EMC ET",
            &[simple_font("F1")],
        );
        assert_eq!(out.glyphs.len(), 3);
        assert_eq!(
            out.mcid_spans,
            vec![
                McidSpan {
                    mcid: 0,
                    start: 0,
                    end: 2
                },
                McidSpan {
                    mcid: 7,
                    start: 2,
                    end: 3
                },
            ]
        );
    }

    #[test]
    fn an_untagged_page_records_no_mcids() {
        // The overwhelmingly common case: no `/MCID` anywhere, so the tagged
        // path must stay entirely out of the way.
        let out = run("BT /F1 12 Tf (x) Tj ET", &[simple_font("F1")]);
        assert!(out.mcid_spans.is_empty());
    }

    #[test]
    fn an_mcid_section_that_paints_nothing_is_not_recorded() {
        // An empty span would name a glyph range that does not exist, and the
        // structure tree would then point at nothing.
        let out = run("/P <</MCID 0>> BDC EMC", &[simple_font("F1")]);
        assert!(out.mcid_spans.is_empty());
    }

    #[test]
    fn an_unmatched_emc_is_survivable() {
        let out = run("EMC BT /F1 12 Tf (x) Tj ET", &[simple_font("F1")]);
        assert_eq!(out.glyphs.len(), 1);
        assert!(out.actual_text.is_empty());
    }

    // ---- ruled lines ------------------------------------------------------

    #[test]
    fn a_stroked_segment_becomes_a_ruled_line() {
        let out = run("100 700 m 400 700 l S", &[]);
        assert_eq!(out.ruled_lines.len(), 1);
        let line = out.ruled_lines[0];
        assert!(line.is_horizontal());
        assert_eq!((line.x0, line.x1), (100.0, 400.0));
        assert_eq!(line.length(), 300.0);
    }

    #[test]
    fn a_thin_filled_rectangle_is_also_a_ruled_line() {
        // At least as common as a stroke, and collapsed to the same
        // representation so that downstream code sees one kind of border.
        let out = run("100 700 300 0.7 re f", &[]);
        assert_eq!(out.ruled_lines.len(), 1);
        let line = out.ruled_lines[0];
        assert!(line.is_horizontal());
        // Collapsed to the rectangle's centre line.
        // Tolerance note: lopdf stores a PDF real as , so 0.7 reaches us
        // as 0.69999999. That is the file's precision, not our error.
        assert!((line.y0 - 700.35).abs() < 1e-4, "got {}", line.y0);
    }

    #[test]
    fn a_thin_vertical_rectangle_is_a_vertical_rule() {
        let out = run("100 400 0.7 300 re f", &[]);
        assert_eq!(out.ruled_lines.len(), 1);
        assert!(out.ruled_lines[0].is_vertical());
        assert_eq!(out.ruled_lines[0].length(), 300.0);
    }

    #[test]
    fn a_clipping_rectangle_paints_nothing() {
        // The case that would put a border around every page: our own corpus
        // opens each one with a full-page clip.
        let out = run("0 0 595 842 re W n", &[]);
        assert!(out.ruled_lines.is_empty());
    }

    #[test]
    fn a_filled_block_is_not_a_border() {
        // A shaded cell background. Reading its edges as rules would invent
        // four borders the table does not have.
        let out = run("100 100 200 150 re f", &[]);
        assert!(out.ruled_lines.is_empty());
    }

    #[test]
    fn diagonals_and_ticks_are_not_borders() {
        // A diagonal is artwork; a 2pt dash is a bullet.
        let out = run("0 0 m 100 100 l S 10 10 m 12 10 l S", &[]);
        assert!(out.ruled_lines.is_empty());
    }

    #[test]
    fn the_ctm_places_the_line() {
        // A rule inside a transformed group lands where the transform puts it,
        // and a later `Q` must not move it back.
        let out = run("q 2 0 0 2 50 50 cm 0 100 m 100 100 l S Q", &[]);
        assert_eq!(out.ruled_lines.len(), 1);
        let line = out.ruled_lines[0];
        assert_eq!((line.x0, line.x1), (50.0, 250.0));
        assert_eq!(line.y0, 250.0);
    }

    #[test]
    fn a_closed_rectangle_path_gives_four_borders() {
        // Drawn as four strokes rather than `re`, which real writers do.
        let out = run("100 100 m 300 100 l 300 200 l 100 200 l h S", &[]);
        assert_eq!(out.ruled_lines.len(), 4);
        assert_eq!(
            out.ruled_lines.iter().filter(|l| l.is_horizontal()).count(),
            2
        );
        assert_eq!(
            out.ruled_lines.iter().filter(|l| l.is_vertical()).count(),
            2
        );
    }

    #[test]
    fn a_stream_that_will_not_parse_yields_nothing_rather_than_panicking() {
        let out = run("BT /F1 12 Tf (unterminated", &[simple_font("F1")]);
        assert!(out.glyphs.is_empty());
    }
}
