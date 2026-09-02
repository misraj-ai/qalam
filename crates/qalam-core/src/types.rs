//! The vocabulary of the whole pipeline: plain data, no behaviour that touches PDFs.
//!
//! Keeping the *nouns* in one module means every later layer (`content`, `font`,
//! `layout`, ...) speaks the same language, and none of them has to depend on
//! another just to name a rectangle.
//!
//! Right now this holds only what milestone M0 needs (page geometry + font
//! summaries). `Glyph`, `Line`, `Block` and friends land here as the pipeline
//! grows — see PLAN.md §3.

/// A rectangle in PDF *user space*.
///
/// PDF's coordinate system is unlike a screen's: the origin is the **bottom-left**
/// corner and `y` grows **upwards**. Units are points, 1/72 inch, so A4 is
/// `595.276 x 841.89`.
///
/// # Rust lesson: `derive`
///
/// `#[derive(...)]` asks the compiler to write boring impls for us:
/// - `Debug`    → printing with `{:?}`
/// - `Clone`    → explicit duplication via `.clone()`
/// - `Copy`     → the type is cheap enough (32 bytes) to duplicate *implicitly* on
///   assignment instead of being moved. Only valid because every field is itself `Copy`.
/// - `PartialEq`→ `==`. Not full `Eq`, because `f64` has `NaN`, which is not equal
///   to itself; Rust encodes that fact in the type system.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x0: f64,
    /// Bottom edge — remember, `y` grows upwards in PDF space.
    pub y0: f64,
    /// Right edge.
    pub x1: f64,
    /// Top edge.
    pub y1: f64,
}

impl Rect {
    /// Build a rect from any two opposite corners, normalising so that
    /// `x0 <= x1` and `y0 <= y1`.
    ///
    /// PDF writers are allowed to emit a `/MediaBox` with the corners in either
    /// order, so normalising once here saves every downstream consumer from
    /// worrying about it.
    pub fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self {
            x0: x0.min(x1),
            y0: y0.min(y1),
            x1: x0.max(x1),
            y1: y0.max(y1),
        }
    }

    /// Horizontal extent in points.
    pub fn width(&self) -> f64 {
        self.x1 - self.x0
    }

    /// Vertical extent in points.
    pub fn height(&self) -> f64 {
        self.y1 - self.y0
    }
}

/// How a page's content should be rotated for display, in degrees clockwise.
///
/// PDF's `/Rotate` entry is constrained by the spec to a multiple of 90. Encoding
/// that as an enum rather than an `i64` means the rest of the code can never be
/// handed 37 degrees and has no "impossible" branch to write. This is the core
/// Rust idea of *making illegal states unrepresentable*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    /// `#[default]` marks which variant `Rotation::default()` returns — used when
    /// a page has no `/Rotate` entry at all, which the spec says means 0.
    #[default]
    None,
    /// A quarter turn clockwise.
    Cw90,
    /// Upside down.
    Cw180,
    /// A quarter turn anticlockwise.
    Cw270,
}

impl Rotation {
    /// Convert a raw `/Rotate` value into the enum.
    ///
    /// Returns `Option` rather than an `Error` because a nonsensical rotation is
    /// not fatal — the caller can reasonably fall back to `Rotation::None`.
    ///
    /// `rem_euclid` is a modulo that always yields a non-negative result, which
    /// matters because `/Rotate -90` is legal and means the same as 270.
    pub fn from_degrees(deg: i64) -> Option<Self> {
        match deg.rem_euclid(360) {
            0 => Some(Rotation::None),
            90 => Some(Rotation::Cw90),
            180 => Some(Rotation::Cw180),
            270 => Some(Rotation::Cw270),
            _ => None,
        }
    }

    /// The rotation as a plain number of degrees, for display.
    pub fn degrees(self) -> u16 {
        match self {
            Rotation::None => 0,
            Rotation::Cw90 => 90,
            Rotation::Cw180 => 180,
            Rotation::Cw270 => 270,
        }
    }
}

/// Everything we know about one page *before* looking at its content stream.
#[derive(Debug, Clone)]
pub struct PageInfo {
    /// 1-based page number as a human would say it.
    pub number: u32,
    /// The page's visible area (`/MediaBox`), possibly inherited from a parent
    /// node in the page tree.
    pub media_box: Rect,
    /// The page's `/Rotate`, also inheritable.
    pub rotation: Rotation,
    /// The fonts named in this page's `/Resources /Font` dictionary.
    pub fonts: Vec<FontInfo>,
}

/// How a font maps the glyph codes in the content stream back to characters.
///
/// This is the single most important fact about a font for our purposes: it
/// decides whether a page is recoverable without OCR at all (PLAN.md §2, #4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeToUnicode {
    /// The font carries a `/ToUnicode` CMap: a real reverse map, the good case.
    ToUnicode,
    /// No `/ToUnicode`, but a simple font's `/Encoding` may still name glyphs we
    /// can resolve (e.g. `/WinAnsiEncoding`, or `/Differences` with glyph names).
    EncodingOnly,
    /// Neither. Meaning must be guessed from the embedded font program's own
    /// cmap, or the page is simply not recoverable by parsing.
    None,
}

/// A summary of one font resource on a page.
///
/// Deliberately *not* the font itself — parsing CMaps and font programs is L2's
/// job (`font.rs`). This is the cheap reconnaissance we can do at M0.
#[derive(Debug, Clone)]
pub struct FontInfo {
    /// The resource name the content stream uses, e.g. `C2_0` in `/C2_0 1 Tf`.
    pub resource_name: String,
    /// `/Subtype`: `Type0`, `Type1`, `TrueType`, ...
    ///
    /// `Type0` means a *composite* font: codes are usually 2 bytes wide, not 1.
    /// Getting this wrong shreds every string in the page.
    pub subtype: String,
    /// `/BaseFont`, the font's own name, e.g. `ABCDEF+SSTArabic-Bold`.
    /// A `SUBSET+` prefix (six uppercase letters and a plus) means only the used
    /// glyphs were embedded.
    pub base_font: Option<String>,
    /// `/Encoding` when it is a plain name, e.g. `Identity-H` or `WinAnsiEncoding`.
    /// `None` here also covers the case where `/Encoding` is a dictionary, which
    /// L2 will need to inspect properly.
    pub encoding: Option<String>,
    /// Which reverse-mapping route is available for this font.
    pub code_to_unicode: CodeToUnicode,
}

impl FontInfo {
    /// `true` when codes in the content stream are 2 bytes wide rather than 1.
    ///
    /// A first approximation only: composite (`Type0`) fonts with `Identity-H`
    /// use 2-byte codes, which covers the overwhelming majority of Arabic PDFs
    /// (PLAN.md §10.1). The general answer comes from the CMap's
    /// `codespacerange`, which L2 will parse.
    pub fn is_two_byte(&self) -> bool {
        self.subtype == "Type0"
    }
}

// ---------------------------------------------------------------------------
// Styling
//
// Everything below exists so that a later HTML/rich export is *possible*. None
// of it is consulted when extracting plain text. The reason it lives here from
// the start is timing, not ambition: colour is set by graphics operators in the
// content stream (`rg`, `g`, `k`, `scn`) and is simply gone by the time glyphs
// reach the Arabic reconstruction stage. Capturing it costs a few bytes per
// glyph; recovering it later would cost a second parse of the whole stream.
// ---------------------------------------------------------------------------

/// A colour taken from the PDF graphics state.
///
/// The variants mirror PDF's three *device* colour spaces. Components are in the
/// range 0.0–1.0, as PDF stores them — not 0–255.
///
/// # Rust lesson: enums carry data
///
/// Unlike a C `enum`, a Rust enum variant can hold fields, and different variants
/// can hold *different* fields. `Color` is one value that is either a single grey
/// level or three RGB components or four CMYK ones — and the compiler forces you
/// to say which before you can read any of them. This is the same machinery
/// behind `Option` and `Result`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// `DeviceGray`, set by the `g` / `G` operators. 0.0 is black, 1.0 is white.
    Gray(f32),
    /// `DeviceRGB`, set by `rg` / `RG`.
    Rgb(f32, f32, f32),
    /// `DeviceCMYK`, set by `k` / `K`. Subtractive: used by print-oriented PDFs.
    Cmyk(f32, f32, f32, f32),
    /// A colour space we chose not to resolve — `/Separation`, `/DeviceN`,
    /// `/Indexed`, patterns — reached via `cs` + `scn` with a `/ColorSpace`
    /// resource lookup and, often, a tint-transform function.
    ///
    /// Recording our ignorance explicitly beats inventing a plausible-looking
    /// wrong colour: an exporter can then fall back to the default rather than
    /// confidently painting text the wrong shade. See PLAN.md §8.
    Unknown,
}

impl Color {
    /// PDF's initial fill and stroke colour: black, in `DeviceGray`.
    ///
    /// # Rust lesson: `const fn`
    ///
    /// Marking a function `const` lets it run at *compile* time, so `BLACK` can
    /// be used where a constant is required. It costs nothing to allow here.
    pub const BLACK: Color = Color::Gray(0.0);

    /// Convert to 8-bit sRGB, the form an HTML/CSS exporter needs.
    ///
    /// The CMYK conversion is the naive `(1-c)(1-k)` formula, *not* colour-managed:
    /// a real conversion needs the document's ICC profile. For displaying text on
    /// a screen this is the standard approximation and visually close enough.
    ///
    /// [`Color::Unknown`] resolves to black — the PDF default — so callers never
    /// have to handle a "no colour" case.
    pub fn to_rgb8(self) -> [u8; 3] {
        // Clamp to 0.0–1.0 and scale. A malformed PDF can hand us 1.5 or -0.2,
        // and `as u8` on an out-of-range float would saturate silently rather
        // than wrap, but clamping first makes the intent explicit.
        fn byte(v: f32) -> u8 {
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        }

        match self {
            Color::Gray(g) => {
                let v = byte(g);
                [v, v, v]
            }
            Color::Rgb(r, g, b) => [byte(r), byte(g), byte(b)],
            Color::Cmyk(c, m, y, k) => [
                byte((1.0 - c) * (1.0 - k)),
                byte((1.0 - m) * (1.0 - k)),
                byte((1.0 - y) * (1.0 - k)),
            ],
            Color::Unknown => [0, 0, 0],
        }
    }

    /// Render as a CSS hex colour, e.g. `#1a2b3c`.
    ///
    /// `{:02x}` means "lower-case hex, at least 2 digits, zero-padded".
    pub fn to_css_hex(self) -> String {
        let [r, g, b] = self.to_rgb8();
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    /// `true` if this is (approximately) pure black, the overwhelmingly common
    /// case for body text.
    ///
    /// An exporter can use this to omit a redundant `color:` declaration, and the
    /// span-splitting logic can use it to avoid fragmenting a line over rounding
    /// noise. The tolerance absorbs writers that emit `0.0001` instead of `0`.
    pub fn is_black(self) -> bool {
        self.to_rgb8() == [0, 0, 0]
    }
}

/// PDF's initial colour is black; `Default` says so once, here.
impl Default for Color {
    fn default() -> Self {
        Color::BLACK
    }
}

/// How glyphs are painted, from the `Tr` (text rendering mode) operator.
///
/// This matters twice over. It decides *which* colour paints a glyph — fill or
/// stroke — and mode 3 is the classic signature of an OCR text layer stapled
/// invisibly on top of a scanned image, which is a strong signal for the
/// recoverability detector (PLAN.md §3, L4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextRenderMode {
    /// 0 — fill. The default, and what nearly all real text uses.
    #[default]
    Fill,
    /// 1 — stroke (outline only).
    Stroke,
    /// 2 — fill then stroke.
    FillStroke,
    /// 3 — invisible. Painted nowhere; used for OCR layers over scans.
    Invisible,
    /// 4–6 — the above, and also add the glyphs to the clipping path.
    /// We keep the paint behaviour and note the clip separately rather than
    /// modelling seven near-duplicate variants.
    Clip(ClipTextMode),
    /// 7 — add to clipping path only, paint nothing.
    ClipOnly,
}

/// The paint behaviour of the clipping render modes 4–6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipTextMode {
    /// 4 — fill and clip.
    Fill,
    /// 5 — stroke and clip.
    Stroke,
    /// 6 — fill, stroke and clip.
    FillStroke,
}

impl TextRenderMode {
    /// Interpret the operand of `Tr`. Anything outside 0–7 is invalid per the
    /// spec; we fall back to the default rather than failing the page.
    pub fn from_operand(mode: i64) -> Self {
        match mode {
            0 => TextRenderMode::Fill,
            1 => TextRenderMode::Stroke,
            2 => TextRenderMode::FillStroke,
            3 => TextRenderMode::Invisible,
            4 => TextRenderMode::Clip(ClipTextMode::Fill),
            5 => TextRenderMode::Clip(ClipTextMode::Stroke),
            6 => TextRenderMode::Clip(ClipTextMode::FillStroke),
            7 => TextRenderMode::ClipOnly,
            _ => TextRenderMode::default(),
        }
    }

    /// Whether these glyphs actually appear on the page.
    ///
    /// Invisible text is still *extracted* — it is often the only real text a
    /// scanned PDF has — but the detector counts it, and an HTML exporter would
    /// want to know.
    pub fn is_visible(self) -> bool {
        !matches!(self, TextRenderMode::Invisible | TextRenderMode::ClipOnly)
    }

    /// Whether the *stroke* colour, rather than the fill colour, is what a reader
    /// sees. True only for pure stroking (mode 1 and 5).
    pub fn paints_with_stroke_color(self) -> bool {
        matches!(
            self,
            TextRenderMode::Stroke | TextRenderMode::Clip(ClipTextMode::Stroke)
        )
    }
}

/// The visual styling shared by a run of glyphs.
///
/// Assembled by L1 from the graphics state at the moment each glyph is painted,
/// then used by L3 to split a line into uniformly-styled [spans](PLAN.md §3).
/// Plain-text extraction ignores it entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// The colour a reader actually sees — already resolved from fill vs stroke
    /// according to `render_mode`, so consumers need not reason about it.
    pub color: Color,
    /// The font resource name in effect, e.g. `C2_0` from `/C2_0 1 Tf`.
    pub font: String,
    /// Effective size in points.
    ///
    /// **Not** the operand of `Tf`. PDF writers commonly emit `/C2_0 1 Tf` and
    /// then put the real scale in the text matrix — PLAN.md §10.1 saw exactly
    /// that, with a true size of ~20.56pt behind a `Tf` of 1. This field holds
    /// the composed result: `Tf size x text matrix x CTM`.
    pub size: f64,
    /// How the glyphs are painted (see [`TextRenderMode`]).
    pub render_mode: TextRenderMode,
}

impl Style {
    /// Whether two runs are similar enough to merge into one span.
    ///
    /// Sizes are compared with a tolerance because they are computed through a
    /// matrix multiplication: two runs meant to be 12pt can differ by a hair of
    /// floating-point noise, and splitting a span over that would be absurd.
    ///
    /// # Rust lesson: never compare floats with `==`
    ///
    /// `0.1 + 0.2 == 0.3` is `false`. Compare the absolute difference against a
    /// tolerance instead — which is also why [`Style`] derives `PartialEq` but
    /// this method exists alongside it.
    pub fn merges_with(&self, other: &Style) -> bool {
        const SIZE_TOLERANCE: f64 = 0.01;

        self.color == other.color
            && self.font == other.font
            && self.render_mode == other.render_mode
            && (self.size - other.size).abs() < SIZE_TOLERANCE
    }
}

/// One glyph as the content stream painted it: a font code at a position.
///
/// This is L1's output and the pipeline's atom. Note what it is **not**: there
/// is no `char` here. `code` is an index into a font, and only that font's
/// `/ToUnicode` map can say which character it means — which is why turning
/// glyphs into text is a separate layer (L2, `font.rs`). Conflating the two is
/// the root of most broken PDF extractors.
///
/// Positions are in **device space**, with PDF's origin at the bottom-left and
/// `y` growing upwards, so a larger `y` means higher on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Glyph {
    /// The font-specific code: one byte for a simple font, two for a composite
    /// (`Identity-H`) one, where it is a CID.
    pub code: u32,
    /// Horizontal position of the glyph's origin.
    pub x: f64,
    /// Vertical position of the glyph's baseline.
    pub y: f64,
    /// How far the pen moved after painting, in device space. Used to spot word
    /// gaps and line breaks by geometry rather than by trusting stream order.
    pub advance: f64,
    /// Colour, font, effective size and render mode at the moment of painting.
    pub style: Style,
}

/// A font's data pulled out of the PDF object graph, in plain Rust form.
///
/// This is the hand-off from L0 to L2. `parser.rs` knows how to walk `/ToUnicode`
/// references, `/DescendantFonts` and `/W` arrays; `font.rs` knows how to make
/// sense of what comes out. Splitting them at plain data means the CMap parser
/// can be unit-tested on a byte string with no PDF anywhere in sight.
///
/// All widths here are in **glyph space**: thousandths of an em, as PDF stores
/// them. A width of 500 is half an em.
#[derive(Debug, Clone)]
pub struct RawFont {
    /// The summary from L0 — subtype, encoding, recoverability route.
    pub info: FontInfo,
    /// The undecoded bytes of the `/ToUnicode` CMap stream, if the font has one.
    pub to_unicode: Option<Vec<u8>>,
    /// `/FirstChar`: the code that `widths[0]` describes. Simple fonts only.
    pub first_char: u32,
    /// `/Widths`: one entry per code from `first_char` upwards. Simple fonts only.
    pub widths: Vec<f64>,
    /// `/FontDescriptor /MissingWidth`: the width for codes outside `/Widths`.
    /// Defaults to 0, per the spec.
    pub missing_width: f64,
    /// `/DW`, the default width for a CID font. The spec's default is 1000.
    pub default_width: f64,
    /// `/W`, flattened to inclusive `(first_cid, last_cid, width)` triples.
    /// Composite fonts only.
    pub cid_widths: Vec<(u32, u32, f64)>,
}

impl RawFont {
    /// An empty font description, used as the starting point when filling one in.
    pub fn new(info: FontInfo) -> Self {
        Self {
            info,
            to_unicode: None,
            first_char: 0,
            widths: Vec::new(),
            missing_width: 0.0,
            // The spec's default when `/DW` is absent.
            default_width: 1000.0,
            cid_widths: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_and_rgb_convert_to_srgb() {
        assert_eq!(Color::Gray(0.0).to_rgb8(), [0, 0, 0]);
        assert_eq!(Color::Gray(1.0).to_rgb8(), [255, 255, 255]);
        assert_eq!(Color::Rgb(1.0, 0.0, 0.0).to_rgb8(), [255, 0, 0]);
        assert_eq!(Color::Rgb(1.0, 0.0, 0.0).to_css_hex(), "#ff0000");
    }

    #[test]
    fn cmyk_black_is_black_and_cyan_is_cyan() {
        // Pure K: c=m=y=0, k=1 → all channels (1-0)*(1-1) = 0.
        assert_eq!(Color::Cmyk(0.0, 0.0, 0.0, 1.0).to_rgb8(), [0, 0, 0]);
        // Pure cyan ink absorbs red.
        assert_eq!(Color::Cmyk(1.0, 0.0, 0.0, 0.0).to_rgb8(), [0, 255, 255]);
    }

    #[test]
    fn out_of_range_components_are_clamped_not_wrapped() {
        // A malformed PDF can emit values outside 0.0–1.0; they must not wrap
        // around into a wildly wrong colour.
        assert_eq!(Color::Rgb(2.0, -1.0, 0.5).to_rgb8(), [255, 0, 128]);
    }

    #[test]
    fn unknown_color_falls_back_to_the_pdf_default() {
        assert_eq!(Color::Unknown.to_rgb8(), Color::BLACK.to_rgb8());
        assert!(Color::default().is_black());
    }

    #[test]
    fn render_modes_map_from_operands() {
        assert_eq!(TextRenderMode::from_operand(0), TextRenderMode::Fill);
        assert_eq!(TextRenderMode::from_operand(3), TextRenderMode::Invisible);
        assert_eq!(
            TextRenderMode::from_operand(5),
            TextRenderMode::Clip(ClipTextMode::Stroke)
        );
        // Out of range → the default, rather than a panic or a wrong mode.
        assert_eq!(TextRenderMode::from_operand(99), TextRenderMode::Fill);
    }

    #[test]
    fn invisible_text_is_the_ocr_layer_signal() {
        assert!(!TextRenderMode::Invisible.is_visible());
        assert!(!TextRenderMode::ClipOnly.is_visible());
        assert!(TextRenderMode::Fill.is_visible());
        assert!(TextRenderMode::Stroke.paints_with_stroke_color());
        assert!(!TextRenderMode::Fill.paints_with_stroke_color());
    }

    #[test]
    fn spans_merge_across_floating_point_noise_but_not_real_changes() {
        let style = |size: f64, color: Color| Style {
            color,
            font: "C2_0".to_string(),
            size,
            render_mode: TextRenderMode::Fill,
        };

        let a = style(12.0, Color::BLACK);
        // The same nominal size, arrived at through matrix arithmetic.
        assert!(a.merges_with(&style(12.000_000_1, Color::BLACK)));
        // A genuine size change, and a genuine colour change, both split.
        assert!(!a.merges_with(&style(14.0, Color::BLACK)));
        assert!(!a.merges_with(&style(12.0, Color::Rgb(1.0, 0.0, 0.0))));
    }
}
