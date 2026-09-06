//! **L1 (part 1) — the graphics state.**
//!
//! A PDF content stream is a little stack machine. Operators do not carry their
//! own context: `Tj` paints text using whatever font, colour and transform were
//! set by *earlier* operators, and `q` / `Q` push and pop that whole context.
//! Interpreting text therefore means maintaining the same state the renderer
//! would.
//!
//! This module owns the part of that state which is not text-specific: the
//! current transformation matrix and the colours. `content.rs` owns the text
//! state (font, size, `Tm`) and drives both.
//!
//! We deliberately model only what affects *where text lands and what colour it
//! is*. Line joins, dash patterns, blend modes and clipping paths are tracked
//! nowhere — see PLAN.md §1, "out of scope".

use crate::types::{Color, TextRenderMode};

/// A PDF transformation matrix.
///
/// PDF writes it as six numbers `[a b c d e f]`, standing for the 3x3 matrix
///
/// ```text
///   | a  b  0 |
///   | c  d  0 |
///   | e  f  1 |
/// ```
///
/// The last column is always `(0, 0, 1)`, so only six values are stored. `a`/`d`
/// scale, `b`/`c` skew and rotate, and `e`/`f` translate.
///
/// # Rust lesson: `Copy` for small value types
///
/// Six `f64`s is 48 bytes — small enough that copying beats referencing. Deriving
/// `Copy` means a `Matrix` is duplicated on assignment instead of *moved*, so
/// using one does not invalidate the original. Reach for it on small,
/// arithmetic-like types; avoid it on anything owning a heap allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix {
    /// Horizontal scale.
    pub a: f64,
    /// Vertical skew.
    pub b: f64,
    /// Horizontal skew.
    pub c: f64,
    /// Vertical scale.
    pub d: f64,
    /// Horizontal translation.
    pub e: f64,
    /// Vertical translation.
    pub f: f64,
}

impl Matrix {
    /// The do-nothing matrix: `[1 0 0 1 0 0]`.
    pub const IDENTITY: Matrix = Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// Build from the six operands of a `cm` or `Tm` operator, in stream order.
    pub fn new(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Self {
        Self { a, b, c, d, e, f }
    }

    /// A pure translation by `(tx, ty)` — what `Td` and `TD` produce.
    pub fn translation(tx: f64, ty: f64) -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    /// A pure scale — used to fold the font size into the text matrix.
    pub fn scale(sx: f64, sy: f64) -> Self {
        Self::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// Matrix product `self x other`.
    ///
    /// **Order matters and is easy to get backwards.** PDF composes
    /// left-to-right: applying `self` and *then* `other` is `self.then(other)`.
    /// The `cm` operator means `CTM = operand.then(old_CTM)`, because the new
    /// transform applies to coordinates *before* the existing one does.
    ///
    /// Naming it `then` rather than `multiply` makes the direction impossible to
    /// misread at the call site.
    pub fn then(self, other: Matrix) -> Matrix {
        Matrix {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }

    /// Map a point through this matrix, giving device-space coordinates.
    pub fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// How much this matrix scales vertical distances — the number that turns a
    /// nominal font size into an effective one.
    ///
    /// For an unrotated matrix this is just `d`. The general form is the length
    /// of the transformed unit-y vector, `sqrt(c^2 + d^2)`, which stays correct
    /// when the text is rotated. This is the "size 1 Tf plus scale in Tm" idiom
    /// from PLAN.md §10.1: `/C2_0 1 Tf` with `20.5559 0 0 20.5559 ... Tm` is
    /// 20.56pt text, and reading `Tf` alone reports 1pt.
    pub fn vertical_scale(self) -> f64 {
        (self.c * self.c + self.d * self.d).sqrt()
    }

    /// How much this matrix scales horizontal distances, `sqrt(a^2 + b^2)`.
    /// Used to turn a glyph's advance width into a device-space distance.
    pub fn horizontal_scale(self) -> f64 {
        (self.a * self.a + self.b * self.b).sqrt()
    }
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix::IDENTITY
    }
}

/// Which colour space is currently selected, remembered only so that the
/// component-taking operators (`sc` / `scn`) know how to read their operands.
///
/// `rg` / `g` / `k` set the space *and* the colour in one go, so they do not
/// need this; `cs` followed by `scn` splits the two, and then the count of
/// operands alone is ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSpaceKind {
    /// `/DeviceGray`, `/CalGray`, or an `/ICCBased` space with `/N 1`.
    #[default]
    Gray,
    /// `/DeviceRGB`, `/CalRGB`, `/Lab`, or `/ICCBased` with `/N 3`.
    Rgb,
    /// `/DeviceCMYK`, or `/ICCBased` with `/N 4`.
    Cmyk,
    /// Anything we do not resolve: `/Separation`, `/DeviceN`, `/Indexed`,
    /// `/Pattern`. Colours set in these become [`Color::Unknown`] rather than a
    /// confident guess (PLAN.md §8).
    Other,
}

impl ColorSpaceKind {
    /// Classify a colour-space *name* as it appears in a `cs` / `CS` operator.
    ///
    /// A name that is not one of the device spaces is usually a key into the
    /// page's `/Resources /ColorSpace` dictionary, which we do not follow yet —
    /// hence [`ColorSpaceKind::Other`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "DeviceGray" | "CalGray" | "G" => ColorSpaceKind::Gray,
            "DeviceRGB" | "CalRGB" | "Lab" | "RGB" => ColorSpaceKind::Rgb,
            "DeviceCMYK" | "CMYK" => ColorSpaceKind::Cmyk,
            _ => ColorSpaceKind::Other,
        }
    }

    /// Interpret the operands of `sc` / `scn` in this space.
    ///
    /// `scn` may also take a trailing pattern *name*, in which case the numeric
    /// operands do not describe a colour at all; the caller strips names before
    /// calling, and an operand count that does not match the space yields
    /// [`Color::Unknown`] rather than a misreading.
    pub fn color_from_components(self, comps: &[f64]) -> Color {
        // `as f32` narrows: PDF numbers are f64 here, colours are stored as f32
        // because 24-bit output needs nothing like f64 precision.
        match (self, comps) {
            (ColorSpaceKind::Gray, [g]) => Color::Gray(*g as f32),
            (ColorSpaceKind::Rgb, [r, g, b]) => Color::Rgb(*r as f32, *g as f32, *b as f32),
            (ColorSpaceKind::Cmyk, [c, m, y, k]) => {
                Color::Cmyk(*c as f32, *m as f32, *y as f32, *k as f32)
            }
            // A `/Separation` space takes a single tint that would need the
            // space's tint-transform function to become a real colour.
            _ => Color::Unknown,
        }
    }
}

/// The text state *parameters*, which live in the graphics state.
///
/// # Rust lesson: the spec decides where state belongs
///
/// It is tempting to keep everything text-related together in the text
/// interpreter. ISO 32000 §8.4.1 says otherwise: these eight parameters are
/// part of the **graphics** state, so `q` saves them and `Q` restores them,
/// exactly like the transform and the colours.
///
/// Treating them as interpreter globals instead is not a subtle difference. A
/// PDF that sets `-4.02 Tc` inside one `q … Q` block expects it to end at the
/// `Q`; if it does not, every later glyph on the page is advanced 4pt too
/// little, the computed positions collapse into each other, and text sorted by
/// position comes out shuffled. Fixture `1.pdf` does exactly this
/// (PLAN.md §10.17).
///
/// The two text *matrices* are **not** here, because they are not part of the
/// graphics state: they are created by `BT` and die at `ET`.
#[derive(Debug, Clone, PartialEq)]
pub struct TextParams {
    /// The font resource name from `Tf`.
    pub font: String,
    /// The `Tf` size operand — often a meaningless 1, with the real scale in
    /// the text matrix. Never report this as the font size.
    pub font_size: f64,
    /// `Tc`, extra space after every glyph, in unscaled text units.
    pub char_spacing: f64,
    /// `Tw`, extra space after single-byte code 32 only.
    pub word_spacing: f64,
    /// `Tz` as a fraction: the operator takes a percentage, 1.0 means 100.
    pub horizontal_scale: f64,
    /// `TL`, the line height used by `T*`, `'` and `"`.
    pub leading: f64,
    /// `Ts`, superscript and subscript offset.
    pub rise: f64,
    /// `Tr`, how glyphs are painted — including mode 3, invisible.
    pub render_mode: TextRenderMode,
}

impl Default for TextParams {
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
        }
    }
}

/// The graphics state saved by `q` and restored by `Q`.
///
/// Every field here has a spec-defined initial value, which is what
/// [`Default`] produces: identity transform, black fill and stroke, grey space.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphicsState {
    /// Current transformation matrix: user space → device space.
    pub ctm: Matrix,
    /// Non-stroking (fill) colour — what text is normally painted with.
    pub fill_color: Color,
    /// Stroking colour — relevant to text only in render modes 1 and 5.
    pub stroke_color: Color,
    /// Space selected by `cs`, needed to read a later `scn`.
    pub fill_space: ColorSpaceKind,
    /// Space selected by `CS`, needed to read a later `SCN`.
    pub stroke_space: ColorSpaceKind,
    /// The text state parameters — saved and restored with everything else.
    pub text: TextParams,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self {
            ctm: Matrix::IDENTITY,
            fill_color: Color::BLACK,
            stroke_color: Color::BLACK,
            fill_space: ColorSpaceKind::Gray,
            stroke_space: ColorSpaceKind::Gray,
            text: TextParams::default(),
        }
    }
}

/// The `q` / `Q` stack: a current state plus the ones saved beneath it.
///
/// # Rust lesson: invariants enforced by construction
///
/// `current` is a separate field rather than the top of `saved`, so there is
/// always exactly one current state. A single `Vec` could be emptied by a
/// stray `Q`, and every reader would then need to handle "no state at all".
/// Here that situation cannot arise, so no code downstream has to consider it.
#[derive(Debug, Clone, Default)]
pub struct GraphicsStack {
    current: GraphicsState,
    saved: Vec<GraphicsState>,
}

impl GraphicsStack {
    /// A fresh stack holding the spec's initial state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the state in effect right now.
    pub fn current(&self) -> &GraphicsState {
        &self.current
    }

    /// Modify the state in effect right now.
    ///
    /// Returns `&mut` so callers can write `stack.current_mut().fill_color = c;`
    /// — the borrow checker guarantees no one else holds a reference meanwhile.
    pub fn current_mut(&mut self) -> &mut GraphicsState {
        &mut self.current
    }

    /// `q` — push a copy of the current state.
    pub fn save(&mut self) {
        // Guard against a pathological stream with millions of unmatched `q`s,
        // which would otherwise grow this Vec without bound.
        const MAX_DEPTH: usize = 64;
        if self.saved.len() < MAX_DEPTH {
            self.saved.push(self.current.clone());
        }
    }

    /// `Q` — restore the most recently saved state.
    ///
    /// An unmatched `Q` is silently ignored. Real-world PDFs are not always
    /// balanced, and refusing to render one is worse than carrying on.
    pub fn restore(&mut self) {
        if let Some(previous) = self.saved.pop() {
            self.current = previous;
        }
    }

    /// How many states are stacked below the current one. For diagnostics.
    pub fn depth(&self) -> usize {
        self.saved.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare two matrices allowing for floating-point drift.
    fn close(m: Matrix, expected: [f64; 6]) {
        let got = [m.a, m.b, m.c, m.d, m.e, m.f];
        for (g, e) in got.iter().zip(expected.iter()) {
            assert!((g - e).abs() < 1e-9, "got {got:?}, expected {expected:?}");
        }
    }

    #[test]
    fn identity_changes_nothing() {
        let m = Matrix::new(2.0, 0.0, 0.0, 3.0, 10.0, 20.0);
        close(m.then(Matrix::IDENTITY), [2.0, 0.0, 0.0, 3.0, 10.0, 20.0]);
        close(Matrix::IDENTITY.then(m), [2.0, 0.0, 0.0, 3.0, 10.0, 20.0]);
    }

    #[test]
    fn scale_then_translate_is_not_translate_then_scale() {
        let scale = Matrix::scale(2.0, 2.0);
        let translate = Matrix::translation(10.0, 0.0);
        // Scale first: the translation is applied afterwards, untouched.
        assert_eq!(scale.then(translate).apply(1.0, 0.0), (12.0, 0.0));
        // Translate first: the scale then doubles the translation too.
        assert_eq!(translate.then(scale).apply(1.0, 0.0), (22.0, 0.0));
    }

    #[test]
    fn vertical_scale_recovers_the_real_font_size() {
        // The exact matrix from PLAN.md §10.1, paired with `/C2_0 1 Tf`.
        let tm = Matrix::new(20.5559, 0.0, 0.0, 20.5559, 118.66, 476.77);
        assert!((tm.vertical_scale() - 20.5559).abs() < 1e-9);
    }

    #[test]
    fn vertical_scale_survives_rotation() {
        // 90 degrees: [0 1 -1 0 0 0] scaled by 12. `d` alone would report 0.
        let rotated = Matrix::new(0.0, 12.0, -12.0, 0.0, 0.0, 0.0);
        assert_eq!(rotated.d, 0.0);
        assert!((rotated.vertical_scale() - 12.0).abs() < 1e-9);
    }

    #[test]
    fn q_and_q_restore_colour() {
        let mut stack = GraphicsStack::new();
        assert!(stack.current().fill_color.is_black());

        stack.save();
        stack.current_mut().fill_color = Color::Rgb(1.0, 0.0, 0.0);
        assert_eq!(stack.current().fill_color, Color::Rgb(1.0, 0.0, 0.0));
        assert_eq!(stack.depth(), 1);

        stack.restore();
        assert!(stack.current().fill_color.is_black());
        assert_eq!(stack.depth(), 0);
    }

    #[test]
    fn q_and_q_restore_the_text_parameters_too() {
        // The bug in `1.pdf`: `-4.02 Tc` set inside a `q … Q` block leaked out
        // of it, so every later glyph advanced 4pt too little and the computed
        // positions collapsed together. ISO 32000 §8.4.1 puts these parameters
        // in the graphics state precisely so this cannot happen.
        let mut stack = GraphicsStack::new();
        assert_eq!(stack.current().text.char_spacing, 0.0);

        stack.save();
        stack.current_mut().text.char_spacing = -4.02;
        stack.current_mut().text.font = "F4".to_string();
        stack.current_mut().text.horizontal_scale = 0.5;

        stack.restore();
        assert_eq!(stack.current().text.char_spacing, 0.0, "Tc leaked past `Q`");
        assert_eq!(stack.current().text.font, "");
        assert_eq!(stack.current().text.horizontal_scale, 1.0);
    }

    #[test]
    fn unmatched_restore_is_survivable() {
        let mut stack = GraphicsStack::new();
        stack.current_mut().fill_color = Color::Rgb(0.0, 1.0, 0.0);
        // A stray `Q` with nothing saved must not panic or clear the state.
        stack.restore();
        assert_eq!(stack.current().fill_color, Color::Rgb(0.0, 1.0, 0.0));
    }

    #[test]
    fn color_spaces_read_their_operands() {
        assert_eq!(
            ColorSpaceKind::Rgb.color_from_components(&[1.0, 0.0, 0.0]),
            Color::Rgb(1.0, 0.0, 0.0)
        );
        // Right space, wrong operand count → we admit ignorance.
        assert_eq!(
            ColorSpaceKind::Rgb.color_from_components(&[1.0]),
            Color::Unknown
        );
        // A separation tint needs a transform function we do not evaluate.
        assert_eq!(
            ColorSpaceKind::Other.color_from_components(&[0.5]),
            Color::Unknown
        );
    }
}
