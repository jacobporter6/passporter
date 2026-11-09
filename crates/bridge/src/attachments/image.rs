//! Turning an uploaded image into one that is safe to keep: sniff the
//! format from its bytes (the declared type is never trusted), decode it
//! under strict limits, apply its EXIF orientation, shrink it to a sane
//! size and encode the pixels afresh.
//!
//! Re-encoding is the metadata strip: the encoders here are never handed
//! EXIF, XMP, ICC or text chunks, so GPS positions, camera serials and
//! comments in the upload never reach the disk. Only pixels survive.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use protocol::ImageType;

/// Longest edge kept, in pixels. The phone already shrinks photos to
/// 2048 px; this bounds what any other client can make the agent read.
pub(crate) const MAX_EDGE: u32 = 4096;
/// Largest image decoded at all, per edge.
const MAX_DECODE_EDGE: u32 = 12_000;
/// Most memory a decode may take: a 12000 x 12000 RGBA image is 576 MB,
/// so this also caps the pixel count well below that.
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// JPEG quality of the stored file: text in a screenshot stays legible.
const JPEG_QUALITY: u8 = 88;

/// An upload's format, from its leading bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sniffed {
    Jpeg,
    Png,
    Webp,
}

impl Sniffed {
    /// The format `bytes` start with, if it is one this bridge takes.
    pub(crate) fn of(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0xFF, 0xD8, 0xFF, ..] => Some(Self::Jpeg),
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some(Self::Png),
            [
                b'R',
                b'I',
                b'F',
                b'F',
                _,
                _,
                _,
                _,
                b'W',
                b'E',
                b'B',
                b'P',
                ..,
            ] => Some(Self::Webp),
            _ => None,
        }
    }

    const fn format(self) -> ImageFormat {
        match self {
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Png => ImageFormat::Png,
            Self::Webp => ImageFormat::WebP,
        }
    }
}

/// Whether a declared `Content-Type` is one of the image types accepted.
/// Only a first filter: the bytes decide ([`Sniffed::of`]).
pub(crate) fn declared_ok(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(essence.as_str(), "image/jpeg" | "image/png" | "image/webp")
}

/// A re-encoded image, ready to store.
#[derive(Debug)]
pub(crate) struct Clean {
    pub(crate) bytes: Vec<u8>,
    pub(crate) kind: ImageType,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Why an upload could not be made safe.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CleanError {
    /// Not a JPEG, PNG or WebP, whatever it claimed to be.
    #[error("not a JPEG, PNG or WebP image")]
    NotAnImage,
    /// It claimed to be an image but does not decode (truncated, corrupt,
    /// or over the size limits).
    #[error("the image does not decode: {0}")]
    Undecodable(#[source] image::ImageError),
    /// Encoding the clean copy failed.
    #[error("re-encoding the image failed: {0}")]
    Encode(#[source] image::ImageError),
}

/// Decodes `upload` and encodes its pixels afresh (see the module docs).
/// Lossless sources (PNG) and images with transparency stay PNG; the rest
/// become JPEG. CPU-bound: run it off the async threads.
pub(crate) fn clean(upload: &[u8]) -> Result<Clean, CleanError> {
    let sniffed = Sniffed::of(upload).ok_or(CleanError::NotAnImage)?;
    let mut reader = ImageReader::with_format(Cursor::new(upload), sniffed.format());
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(CleanError::Undecodable)?;
    // A broken EXIF block only loses the rotation, never the image.
    let orientation = decoder.orientation().ok();
    let mut pixels = DynamicImage::from_decoder(decoder).map_err(CleanError::Undecodable)?;
    if let Some(orientation) = orientation {
        pixels.apply_orientation(orientation);
    }
    if pixels.width().max(pixels.height()) > MAX_EDGE {
        // `resize` keeps the aspect ratio within the box.
        pixels = pixels.resize(MAX_EDGE, MAX_EDGE, FilterType::Lanczos3);
    }
    let kind = if sniffed == Sniffed::Png || pixels.has_alpha() {
        ImageType::Png
    } else {
        ImageType::Jpeg
    };
    let mut bytes = Vec::new();
    match kind {
        ImageType::Png => pixels.write_with_encoder(PngEncoder::new(&mut bytes)),
        ImageType::Jpeg => DynamicImage::ImageRgb8(pixels.to_rgb8())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)),
    }
    .map_err(CleanError::Encode)?;
    Ok(Clean {
        bytes,
        kind,
        width: pixels.width(),
        height: pixels.height(),
    })
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Images made at runtime, with metadata planted in them.

    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    /// The GPS latitude reference and a marker string planted in the
    /// metadata, to look for in the output.
    pub(crate) const PLANTED: &[u8] = b"PLANTED-GPS-51.5007N";

    /// A `width` x `height` JPEG of a gradient.
    pub(crate) fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let pixels = RgbImage::from_fn(width, height, |x, y| Rgb([byte(x), byte(y), 128]));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(pixels)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
            .expect("encode");
        out
    }

    fn byte(n: u32) -> u8 {
        u8::try_from(n % 256).unwrap_or_default()
    }

    /// A small PNG, opaque or with transparency.
    pub(crate) fn png(alpha: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let image = if alpha {
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 6, Rgba([10, 20, 30, 40])))
        } else {
            DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 6, Rgb([10, 20, 30])))
        };
        image
            .write_with_encoder(PngEncoder::new(&mut out))
            .expect("encode");
        out
    }

    /// A little-endian TIFF block for an EXIF APP1 segment: IFD0 with an
    /// orientation tag (`orientation`) and a pointer to a GPS IFD holding
    /// a latitude reference, latitude and an ASCII tag carrying
    /// [`PLANTED`].
    fn exif_tiff(orientation: u16) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b"II*\0");
        t.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: two entries.
        let entry = |t: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32| {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&kind.to_le_bytes());
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&value.to_le_bytes());
        };
        t.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut t, 0x0112, 3, 1, u32::from(orientation)); // Orientation, SHORT
        let gps_ifd = 8 + 2 + 2 * 12 + 4;
        entry(&mut t, 0x8825, 4, 1, gps_ifd); // GPS IFD pointer, LONG
        t.extend_from_slice(&0u32.to_le_bytes());
        // GPS IFD: LatitudeRef "N", Latitude (3 RATIONALs), and the marker
        // as GPSProcessingMethod (UNDEFINED).
        let entries = 3u32;
        let data = gps_ifd + 2 + entries * 12 + 4;
        let marker_len = u32::try_from(PLANTED.len()).expect("short");
        t.extend_from_slice(&3u16.to_le_bytes());
        entry(&mut t, 0x0001, 2, 2, u32::from_le_bytes(*b"N\0\0\0"));
        entry(&mut t, 0x0002, 5, 3, data);
        entry(&mut t, 0x001B, 7, marker_len, data + 24);
        t.extend_from_slice(&0u32.to_le_bytes());
        for (num, den) in [(51u32, 1u32), (30, 1), (2, 1)] {
            t.extend_from_slice(&num.to_le_bytes());
            t.extend_from_slice(&den.to_le_bytes());
        }
        t.extend_from_slice(PLANTED);
        t
    }

    /// `jpeg` with an EXIF APP1 segment (orientation and GPS) and a COM
    /// comment segment inserted right after its SOI marker.
    pub(crate) fn with_exif(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&exif_tiff(orientation));
        let mut out = jpeg[..2].to_vec();
        for (marker, payload) in [(0xE1u8, app1), (0xFE, PLANTED.to_vec())] {
            out.extend_from_slice(&[0xFF, marker]);
            let len = u16::try_from(payload.len() + 2).expect("short segment");
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&payload);
        }
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// `png` with `eXIf` (GPS) and `tEXt` chunks inserted after `IHDR`.
    pub(crate) fn png_with_metadata(png: &[u8]) -> Vec<u8> {
        let chunk = |kind: &[u8; 4], data: &[u8]| {
            let mut c = u32::try_from(data.len())
                .expect("short")
                .to_be_bytes()
                .to_vec();
            c.extend_from_slice(kind);
            c.extend_from_slice(data);
            let mut crc_input = kind.to_vec();
            crc_input.extend_from_slice(data);
            c.extend_from_slice(&crc32(&crc_input).to_be_bytes());
            c
        };
        // Signature (8) + IHDR (4 + 4 + 13 + 4).
        let after_ihdr = 8 + 25;
        let mut text = b"Comment\0".to_vec();
        text.extend_from_slice(PLANTED);
        let mut out = png[..after_ihdr].to_vec();
        out.extend(chunk(b"eXIf", &exif_tiff(1)));
        out.extend(chunk(b"tEXt", &text));
        out.extend_from_slice(&png[after_ihdr..]);
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Whether `haystack` contains `needle`.
    pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn sniffs_formats_from_bytes_alone() {
        assert_eq!(Sniffed::of(&jpeg(4, 4)), Some(Sniffed::Jpeg));
        assert_eq!(Sniffed::of(&png(false)), Some(Sniffed::Png));
        assert_eq!(Sniffed::of(b"RIFF\0\0\0\0WEBPVP8 "), Some(Sniffed::Webp));
        for decoy in [
            &b"<html><script>alert(1)</script>"[..],
            b"GIF89a....",
            b"%PDF-1.7",
            b"\x7fELF\x02\x01",
            b"RIFF\0\0\0\0WAVEfmt ",
            b"",
            b"\xFF\xD8",
        ] {
            assert_eq!(Sniffed::of(decoy), None, "{decoy:?}");
        }
    }

    #[test]
    fn only_image_content_types_are_declarable() {
        assert!(declared_ok("image/jpeg"));
        assert!(declared_ok("Image/PNG; charset=binary"));
        assert!(declared_ok("image/webp"));
        assert!(!declared_ok("image/svg+xml"));
        assert!(!declared_ok("text/html"));
        assert!(!declared_ok("application/octet-stream"));
        assert!(!declared_ok(""));
    }

    #[test]
    fn exif_and_gps_never_survive_a_jpeg() {
        let upload = with_exif(&jpeg(40, 20), 1);
        assert!(contains(&upload, b"Exif\0\0"), "the fixture has EXIF");
        assert!(contains(&upload, PLANTED), "the fixture has the GPS marker");
        let clean = clean(&upload).expect("cleans");
        assert_eq!(clean.kind, ImageType::Jpeg);
        assert_eq!((clean.width, clean.height), (40, 20));
        assert!(!contains(&clean.bytes, b"Exif"), "no EXIF block");
        assert!(!contains(&clean.bytes, PLANTED), "no GPS data or comment");
        assert!(!contains(&clean.bytes, &[0xFF, 0xE1]), "no APP1 segment");
        assert!(!contains(&clean.bytes, &[0xFF, 0xFE]), "no COM segment");
        // Still a picture.
        assert!(image::load_from_memory(&clean.bytes).is_ok());
    }

    #[test]
    fn png_metadata_chunks_are_dropped_and_png_stays_png() {
        let upload = png_with_metadata(&png(false));
        assert!(
            image::load_from_memory(&upload).is_ok(),
            "a valid PNG fixture"
        );
        let clean = clean(&upload).expect("cleans");
        assert_eq!(clean.kind, ImageType::Png);
        assert!(!contains(&clean.bytes, b"eXIf"));
        assert!(!contains(&clean.bytes, b"tEXt"));
        assert!(!contains(&clean.bytes, PLANTED));
        assert_eq!(clean_kind(&png(true)), ImageType::Png);
    }

    fn clean_kind(upload: &[u8]) -> ImageType {
        clean(upload).expect("cleans").kind
    }

    #[test]
    fn the_exif_orientation_is_applied_to_the_pixels() {
        // 6: rotate 90° clockwise to display; the clean copy is upright.
        let clean = clean(&with_exif(&jpeg(40, 20), 6)).expect("cleans");
        assert_eq!((clean.width, clean.height), (20, 40));
    }

    #[test]
    fn large_images_shrink_to_the_longest_edge() {
        let clean = clean(&jpeg(MAX_EDGE + 400, 100)).expect("cleans");
        assert_eq!(clean.width, MAX_EDGE);
        assert!(clean.height < 100);
    }

    #[test]
    fn decoys_and_broken_images_are_refused() {
        assert!(matches!(
            clean(b"<svg onload=alert(1)>"),
            Err(CleanError::NotAnImage)
        ));
        let whole = jpeg(16, 16);
        assert!(matches!(
            clean(&whole[..whole.len() / 3]),
            Err(CleanError::Undecodable(_))
        ));
        assert!(matches!(
            clean(b"\x89PNG\r\n\x1a\nnot really"),
            Err(CleanError::Undecodable(_))
        ));
    }
}
