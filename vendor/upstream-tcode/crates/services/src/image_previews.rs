use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, RgbaImage, imageops::FilterType,
    metadata::Orientation,
};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Cursor, Read},
    path::Path,
    sync::Mutex,
    time::UNIX_EPOCH,
};

/// Sources past these bounds get no preview; a client opens the original only when asked to.
const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;
const JPEG_QUALITY: u8 = 80;
const MAX_PREVIEW_SIDE: u32 = 1024;

/// A preview of the source's own shape for a `pixels` square: fitted inside it, or with `cover`
/// reaching both of its edges for the client to clip. The original is the preview when it already
/// has that size and is a still PNG or JPEG.
pub fn read(path: &Path, pixels: u32, cover: bool, data_dir: &Path) -> io::Result<Vec<u8>> {
    let path = path.canonicalize()?;
    let identity = std::fs::metadata(&path)?;
    if identity.len() > MAX_SOURCE_BYTES {
        return Err(io::Error::other("Image is too large to preview"));
    }
    let modified = identity.modified()?;
    let mut hash = Sha256::new();
    hash.update(b"file\0");
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(identity.len().to_le_bytes());
    let (before_epoch, timestamp) = match modified.duration_since(UNIX_EPOCH) {
        Ok(timestamp) => (false, timestamp),
        Err(error) => (true, error.duration()),
    };
    hash.update([u8::from(before_epoch)]);
    hash.update(timestamp.as_nanos().to_le_bytes());
    let key = cache_key(hash, pixels, pixels, cover);
    cached(data_dir, key, || {
        let file = std::fs::File::open(&path)?;
        let mut bytes = Vec::with_capacity(identity.len() as usize);
        file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(io::Error::other("Image is too large to preview"));
        }
        let output = preview(&bytes, pixels, pixels, cover)?;
        let current = std::fs::metadata(&path)?;
        if current.len() != identity.len() || current.modified()? != modified {
            return Err(io::Error::other(
                "Image changed while generating its preview",
            ));
        }
        Ok(output)
    })
}

/// A preview covering a `width`×`height` tile, in the source's own shape.
pub fn read_bytes(bytes: &[u8], width: u32, height: u32, data_dir: &Path) -> io::Result<Vec<u8>> {
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(io::Error::other("Image is too large to preview"));
    }
    let mut hash = Sha256::new();
    hash.update(b"bytes\0");
    hash.update(bytes);
    cached(data_dir, cache_key(hash, width, height, true), || {
        preview(bytes, width, height, true)
    })
}

/// How much of an image [`dimensions`] reads: its header and any orientation tag come first in
/// the still formats a viewer shows.
const HEADER_BYTES: u64 = 256 * 1024;

/// The displayed width and height of the image `source` begins with, orientation applied, read
/// from its header alone.
pub fn dimensions(source: impl Read) -> Option<(u32, u32)> {
    let mut head = Vec::new();
    source.take(HEADER_BYTES).read_to_end(&mut head).ok()?;
    let mut decoder = image::ImageReader::new(Cursor::new(head))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let (width, height) = decoder.dimensions();
    Some(if turns(decoder.orientation().ok()?) {
        (height, width)
    } else {
        (width, height)
    })
}

/// Whether `orientation` swaps the stored width and height on display.
fn turns(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 2048;
static GENERATING: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
static EVICTING: Mutex<()> = Mutex::new(());

fn cache_key(mut hash: Sha256, width: u32, height: u32, cover: bool) -> [u8; 32] {
    hash.update(width.to_le_bytes());
    hash.update(height.to_le_bytes());
    // 1 keyed the cropped previews this cache held before; a cover must never be served one.
    hash.update([if cover { 2 } else { 0 }]);
    hash.finalize().into()
}

fn cached(
    data_dir: &Path,
    key: [u8; 32],
    generate: impl FnOnce() -> io::Result<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    // Concurrent tiles for the same source share generation without serializing every image.
    let _generation = GENERATING[usize::from(key[0]) % GENERATING.len()]
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let directory = data_dir.join("image-previews-v1");
    let name = key
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = directory.join(format!("{name}.preview"));
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    let bytes = generate()?;
    // A read-only or full data directory must not prevent displaying a preview.
    let _ = (|| -> io::Result<()> {
        let _eviction = EVICTING.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(&directory)?;
        let temporary = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let written = (|| {
            std::fs::write(&temporary, &bytes)?;
            std::fs::rename(&temporary, &path)
        })();
        let _ = std::fs::remove_file(&temporary);
        written?;
        let mut entries = Vec::new();
        let mut total = 0;
        for entry in std::fs::read_dir(&directory)?.flatten() {
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            total += metadata.len();
            entries.push((
                metadata.modified().unwrap_or(UNIX_EPOCH),
                metadata.len(),
                entry.path(),
            ));
        }
        entries.sort_unstable();
        let mut count = entries.len();
        for (_, len, path) in entries {
            if total <= MAX_CACHE_BYTES && count <= MAX_CACHE_ENTRIES {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total -= len;
                count -= 1;
            }
        }
        Ok(())
    })();
    Ok(bytes)
}

fn preview(
    bytes: &[u8],
    target_width: u32,
    target_height: u32,
    cover: bool,
) -> io::Result<Vec<u8>> {
    if !(1..=MAX_PREVIEW_SIDE).contains(&target_width)
        || !(1..=MAX_PREVIEW_SIDE).contains(&target_height)
    {
        return Err(io::Error::other(
            "Preview size is outside the supported range",
        ));
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    // Animated sources decode their first frame; the original carries the animation.
    let jpeg = reader.format() == Some(ImageFormat::Jpeg);
    let (mut decoder, still): (Box<dyn ImageDecoder>, bool) = match reader.format() {
        Some(ImageFormat::Png) => {
            let decoder = image::codecs::png::PngDecoder::new(reader.into_inner())
                .map_err(io::Error::other)?;
            let still = !decoder.is_apng().map_err(io::Error::other)?;
            (Box::new(decoder), still)
        }
        Some(ImageFormat::Jpeg) => (
            Box::new(
                image::codecs::jpeg::JpegDecoder::new(reader.into_inner())
                    .map_err(io::Error::other)?,
            ),
            true,
        ),
        _ => (
            Box::new(reader.into_decoder().map_err(io::Error::other)?),
            false,
        ),
    };
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err(io::Error::other("Image is too large to preview"));
    }
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().map_err(io::Error::other)?;
    let turned = turns(orientation);
    let shown = if turned {
        (height, width)
    } else {
        (width, height)
    };
    let (preview_width, preview_height) = preview_size(shown, (target_width, target_height), cover);
    if still
        && orientation == Orientation::NoTransforms
        && (preview_width, preview_height) == (width, height)
    {
        return Ok(bytes.to_vec());
    }
    let needed = if turned {
        (preview_height, preview_width)
    } else {
        (preview_width, preview_height)
    };
    let mut image = if jpeg {
        scaled_jpeg(bytes, (width, height), needed)?
    } else {
        None
    }
    .map_or_else(
        || DynamicImage::from_decoder(decoder).map_err(io::Error::other),
        Ok,
    )?;
    image.apply_orientation(orientation);
    if image.color().has_alpha() {
        let mut rgba = image.into_rgba8();
        premultiply(&mut rgba);
        let mut rgba = scale(&rgba, preview_width, preview_height);
        unpremultiply(&mut rgba);
        if rgba.pixels().any(|pixel| pixel[3] < u8::MAX) {
            return encode_png(rgba);
        }
        encode_jpeg(DynamicImage::ImageRgba8(rgba).into_rgb8())
    } else {
        encode_jpeg(scale(&image.into_rgb8(), preview_width, preview_height))
    }
}

/// The size a `width`×`height` source is previewed at for a target box: inside it, or for a cover
/// reaching both of its edges with neither side past [`MAX_PREVIEW_SIDE`]. Never enlarged.
fn preview_size(
    (width, height): (u32, u32),
    (target_width, target_height): (u32, u32),
    cover: bool,
) -> (u32, u32) {
    let horizontal = f64::from(target_width) / f64::from(width);
    let vertical = f64::from(target_height) / f64::from(height);
    let ratio = if cover {
        horizontal
            .max(vertical)
            .min(f64::from(MAX_PREVIEW_SIDE) / f64::from(width.max(height)))
    } else {
        horizontal.min(vertical)
    }
    .min(1.);
    let side = |source: u32| ((f64::from(source) * ratio).round() as u32).clamp(1, source);
    (side(width), side(height))
}

/// Decodes a JPEG at the smallest DCT scale still at least `needed`, when one is smaller than the
/// source.
fn scaled_jpeg(
    bytes: &[u8],
    (width, height): (u32, u32),
    needed: (u32, u32),
) -> io::Result<Option<DynamicImage>> {
    let Some(factor) = [8, 4, 2]
        .into_iter()
        .find(|factor| width.div_ceil(*factor) >= needed.0 && height.div_ceil(*factor) >= needed.1)
    else {
        return Ok(None);
    };
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    decoder.set_max_decoding_buffer_size(MAX_DECODED_BYTES as usize);
    decoder.read_info().map_err(io::Error::other)?;
    let format = decoder
        .info()
        .ok_or_else(|| io::Error::other("Missing JPEG metadata"))?
        .pixel_format;
    if !matches!(
        format,
        jpeg_decoder::PixelFormat::L8 | jpeg_decoder::PixelFormat::RGB24
    ) {
        return Ok(None);
    }
    // scale() accepts either axis; both requests must exclude the next smaller DCT size.
    let request = if factor == 8 {
        (width.div_ceil(8), height.div_ceil(8))
    } else {
        (
            width.div_ceil(factor * 2) + 1,
            height.div_ceil(factor * 2) + 1,
        )
    };
    let (width, height) = decoder
        .scale(request.0 as u16, request.1 as u16)
        .map_err(io::Error::other)?;
    let pixels = decoder.decode().map_err(io::Error::other)?;
    let image = match format {
        jpeg_decoder::PixelFormat::L8 => {
            image::GrayImage::from_raw(width.into(), height.into(), pixels)
                .map(DynamicImage::ImageLuma8)
        }
        _ => image::RgbImage::from_raw(width.into(), height.into(), pixels)
            .map(DynamicImage::ImageRgb8),
    };
    image
        .map(Some)
        .ok_or_else(|| io::Error::other("Invalid scaled JPEG dimensions"))
}

/// Scales `image` to `width`×`height`. A box filter first brings a large source near twice that
/// size, so the Lanczos pass and its float rows stay small.
fn scale<P: image::Pixel<Subpixel = u8> + 'static>(
    image: &impl GenericImageView<Pixel = P>,
    width: u32,
    height: u32,
) -> image::ImageBuffer<P, Vec<u8>> {
    let (source_width, source_height) = image.dimensions();
    let (box_width, box_height) = (width.saturating_mul(2), height.saturating_mul(2));
    if box_width >= source_width || box_height >= source_height {
        return image::imageops::resize(image, width, height, FilterType::Lanczos3);
    }
    let reduced = image::imageops::thumbnail(image, box_width, box_height);
    image::imageops::resize(&reduced, width, height, FilterType::Lanczos3)
}

/// Averaging premultiplied colours keeps transparent pixels from darkening the edges they border.
fn premultiply(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

fn unpremultiply(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = match alpha {
                0 => 0,
                alpha => ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8,
            };
        }
    }
}

fn encode_png(image: RgbaImage) -> io::Result<Vec<u8>> {
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .map_err(io::Error::other)?;
    Ok(png.into_inner())
}

fn encode_jpeg(image: image::RgbImage) -> io::Result<Vec<u8>> {
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&image)
        .map_err(io::Error::other)?;
    Ok(jpeg)
}

pub(crate) fn resize_to_png(image: DynamicImage, pixels: u32) -> io::Result<Vec<u8>> {
    let mut rgba = image.into_rgba8();
    premultiply(&mut rgba);
    let (width, height) = preview_size(rgba.dimensions(), (pixels, pixels), false);
    let mut rgba = scale(&rgba, width, height);
    unpremultiply(&mut rgba);
    encode_png(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(image: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.into().write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn attachment_preview(bytes: &[u8], pixels: u32, cover: bool) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!("tcode-preview-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        let cache = path.with_extension("cache");
        let preview = read(&path, pixels, cover, &cache);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir_all(cache);
        preview.unwrap()
    }

    #[test]
    fn unchanged_sources_reuse_previews_and_changed_sources_regenerate() {
        let root =
            std::env::temp_dir().join(format!("tcode-preview-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source.png");
        let source = encoded(
            image::RgbImage::from_pixel(800, 600, image::Rgb([255, 0, 0])),
            ImageFormat::Png,
        );
        std::fs::write(&path, &source).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let first = read(&path, 120, false, &root).unwrap();
        // Keeping the source identity while making decoding fail proves a cache hit skips decoding.
        std::fs::write(&path, vec![0; source.len()]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(read(&path, 120, false, &root).unwrap(), first);
        let changed = encoded(
            image::RgbImage::from_pixel(800, 600, image::Rgb([0, 0, 255])),
            ImageFormat::Png,
        );
        std::fs::write(&path, changed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(modified + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        let next = read(&path, 120, false, &root).unwrap();
        assert_ne!(next, first);
        assert_eq!(
            image::load_from_memory(&next).unwrap().dimensions(),
            (120, 90)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Camera-resolution sources are previewed rather than handed to the client in full.
    #[test]
    fn sources_over_sixteen_megapixels_get_a_preview() {
        let source = encoded(image::GrayImage::new(4100, 4100), ImageFormat::Png);
        let preview = attachment_preview(&source, 360, true);
        let decoded = image::load_from_memory(&preview).unwrap();
        assert_eq!(decoded.dimensions(), (360, 360));
    }

    /// A cover keeps the source's shape: the tile clips it, and the viewer it opens shows the
    /// whole image while the original loads.
    #[test]
    fn covers_keep_the_source_shape() {
        let size = |preview: Vec<u8>| image::load_from_memory(&preview).unwrap().dimensions();
        let wide = encoded(image::RgbImage::new(1600, 1000), ImageFormat::Png);
        assert_eq!(size(attachment_preview(&wide, 240, true)), (384, 240));
        let cache = std::env::temp_dir().join(format!("tcode-preview-{}", uuid::Uuid::new_v4()));
        let tile = read_bytes(&wide, 240, 160, &cache);
        let _ = std::fs::remove_dir_all(&cache);
        assert_eq!(size(tile.unwrap()), (256, 160));
        // An extreme shape stops at the longest side a preview may have.
        let banner = encoded(image::RgbImage::new(8000, 200), ImageFormat::Png);
        assert_eq!(size(attachment_preview(&banner, 240, true)), (1024, 26));
    }

    /// A viewer learns an original's size from the first bytes of a source too large to buffer.
    #[test]
    fn dimensions_come_from_the_header() {
        let noise = image::RgbImage::from_fn(1200, 900, |x, y| {
            let value = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503);
            image::Rgb([value as u8, (value >> 8) as u8, (value >> 16) as u8])
        });
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            let source = encoded(noise.clone(), format);
            assert!(source.len() as u64 > HEADER_BYTES, "{format:?}");
            assert_eq!(dimensions(&source[..]), Some((1200, 900)), "{format:?}");
        }
        assert_eq!(dimensions(&b"not an image"[..]), None);
    }

    /// An opaque photo crosses the link as a JPEG, smaller than the PNG previews it replaces.
    #[test]
    fn opaque_sources_are_previewed_as_jpeg() {
        let photo = image::RgbImage::from_fn(1600, 1000, |x, y| {
            let noise = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) % 32;
            image::Rgb([
                (x * 200 / 1600 + noise) as u8,
                (y * 200 / 1000 + noise) as u8,
                (120 + noise) as u8,
            ])
        });
        let preview = attachment_preview(&encoded(photo, ImageFormat::Jpeg), 512, false);
        assert_eq!(image::guess_format(&preview).unwrap(), ImageFormat::Jpeg);
        let decoded = image::load_from_memory(&preview).unwrap();
        assert_eq!(decoded.dimensions(), (512, 320));
        assert!(preview.len() < encoded(decoded.into_rgba8(), ImageFormat::Png).len());
    }

    /// Transparency survives the preview, and an original that already fits is the preview.
    #[test]
    fn transparent_sources_stay_png() {
        let source = encoded(
            image::RgbaImage::from_fn(1600, 800, |x, _| {
                image::Rgba([255, 0, 0, if x < 800 { 255 } else { 0 }])
            }),
            ImageFormat::Png,
        );
        let preview = attachment_preview(&source, 240, false);
        assert_eq!(image::guess_format(&preview).unwrap(), ImageFormat::Png);
        let decoded = image::load_from_memory(&preview).unwrap().into_rgba8();
        assert_eq!(decoded.dimensions(), (240, 120));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(239, 0)[3], 0);
        let small = encoded(image::RgbaImage::new(200, 100), ImageFormat::Png);
        assert_eq!(attachment_preview(&small, 240, false), small);
    }
}
