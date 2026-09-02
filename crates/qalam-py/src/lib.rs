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

/// One cell of a reconstructed table.
#[pyclass(frozen, module = "qalam")]
pub struct Cell {
    /// The cell's text, its lines joined with spaces.
    #[pyo3(get)]
    text: String,
    /// Row index, counting from the top.
    #[pyo3(get)]
    row: usize,
    /// Column index **in reading order** — 0 is the rightmost cell on an
    /// Arabic page, the leftmost on a Latin one.
    #[pyo3(get)]
    column: usize,
    /// `(x0, y0, x1, y1)` in PDF points from the bottom-left of the page.
    #[pyo3(get)]
    bbox: (f64, f64, f64, f64),
}

#[pymethods]
impl Cell {
    fn __repr__(&self) -> String {
        format!("<Cell r{} c{} {:?}>", self.row, self.column, self.text)
    }

    fn __str__(&self) -> &str {
        &self.text
    }
}

/// A run of text: a paragraph, a heading, one card's contents.
#[pyclass(frozen, module = "qalam")]
pub struct TextBlock {
    /// Always `"text"`. Lets a caller sort a mixed list of blocks without
    /// reaching for `isinstance`.
    #[pyo3(get)]
    kind: &'static str,
    /// Position in the page's reading order, counting from 0.
    #[pyo3(get)]
    reading_index: usize,
    /// `(x0, y0, x1, y1)` in PDF points.
    #[pyo3(get)]
    bbox: (f64, f64, f64, f64),
    /// The block's text, lines joined with newlines.
    #[pyo3(get)]
    text: String,
    /// Fraction of the block's glyphs that resolved, 0.0 to 1.0.
    #[pyo3(get)]
    confidence: f64,
    lines: Vec<Py<Line>>,
}

#[pymethods]
impl TextBlock {
    /// The block's lines, in reading order.
    #[getter]
    fn lines(&self, py: Python<'_>) -> Vec<Py<Line>> {
        self.lines.iter().map(|l| l.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        let preview: String = self.text.chars().take(30).collect();
        format!("<TextBlock {} {:?}…>", self.reading_index, preview)
    }

    fn __str__(&self) -> &str {
        &self.text
    }
}

/// A picture drawn on the page.
#[pyclass(frozen, module = "qalam")]
pub struct ImageBlock {
    /// Always `"image"`.
    #[pyo3(get)]
    kind: &'static str,
    /// Position in the page's reading order.
    #[pyo3(get)]
    reading_index: usize,
    /// `(x0, y0, x1, y1)` in PDF points, or `None` when we never saw the image
    /// drawn — almost certainly because it lives inside a form XObject we do
    /// not enter. Not proof it is absent from the page.
    #[pyo3(get)]
    bbox: Option<(f64, f64, f64, f64)>,
    /// True when the image covers most of the page: a background, not a figure.
    #[pyo3(get)]
    is_background: bool,
    /// Pixel width, or `None` if the image could not be decoded.
    #[pyo3(get)]
    width: Option<u32>,
    /// Pixel height.
    #[pyo3(get)]
    height: Option<u32>,
    /// `"jpg"`, `"jp2"` or `"png"`, or `None` if undecoded.
    #[pyo3(get)]
    format: Option<String>,
    /// A conventional file name such as `"Im0.jpg"`.
    #[pyo3(get)]
    file_name: Option<String>,
    /// Why the image could not be decoded, or `None` when it was.
    ///
    /// Always one or the other: an undecodable image is reported with its
    /// reason rather than dropped.
    #[pyo3(get)]
    unsupported_reason: Option<String>,
    /// True when the image declared an `/SMask` we did not composite.
    ///
    /// Stronger than it sounds: a logo is routinely stored as a *blank* image
    /// whose whole shape lives in the mask, so the extraction can be
    /// byte-correct and visually empty.
    #[pyo3(get)]
    dropped_transparency: bool,
    data: Option<Vec<u8>>,
}

#[pymethods]
impl ImageBlock {
    /// The encoded image file's bytes, ready to write to disk.
    #[getter]
    fn data(&self, py: Python<'_>) -> Option<Py<pyo3::types::PyBytes>> {
        use pyo3::types::PyBytes;
        self.data
            .as_ref()
            .map(|bytes| PyBytes::new(py, bytes).unbind())
    }

    /// Always empty: a picture contributes no text.
    ///
    /// Present so that every block kind answers `text`, and a caller walking
    /// `page.blocks` never has to special-case one.
    #[getter]
    fn text(&self) -> &'static str {
        ""
    }

    /// Write the image to a file.
    ///
    /// Raises `ValueError` for an image that could not be decoded — the bytes
    /// simply do not exist, and writing an empty file would hide that.
    fn save(&self, path: std::path::PathBuf) -> PyResult<()> {
        let Some(bytes) = &self.data else {
            return Err(pyo3::exceptions::PyValueError::new_err(
                self.unsupported_reason
                    .clone()
                    .unwrap_or_else(|| "image was not decoded".to_string()),
            ));
        };
        std::fs::write(&path, bytes)
            .map_err(|e| pyo3::exceptions::PyOSError::new_err(e.to_string()))
    }

    fn __repr__(&self) -> String {
        match (&self.file_name, &self.unsupported_reason) {
            (Some(name), _) => format!(
                "<ImageBlock {} {} {}x{}>",
                self.reading_index,
                name,
                self.width.unwrap_or(0),
                self.height.unwrap_or(0)
            ),
            (None, Some(reason)) => {
                format!("<ImageBlock {} undecoded: {reason}>", self.reading_index)
            }
            _ => format!("<ImageBlock {}>", self.reading_index),
        }
    }
}

/// A table reconstructed from the lines ruled around it.
#[pyclass(frozen, module = "qalam")]
pub struct TableBlock {
    /// Always `"table"`.
    #[pyo3(get)]
    kind: &'static str,
    /// Position in the page's reading order.
    #[pyo3(get)]
    reading_index: usize,
    /// `(x0, y0, x1, y1)` in PDF points.
    #[pyo3(get)]
    bbox: (f64, f64, f64, f64),
    /// How much of the grid was actually drawn, 0.0 to 1.0.
    ///
    /// Reconstruction is best-effort; this says how much to trust it.
    #[pyo3(get)]
    confidence: f64,
    /// Number of rows.
    #[pyo3(get)]
    row_count: usize,
    /// Number of columns.
    #[pyo3(get)]
    column_count: usize,
    rows: Vec<Vec<Py<Cell>>>,
}

#[pymethods]
impl TableBlock {
    /// The cells, by row and then by column in reading order.
    #[getter]
    fn rows(&self, py: Python<'_>) -> Vec<Vec<Py<Cell>>> {
        self.rows
            .iter()
            .map(|row| row.iter().map(|c| c.clone_ref(py)).collect())
            .collect()
    }

    /// The table flattened: cells joined by tabs, rows by newlines.
    ///
    /// Every block kind exposes `text`, so a caller can walk `page.blocks` and
    /// join them without asking what each one is — which is exactly what
    /// `page.text` does.
    #[getter]
    fn text(&self) -> String {
        self.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| c.get().text.as_str())
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The table as a list of rows of strings — the shape `csv.writer` wants.
    fn to_rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| row.iter().map(|c| c.get().text.clone()).collect())
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "<TableBlock {} {}x{} confidence={:.2}>",
            self.reading_index, self.row_count, self.column_count, self.confidence
        )
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
    /// True when this page's reading order came from the document's own
    /// structure tree rather than from geometry.
    ///
    /// The lucky case: the writer stated the order and we followed it. `False`
    /// means we reconstructed it, which is best-effort.
    #[pyo3(get)]
    tagged: bool,
    lines: Vec<Py<Line>>,
    blocks: Vec<Py<PyAny>>,
    images: Vec<Py<ImageBlock>>,
    tables: Vec<Py<TableBlock>>,
}

#[pymethods]
impl Page {
    /// The page's lines, in reading order — columns already resolved.
    #[getter]
    fn lines(&self, py: Python<'_>) -> Vec<Py<Line>> {
        // `clone_ref` bumps Python's reference count; it does not copy a Line.
        self.lines.iter().map(|l| l.clone_ref(py)).collect()
    }

    /// The page's content as typed blocks, in reading order.
    ///
    /// A mixed list of `TextBlock`, `ImageBlock` and `TableBlock`. Each carries
    /// a `kind` of `"text"`, `"image"` or `"table"`, so a caller can branch on
    /// that rather than on `isinstance`.
    #[getter]
    fn blocks(&self, py: Python<'_>) -> Vec<Py<PyAny>> {
        self.blocks.iter().map(|b| b.clone_ref(py)).collect()
    }

    /// Just the images on this page.
    #[getter]
    fn images(&self, py: Python<'_>) -> Vec<Py<ImageBlock>> {
        self.images.iter().map(|b| b.clone_ref(py)).collect()
    }

    /// Just the tables on this page.
    #[getter]
    fn tables(&self, py: Python<'_>) -> Vec<Py<TableBlock>> {
        self.tables.iter().map(|b| b.clone_ref(py)).collect()
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
    /// Kept so the document can be re-rendered without extracting again.
    document: qalam_core::Document,
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
            document: doc,
        })
    }

    /// Render the document as a complete, standalone HTML page.
    ///
    /// Reading order, headings inferred from type size, colour, tables with
    /// their direction stated, and images inlined as `data:` URIs.
    ///
    /// `include_images=False` leaves the pictures out, which for a
    /// picture-heavy document is the difference between a few kilobytes and a
    /// few megabytes.
    #[pyo3(signature = (*, include_images = true, include_color = true, title = None))]
    fn to_html(
        &self,
        py: Python<'_>,
        include_images: bool,
        include_color: bool,
        title: Option<String>,
    ) -> String {
        let options = qalam_core::HtmlOptions {
            include_images,
            include_color,
            title: title.unwrap_or_else(|| "Extracted document".to_string()),
        };
        // Rendering is pure Rust over data we already hold, so the GIL can go.
        py.detach(|| qalam_core::to_html(&self.document, &options))
    }

    /// The type size the document's body text is set in, and the heading levels
    /// inferred from it.
    ///
    /// Returned as `(body_size, [(size, level), ...])`, largest first. A PDF
    /// never says "this is a heading" — it says some text is larger — so this
    /// is a guess, and one worth being able to inspect. A document that signals
    /// headings by weight or colour rather than size comes back with an empty
    /// list: flat, not wrong.
    fn heading_sizes(&self) -> (f64, Vec<(f64, u8)>) {
        let headings = qalam_core::Headings::analyse(&self.document);
        (headings.body_size(), headings.inferred())
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
        .map(|line| Py::new(py, to_py_line(line)))
        .collect::<PyResult<Vec<_>>>()?;

    // The blocks, in reading order. Each variant becomes its own Python type,
    // so a caller gets real attributes rather than a tagged union to unpack.
    let mut blocks: Vec<Py<PyAny>> = Vec::with_capacity(page.blocks.len());
    let mut images = Vec::new();
    let mut tables = Vec::new();

    for block in &page.blocks {
        match block {
            qalam_core::Block::Text(text) => {
                let lines = text
                    .lines
                    .iter()
                    .map(|line| Py::new(py, to_py_line(line)))
                    .collect::<PyResult<Vec<_>>>()?;
                let object = Py::new(
                    py,
                    TextBlock {
                        kind: "text",
                        reading_index: text.reading_index,
                        bbox: rect(text.bbox),
                        text: text.text(),
                        confidence: text.confidence,
                        lines,
                    },
                )?;
                blocks.push(object.into_any());
            }
            qalam_core::Block::Image(image) => {
                let object = Py::new(py, to_py_image(image))?;
                images.push(object.clone_ref(py));
                blocks.push(object.into_any());
            }
            qalam_core::Block::Table(table) => {
                let object = Py::new(py, to_py_table(py, table)?)?;
                tables.push(object.clone_ref(py));
                blocks.push(object.into_any());
            }
        }
    }

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
        tagged: page.tagged,
        lines,
        blocks,
        images,
        tables,
    })
}

/// A core rectangle as the `(x0, y0, x1, y1)` tuple Python sees.
///
/// A tuple rather than a class: it is four numbers with no behaviour, and
/// Python programmers already know how to unpack one.
fn rect(r: qalam_core::Rect) -> (f64, f64, f64, f64) {
    (r.x0, r.y0, r.x1, r.y1)
}

/// Convert one extracted line.
fn to_py_line(line: &qalam_core::TextLine) -> Line {
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
    }
}

/// Convert one image block, decoded or not.
fn to_py_image(block: &qalam_core::ImageBlock) -> ImageBlock {
    let common = ImageBlock {
        kind: "image",
        reading_index: block.reading_index,
        bbox: block.bbox.map(rect),
        is_background: block.is_background,
        width: None,
        height: None,
        format: None,
        file_name: None,
        unsupported_reason: None,
        dropped_transparency: false,
        data: None,
    };

    match &block.image {
        qalam_core::ExtractedImage::Ready(image) => ImageBlock {
            width: Some(image.width),
            height: Some(image.height),
            format: Some(image.format.extension().to_string()),
            file_name: Some(image.file_name()),
            dropped_transparency: image.dropped_transparency,
            data: Some(image.data.clone()),
            ..common
        },
        // An image we declined to decode is reported with its reason, never
        // silently dropped.
        qalam_core::ExtractedImage::Unsupported { reason, .. } => ImageBlock {
            unsupported_reason: Some(reason.clone()),
            ..common
        },
    }
}

/// Convert one table block.
fn to_py_table(py: Python<'_>, block: &qalam_core::TableBlock) -> PyResult<TableBlock> {
    let table = &block.table;
    let rows = table
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| {
                    Py::new(
                        py,
                        Cell {
                            text: cell.text.clone(),
                            row: cell.row,
                            column: cell.column,
                            bbox: rect(cell.bbox),
                        },
                    )
                })
                .collect::<PyResult<Vec<_>>>()
        })
        .collect::<PyResult<Vec<_>>>()?;

    Ok(TableBlock {
        kind: "table",
        reading_index: block.reading_index,
        bbox: rect(table.bbox),
        confidence: table.confidence,
        row_count: table.row_count(),
        column_count: table.column_count(),
        rows,
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
    m.add_class::<TextBlock>()?;
    m.add_class::<ImageBlock>()?;
    m.add_class::<TableBlock>()?;
    m.add_class::<Cell>()?;
    m.add_function(wrap_pyfunction!(extract_text, m)?)?;
    Ok(())
}
