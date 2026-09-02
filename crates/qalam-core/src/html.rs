//! **Tier D — rendering the block model as HTML.**
//!
//! The payoff for capturing styling since M2 (PLAN.md §1). Everything needed is
//! already in the model: reading order, block types, table structure, per-line
//! colour, size and direction. This module only decides how to write it down.
//!
//! Purely additive — it consumes [`Document`] and no layer below it changes.
//!
//! # Why HTML and not Markdown
//!
//! Markdown cannot express either of the two things this document most needs.
//! It has no colour (GitHub strips `style` attributes from inline HTML), and no
//! way to state text direction — so an Arabic table, whose first column is the
//! **rightmost**, renders with its columns backwards and nothing can be done
//! about it. `<table dir="rtl">` fixes that for free.
//!
//! HTML → Markdown converters are plentiful for anyone who wants the other
//! format; Markdown → correct RTL is not recoverable.
//!
//! # Headings are inferred, not read
//!
//! A PDF does not say "this is a heading". It says some text is larger. The
//! inference is in [`Headings`], and it is a *guess* — one this module makes
//! visible rather than hiding, so a caller can see what was assumed.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::bidi::Direction;
use crate::blocks::Block;
use crate::document::{Document, Page};
use crate::images::ExtractedImage;
use crate::types::Color;

/// How to render.
#[derive(Debug, Clone)]
pub struct HtmlOptions {
    /// Embed images as `data:` URIs.
    ///
    /// Makes the output a single self-contained file, at the cost of roughly
    /// a third more bytes than the images themselves — base64 is not free, and
    /// a picture-heavy document produces a large page.
    pub include_images: bool,
    /// Carry each block's colour through as an inline `style`.
    pub include_color: bool,
    /// The document's `<title>`.
    pub title: String,
}

impl Default for HtmlOptions {
    fn default() -> Self {
        Self {
            include_images: true,
            include_color: true,
            title: "Extracted document".to_string(),
        }
    }
}

/// The mapping from type size to heading level, inferred from a document.
///
/// # How the body size is found
///
/// By **character count**, not by how many lines or blocks use a size. A
/// heading is one short line; body text is thousands of characters. Counting
/// lines would let a document with many headings and few paragraphs decide that
/// its headings *are* the body.
///
/// Measured on the corpus, the shape is unmistakable — one size holds about
/// half of all characters and everything larger holds fractions of a percent:
///
/// ```text
///   36.0pt   0.0% of characters   x3.00 body
///   24.0pt   0.1%                 x2.00
///   18.0pt   0.3%                 x1.50
///   12.0pt  48.2%                 body
/// ```
#[derive(Debug, Clone)]
pub struct Headings {
    body_size: f64,
    /// Size → heading level, for sizes large enough to be headings.
    levels: HashMap<u64, u8>,
}

/// A size must exceed the body size by this much to be a heading.
///
/// Below it, the difference is more likely to be a different font or a
/// rounding artefact than an authorial decision.
const MIN_HEADING_RATIO: f64 = 1.15;

/// HTML has six heading levels; deeper distinctions have nowhere to go.
const MAX_LEVEL: u8 = 6;

/// Sizes are bucketed to this many points before counting, so that 11.98pt and
/// 12.0pt are recognised as the same size rather than two.
const SIZE_BUCKET: f64 = 0.5;

/// Round a size to its bucket, as an integer key a `HashMap` can hold.
///
/// `f64` is not `Hash` — it has no total equality, since `NaN != NaN` — so the
/// bucket index is the key rather than the size itself.
fn bucket(size: f64) -> u64 {
    (size / SIZE_BUCKET).round().max(0.0) as u64
}

impl Headings {
    /// Infer the heading levels used by a document.
    pub fn analyse(doc: &Document) -> Self {
        // Weight each size by how many characters are set in it.
        let mut weight: HashMap<u64, usize> = HashMap::new();
        for page in doc.pages() {
            for line in &page.lines {
                *weight.entry(bucket(line.style.size)).or_default() += line.text.chars().count();
            }
        }

        let Some((&body_bucket, _)) = weight.iter().max_by_key(|(_, chars)| **chars) else {
            return Self {
                body_size: 0.0,
                levels: HashMap::new(),
            };
        };
        let body_size = body_bucket as f64 * SIZE_BUCKET;

        // Everything meaningfully larger than the body is a heading. Sizes
        // *smaller* are captions, footnotes and table cells — a naive ranking
        // over all distinct sizes would turn those into deep headings.
        let mut larger: Vec<u64> = weight
            .keys()
            .copied()
            .filter(|b| (*b as f64 * SIZE_BUCKET) >= body_size * MIN_HEADING_RATIO)
            .collect();
        larger.sort_unstable_by(|a, b| b.cmp(a));

        let levels = larger
            .into_iter()
            .enumerate()
            .map(|(i, b)| (b, (i as u8 + 1).min(MAX_LEVEL)))
            .collect();

        Self { body_size, levels }
    }

    /// The size most of the document's text is set in.
    pub fn body_size(&self) -> f64 {
        self.body_size
    }

    /// The heading level for a size, or `None` if it is body text or smaller.
    pub fn level_for(&self, size: f64) -> Option<u8> {
        self.levels.get(&bucket(size)).copied()
    }

    /// Every inferred heading size and its level, largest first.
    ///
    /// Exposed so a caller can see what was assumed. The inference is a guess,
    /// and a document that signals headings by weight or colour rather than
    /// size will come back with none — flat, not wrong.
    pub fn inferred(&self) -> Vec<(f64, u8)> {
        let mut out: Vec<(f64, u8)> = self
            .levels
            .iter()
            .map(|(b, level)| (*b as f64 * SIZE_BUCKET, *level))
            .collect();
        out.sort_by(|a, b| b.0.total_cmp(&a.0));
        out
    }
}

/// Render a document as a complete, standalone HTML page.
pub fn to_html(doc: &Document, options: &HtmlOptions) -> String {
    let headings = Headings::analyse(doc);
    let rtl = document_direction(doc) == Direction::Rtl;
    let dir = if rtl { "rtl" } else { "ltr" };

    let mut out = String::new();
    let _ = write!(
        out,
        "<!DOCTYPE html>\n<html lang=\"{}\" dir=\"{dir}\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n",
        if rtl { "ar" } else { "en" },
        escape(&options.title),
    );

    for page in doc.pages() {
        render_page(&mut out, page, &headings, options, rtl);
    }

    out.push_str("</body>\n</html>\n");
    out
}

/// The direction most of the document reads in.
///
/// Weighted by **characters**, not by lines. A table of figures contributes a
/// great many short left-to-right lines — one per cell — and counting lines
/// lets those outvote the prose around them. The corpus's Arabic statistical
/// reports come out left-to-right on a line count, which would render every
/// table mirrored.
fn document_direction(doc: &Document) -> Direction {
    let mut rtl = 0usize;
    let mut total = 0usize;
    for line in doc.pages().iter().flat_map(|p| &p.lines) {
        let chars = line.text.chars().count();
        total += chars;
        if line.direction == Direction::Rtl {
            rtl += chars;
        }
    }

    if rtl * 2 > total {
        Direction::Rtl
    } else {
        Direction::Ltr
    }
}

/// Render one page's blocks, in reading order.
fn render_page(
    out: &mut String,
    page: &Page,
    headings: &Headings,
    options: &HtmlOptions,
    rtl: bool,
) {
    let _ = write!(
        out,
        "<section class=\"page\" id=\"page-{}\" data-confidence=\"{:.2}\">\n\
         <div class=\"page-number\">{}</div>\n",
        page.number,
        page.confidence(),
        page.number
    );

    // A page with no usable text layer says so. Emitting nothing would be
    // indistinguishable from a blank page, which is the deception the whole
    // project is built to avoid.
    if page.needs_ocr() {
        let _ = writeln!(
            out,
            "<p class=\"needs-ocr\">No text layer on this page — it needs OCR.{}</p>",
            page.report
                .reasons
                .first()
                .map(|r| format!(" ({})", escape(r)))
                .unwrap_or_default()
        );
    }

    for block in &page.blocks {
        match block {
            Block::Text(text) => render_text(out, text, headings, options),
            Block::Table(table) => render_table(out, &table.table, rtl),
            Block::Image(image) => render_image(out, image, options),
        }
    }

    out.push_str("</section>\n");
}

/// Render a text block as a heading or a paragraph.
fn render_text(
    out: &mut String,
    block: &crate::blocks::TextBlock,
    headings: &Headings,
    options: &HtmlOptions,
) {
    let Some(first) = block.lines.first() else {
        return;
    };

    // Lines within a block are joined with a space, not `<br>`: the block
    // boundaries came from the layout pass, so a block is already about a
    // paragraph, and the line breaks inside it are the column's width talking
    // rather than the author's intent.
    let text: String = block
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");

    let tag = match headings.level_for(first.style.size) {
        Some(level) => format!("h{level}"),
        None => "p".to_string(),
    };

    let mut attrs = String::new();
    if options.include_color && is_worth_showing(first.style.color) {
        let _ = write!(attrs, " style=\"color:{}\"", first.style.color.to_css_hex());
    }
    // Only state the direction when it differs from the document's, so the
    // markup stays quiet in the common case.
    if first.direction == Direction::Ltr {
        attrs.push_str(" dir=\"ltr\"");
    }
    // Reconstructed blocks are best-effort; say so where it is not certain.
    if block.confidence < 1.0 {
        let _ = write!(attrs, " data-confidence=\"{:.2}\"", block.confidence);
    }

    let _ = writeln!(out, "<{tag}{attrs}>{}</{tag}>", escape(&text));
}

/// Render a table.
///
/// `dir` is set on the element because our columns are in **reading order** —
/// on an Arabic page the first is the rightmost. Without it a browser lays them
/// out left to right and the table comes out mirrored. This is the thing
/// Markdown cannot express.
fn render_table(out: &mut String, table: &crate::tables::Table, rtl: bool) {
    let _ = writeln!(
        out,
        "<table dir=\"{}\" data-confidence=\"{:.2}\">",
        if rtl { "rtl" } else { "ltr" },
        table.confidence
    );

    // How many leading rows form the header.
    //
    // A heuristic, and a modest one: a spanning header — `٢٠١٦` sitting over
    // three columns — leaves the cells beside it empty, because we have no
    // `colspan` to reconstruct from ruled lines. So the header runs through the
    // leading rows that have gaps in them, plus the first fully populated row
    // beneath. On the corpus that captures both the year band and the column
    // names; on a table with a single header row it captures just that row.
    let header_rows = table
        .rows
        .iter()
        .position(|row| !row.is_empty() && row.iter().all(|c| !c.text.is_empty()))
        .map_or(0, |i| i + 1);

    for (index, row) in table.rows.iter().enumerate() {
        if index == 0 && header_rows > 0 {
            out.push_str("<thead>\n");
        }
        if index == header_rows {
            if header_rows > 0 {
                out.push_str("</thead>\n");
            }
            out.push_str("<tbody>\n");
        }

        let cell = if index < header_rows { "th" } else { "td" };
        out.push_str("<tr>");
        for c in row {
            let _ = write!(out, "<{cell}>{}</{cell}>", escape(&c.text));
        }
        out.push_str("</tr>\n");
    }
    if header_rows >= table.rows.len() && header_rows > 0 {
        out.push_str("</thead>\n");
    } else if !table.rows.is_empty() {
        out.push_str("</tbody>\n");
    }
    out.push_str("</table>\n");
}

/// Render an image, inlined as a `data:` URI when asked for.
fn render_image(out: &mut String, block: &crate::blocks::ImageBlock, options: &HtmlOptions) {
    match &block.image {
        ExtractedImage::Ready(image) if options.include_images => {
            let mime = match image.format {
                crate::images::ImageFormat::Jpeg => "image/jpeg",
                crate::images::ImageFormat::Jpeg2000 => "image/jp2",
                crate::images::ImageFormat::Png => "image/png",
            };
            let class = if block.is_background {
                " class=\"background\""
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "<img{class} alt=\"{}\" width=\"{}\" height=\"{}\" src=\"data:{mime};base64,{}\">",
                escape(&image.file_name()),
                image.width,
                image.height,
                base64(&image.data),
            );
        }
        // An image we could not decode is announced, not dropped — the same
        // rule the text pipeline follows for an unmappable glyph.
        ExtractedImage::Unsupported { reason, .. } => {
            let _ = writeln!(
                out,
                "<p class=\"undecoded\">[image: {}]</p>",
                escape(reason)
            );
        }
        ExtractedImage::Ready(image) => {
            let _ = writeln!(
                out,
                "<p class=\"undecoded\">[image: {}]</p>",
                escape(&image.file_name())
            );
        }
    }
}

/// Should this colour be carried through to the output?
///
/// Black is the default and adds nothing. **Near-white is worse than nothing**:
/// in the PDF that text sits on a coloured banner or a photograph, and we do
/// not reproduce page backgrounds — so honouring the colour would paint white
/// text on a white page and make it vanish. The corpus has 46 such blocks.
///
/// Losing the colour is a cosmetic loss; losing the text is not.
fn is_worth_showing(color: Color) -> bool {
    /// Above this relative luminance, text would disappear against the page.
    const TOO_PALE: f64 = 0.85;

    if color.is_black() {
        return false;
    }
    let [r, g, b] = color.to_rgb8();
    // The usual perceptual weights: the eye is far more sensitive to green
    // than to blue, so a plain average would misjudge which colours are pale.
    let luminance = (0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)) / 255.0;
    luminance <= TOO_PALE
}

/// Escape the five characters that would otherwise be markup.
///
/// Not optional and not a nicety: extracted text is arbitrary content from a
/// file we did not write, and a `<` in it would otherwise start a tag.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Base64-encode bytes for a `data:` URI.
///
/// Written out rather than pulled in as a dependency: it is fifteen lines, and
/// the alternative is a crate in the tree for one call site.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        // Pack up to three bytes into 24 bits, then read four 6-bit groups.
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);

        for i in 0..4 {
            // A short final chunk pads with '=' rather than encoding the zeros
            // we invented.
            if i > chunk.len() {
                out.push('=');
            } else {
                out.push(ALPHABET[(n >> (18 - i * 6) & 0x3f) as usize] as char);
            }
        }
    }
    out
}

/// The stylesheet, kept deliberately plain: readable defaults, not a design.
const STYLE: &str = "
:root { color-scheme: light dark; }
body {
  margin: 0 auto; padding: 2rem 1rem; max-width: 46rem;
  font-family: 'Noto Naskh Arabic', 'Amiri', 'Segoe UI', system-ui, sans-serif;
  font-size: 1.05rem; line-height: 1.9;
}
.page { position: relative; padding-top: 2.5rem; }
.page + .page { border-top: 1px solid rgba(128,128,128,.35); margin-top: 3rem; }
.page-number {
  position: absolute; top: .75rem; inset-inline-end: 0;
  font-size: .75rem; opacity: .5; font-family: system-ui, sans-serif;
}
h1, h2, h3, h4, h5, h6 { line-height: 1.4; margin: 1.6em 0 .6em; }
p { margin: 0 0 1em; }
table { border-collapse: collapse; margin: 1.5em 0; width: 100%; font-size: .95rem; }
th, td { border: 1px solid rgba(128,128,128,.45); padding: .4em .6em; text-align: start; }
th { font-weight: 600; background: rgba(128,128,128,.12); }
img { max-width: 100%; height: auto; margin: 1em 0; }
img.background { opacity: .55; }
.needs-ocr, .undecoded {
  font-family: system-ui, sans-serif; font-size: .85rem;
  opacity: .7; font-style: italic;
}
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_specification() {
        // The examples from RFC 4648, which exercise every padding case.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_bytes_that_are_not_text() {
        // Image data is arbitrary bytes, including the high bits that a
        // sign-extending shift would corrupt.
        assert_eq!(base64(&[0xFF, 0xFF, 0xFF]), "////");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64(&[0xFF, 0xD8, 0xFF]), "/9j/");
    }

    #[test]
    fn markup_characters_in_the_text_are_escaped() {
        // Extracted text is arbitrary content from a file we did not write.
        // A `<` in it would otherwise open a tag.
        assert_eq!(escape("a < b & c"), "a &lt; b &amp; c");
        assert_eq!(escape("<script>"), "&lt;script&gt;");
        assert_eq!(escape("say \"hi\""), "say &quot;hi&quot;");
        // Arabic passes through untouched.
        assert_eq!(escape("الدليل"), "الدليل");
    }

    #[test]
    fn pale_text_keeps_its_colour_off() {
        // White text sits on a coloured banner in the PDF; we do not reproduce
        // page backgrounds, so honouring the colour would make it vanish.
        assert!(!is_worth_showing(Color::Rgb(1.0, 1.0, 1.0)));
        assert!(!is_worth_showing(Color::Gray(0.95)));
        // Black adds nothing over the default.
        assert!(!is_worth_showing(Color::BLACK));
        // A real heading colour is kept.
        assert!(is_worth_showing(Color::Rgb(0.04, 0.25, 0.55)));
        assert!(is_worth_showing(Color::Rgb(1.0, 0.0, 0.0)));
    }

    #[test]
    fn luminance_is_weighted_perceptually() {
        // Two colours with identical channel *values*, differing only in which
        // channel is saturated. Green is far brighter to the eye than blue, so
        // the light green vanishes against the page and the light blue does
        // not. A plain average would call them the same and get one wrong.
        assert!(!is_worth_showing(Color::Rgb(0.5, 1.0, 0.5)));
        assert!(is_worth_showing(Color::Rgb(0.5, 0.5, 1.0)));
    }

    #[test]
    fn heading_levels_are_relative_to_the_body_size() {
        // Built by hand rather than from a PDF: the mapping is arithmetic on
        // sizes, and tying the test to a fixture would obscure that.
        let headings = Headings {
            body_size: 12.0,
            levels: [(bucket(24.0), 1), (bucket(18.0), 2), (bucket(14.0), 3)]
                .into_iter()
                .collect(),
        };

        assert_eq!(headings.level_for(24.0), Some(1));
        assert_eq!(headings.level_for(18.0), Some(2));
        // Body text is not a heading.
        assert_eq!(headings.level_for(12.0), None);
        // Nor is anything smaller — captions and footnotes must not become
        // deep headings, which a naive ranking over all sizes would do.
        assert_eq!(headings.level_for(9.0), None);
    }

    #[test]
    fn sizes_are_bucketed_so_rounding_does_not_split_them() {
        // Effective size comes out of a matrix multiply, so nominally equal
        // text can differ in the last decimal.
        assert_eq!(bucket(12.0), bucket(11.9));
        assert_eq!(bucket(12.0), bucket(12.2));
        assert_ne!(bucket(12.0), bucket(14.0));
    }

    #[test]
    fn inferred_levels_are_reported_largest_first() {
        let headings = Headings {
            body_size: 10.0,
            levels: [(bucket(20.0), 2), (bucket(30.0), 1)].into_iter().collect(),
        };
        let inferred = headings.inferred();
        assert_eq!(inferred[0].1, 1, "the largest size is level 1");
        assert!(inferred[0].0 > inferred[1].0);
    }
}
