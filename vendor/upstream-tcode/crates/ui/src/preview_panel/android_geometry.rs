//! Android child coordinates are physical window pixels. The shell's canvas
//! bounds already include its safe-area padding; callers convert them to
//! content-relative bounds before applying this mapping.
use gpui::{Bounds, Pixels, Point};

pub(super) fn physical_bounds(
    content_bounds: Bounds<Pixels>,
    safe_origin: Point<Pixels>,
    scale: f32,
) -> [i32; 4] {
    let origin = content_bounds.origin + safe_origin;
    let left = (f32::from(origin.x) * scale).round() as i32;
    let top = (f32::from(origin.y) * scale).round() as i32;
    let right = (f32::from(origin.x + content_bounds.size.width) * scale).round() as i32;
    let bottom = (f32::from(origin.y + content_bounds.size.height) * scale).round() as i32;
    [left, top, (right - left).max(0), (bottom - top).max(0)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn physical_child_tracks_safe_content_without_double_inset() {
        for (origin, extent, safe, scale, expected) in [
            // Portrait status bar, then landscape cutout; neither is applied twice.
            (
                (8., 100.),
                (384., 500.),
                (0., 24.),
                3.,
                [24, 372, 1152, 1500],
            ),
            (
                (8., 100.),
                (700., 240.),
                (44., 0.),
                2.,
                [104, 200, 1400, 480],
            ),
            // Round edges independently; a collapsed child still has no area.
            ((1., 1.), (1., 0.), (0., 0.), 1.5, [2, 2, 1, 0]),
        ] {
            let bounds = Bounds {
                origin: point(px(origin.0), px(origin.1)),
                size: size(px(extent.0), px(extent.1)),
            };
            assert_eq!(
                physical_bounds(bounds, point(px(safe.0), px(safe.1)), scale),
                expected
            );
        }
    }
}
