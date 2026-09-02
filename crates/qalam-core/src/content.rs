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
use crate::types::{FontInfo, Glyph, Style, TextRenderMode};

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

/// Everything the interpreter recovered from one page.
#[derive(Debug, Default)]
pub struct PageGlyphs {
    /// Every glyph painted, in the order the stream painted them — which for
    /// Arabic is **visual** order, not reading order. Fixing that is L3's job.
    pub glyphs: Vec<Glyph>,
    /// Count of form XObjects (`/Fm0 Do`) we walked past without entering.
    ///
    /// A form is a reusable sub-stream with its own resources, and it may
    /// contain text. Recursing into one needs the page's `/XObject` dictionary,
    /// which this module deliberately does not have. Reporting the count keeps
    /// the omission visible instead of silently losing text.
    pub skipped_forms: usize,
    /// Operators we recognised as text-affecting but chose not to implement.
    /// Useful while building; expected to stay empty on ordinary files.
    pub unsupported: Vec<String>,
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

            // ---- everything else -----------------------------------------
            "Do" => {
                // Image XObjects are L7's business. Form XObjects may hold text
                // we are missing, so count them (see `PageGlyphs::skipped_forms`).
                self.out.skipped_forms += 1;
            }
            _ => {
                // Paths, clipping, shading, marked content, inline images: all
                // irrelevant to where text lands, so silently ignored.
            }
        }
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
    fn form_xobjects_are_counted_not_silently_dropped() {
        let out = run("q /Fm0 Do Q", &[]);
        assert_eq!(out.skipped_forms, 1);
        assert!(out.glyphs.is_empty());
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

    #[test]
    fn a_stream_that_will_not_parse_yields_nothing_rather_than_panicking() {
        let out = run("BT /F1 12 Tf (unterminated", &[simple_font("F1")]);
        assert!(out.glyphs.is_empty());
    }
}
