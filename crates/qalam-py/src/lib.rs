//! **L5 — the Python bindings.**
//!
//! A thin shell over [`qalam_core::Document`]. Every decision about *how* to
//! extract text lives in the core crate; this file only decides how that
//! result should look to a Python programmer. If you find yourself writing
//! extraction logic here, it belongs one crate down.
//!
//! ```python
//! import qalam
//!
//! text = qalam.extract_text("guide.pdf")
//!
//! doc = qalam.Document("guide.pdf")
//! print(doc.confidence, doc.pages_needing_ocr)
//! for page in doc.pages:
//!     if page.needs_ocr:
//!         print(f"page {page.number}: {page.reasons}")
//! ```
//!
//! # Rust lesson: what a `#[pyclass]` really is
//!
//! PyO3 generates, for each `#[pyclass]`, a C-level Python type object whose
//! instances own a Rust value. A `Py<Page>` is a *reference-counted handle* to
//! one of those Python objects — cloning it bumps Python's refcount rather than
//! copying the data, which is why [`Document`] stores its pages that way: the
//! `pages` property can be read a hundred times without rebuilding anything.

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyIndexError, PyOSError, PyPermissionError};
use pyo3::prelude::*;

// The base exception for anything qalam-specific, e.g. a malformed PDF.
//
// Deliberately *not* used for everything: a missing file raises
// `FileNotFoundError` and a bad page number raises `IndexError`, because those
// are the errors a Python programmer already knows how to catch.
//
// A `///` comment here would be discarded — the macro takes the Python-visible
// docstring as its fourth argument instead, which is what `help(QalamError)`
// shows.
create_exception!(
    qalam,
    QalamError,
    PyException,
    "Raised when a PDF cannot be parsed."
);

/// Translate a core error into the Python exception a caller would expect.
///
/// # Rust lesson: matching on a nested value
///
/// The `Error::Io` arm destructures the wrapped `std::io::Error` and matches on
/// its *kind*, so "the file is not there" and "you may not read it" become
/// different Python exceptions rather than one generic failure.
fn to_py_err(err: qalam_core::Error) -> PyErr {
    let message = err.to_string();
    match err {
        qalam_core::Error::Io { source, .. } => match source.kind() {
            std::io::ErrorKind::NotFound => pyo3::exceptions::PyFileNotFoundError::new_err(message),
            std::io::ErrorKind::PermissionDenied => PyPermissionError::new_err(message),
            _ => PyOSError::new_err(message),
        },
        qalam_core::Error::PageNotFound(_) => PyIndexError::new_err(message),
        qalam_core::Error::Pdf(_) => QalamError::new_err(message),
    }
}

/// One line of extracted text, with its geometry and styling.
#[pyclass(frozen, module = "qalam")]
pub struct Line {
    /// The text, in logical order and normalised — base letters, not
    /// presentation forms.
    #[pyo3(get)]
    text: String,
    /// The y coordinate of the line's baseline, in PDF points from the bottom
    /// of the page.
    #[pyo3(get)]
    baseline: f64,
    /// `"rtl"` or `"ltr"`: the base direction this line was reordered with.
    #[pyo3(get)]
    direction: String,
    /// The font resource name most of the line was set in.
    #[pyo3(get)]
    font: String,
    /// Effective type size in points — composed from the text matrix, not the
    /// `Tf` operand, which is often a meaningless 1.
    #[pyo3(get)]
    size: f64,
    /// The line's colour as a CSS hex string, e.g. `"#04684b"`.
    #[pyo3(get)]
    color: String,
    /// Fraction of this line's glyphs that resolved to characters, 0.0 to 1.0.
    #[pyo3(get)]
    confidence: f64,
}

#[pymethods]
impl Line {
    /// `repr()` shows a truncated preview, so printing a page's lines in a REPL
    /// stays readable.
    fn __repr__(&self) -> String {
        let preview: String = self.text.chars().take(30).collect();
        let ellipsis = if self.text.chars().count() > 30 {
            "…"
        } else {
            ""
        };
        format!("<Line {:.1}pt {:?}{}>", self.size, preview, ellipsis)
    }

    /// `str()` gives the text itself, so `"".join(page.lines)` reads naturally.
    fn __str__(&self) -> &str {
        &self.text
    }
}

/// One page of a document.
#[pyclass(frozen, module = "qalam")]
pub struct Page {
    /// 1-based page number.
    #[pyo3(get)]
    number: u32,
    /// Page width in points.
    #[pyo3(get)]
    width: f64,
    /// Page height in points.
    #[pyo3(get)]
    height: f64,
    /// Display rotation in degrees clockwise: 0, 90, 180 or 270.
    #[pyo3(get)]
    rotation: u16,
    /// The page's text, lines joined with newlines.
    ///
    /// Returned whatever the verdict — a caller who has checked `needs_ocr` may
    /// still want to inspect degraded text. It is `Document.text` that declines
    /// to hand back the untrustworthy pages.
    #[pyo3(get)]
    text: String,
    /// `"ok"`, `"degraded"` or `"needs_ocr"`.
    #[pyo3(get)]
    verdict: String,
    /// True when this page has no usable text layer and should go to OCR.
    #[pyo3(get)]
    needs_ocr: bool,
    /// How far the extracted text can be trusted, 0.0 to 1.0.
    #[pyo3(get)]
    confidence: f64,
    /// Human-readable reasons behind the verdict; empty when the page is clean.
    #[pyo3(get)]
    reasons: Vec<String>,
    lines: Vec<Py<Line>>,
}

#[pymethods]
impl Page {
    /// The page's lines, in reading order — columns already resolved.
    #[getter]
    fn lines(&self, py: Python<'_>) -> Vec<Py<Line>> {
        // `clone_ref` bumps Python's reference count; it does not copy a Line.
        self.lines.iter().map(|l| l.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "<Page {} {}x{}pt {} confidence={:.2}>",
            self.number,
            self.width.round(),
            self.height.round(),
            self.verdict,
            self.confidence,
        )
    }

    fn __str__(&self) -> &str {
        &self.text
    }
}

/// An extracted PDF document.
///
/// Extraction happens once, when the object is constructed, so every property
/// below is a plain lookup that cannot fail.
#[pyclass(frozen, module = "qalam")]
pub struct Document {
    /// The whole document's text, pages separated by a blank line.
    ///
    /// **Pages needing OCR contribute nothing.** A page whose text layer is
    /// missing or unreadable must not quietly return plausible-looking garbage
    /// that a caller mistakes for a result — see `pages_needing_ocr` for what
    /// was left out, or read `page.text` to see it anyway.
    #[pyo3(get)]
    text: String,
    /// Mean per-page confidence, 0.0 to 1.0.
    ///
    /// Pages needing OCR count as 0, so a document that is half scans scores
    /// about 0.5 rather than a misleading 1.0.
    #[pyo3(get)]
    confidence: f64,
    /// The 1-based numbers of the pages that `text` omitted. Hand these to an
    /// OCR engine.
    #[pyo3(get)]
    pages_needing_ocr: Vec<u32>,
    pages: Vec<Py<Page>>,
}

#[pymethods]
impl Document {
    /// Open a PDF and extract every page.
    ///
    /// Raises `FileNotFoundError` if the path does not exist, `PermissionError`
    /// if it cannot be read, and `qalam.QalamError` if the PDF is malformed.
    #[new]
    #[pyo3(signature = (path))]
    fn new(py: Python<'_>, path: std::path::PathBuf) -> PyResult<Self> {
        // Parsing a large PDF is pure Rust work that touches no Python objects,
        // so release the GIL for it: other Python threads keep running while we
        // extract. This is the main reason a native extension is worth writing.
        let doc = py
            .detach(|| qalam_core::Document::open(&path))
            .map_err(to_py_err)?;

        let pages = doc
            .pages()
            .iter()
            .map(|page| Py::new(py, to_py_page(py, page)?))
            .collect::<PyResult<Vec<_>>>()?;

        Ok(Self {
            text: doc.text(),
            confidence: doc.confidence(),
            pages_needing_ocr: doc.pages_needing_ocr(),
            pages,
        })
    }

    /// Every page, in document order.
    #[getter]
    fn pages(&self, py: Python<'_>) -> Vec<Py<Page>> {
        self.pages.iter().map(|p| p.clone_ref(py)).collect()
    }

    /// Look up a page by its 1-based number.
    ///
    /// Raises `IndexError` if there is no such page.
    fn page(&self, py: Python<'_>, number: u32) -> PyResult<Py<Page>> {
        self.pages
            .iter()
            .find(|p| p.get().number == number)
            .map(|p| p.clone_ref(py))
            .ok_or_else(|| PyIndexError::new_err(format!("no page {number}")))
    }

    /// `len(doc)` is the page count.
    fn __len__(&self) -> usize {
        self.pages.len()
    }

    /// `for page in doc:` walks the pages, so a `Document` reads like a list.
    fn __iter__(slf: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let pages: Vec<Py<Page>> = slf.pages.iter().map(|p| p.clone_ref(py)).collect();
        Ok(pages.into_pyobject(py)?.try_iter()?.unbind().into_any())
    }

    fn __repr__(&self) -> String {
        format!(
            "<Document {} page(s), confidence={:.2}, {} needing OCR>",
            self.pages.len(),
            self.confidence,
            self.pages_needing_ocr.len(),
        )
    }
}

/// Convert a core page into its Python-facing form.
fn to_py_page(py: Python<'_>, page: &qalam_core::Page) -> PyResult<Page> {
    let lines = page
        .lines
        .iter()
        .map(|line| {
            Py::new(
                py,
                Line {
                    text: line.text.clone(),
                    baseline: line.baseline,
                    direction: match line.direction {
                        qalam_core::Direction::Rtl => "rtl".to_string(),
                        qalam_core::Direction::Ltr => "ltr".to_string(),
                    },
                    font: line.style.font.clone(),
                    size: line.style.size,
                    color: line.style.color.to_css_hex(),
                    confidence: line.resolution_rate(),
                },
            )
        })
        .collect::<PyResult<Vec<_>>>()?;

    Ok(Page {
        number: page.number,
        width: page.width,
        height: page.height,
        rotation: page.rotation.degrees(),
        text: page.text(),
        verdict: page.report.verdict.as_str().to_string(),
        needs_ocr: page.needs_ocr(),
        confidence: page.confidence(),
        reasons: page.report.reasons.clone(),
        lines,
    })
}

/// Extract a document's text in one call.
///
/// Equivalent to `Document(path).text`, and the simplest way in.
#[pyfunction]
#[pyo3(signature = (path))]
fn extract_text(py: Python<'_>, path: std::path::PathBuf) -> PyResult<String> {
    py.detach(|| qalam_core::extract_text(&path))
        .map_err(to_py_err)
}

/// The `qalam` module.
///
/// The function's name is the module name Python imports, and must match the
/// `[lib] name` in `Cargo.toml`.
#[pymodule]
fn qalam(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__doc__", "Arabic PDF text extraction without OCR.")?;
    m.add("QalamError", m.py().get_type::<QalamError>())?;
    m.add_class::<Document>()?;
    m.add_class::<Page>()?;
    m.add_class::<Line>()?;
    m.add_function(wrap_pyfunction!(extract_text, m)?)?;
    Ok(())
}
