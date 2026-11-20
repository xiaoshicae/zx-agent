//! Images fitted to the limits of the model APIs before they enter a conversation (pi:
//! image-resize-core.ts). A PNG, JPEG, GIF or WebP file at most 2000 pixels on a side and
//! 4.5 MiB in base64 goes as it is; a larger one is scaled to fit and encoded again, as PNG or
//! else as JPEG of falling quality, and made smaller still until an encoding fits. Every file
//! is decoded first, so a broken one is refused here rather than by each later request. The
//! tools, the MCP client and the hosts' input (ACP, `run`, IM) fit their images here.

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};

use crate::ai::Image;

/// The formats the model APIs take.
const FORMATS: [ImageFormat; 4] = [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif, ImageFormat::WebP];

/// The limits an image is fitted to (pi: `maxWidth`, `maxHeight`, `maxBytes`): 2000 pixels
/// on a side, and 4.5 MiB in base64, below the 5 MB the Anthropic Messages API takes.
const LIMITS: Bounds = Bounds {
    side: 2000,
    base64_bytes: 4_718_592,
};

/// The JPEG qualities tried in turn, after PNG, for an image encoded again (pi:
/// `jpegQuality` and the steps after it).
const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

/// The most memory that decoding one image may take: 64 megapixels as RGBA, more than a
/// phone's photograph. A larger picture is refused rather than decoded.
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// How large an image may be.
#[derive(Clone, Copy)]
struct Bounds {
    /// The longest side, in pixels.
    side: u32,
    /// The most bytes of its base64.
    base64_bytes: usize,
}

/// An image fitted to the limits, with its size in pixels before and after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fitted {
    pub image: Image,
    /// Width and height of the image as given.
    pub original: (u32, u32),
    /// Width and height of the image as sent: `original` unless it was scaled down.
    pub size: (u32, u32),
}

impl Fitted {
    /// What a reader of the image, such as a model that points at what it sees, needs to know
    /// of its scaling; `None` when it was not scaled (pi: `formatDimensionNote`).
    pub fn scaling_note(&self) -> Option<String> {
        if self.size == self.original {
            return None;
        }
        let ((width, height), (shown_width, shown_height)) = (self.original, self.size);
        let scale = f64::from(width) / f64::from(shown_width);
        Some(format!(
            "[Image: original {width}x{height}, displayed at {shown_width}x{shown_height}. Multiply coordinates by \
             {scale:.2} to map to original image.]"
        ))
    }
}

/// Whether `bytes` start as a file of a format the model APIs take: PNG, JPEG, GIF or WebP.
pub fn is_image(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok_and(|format| FORMATS.contains(&format))
}

/// `bytes`, an image file, fitted to the limits. Fails, saying why for the model or the
/// person who sent it, on a file that is not a PNG, JPEG, GIF or WebP image, one that cannot
/// be decoded within the memory allowed, and one that no encoding brings under the limits.
/// Decoding and encoding take a while, so they run on Tokio's blocking pool.
pub async fn fit(bytes: Vec<u8>) -> Result<Fitted, String> {
    tokio::task::spawn_blocking(move || fit_within(&bytes, LIMITS))
        .await
        .unwrap_or_else(|error| Err(format!("fitting the image failed: {error}")))
}

/// [`fit`] for an image in base64, as MCP servers and ACP clients send them.
pub async fn fit_base64(data: &str) -> Result<Fitted, String> {
    let bytes = STANDARD
        .decode(data.trim())
        .map_err(|error| format!("the image is not valid base64: {error}"))?;
    fit(bytes).await
}

fn fit_within(bytes: &[u8], bounds: Bounds) -> Result<Fitted, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("cannot read the image: {error}"))?;
    let format = reader
        .format()
        .filter(|format| FORMATS.contains(format))
        .ok_or("not a PNG, JPEG, GIF or WebP image")?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let undecodable = |error: image::ImageError| format!("cannot decode the image: {error}");
    let mut decoder = reader.into_decoder().map_err(undecodable)?;
    let orientation = decoder.orientation().map_err(undecodable)?;
    let mut picture = DynamicImage::from_decoder(decoder).map_err(undecodable)?;
    let stored = picture.dimensions();
    if fits(stored, bounds) && base64_len(bytes.len()) <= bounds.base64_bytes {
        let image = Image {
            mime_type: format.to_mime_type().to_string(),
            data: STANDARD.encode(bytes),
        };
        return Ok(Fitted {
            image,
            original: stored,
            size: stored,
        });
    }
    // The file says which way up it is shown; the encoding written here does not.
    picture.apply_orientation(orientation);
    let original = picture.dimensions();
    let mut size = scaled_into(original, bounds.side);
    loop {
        let scaled = picture.resize_exact(size.0, size.1, FilterType::Lanczos3);
        if let Some(image) = first_encoding_within(&scaled, bounds.base64_bytes)? {
            return Ok(Fitted { image, original, size });
        }
        // pi: three quarters each time, down to a pixel.
        let smaller = (shrunk(size.0), shrunk(size.1));
        if smaller == size {
            return Err("no encoding of the image is small enough".to_string());
        }
        size = smaller;
    }
}

/// `picture` as PNG, or else as JPEG at the first quality whose base64 takes at most
/// `base64_bytes`; `None` when none does. Each encoding is made only when the ones before it
/// are too large.
fn first_encoding_within(picture: &DynamicImage, base64_bytes: usize) -> Result<Option<Image>, String> {
    let png = encoded(picture, ImageFormat::Png, |buffer| {
        picture.write_to(Cursor::new(buffer), ImageFormat::Png)
    })?;
    if png.data.len() <= base64_bytes {
        return Ok(Some(png));
    }
    // JPEG has no transparency: the colour channels alone.
    let opaque = DynamicImage::ImageRgb8(picture.to_rgb8());
    for quality in JPEG_QUALITIES {
        let jpeg = encoded(picture, ImageFormat::Jpeg, |buffer| {
            opaque.write_with_encoder(JpegEncoder::new_with_quality(buffer, quality))
        })?;
        if jpeg.data.len() <= base64_bytes {
            return Ok(Some(jpeg));
        }
    }
    Ok(None)
}

/// The image `write` encodes as `format`.
fn encoded(
    picture: &DynamicImage,
    format: ImageFormat,
    write: impl FnOnce(&mut Vec<u8>) -> image::ImageResult<()>,
) -> Result<Image, String> {
    let mut buffer = Vec::new();
    write(&mut buffer).map_err(|error| {
        let (width, height) = picture.dimensions();
        format!("cannot encode the image at {width}x{height} as {format:?}: {error}")
    })?;
    Ok(Image {
        mime_type: format.to_mime_type().to_string(),
        data: STANDARD.encode(buffer),
    })
}

/// Whether an image of `size` has no side longer than `bounds` allow.
fn fits((width, height): (u32, u32), bounds: Bounds) -> bool {
    width <= bounds.side && height <= bounds.side
}

/// `size` scaled down, keeping its proportions, until no side is longer than `side`.
fn scaled_into((width, height): (u32, u32), side: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= side {
        return (width, height);
    }
    let scale = |length: u32| {
        let scaled = (u64::from(length) * u64::from(side) + u64::from(longest) / 2) / u64::from(longest);
        // At most `side`, so it fits in a u32; at least a pixel.
        u32::try_from(scaled).unwrap_or(side).max(1)
    };
    (scale(width), scale(height))
}

/// Three quarters of `length`, down to a pixel.
fn shrunk(length: u32) -> u32 {
    (length / 4 * 3 + length % 4 * 3 / 4).max(1)
}

/// The length of the base64 of `bytes` bytes, with padding.
fn base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3).saturating_mul(4)
}

#[cfg(test)]
mod tests;
