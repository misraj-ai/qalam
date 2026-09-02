//! **L7 — image extraction.**
//!
//! The lowest-risk structure feature (PLAN.md §6, M6), because images are
//! already first-class objects in a PDF. There is nothing to *reconstruct*: an
//! image XObject knows its own width, height and colour space. The work is
//! deciding what to do with the bytes.
//!
//! # Two strategies, and the important one is doing nothing
//!
//! - **Passthrough.** A `/DCTDecode` image *is* a JPEG file. Its bytes can be
//!   written straight to disk with a `.jpg` extension. Decoding and re-encoding
//!   it would cost time and lose quality to no purpose. The same is true of
//!   `/JPXDecode` (JPEG 2000).
//! - **Encode.** Everything else arrives as raw samples — a flat array of
//!   component values — which no viewer understands. Those are packed into a
//!   PNG, which is lossless, so nothing is degraded on the way.
//!
//! # What we decline to do
//!
//! An honest `Unsupported` is a result too. CCITT and JBIG2 are fax codecs used
//! by scanners, `/Indexed` needs its palette expanded, and CMYK has no direct
//! PNG representation. Rather than emit a plausible-looking wrong picture, each
//! comes back as [`ExtractedImage::Unsupported`] carrying the reason — the same
//! rule the text pipeline follows for an unmappable glyph.
//!
//! A full-page image on a page with no text is not a figure; it is a scan, and
//! the detector (L4) is what should be consulted about it.

use crate::content::XObjectUse;
use crate::graphics::Matrix;
use crate::types::{ImageColorSpace, Palette, RawImage, Rect};

/// A container format an extracted image can be written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// JPEG, passed through untouched from `/DCTDecode`.
    Jpeg,
    /// JPEG 2000, passed through untouched from `/JPXDecode`.
    Jpeg2000,
    /// PNG, encoded here from raw samples.
    Png,
}

impl ImageFormat {
    /// The conventional file extension, without a dot.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Jpeg2000 => "jp2",
            ImageFormat::Png => "png",
        }
    }
}

/// The result of trying to extract one image.
///
/// # Rust lesson: an enum instead of `Option` plus a comment
///
/// This could have been `Option<Image>`, with the reason for a `None` left to
/// the reader's imagination. Making the failure a variant that *carries* its
/// reason means a caller can report "CCITTFaxDecode is not supported" rather
/// than "something went wrong", and the compiler makes them acknowledge that
/// the case exists.
#[derive(Debug, Clone)]
pub enum ExtractedImage {
    /// A usable image.
    Ready(Image),
    /// We know what this is and chose not to guess at it.
    Unsupported {
        /// The `/Resources` name, so the caller can say which image.
        resource_name: String,
        /// Why, in words a user can act on.
        reason: String,
    },
}

/// An image ready to be written to a file.
#[derive(Debug, Clone)]
pub struct Image {
    /// The `/Resources /XObject` name the content stream draws with.
    pub resource_name: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// The container the [`Image::data`] bytes are in.
    pub format: ImageFormat,
    /// The file's bytes — write them out and they open.
    pub data: Vec<u8>,
    /// Whether the source declared a soft mask we did **not** composite.
    ///
    /// Stronger than it sounds. A logo is routinely stored as a *blank* image
    /// whose entire shape lives in the mask — page 1 of the test fixture holds
    /// a 519x145 image whose every pixel is within one step of white. Without
    /// its `/SMask` the extraction is byte-correct and visually empty. Treat
    /// this flag as "this picture may be meaningless on its own", not as a note
    /// about edge quality. PLAN.md §8 tracks merging the mask.
    pub dropped_transparency: bool,
}

impl Image {
    /// A conventional file name, e.g. `Im0.jpg`.
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.resource_name, self.format.extension())
    }
}

/// Extract every image on a page.
pub fn extract(raws: &[RawImage]) -> Vec<ExtractedImage> {
    raws.iter().map(extract_one).collect()
}

/// An image together with where on the page it was painted.
#[derive(Debug, Clone)]
pub struct PlacedImage {
    /// The image, or the reason we declined to decode it.
    pub image: ExtractedImage,
    /// Where it landed, in PDF points from the bottom-left of the page.
    ///
    /// `None` means the image is declared in `/Resources` but we never saw it
    /// drawn. That is not proof it is absent from the page: it is very likely
    /// drawn *inside a form XObject*, which we do not enter. Reporting the
    /// image with an unknown position beats dropping it and beats inventing
    /// one.
    pub bbox: Option<Rect>,
}

/// Extract a page's images and locate each one.
///
/// `uses` comes from [`crate::content::interpret`]; names in it that match no
/// image resource are forms, and are ignored here.
///
/// An image drawn more than once appears once per placement, so the decoded
/// bytes are duplicated. Real pages almost never do this, and the alternative —
/// handing callers an index into a separate list — makes every caller pay for a
/// case that rarely happens.
pub fn extract_placed(raws: &[RawImage], uses: &[XObjectUse]) -> Vec<PlacedImage> {
    let mut placed = Vec::new();

    for raw in raws {
        let decoded = extract_one(raw);

        // Every `Do` that names this image resource.
        let mut drawn = uses
            .iter()
            .filter(|use_| use_.name == raw.resource_name)
            .peekable();

        if drawn.peek().is_none() {
            placed.push(PlacedImage {
                image: decoded,
                bbox: None,
            });
            continue;
        }

        for use_ in drawn {
            placed.push(PlacedImage {
                image: decoded.clone(),
                bbox: Some(placement_box(use_.ctm)),
            });
        }
    }
    placed
}

/// Where the unit square lands once the CTM is applied.
///
/// PDF paints every image into the square from (0,0) to (1,1); the matrix does
/// all the scaling, rotation and positioning. So the placed rectangle is that
/// square's four corners transformed — all four, not just two, because a
/// rotated or flipped matrix would otherwise give a nonsensical box.
///
/// A negative `d` is completely normal here, and is why the corners must be
/// normalised afterwards: images are commonly placed with a flipped y axis,
/// since image rows run top-down while PDF space runs bottom-up.
fn placement_box(ctm: Matrix) -> Rect {
    let corners = [
        ctm.apply(0.0, 0.0),
        ctm.apply(1.0, 0.0),
        ctm.apply(0.0, 1.0),
        ctm.apply(1.0, 1.0),
    ];

    let xs = corners.map(|(x, _)| x);
    let ys = corners.map(|(_, y)| y);

    // `fold` with `f64::min` rather than `.min()` on an iterator, because
    // `f64` is only `PartialOrd` — there is no total order to take a minimum
    // over without deciding what to do about NaN.
    Rect::new(
        xs.iter().copied().fold(f64::INFINITY, f64::min),
        ys.iter().copied().fold(f64::INFINITY, f64::min),
        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    )
}

/// Decide what one image is and produce it.
fn extract_one(raw: &RawImage) -> ExtractedImage {
    let unsupported = |reason: String| ExtractedImage::Unsupported {
        resource_name: raw.resource_name.clone(),
        reason,
    };

    // The codec, if any, is the last filter in the chain.
    match raw.filters.last().map(String::as_str) {
        // Already a file. Hand the bytes over untouched.
        Some("DCTDecode" | "DCT") => return passthrough(raw, ImageFormat::Jpeg),
        Some("JPXDecode") => return passthrough(raw, ImageFormat::Jpeg2000),

        // Fax codecs, used by scanners. Decoding them is a project of its own,
        // and a page carrying one is usually a scan the detector has already
        // flagged for OCR.
        Some(codec @ ("CCITTFaxDecode" | "CCF" | "JBIG2Decode")) => {
            return unsupported(format!("{codec} is not decoded"));
        }
        _ => {}
    }

    // Everything else is raw samples, which have to be encoded into something
    // a viewer can open.
    encode_samples(raw).unwrap_or_else(unsupported)
}

/// Wrap already-encoded bytes without touching them.
fn passthrough(raw: &RawImage, format: ImageFormat) -> ExtractedImage {
    ExtractedImage::Ready(Image {
        resource_name: raw.resource_name.clone(),
        width: raw.width,
        height: raw.height,
        format,
        data: raw.data.clone(),
        dropped_transparency: raw.has_smask,
    })
}

/// Encode raw samples as a PNG.
///
/// Returns `Err(reason)` for the sample layouts we decline to guess at, so the
/// caller can report which and why.
fn encode_samples(raw: &RawImage) -> std::result::Result<ExtractedImage, String> {
    // PNG stores 8 or 16 bits per channel. Sub-byte depths are packed several
    // pixels to a byte and would need unpacking against the row stride; that is
    // real work and none of it is guesswork, so it is a candidate for later
    // rather than something to fake now.
    if raw.bits_per_component != 8 {
        return Err(format!(
            "{}-bit samples are not yet unpacked",
            raw.bits_per_component
        ));
    }

    let color = match raw.color_space {
        ImageColorSpace::Gray => image::ColorType::L8,
        ImageColorSpace::Rgb => image::ColorType::Rgb8,
        ImageColorSpace::Cmyk => {
            // PNG has no CMYK. A correct conversion needs the document's ICC
            // profile; the naive formula would shift every colour.
            return Err("CMYK samples need a colour-managed conversion".to_string());
        }
        ImageColorSpace::Indexed => {
            // Expand the indices into real colours, then carry on as if the
            // image had been in the palette's own space all along.
            let palette = raw
                .palette
                .as_ref()
                .ok_or_else(|| "indexed image with no palette".to_string())?;
            return encode_indexed(raw, palette);
        }
        ImageColorSpace::Other => {
            return Err("unresolved colour space".to_string());
        }
    };

    // Guard before trusting the dimensions: a truncated stream would otherwise
    // make the encoder read past the end of the buffer.
    let components = raw.color_space.components().unwrap_or(0);
    let expected = (raw.width as usize)
        .checked_mul(raw.height as usize)
        .and_then(|px| px.checked_mul(components))
        .ok_or_else(|| "image dimensions overflow".to_string())?;

    if raw.data.len() < expected {
        return Err(format!(
            "truncated: {} sample bytes for a {}x{} image needing {}",
            raw.data.len(),
            raw.width,
            raw.height,
            expected
        ));
    }

    let mut data = Vec::new();
    image::write_buffer_with_format(
        // The encoder writes through `Seek`, which a bare `Vec` does not
        // provide; `Cursor` adds a position to it.
        &mut std::io::Cursor::new(&mut data),
        // Trailing bytes beyond the declared size are padding; hand over
        // exactly what the dimensions call for.
        &raw.data[..expected],
        raw.width,
        raw.height,
        color,
        image::ImageFormat::Png,
    )
    .map_err(|e| format!("PNG encoding failed: {e}"))?;

    Ok(ExtractedImage::Ready(Image {
        resource_name: raw.resource_name.clone(),
        width: raw.width,
        height: raw.height,
        format: ImageFormat::Png,
        data,
        dropped_transparency: raw.has_smask,
    }))
}

/// Expand an `/Indexed` image into its palette's colour space and encode that.
///
/// Each sample is a subscript, not a colour. An out-of-range index — which a
/// malformed file readily contains — is written as black rather than being
/// allowed to read past the end of the table.
fn encode_indexed(
    raw: &RawImage,
    palette: &Palette,
) -> std::result::Result<ExtractedImage, String> {
    let width = palette
        .base
        .components()
        .ok_or_else(|| "palette in an unresolved colour space".to_string())?;

    let pixels = (raw.width as usize)
        .checked_mul(raw.height as usize)
        .ok_or_else(|| "image dimensions overflow".to_string())?;

    if raw.data.len() < pixels {
        return Err(format!(
            "truncated: {} index bytes for a {}x{} image",
            raw.data.len(),
            raw.width,
            raw.height
        ));
    }

    let mut expanded = Vec::with_capacity(pixels * width);
    for &index in &raw.data[..pixels] {
        match palette.get(index as usize) {
            Some(entry) => expanded.extend_from_slice(entry),
            None => expanded.extend(std::iter::repeat_n(0u8, width)),
        }
    }

    // Re-enter the ordinary path with the samples now in the base space.
    encode_samples(&RawImage {
        color_space: palette.base,
        palette: None,
        data: expanded,
        ..raw.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(filters: &[&str], cs: ImageColorSpace, bpc: u8, data: Vec<u8>) -> RawImage {
        RawImage {
            resource_name: "Im0".to_string(),
            width: 2,
            height: 2,
            bits_per_component: bpc,
            color_space: cs,
            filters: filters.iter().map(|f| f.to_string()).collect(),
            data,
            palette: None,
            has_smask: false,
        }
    }

    fn reason(result: &ExtractedImage) -> &str {
        match result {
            ExtractedImage::Unsupported { reason, .. } => reason,
            ExtractedImage::Ready(_) => panic!("expected Unsupported"),
        }
    }

    #[test]
    fn jpeg_bytes_pass_through_untouched() {
        // The point of passthrough: the output is byte-identical to the input.
        // Re-encoding would lose quality for nothing.
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        let out = extract_one(&raw(&["DCTDecode"], ImageColorSpace::Rgb, 8, bytes.clone()));

        let ExtractedImage::Ready(img) = out else {
            panic!("a JPEG should extract");
        };
        assert_eq!(img.format, ImageFormat::Jpeg);
        assert_eq!(img.data, bytes);
        assert_eq!(img.file_name(), "Im0.jpg");
    }

    #[test]
    fn a_flate_chain_before_the_codec_still_reads_as_jpeg() {
        // `/Filter [/FlateDecode /DCTDecode]` is legal; the codec is last, and
        // the parser has already undone the Flate layer.
        let out = extract_one(&raw(
            &["FlateDecode", "DCTDecode"],
            ImageColorSpace::Rgb,
            8,
            vec![0xFF, 0xD8],
        ));
        assert!(matches!(
            out,
            ExtractedImage::Ready(Image {
                format: ImageFormat::Jpeg,
                ..
            })
        ));
    }

    #[test]
    fn gray_samples_encode_to_a_real_png() {
        let out = extract_one(&raw(
            &["FlateDecode"],
            ImageColorSpace::Gray,
            8,
            vec![0, 64, 128, 255],
        ));
        let ExtractedImage::Ready(img) = out else {
            panic!("gray samples should encode");
        };
        assert_eq!(img.format, ImageFormat::Png);
        // The PNG magic number, so this is a file and not just bytes.
        assert_eq!(
            &img.data[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert_eq!(img.file_name(), "Im0.png");
    }

    #[test]
    fn rgb_samples_encode_to_a_real_png() {
        let pixels = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let out = extract_one(&raw(&[], ImageColorSpace::Rgb, 8, pixels));
        assert!(matches!(
            out,
            ExtractedImage::Ready(Image {
                format: ImageFormat::Png,
                ..
            })
        ));
    }

    #[test]
    fn a_truncated_stream_is_refused_not_read_past() {
        // A 2x2 RGB image needs 12 bytes. Trusting the header and reading 12
        // out of a 4-byte buffer is how an extractor gets a CVE.
        let out = extract_one(&raw(&[], ImageColorSpace::Rgb, 8, vec![1, 2, 3, 4]));
        assert!(reason(&out).contains("truncated"), "got: {}", reason(&out));
    }

    #[test]
    fn unsupported_cases_say_which_and_why() {
        // Each of these is a deliberate refusal, not a bug. The reason is the
        // deliverable: a caller can act on it.
        let cases = [
            (
                extract_one(&raw(&["CCITTFaxDecode"], ImageColorSpace::Gray, 8, vec![])),
                "CCITTFaxDecode",
            ),
            (
                extract_one(&raw(&["JBIG2Decode"], ImageColorSpace::Gray, 8, vec![])),
                "JBIG2Decode",
            ),
            (
                extract_one(&raw(&[], ImageColorSpace::Cmyk, 8, vec![0; 16])),
                "colour-managed",
            ),
            (
                extract_one(&raw(&[], ImageColorSpace::Indexed, 8, vec![0; 4])),
                "no palette",
            ),
            (
                extract_one(&raw(&[], ImageColorSpace::Gray, 4, vec![0; 4])),
                "4-bit",
            ),
        ];

        for (result, expected) in cases {
            assert!(
                reason(&result).contains(expected),
                "expected {expected:?} in: {}",
                reason(&result)
            );
        }
    }

    fn use_of(name: &str, ctm: Matrix) -> XObjectUse {
        XObjectUse {
            name: name.to_string(),
            ctm,
            glyph_index: 0,
        }
    }

    #[test]
    fn placement_transforms_the_unit_square() {
        // 200x100 points, with its lower-left corner at (50, 600).
        let bbox = placement_box(Matrix::new(200.0, 0.0, 0.0, 100.0, 50.0, 600.0));
        assert_eq!((bbox.x0, bbox.y0), (50.0, 600.0));
        assert_eq!((bbox.width(), bbox.height()), (200.0, 100.0));
    }

    #[test]
    fn a_flipped_image_still_gets_a_sane_box() {
        // A negative `d` is normal: image rows run top-down, PDF space runs
        // bottom-up, so placements routinely flip the y axis. Without
        // normalising the corners this would come out inside out.
        let bbox = placement_box(Matrix::new(200.0, 0.0, 0.0, -100.0, 50.0, 700.0));
        assert_eq!((bbox.y0, bbox.y1), (600.0, 700.0));
        assert_eq!(bbox.height(), 100.0);
    }

    #[test]
    fn a_rotated_placement_uses_all_four_corners() {
        // Quarter turn: taking only two corners would give a zero-area box.
        let bbox = placement_box(Matrix::new(0.0, 100.0, -50.0, 0.0, 0.0, 0.0));
        assert_eq!((bbox.width(), bbox.height()), (50.0, 100.0));
    }

    #[test]
    fn an_image_never_drawn_is_reported_with_no_position() {
        // Almost certainly drawn inside a form XObject we do not enter.
        // Dropping it would hide a real image; inventing a box would be worse.
        let raws = [raw(
            &["DCTDecode"],
            ImageColorSpace::Rgb,
            8,
            vec![0xFF, 0xD8],
        )];
        let placed = extract_placed(&raws, &[]);
        assert_eq!(placed.len(), 1);
        assert!(placed[0].bbox.is_none());
    }

    #[test]
    fn an_image_drawn_twice_is_placed_twice() {
        let raws = [raw(
            &["DCTDecode"],
            ImageColorSpace::Rgb,
            8,
            vec![0xFF, 0xD8],
        )];
        let uses = [
            use_of("Im0", Matrix::new(10.0, 0.0, 0.0, 10.0, 0.0, 0.0)),
            use_of("Im0", Matrix::new(10.0, 0.0, 0.0, 10.0, 100.0, 100.0)),
        ];
        let placed = extract_placed(&raws, &uses);
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].bbox.map(|b| b.x0), Some(0.0));
        assert_eq!(placed[1].bbox.map(|b| b.x0), Some(100.0));
    }

    #[test]
    fn a_form_invocation_matches_no_image() {
        // `/Fm0 Do` names a form, not an image resource; it must not attach
        // itself to an unrelated image.
        let raws = [raw(
            &["DCTDecode"],
            ImageColorSpace::Rgb,
            8,
            vec![0xFF, 0xD8],
        )];
        let uses = [use_of("Fm0", Matrix::IDENTITY)];
        let placed = extract_placed(&raws, &uses);
        assert_eq!(placed.len(), 1);
        assert!(placed[0].bbox.is_none());
    }

    #[test]
    fn a_dropped_soft_mask_is_reported() {
        // The image is complete but opaque; a caller must be able to tell.
        let mut r = raw(&["DCTDecode"], ImageColorSpace::Rgb, 8, vec![0xFF, 0xD8]);
        r.has_smask = true;
        let ExtractedImage::Ready(img) = extract_one(&r) else {
            panic!("should extract");
        };
        assert!(img.dropped_transparency);
    }
}
