//! # qalam-core
//!
//! Extract correct, logical-order Unicode Arabic text from digitally-born PDFs —
//! and say honestly when a PDF cannot be recovered without OCR.
//!
//! This crate knows nothing about Python or the command line. It is the library;
//! `qalam-cli` and (later) `qalam-py` are thin shells around it.
//!
//! ## Pipeline
//!
//! The layers below map 1:1 onto modules, and data flows strictly downward. Each
//! layer is testable on its own, which is the point of splitting them:
//!
//! | Layer | Module | Job |
//! |---|---|---|
//! | L0 | [`parser`] | PDF object graph → pages, geometry, font resources |
//! | L1 | [`content`]  | content stream → positioned, styled glyph codes |
//! | L1 | [`graphics`] | graphics-state stack (`q`/`Q`, `cm`) and colour operators |
//! | L2 | [`font`]     | glyph code → Unicode, via `/ToUnicode` & friends |
//! | L3 | [`arabic`]   | line grouping → bidi reorder → NFKC normalise |
//! | L3 | [`bidi`]     | UAX #9 wrapper: visual order → logical order |
//! | L4 | [`detect`]   | recoverability scoring: `ok` vs `needs_ocr` |
//! | L6 | [`layout`]   | recursive XY-cut: columns and reading order (RTL) |
//!
//! Alongside the text, L1 records the *styling* each glyph was painted with —
//! fill colour, font, effective size, render mode — as a [`Style`]. Nothing in
//! Tier A reads it, but colour lives in the graphics state and is unrecoverable
//! once the stream has been walked, so it is captured at the only moment it is
//! available. It is what a later HTML export would be built on. See PLAN.md §1.
//!
//! The one rule that is easy to get wrong: **reorder before normalising**.
//! NFKC expands the lam-alef ligature `ﻻ` into two characters, and reordering
//! *after* that flips them, turning `ولا` into `وال`. See PLAN.md §3.
//!
//! ## Rust lesson: what `lib.rs` is
//!
//! `lib.rs` is the crate root. Rust does not scan the directory for source files:
//! a module exists only once it is declared with `mod`. `pub mod` re-publishes it
//! to users of the crate; a bare `mod` keeps it internal.

// Enforce documentation on everything public. A learning project benefits most
// from a compiler that nags about unexplained APIs.
#![warn(missing_docs)]

pub mod arabic;
pub mod bidi;
pub mod content;
pub mod detect;
pub mod encoding;
pub mod error;
pub mod font;
pub mod graphics;
pub mod layout;
pub mod parser;
pub mod types;

// Re-export the handful of names most callers need, so they can write
// `use qalam_core::Pdf;` instead of `use qalam_core::parser::Pdf;`.
// The module paths stay public too, for anyone who wants the long form.
pub use arabic::{lines_to_text, reconstruct, TextLine};
pub use bidi::Direction;
pub use content::{interpret, ActualText, AssumedWidths, GlyphWidths, PageGlyphs};
pub use detect::{assess, PageReport, Recoverability, Signals};
pub use error::{Error, Result};
pub use font::{CMap, Font, FontMap};
pub use graphics::Matrix;
pub use parser::Pdf;
pub use types::{
    ClipTextMode, CodeToUnicode, Color, FontInfo, Glyph, PageInfo, RawFont, Rect, Rotation, Style,
    TextRenderMode,
};
