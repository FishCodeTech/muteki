use super::super::{BackendError, BackendErrorCode, RootInfo};
use image::GenericImageView;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LONG_EDGE: u32 = 1_568;
const MAX_PIXEL_AREA: u64 = 629_145;
const JPEG_QUALITY: u8 = 80;

pub(super) fn capture_window(root: &RootInfo) -> Result<Vec<u8>, BackendError> {
    let path = std::env::temp_dir().join(format!(
        "tcode-computer-use-{}-{}-{}.png",
        root.window_id,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let result = tcode_services::process::command("screencapture")
        .arg("-x")
        .arg("-l")
        .arg(root.window_id.to_string())
        .arg("-t")
        .arg("png")
        .arg(&path)
        .output()
        .map_err(|error| {
            BackendError::new(
                BackendErrorCode::CaptureFailed,
                format!("failed to spawn screencapture: {error}"),
            )
        })
        .and_then(|output| {
            if output.status.success() {
                let png = std::fs::read(&path).map_err(|error| {
                    BackendError::new(
                        BackendErrorCode::CaptureFailed,
                        format!("failed to read captured PNG: {error}"),
                    )
                })?;
                png_to_scaled_jpeg(&png)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                let detail = if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                };
                Err(BackendError::new(
                    BackendErrorCode::CaptureFailed,
                    format!(
                        "screencapture exited with {}{detail}; when Screen Recording is granted in System Settings yet capture is refused, the grant is not in effect for this Tcode process, which happens after Tcode's executable changed on disk, so restart Tcode",
                        output.status
                    ),
                ))
            }
        });
    let _ = std::fs::remove_file(path);
    result
}

fn png_to_scaled_jpeg(png: &[u8]) -> Result<Vec<u8>, BackendError> {
    let image =
        image::load_from_memory_with_format(png, image::ImageFormat::Png).map_err(|error| {
            BackendError::new(
                BackendErrorCode::CaptureFailed,
                format!("failed to decode captured PNG: {error}"),
            )
        })?;
    let (width, height) = image.dimensions();
    let (scaled_width, scaled_height) = scaled_dimensions(width, height);
    let image = if (scaled_width, scaled_height) == (width, height) {
        image
    } else {
        image.resize_exact(
            scaled_width,
            scaled_height,
            image::imageops::FilterType::Lanczos3,
        )
    };
    let rgb = image.to_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|error| {
            BackendError::new(
                BackendErrorCode::CaptureFailed,
                format!("failed to encode captured JPEG: {error}"),
            )
        })?;
    Ok(jpeg)
}

fn scaled_dimensions(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let long_edge_scale = f64::from(MAX_LONG_EDGE) / f64::from(width.max(height));
    let area = u64::from(width) * u64::from(height);
    let area_scale = (MAX_PIXEL_AREA as f64 / area as f64).sqrt();
    let scale = 1.0_f64.min(long_edge_scale).min(area_scale);
    let scaled_width = (f64::from(width) * scale).floor().max(1.0) as u32;
    let scaled_height = (f64::from(height) * scale).floor().max(1.0) as u32;
    (scaled_width, scaled_height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_scaling_respects_edge_area_and_aspect_ratio() {
        for (width, height) in [(320, 240), (4_000, 1_000), (1_920, 1_080), (900, 3_000)] {
            let (scaled_width, scaled_height) = scaled_dimensions(width, height);
            assert!(scaled_width <= width && scaled_height <= height);
            assert!(scaled_width.max(scaled_height) <= MAX_LONG_EDGE);
            assert!(u64::from(scaled_width) * u64::from(scaled_height) <= MAX_PIXEL_AREA);
            let original_ratio = f64::from(width) / f64::from(height);
            let scaled_ratio = f64::from(scaled_width) / f64::from(scaled_height);
            let rounding_tolerance = 2.0 / f64::from(scaled_width.min(scaled_height));
            assert!((original_ratio - scaled_ratio).abs() <= rounding_tolerance);
        }
        assert_eq!(scaled_dimensions(320, 240), (320, 240));
    }
}
