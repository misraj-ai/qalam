//! One error type for the whole crate.
//!
//! # Rust lesson: errors are values, not exceptions
//!
//! Rust has no `throw`. A function that can fail returns `Result<T, E>`, which is
//! an enum with two variants: `Ok(T)` or `Err(E)`. The caller *must* deal with it
//! (the compiler warns otherwise), which is why Rust programs rarely die by surprise.
//!
//! The convention is: **one error enum per crate**, listing every way this crate can
//! fail. Callers can then `match` on it and react differently per variant, instead of
//! getting a stringly-typed blob.

use std::path::PathBuf;

/// Every way `qalam-core` can fail.
///
/// `#[derive(Debug)]` gives us `{:?}` printing for free.
/// `#[derive(thiserror::Error)]` generates the `std::error::Error` impl plus a
/// `Display` impl built from the `#[error("...")]` strings below — that is the
/// human-readable message users see.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be read at all (missing, no permission, ...).
    ///
    /// `{path}` and `{source}` in the message interpolate the named fields below,
    /// exactly like `format!` / `println!` do.
    #[error("could not read `{path}`: {source}")]
    Io {
        /// The file we tried to open.
        path: PathBuf,
        /// `source` is a magic field name for thiserror: it marks the underlying
        /// error, so tools can walk the whole error *chain* (cause of cause of ...).
        source: std::io::Error,
    },

    /// lopdf refused the file: broken xref, bad object, truncated stream, ...
    ///
    /// `#[from]` generates `impl From<lopdf::Error> for Error`. That is what makes
    /// the `?` operator work: `let doc = Document::load(p)?;` will auto-convert a
    /// `lopdf::Error` into `Error::Pdf` and return early.
    #[error("malformed PDF: {0}")]
    Pdf(#[from] lopdf::Error),

    /// The caller asked for a page number that does not exist in this document.
    ///
    /// A tuple variant: `{0}` in the message is the first (only) field.
    #[error("page {0} does not exist in this document")]
    PageNotFound(u32),
}

/// Crate-local alias so signatures read `Result<Pdf>` instead of
/// `std::result::Result<Pdf, crate::error::Error>`.
///
/// Note the `std::result::Result` on the right-hand side: we are *shadowing* the
/// name `Result` in this crate, so we must spell the std one out fully here or the
/// alias would refer to itself.
pub type Result<T> = std::result::Result<T, Error>;
