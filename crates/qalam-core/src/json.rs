//! Structured JSON serialization for extracted documents.
//!
//! This module is deliberately a presentation layer, just like `html.rs`.
//! It consumes the already-extracted document model and does not participate
//! in PDF parsing, text reconstruction, reading-order detection, or scoring.
//!
//! The important property is that JSON is a serialization of information the
//! extraction pipeline already computed. Nothing is inferred again from text.

use serde::Serialize;

use crate::bidi::Direction;
use crate::blocks::{Block, ImageBlock, TableBlock, TextBlock};
use crate::document::Document;
use crate::images::ExtractedImage;
use crate::types::Rect;

#[derive(Debug, Serialize)]
struct JsonDocument<'a> {
    page_count: usize,
    pages: Vec<JsonPage<'a>>,
}

impl<'a> JsonDocument<'a> {
    fn new(doc: &'a Document) -> Self {
        Self {
            page_count: doc.page_count(),
            pages: doc.pages().iter().map(JsonPage::new).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonPage<'a> {
    number: u32,
    width: f64,
    height: f64,
    rotation: u16,
    verdict: &'static str,
    confidence: f64,
    reasons: &'a [String],
    tagged: bool,
    blocks: Vec<JsonBlock<'a>>,
}

fn rect(rect: Rect) -> [f64; 4] {
    [rect.x0, rect.y0, rect.x1, rect.y1]
}

fn direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Rtl => "rtl",
        Direction::Ltr => "ltr",
    }
}

#[derive(Debug, Serialize)]
struct JsonBlock<'a> {
    reading_index: usize,
    /// Absent only for an image with no known placement.
    #[serde(skip_serializing_if = "Option::is_none")]
    bbox: Option<[f64; 4]>,
    #[serde(flatten)]
    content: Content<'a>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Content<'a> {
    Text {
        text: String,
        confidence: f64,
        lines: Vec<JsonLine<'a>>,
    },

    Table {
        confidence: f64,
        row_count: usize,
        column_count: usize,
        rows: Vec<Vec<JsonCell<'a>>>,
    },

    Image(JsonImage<'a>),
}

impl<'a> JsonPage<'a> {
    fn new(page: &'a crate::document::Page) -> Self {
        Self {
            number: page.number,
            width: page.width,
            height: page.height,
            rotation: page.rotation.degrees(),
            verdict: page.report.verdict.as_str(),
            confidence: page.report.confidence,
            reasons: &page.report.reasons,
            tagged: page.tagged,
            blocks: page.blocks.iter().map(JsonBlock::new).collect(),
        }
    }
}

impl<'a> JsonBlock<'a> {
    fn new(block: &'a Block) -> Self {
        match block {
            Block::Text(block) => Self {
                reading_index: block.reading_index,
                bbox: Some(rect(block.bbox)),
                content: Content::text(block),
            },
            Block::Table(block) => Self {
                reading_index: block.reading_index,
                bbox: Some(rect(block.table.bbox)),
                content: Content::table(block),
            },
            Block::Image(block) => Self {
                reading_index: block.reading_index,
                bbox: block.bbox.map(rect),
                content: Content::Image(JsonImage::new(block)),
            },
        }
    }
}

impl<'a> Content<'a> {
    fn text(block: &'a TextBlock) -> Self {
        Self::Text {
            text: block.text(),
            confidence: block.confidence,
            lines: block.lines.iter().map(JsonLine::new).collect(),
        }
    }

    fn table(block: &'a TableBlock) -> Self {
        let table = &block.table;

        Self::Table {
            confidence: table.confidence,
            row_count: table.row_count(),
            column_count: table.column_count(),
            rows: table
                .rows
                .iter()
                .map(|row| row.iter().map(JsonCell::new).collect())
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonLine<'a> {
    text: &'a str,
    bbox: [f64; 4],
    baseline: f64,
    font: &'a str,
    size: f64,
    color: String,
    direction: &'static str,
    confidence: f64,
}

impl<'a> JsonLine<'a> {
    fn new(line: &'a crate::arabic::TextLine) -> Self {
        Self {
            text: &line.text,
            bbox: rect(line.bbox),
            baseline: line.baseline,
            font: &line.style.font,
            size: line.style.size,
            color: line.style.color.to_css_hex(),
            direction: direction(line.direction),
            confidence: line.resolution_rate(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonCell<'a> {
    text: &'a str,
    row: usize,
    column: usize,
    bbox: [f64; 4],
}

impl<'a> JsonCell<'a> {
    fn new(cell: &'a crate::tables::Cell) -> Self {
        Self {
            text: &cell.text,
            row: cell.row,
            column: cell.column,
            bbox: rect(cell.bbox),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonImage<'a> {
    is_background: bool,
    #[serde(flatten)]
    decoded: Option<DecodedImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unsupported_reason: Option<&'a str>,
}

impl<'a> JsonImage<'a> {
    fn new(block: &'a ImageBlock) -> Self {
        let (decoded, unsupported_reason) = match &block.image {
            ExtractedImage::Ready(image) => (
                Some(DecodedImage {
                    width: image.width,
                    height: image.height,
                    format: image.format.extension(),
                    file_name: image.file_name(),
                    dropped_transparency: image.dropped_transparency,
                }),
                None,
            ),
            ExtractedImage::Unsupported { reason, .. } => (None, Some(reason.as_str())),
        };

        Self {
            is_background: block.is_background,
            decoded,
            unsupported_reason,
        }
    }
}

#[derive(Debug, Serialize)]
struct DecodedImage {
    width: u32,
    height: u32,
    format: &'static str,
    file_name: String,
    dropped_transparency: bool,
}

/// Serialize an extracted document as pretty-printed JSON.
///
/// This is a presentation layer over the existing extraction model. It does
/// not rerun extraction, change reading order, modify text, or recalculate
/// recoverability.
pub fn to_json(doc: &Document) -> String {
    let document = JsonDocument::new(doc);

    serde_json::to_string_pretty(&document)
        .expect("JSON serialization of in-memory extraction data should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_serializes_as_expected() {
        assert_eq!(direction(Direction::Rtl), "rtl");
        assert_eq!(direction(Direction::Ltr), "ltr");
    }

    #[test]
    fn rect_serializes_in_document_order() {
        let value = rect(Rect {
            x0: 1.0,
            y0: 2.0,
            x1: 3.0,
            y1: 4.0,
        });

        assert_eq!(value, [1.0, 2.0, 3.0, 4.0]);
    }
}
