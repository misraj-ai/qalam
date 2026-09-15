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
use crate::blocks::Block;
use crate::document::Document;
use crate::images::ExtractedImage;
use crate::types::Rect;

#[derive(Debug, Serialize)]
struct JsonDocument<'a> {
    source: &'a str,
    page_count: usize,
    pages: Vec<JsonPage>,
}

#[derive(Debug, Serialize)]
struct JsonPage {
    number: u32,
    width: f64,
    height: f64,
    rotation: u16,
    verdict: &'static str,
    score: f64,
    reasons: Vec<String>,
    tagged: bool,
    blocks: Vec<JsonBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum JsonBlock {
    #[serde(rename = "paragraph")]
    Paragraph {
        reading_index: usize,
        text: String,
        bbox: [f64; 4],
        font_size: Option<f64>,
        direction: Option<&'static str>,
        confidence: f64,
        lines: Vec<JsonLine>,
    },

    #[serde(rename = "table")]
    Table {
        reading_index: usize,
        bbox: [f64; 4],
        confidence: f64,
        row_count: usize,
        column_count: usize,
        rows: Vec<Vec<JsonCell>>,
    },

    #[serde(rename = "image")]
    Image {
        reading_index: usize,
        bbox: Option<[f64; 4]>,
        is_background: bool,
        width: Option<u32>,
        height: Option<u32>,
        format: Option<String>,
        file_name: Option<String>,
        unsupported_reason: Option<String>,
        dropped_transparency: bool,
    },
}

#[derive(Debug, Serialize)]
struct JsonLine {
    text: String,
    bbox: [f64; 4],
    baseline: f64,
    font: String,
    font_size: f64,
    color: String,
    direction: &'static str,
    confidence: f64,
}

#[derive(Debug, Serialize)]
struct JsonCell {
    text: String,
    row: usize,
    column: usize,
    bbox: [f64; 4],
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

fn paragraph(block: &crate::blocks::TextBlock) -> JsonBlock {
    let first = block.lines.first();

    let lines = block
        .lines
        .iter()
        .map(|line| JsonLine {
            text: line.text.clone(),
            bbox: rect(line.bbox),
            baseline: line.baseline,
            font: line.style.font.clone(),
            font_size: line.style.size,
            color: line.style.color.to_css_hex(),
            direction: direction(line.direction),
            confidence: line.resolution_rate(),
        })
        .collect();

    JsonBlock::Paragraph {
        reading_index: block.reading_index,
        text: block.text(),
        bbox: rect(block.bbox),
        font_size: first.map(|line| line.style.size),
        direction: first.map(|line| direction(line.direction)),
        confidence: block.confidence,
        lines,
    }
}

fn table(block: &crate::blocks::TableBlock) -> JsonBlock {
    let table = &block.table;

    let rows = table
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| JsonCell {
                    text: cell.text.clone(),
                    row: cell.row,
                    column: cell.column,
                    bbox: rect(cell.bbox),
                })
                .collect()
        })
        .collect();

    JsonBlock::Table {
        reading_index: block.reading_index,
        bbox: rect(table.bbox),
        confidence: table.confidence,
        row_count: table.row_count(),
        column_count: table.column_count(),
        rows,
    }
}

fn image(block: &crate::blocks::ImageBlock) -> JsonBlock {
    let common = |width, height, format, file_name, unsupported_reason, dropped_transparency| {
        JsonBlock::Image {
            reading_index: block.reading_index,
            bbox: block.bbox.map(rect),
            is_background: block.is_background,
            width,
            height,
            format,
            file_name,
            unsupported_reason,
            dropped_transparency,
        }
    };

    match &block.image {
        ExtractedImage::Ready(image) => common(
            Some(image.width),
            Some(image.height),
            Some(image.format.extension().to_string()),
            Some(image.file_name()),
            None,
            image.dropped_transparency,
        ),

        ExtractedImage::Unsupported { reason, .. } => {
            common(None, None, None, None, Some(reason.clone()), false)
        }
    }
}

fn block(block: &Block) -> JsonBlock {
    match block {
        Block::Text(block) => paragraph(block),
        Block::Table(block) => table(block),
        Block::Image(block) => image(block),
    }
}

/// Serialize an extracted document as pretty-printed JSON.
///
/// This is a presentation layer over the existing extraction model. It does
/// not rerun extraction, change reading order, modify text, or recalculate
/// recoverability.
///
/// The `Result` is intentional: JSON cannot represent non-finite floating
/// point values. Returning the serialization error is preferable to silently
/// inventing a value for malformed input.
pub fn to_json(doc: &Document) -> Result<String, serde_json::Error> {
    let pages = doc
        .pages()
        .iter()
        .map(|page| JsonPage {
            number: page.number,
            width: page.width,
            height: page.height,
            rotation: page.rotation.degrees(),
            verdict: page.report.verdict.as_str(),
            score: page.report.confidence,
            reasons: page.report.reasons.clone(),
            tagged: page.tagged,
            blocks: page.blocks.iter().map(block).collect(),
        })
        .collect();

    let document = JsonDocument {
        source: doc.source(),
        page_count: doc.page_count(),
        pages,
    };

    serde_json::to_string_pretty(&document)
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
