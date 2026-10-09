//! Image thumbnails: `GET /api/storage/{id}?thumb=WxH[&fit=contain|cover]` (docs/PROTOCOL.md, "File storage").
//!
//! - **Sources**: files whose content type is `image/jpeg`, `image/png`, `image/webp` or `image/gif` (the first frame),
//!   at most `StorageConfig::thumbnail_max_source_bytes` (32 MiB). The bytes must really be one of those formats.
//! - **Decompression bombs**: the dimensions are read from the header first and a source with more than
//!   `thumbnail_max_source_pixels` (40 MP) is refused before anything is decoded; the decoder then runs with
//!   matching `image::Limits` (dimensions and allocation), so a header that lies can't make it allocate more.
//! - **Sizes**: `W` and `H` between 1 and `thumbnail_max_dimension` (1024), or only the pairs in
//!   `thumbnail_sizes` when that allowlist is set; `thumb=N` means `N×N`. `fit=contain` (default) scales the image down to fit inside
//!   `W×H` keeping its aspect ratio (never up); `fit=cover` fills exactly `W×H`, scaling and cropping the center.
//!   JPEG EXIF orientation is applied.
//! - **Output**: PNG when the image has an alpha channel, JPEG (quality 85) otherwise.
//! - **Cache**: a generated thumbnail is stored in the blob store at `<prefix>/_thumbs/<id>/<W>x<H>-<fit>`, so each
//!   variant is generated once. Deleting the file deletes its thumbnails, and the orphan sweep removes thumbnails
//!   whose file is gone. The ETag of a variant is `"<sha256>.t1.<W>x<H>.<fit>"` (`t1`: the thumbnailer version).
//! - At most a few thumbnails are generated at once (a semaphore), each on a blocking thread.

use std::fmt;
use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

use super::config::StorageConfig;

/// Bumped when the output of [`render`] changes, so cached variants get new ETags and keys.
pub(crate) const VERSION: &str = "t1";

/// The directory (under the storage prefix) holding cached thumbnails, one sub-directory per file id.
pub(crate) const THUMBS_DIR: &str = "_thumbs";

/// How a thumbnail fits its box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fit {
    /// Inside `W×H`, aspect ratio kept, never scaled up.
    Contain,
    /// Exactly `W×H`: scaled to cover it, then the center cropped.
    Cover,
}

impl Fit {
    fn as_str(self) -> &'static str {
        match self {
            Fit::Contain => "contain",
            Fit::Cover => "cover",
        }
    }
}

/// A requested thumbnail variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThumbSpec {
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
}

impl ThumbSpec {
    /// Parse `thumb=WxH` (or `thumb=N`, an `N×N` box) and `fit=…` against the configured limits. The error is safe
    /// to show to clients.
    pub(crate) fn parse(thumb: &str, fit: Option<&str>, config: &StorageConfig) -> Result<ThumbSpec, String> {
        let bad = || format!("`thumb` must be WIDTHxHEIGHT or SIZE, e.g. 200x200 or 480 (got `{thumb}`)");
        let thumb = thumb.trim();
        let (w, h) = thumb.split_once(['x', 'X']).unwrap_or((thumb, thumb));
        let num = |s: &str| if !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit()) { s.parse::<u32>().ok() } else { None };
        let (width, height) = (num(w).ok_or_else(bad)?, num(h).ok_or_else(bad)?);
        let fit = match fit.map(str::trim) {
            None | Some("") | Some("contain") => Fit::Contain,
            Some("cover") => Fit::Cover,
            Some(other) => return Err(format!("`fit` must be contain or cover (got `{other}`)")),
        };
        match &config.thumbnail_sizes {
            Some(sizes) => {
                if !sizes.contains(&(width, height)) {
                    let list: Vec<String> = sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
                    return Err(format!("Thumbnail size {width}x{height} is not allowed (allowed: {})", list.join(", ")));
                }
            }
            None => {
                let max = config.thumbnail_max_dimension;
                if width == 0 || height == 0 || width > max || height > max {
                    return Err(format!("Thumbnail sizes go from 1x1 to {max}x{max} (got {width}x{height})"));
                }
            }
        }
        Ok(ThumbSpec { width, height, fit })
    }

    /// `200x200-cover`: the cache key's file name.
    pub(crate) fn variant(&self) -> String {
        format!("{}x{}-{}", self.width, self.height, self.fit.as_str())
    }

    /// The ETag of this variant of the file with `sha256`.
    pub(crate) fn etag(&self, sha256: &str) -> String {
        format!("\"{sha256}.{VERSION}.{}x{}.{}\"", self.width, self.height, self.fit.as_str())
    }
}

/// Content types thumbnails can be made of.
pub(crate) fn supported_source(content_type: Option<&str>) -> bool {
    let essence = content_type.unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    matches!(essence.as_str(), "image/jpeg" | "image/png" | "image/webp" | "image/gif")
}

/// The content type of an encoded thumbnail (PNG or JPEG), from its magic bytes.
pub(crate) fn content_type_of(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { "image/png" } else { "image/jpeg" }
}

/// Why a thumbnail couldn't be made. The messages are safe to show to clients.
#[derive(Debug)]
pub(crate) enum ThumbError {
    /// Not a JPEG, PNG, WebP or GIF.
    Unsupported,
    /// More pixels than allowed (or a decoder limit was hit).
    TooLarge { width: u32, height: u32, max_pixels: u64 },
    /// The image is corrupt.
    Invalid(String),
}

impl fmt::Display for ThumbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThumbError::Unsupported => f.write_str("Thumbnails can only be made of JPEG, PNG, WebP and GIF images"),
            ThumbError::TooLarge { width, height, max_pixels } => {
                write!(f, "The image is too large for a thumbnail ({width}x{height}; at most {max_pixels} pixels)")
            }
            ThumbError::Invalid(e) => write!(f, "The image could not be decoded: {e}"),
        }
    }
}

/// Decode `source` (checking its dimensions before decoding anything) and render `spec`. CPU-bound: call it on a
/// blocking thread.
pub(crate) fn render(source: &[u8], spec: ThumbSpec, max_pixels: u64) -> Result<Vec<u8>, ThumbError> {
    let reader = || -> Result<ImageReader<Cursor<&[u8]>>, ThumbError> {
        let reader = ImageReader::new(Cursor::new(source)).with_guessed_format().map_err(|e| ThumbError::Invalid(e.to_string()))?;
        match reader.format() {
            Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif) => Ok(reader),
            _ => Err(ThumbError::Unsupported),
        }
    };
    // The header only: a decompression bomb is refused here, before any pixel is decoded.
    let (width, height) = reader()?.into_dimensions().map_err(|e| ThumbError::Invalid(e.to_string()))?;
    let too_large = || ThumbError::TooLarge { width, height, max_pixels };
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(too_large());
    }
    let mut reader = reader()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    // RGBA with 16-bit channels, plus room for the decoder's own buffers.
    limits.max_alloc = Some(max_pixels.saturating_mul(8).saturating_add(64 << 20));
    reader.limits(limits);
    let decode_error = |e: image::ImageError| match e {
        image::ImageError::Limits(_) => too_large(),
        e => ThumbError::Invalid(e.to_string()),
    };
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let orientation = decoder.orientation().map_err(decode_error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);

    let image = match spec.fit {
        Fit::Contain if image.width() <= spec.width && image.height() <= spec.height => image,
        Fit::Contain => image.resize(spec.width, spec.height, FilterType::Triangle),
        Fit::Cover => image.resize_to_fill(spec.width, spec.height, FilterType::Triangle),
    };
    let mut out = Vec::new();
    if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|e| ThumbError::Invalid(e.to_string()))?;
    } else {
        JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&DynamicImage::ImageRgb8(image.to_rgb8()))
            .map_err(|e| ThumbError::Invalid(e.to_string()))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let image = if alpha {
            DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |x, y| image::Rgba([x as u8, y as u8, 0, 128])))
        } else {
            DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| image::Rgb([x as u8, y as u8, 0])))
        };
        let mut out = Vec::new();
        image.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).unwrap();
        out
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes).unwrap().into_rgb8().dimensions()
    }

    #[test]
    fn parse_specs() {
        let config = StorageConfig::default();
        let spec = ThumbSpec::parse("200x100", Some("cover"), &config).unwrap();
        assert_eq!((spec.width, spec.height, spec.fit), (200, 100, Fit::Cover));
        assert_eq!(spec.variant(), "200x100-cover");
        assert_eq!(spec.etag("abc"), "\"abc.t1.200x100.cover\"");
        assert_eq!(ThumbSpec::parse("64X64", None, &config).unwrap().fit, Fit::Contain);
        let square = ThumbSpec::parse("480", None, &config).unwrap();
        assert_eq!((square.width, square.height), (480, 480), "a single number is a square box");
        for bad in ["", "0", "2000", "0x10", "1025x10", "x10", "10x", "-1x5", "10x10x10", "99999999x1"] {
            assert!(ThumbSpec::parse(bad, None, &config).is_err(), "{bad}");
        }
        assert!(ThumbSpec::parse("10x10", Some("stretch"), &config).is_err());
        let only = StorageConfig::default().thumbnail_sizes(&[(64, 64), (256, 256)]);
        assert!(ThumbSpec::parse("64x64", None, &only).is_ok());
        assert!(ThumbSpec::parse("65x64", None, &only).unwrap_err().contains("allowed: 64x64, 256x256"));
    }

    #[test]
    fn fits_and_formats() {
        let source = png(400, 200, false);
        let contain = ThumbSpec { width: 100, height: 100, fit: Fit::Contain };
        let out = render(&source, contain, 1 << 20).unwrap();
        assert_eq!((content_type_of(&out), dims(&out)), ("image/jpeg", (100, 50)), "opaque: JPEG, aspect kept");
        let cover = ThumbSpec { width: 100, height: 100, fit: Fit::Cover };
        assert_eq!(dims(&render(&source, cover, 1 << 20).unwrap()), (100, 100));
        // Never scaled up with contain.
        let big = ThumbSpec { width: 1000, height: 1000, fit: Fit::Contain };
        assert_eq!(dims(&render(&source, big, 1 << 20).unwrap()), (400, 200));
        let transparent = render(&png(50, 50, true), contain, 1 << 20).unwrap();
        assert_eq!(content_type_of(&transparent), "image/png", "alpha: PNG");
        assert!(matches!(render(b"\x89PNG\r\n\x1a\n truncated", contain, 1 << 20), Err(ThumbError::Invalid(_))));
        assert!(matches!(render(b"plain text", contain, 1 << 20), Err(ThumbError::Unsupported)));
        assert!(matches!(render(&source, contain, 400 * 200 - 1), Err(ThumbError::TooLarge { width: 400, height: 200, .. })));
    }
}
