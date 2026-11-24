//! Project defaults and bounded image decoding for the host-owned icon picker.
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use std::{
    fs,
    io::{self, Cursor},
    path::Path,
};
use tcode_core::project::Project;
use tcode_protocol::{IconImageEntry, QueryResponse};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const ICON_SIZE: u32 = 128;

fn supported(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico"
            )
        })
}

/// List one directory, including folders for navigation and supported images only.
pub fn browse(directory: &Path) -> io::Result<QueryResponse> {
    let directory = directory.canonicalize()?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        let is_dir = metadata.is_dir();
        if !is_dir && (!metadata.is_file() || !supported(&entry.path())) {
            continue;
        }
        entries.push(IconImageEntry {
            path: entry.path(),
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir,
        });
    }
    entries.sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
    Ok(QueryResponse::IconImages {
        parent: directory.parent().map(Path::to_path_buf),
        directory,
        entries,
    })
}

fn decode(bytes: &[u8], max_dimension: u32) -> io::Result<image::DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder().map_err(io::Error::other)?;
    let (width, height) = decoder.dimensions();
    // Bound the source before decoding or converting, including grayscale input.
    if u64::from(width) * u64::from(height) > 4 * 1024 * 1024 {
        return Err(io::Error::other("image exceeds the 4 megapixel size limit"));
    }
    limits
        .reserve(decoder.total_bytes())
        .map_err(io::Error::other)?;
    decoder.set_limits(limits).map_err(io::Error::other)?;
    image::DynamicImage::from_decoder(decoder).map_err(io::Error::other)
}

/// Decode away from the host event loop and send only a small, static PNG to clients.
pub fn thumbnail(path: &Path) -> io::Result<Vec<u8>> {
    raster(path, ICON_SIZE)
}

fn raster(path: &Path, pixels: u32) -> io::Result<Vec<u8>> {
    if !(1..=ICON_SIZE).contains(&pixels) {
        return Err(io::Error::other(
            "icon size must be between 1 and 128 pixels",
        ));
    }
    let image = decode(&crate::user_files::read_bounded(path, MAX_BYTES)?, 8192)?;
    crate::image_previews::resize_to_png(image, pixels)
}

pub fn read_project_icon(project: &Project, pixels: u32) -> io::Result<Vec<u8>> {
    let path = match &project.icon_path {
        Some(path) => path.clone(),
        None => crate::project_config::read(&project.root)?
            .icon_path
            .filter(|path| !path.trim().is_empty())
            .map(|path| project.root.join(path))
            .ok_or_else(|| io::Error::other("project has no iconPath"))?,
    };
    raster(&path, pixels)
}

/// Accept only the small PNG produced by the picker, including on remote writes.
pub fn save_override(path: &Path, png: &[u8]) -> io::Result<()> {
    if png.len() > 128 * 1024 || image::guess_format(png).ok() != Some(ImageFormat::Png) {
        return Err(io::Error::other(
            "project icon must be a PNG of at most 128 KiB",
        ));
    }
    decode(png, ICON_SIZE)?;
    fs::create_dir_all(path.parent().expect("managed icon has a parent"))?;
    fs::write(path, png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_preserves_color_at_transparent_edges() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("edge.png");
        // The edge crosses a thumbnail pixel; transparent black must not darken red.
        image::RgbaImage::from_fn(512, 256, |x, _| {
            if x < 242 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        })
        .save(&path)
        .unwrap();
        let mut project = Project::from_root(root.clone());
        project.icon_path = Some(path.clone());
        for size in [12, 14, 16, 20, 32, 128] {
            let png = if size == 128 {
                thumbnail(&path).unwrap()
            } else {
                read_project_icon(&project, size).unwrap()
            };
            let resized = image::load_from_memory(&png).unwrap().into_rgba8();
            assert_eq!(resized.dimensions(), (size, size / 2));
            let edges: Vec<_> = resized
                .pixels()
                .filter(|pixel| pixel[3] > 0 && pixel[3] < 255)
                .collect();
            assert!(!edges.is_empty(), "edge must be antialiased at {size}px");
            assert!(
                edges
                    .iter()
                    .all(|pixel| pixel[0] >= 254 && pixel[1] == 0 && pixel[2] == 0)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_icons_resolve_host_paths_and_manual_choices_override_config() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("assets")).unwrap();
        for (name, color) in [("red", [255, 0, 0, 255]), ("blue", [0, 0, 255, 255])] {
            image::RgbaImage::from_pixel(16, 16, image::Rgba(color))
                .save(root.join("assets").join(format!("{name}.png")))
                .unwrap();
        }
        let mut project = Project::from_root(root.clone());
        for path in [
            Path::new("assets/red.png").to_path_buf(),
            root.join("assets/red.png"),
        ] {
            fs::write(
                root.join("tcode.json"),
                serde_json::json!({"iconPath": path}).to_string(),
            )
            .unwrap();
            let png = read_project_icon(&project, 16).unwrap();
            assert_eq!(
                image::load_from_memory(&png)
                    .unwrap()
                    .into_rgba8()
                    .get_pixel(0, 0)
                    .0,
                [255, 0, 0, 255]
            );
        }
        project.icon_path = Some(root.join("assets/blue.png"));
        fs::write(root.join("tcode.json"), "broken json").unwrap();
        let png = read_project_icon(&project, 16).unwrap();
        assert_eq!(
            image::load_from_memory(&png)
                .unwrap()
                .into_rgba8()
                .get_pixel(0, 0)
                .0,
            [0, 0, 255, 255]
        );
        fs::remove_file(project.icon_path.as_ref().unwrap()).unwrap();
        assert!(read_project_icon(&project, 16).is_err());
        project.icon_path = None;
        for config in [
            "{}",
            r#"{"iconPath": "  "}"#,
            r#"{"iconPath": "missing.png"}"#,
            "broken json",
        ] {
            fs::write(root.join("tcode.json"), config).unwrap();
            assert!(read_project_icon(&project, 16).is_err(), "{config}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn picker_returns_host_paths_and_sorts_folders_before_supported_images() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("Z-folder")).unwrap();
        for name in ["a.PNG", "B.jpg", "notes.txt"] {
            fs::write(root.join(name), []).unwrap();
        }
        let QueryResponse::IconImages {
            directory,
            parent,
            entries,
        } = browse(&root).unwrap()
        else {
            panic!()
        };
        assert_eq!(parent, directory.parent().map(Path::to_path_buf));
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.is_dir))
                .collect::<Vec<_>>(),
            [("Z-folder", true), ("a.PNG", false), ("B.jpg", false)]
        );
        for entry in entries {
            assert_eq!(entry.path, directory.join(&entry.name));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn images_over_resource_limits_are_rejected_before_conversion_or_save() {
        let root = std::env::temp_dir().join(format!("tcode-icons-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("large.png");
        // Tiny compressed grayscale input used to expand into an unbounded RGBA32F buffer.
        image::GrayImage::new(8192, 513).save(&path).unwrap();
        assert!(
            thumbnail(&path)
                .unwrap_err()
                .to_string()
                .contains("megapixel")
        );
        image::GrayImage::new(8193, 1).save(&path).unwrap();
        assert!(thumbnail(&path).is_err());
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(
            thumbnail(&path)
                .unwrap_err()
                .to_string()
                .contains("byte size limit")
        );
        let destination = root.join("managed/icon.png");
        let mut png = Cursor::new(Vec::new());
        image::RgbaImage::new(129, 1)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        for invalid in [
            b"not an image".to_vec(),
            vec![0; 128 * 1024 + 1],
            png.into_inner(),
        ] {
            assert!(save_override(&destination, &invalid).is_err());
            assert!(!destination.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
