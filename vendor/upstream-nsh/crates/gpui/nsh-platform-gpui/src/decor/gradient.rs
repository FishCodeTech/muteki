//! Gradient backgrounds: CSS's linear, radial and conic gradients (and
//! their repeating forms).
//!
//! GPUI's own gradients have two stops and are linear only, so a gradient
//! is drawn into an image once (on the CPU, interpolating premultiplied
//! colors as CSS does) and painted with `paint_image`, which rounds its
//! corners like a quad. Images are kept by what they look like: redrawing
//! a view, or another with the same gradient and size, makes nothing new.
//! The pass's opacity and color filters are in the image's colors (GPUI's
//! image opacity is not public), so a gradient whose opacity animates is
//! drawn again each frame; images are at most [`MAX_SIDE`] pixels a side,
//! stretched beyond (a gradient has no detail finer than its stops).
//!
//! A rotated, skewed or projected view cannot paint an image in its plane:
//! its gradient is filled as bands of solid color (strips across a linear
//! gradient, rings of a radial one, wedges of a conic one), cut to the
//! rounded box.

use std::{cell::RefCell, hash::Hasher, sync::Arc};

use gpui::RenderImage;
use nsh_core::{
    Color, Gradient, GradientKind, GradientSize,
    geometry::{Points, Radians, Size, deg},
};
use rustc_hash::{FxHashMap, FxHasher};

use super::shape::{self, Pt, circular, clip_convex};
use crate::{
    geom::rect,
    render::{DrawCx, corners},
};

/// Most pixels along a side of a gradient's image.
const MAX_SIDE: f32 = 768.0;

/// How many images are kept before those not drawn lately are let go.
const MAX_KEPT: usize = 96;

/// How many drawing passes an image stays after it was last drawn. An
/// image drawn in the current pass (or one GPUI may still show from a
/// recent frame) must stay in the atlas: freeing it would let an image
/// made later reuse its tile, which the earlier sprite then shows.
const KEEP_PASSES: u64 = 120;

/// Gradient images by content, with the drawing pass they were last drawn
/// in.
#[derive(Default)]
struct Kept {
    pass: u64,
    images: FxHashMap<u64, (Arc<RenderImage>, u64)>,
}

impl Kept {
    /// The image kept under `key`, marked drawn in this pass.
    fn get(&mut self, key: u64) -> Option<Arc<RenderImage>> {
        let pass = self.pass;
        self.images.get_mut(&key).map(|(image, used)| {
            *used = pass;
            image.clone()
        })
    }

    /// Keeps `image` under `key`; returns the images let go to make room
    /// (none drawn in the last [`KEEP_PASSES`] passes: over the limit
    /// while all are recent, more are kept).
    fn insert(&mut self, key: u64, image: Arc<RenderImage>) -> Vec<Arc<RenderImage>> {
        let mut dropped = Vec::new();
        if self.images.len() >= MAX_KEPT {
            let pass = self.pass;
            self.images.retain(|_, (image, used)| {
                let keep = *used + KEEP_PASSES >= pass;
                if !keep {
                    dropped.push(image.clone());
                }
                keep
            });
        }
        self.images.insert(key, (image, self.pass));
        dropped
    }
}

thread_local! {
    static KEPT: RefCell<Kept> = RefCell::new(Kept::default());
}

/// Where in a gradient each point of the box is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Geometry {
    /// Along the line from `a` (0) to `b` (1).
    Linear { a: Pt, b: Pt },
    /// Out from `c` to the ellipse with radii `r` (1).
    Radial { c: Pt, r: Pt },
    /// Around `c`, a turn clockwise from `from` degrees.
    Conic { c: Pt, from: f32 },
}

impl Geometry {
    fn of(kind: GradientKind, w: f32, h: f32) -> Geometry {
        match kind {
            GradientKind::Radial { circle, size, x, y } => {
                let c = (x.0 * w, y.0 * h);
                let (rx, ry) = radial_radii(circle, size, c.0, c.1, w, h);
                Geometry::Radial {
                    c,
                    r: (rx.max(0.01), ry.max(0.01)),
                }
            }
            GradientKind::Conic { from, x, y } => Geometry::Conic {
                c: (x.0 * w, y.0 * h),
                from: from.to_degrees(),
            },
            kind => {
                let angle = kind
                    .linear_angle(Size::new(Points(w), Points(h)))
                    .unwrap_or(deg(180.0));
                let (a, b) = linear_line(angle, w, h);
                Geometry::Linear { a, b }
            }
        }
    }

    /// Where `p` is in the gradient (stop positions' units).
    fn t(&self, p: Pt) -> f32 {
        match *self {
            Geometry::Linear { a, b } => {
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let len2 = dx * dx + dy * dy;
                if len2 <= 0.0 {
                    return 0.0;
                }
                ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2
            }
            Geometry::Radial { c, r } => ((p.0 - c.0) / r.0).hypot((p.1 - c.1) / r.1),
            Geometry::Conic { c, from } => conic_t(p.0 - c.0, p.1 - c.1, from),
        }
    }
}

/// A gradient's colors: premultiplied stops, sorted.
struct Ramp {
    stops: Vec<([f32; 4], f32)>,
    repeating: bool,
}

impl Ramp {
    /// The ramp of `g` over a `w` x `h` box, its colors passed through
    /// `color` (the pass's filters and opacity, as straight RGBA).
    fn new(g: &Gradient, w: f32, h: f32, color: impl Fn(Color) -> [f32; 4]) -> Ramp {
        let resolved = g.resolved_stops(Size::new(Points(w), Points(h)));
        let stops: Vec<([f32; 4], f32)> = resolved
            .iter()
            .map(|&(c, p)| {
                let [r, g, b, a] = color(c);
                ([r * a, g * a, b * a, a], p.0)
            })
            .collect();
        let period = match (stops.first(), stops.last()) {
            (Some(f), Some(l)) => l.1 - f.1,
            _ => 0.0,
        };
        Ramp {
            stops,
            repeating: g.repeating && period > 1e-4,
        }
    }

    /// The premultiplied color at `t`.
    fn at(&self, t: f32) -> [f32; 4] {
        let stops = &self.stops;
        let Some(&(first, p0)) = stops.first() else {
            return [0.0; 4];
        };
        let t = if self.repeating {
            let last = stops[stops.len() - 1].1;
            p0 + (t - p0).rem_euclid(last - p0)
        } else {
            t
        };
        if t <= p0 {
            return first;
        }
        for pair in stops.windows(2) {
            let ((a, pa), (b, pb)) = (pair[0], pair[1]);
            if t <= pb {
                let k = if pb - pa > 1e-6 {
                    (t - pa) / (pb - pa)
                } else {
                    1.0
                };
                return std::array::from_fn(|i| a[i] + (b[i] - a[i]) * k);
            }
        }
        stops[stops.len() - 1].0
    }
}

/// A premultiplied color as straight BGRA bytes (GPUI's images).
fn bgra(c: [f32; 4]) -> [u8; 4] {
    let a = c[3].clamp(0.0, 1.0);
    let ch = |v: f32| {
        let straight = if a > 0.0 { v / a } else { 0.0 };
        (straight * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [ch(c[2]), ch(c[1]), ch(c[0]), (a * 255.0).round() as u8]
}

/// A premultiplied color as nsh's (straight RGBA).
fn straight(c: [f32; 4]) -> Color {
    let [b, g, r, a] = bgra(c);
    Color(u32::from_be_bytes([r, g, b, a]))
}

/// A drawing pass starts (images drawn in recent passes are kept).
pub(crate) fn new_pass() {
    KEPT.with_borrow_mut(|kept| kept.pass += 1);
}

/// Draws gradient `g` over the view's `w` x `h` box with corner `radii`
/// (current coordinates).
pub(crate) fn draw(cx: &mut DrawCx<'_>, g: &Gradient, w: f32, h: f32, radii: [f32; 4]) {
    if g.stops.len() < 2 || w <= 0.0 || h <= 0.0 || cx.culled() {
        return;
    }
    let color = |c: Color| {
        let rgba = gpui::Rgba::from(cx.color(c));
        [rgba.r, rgba.g, rgba.b, rgba.a]
    };
    let ramp = Ramp::new(g, w, h, color);
    let geometry = Geometry::of(g.kind, w, h);
    if cx.axis_aligned() {
        draw_image(cx, &ramp, geometry, w, h, radii);
    } else {
        draw_bands(cx, &ramp, geometry, w, h, radii);
    }
}

/// Paints the gradient as an image (axis-aligned views).
fn draw_image(
    cx: &mut DrawCx<'_>,
    ramp: &Ramp,
    geometry: Geometry,
    w: f32,
    h: f32,
    radii: [f32; 4],
) {
    let b = cx.rect(0.0, 0.0, w, h);
    let size = (
        (f32::from(b.size.width) * cx.scale)
            .clamp(1.0, MAX_SIDE)
            .ceil() as u32,
        (f32::from(b.size.height) * cx.scale)
            .clamp(1.0, MAX_SIDE)
            .ceil() as u32,
    );
    let key = image_key(ramp, geometry, w, h, size);
    let image = KEPT.with_borrow_mut(|kept| {
        if let Some(image) = kept.get(key) {
            return Some((image, Vec::new()));
        }
        let image = Arc::new(rasterize(ramp, geometry, w, h, size)?);
        let dropped = kept.insert(key, image.clone());
        Some((image, dropped))
    });
    let Some((image, dropped)) = image else {
        return;
    };
    for old in dropped {
        cx.app.drop_image(old, Some(&mut *cx.window));
    }
    let k = cx.transform_scale();
    let (visible, radii) = cx.clip_rounded(b, corners(radii, k));
    cx.paint(|window, _| {
        if let Err(err) = window.paint_image(visible, b, radii, image, 0, false) {
            log::debug!("decor: gradient not drawn: {err}");
        }
    });
}

/// The key of a gradient image: everything its pixels depend on.
fn image_key(ramp: &Ramp, geometry: Geometry, w: f32, h: f32, size: (u32, u32)) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write_u32(size.0);
    hasher.write_u32(size.1);
    let floats: [f32; 6] = match geometry {
        Geometry::Linear { a, b } => [0.0, a.0, a.1, b.0, b.1, 0.0],
        Geometry::Radial { c, r } => [1.0, c.0, c.1, r.0, r.1, 0.0],
        Geometry::Conic { c, from } => [2.0, c.0, c.1, from, 0.0, 0.0],
    };
    for v in floats.into_iter().chain([w, h]) {
        hasher.write_u32(v.to_bits());
    }
    hasher.write_u8(ramp.repeating as u8);
    for (c, p) in &ramp.stops {
        for v in c.iter().chain([p]) {
            hasher.write_u32(v.to_bits());
        }
    }
    hasher.finish()
}

/// The gradient over a `w` x `h` box drawn into an image of `size` pixels.
fn rasterize(
    ramp: &Ramp,
    geometry: Geometry,
    w: f32,
    h: f32,
    size: (u32, u32),
) -> Option<RenderImage> {
    let (pw, ph) = (size.0 as usize, size.1 as usize);
    let mut buf = vec![0u8; pw * ph * 4];
    let point = |col: usize, row: usize| {
        (
            (col as f32 + 0.5) * w / pw as f32,
            (row as f32 + 0.5) * h / ph as f32,
        )
    };
    let fill_row = |row: usize, buf: &mut [u8]| {
        for col in 0..pw {
            let px = bgra(ramp.at(geometry.t(point(col, row))));
            buf[(row * pw + col) * 4..][..4].copy_from_slice(&px);
        }
    };
    match geometry {
        // Rows of a gradient running across the box are all the same.
        Geometry::Linear { a, b } if (a.1 - b.1).abs() < 1e-4 => {
            fill_row(0, &mut buf);
            let (first, rest) = buf.split_at_mut(pw * 4);
            for row in rest.chunks_exact_mut(pw * 4) {
                row.copy_from_slice(first);
            }
        }
        // As are the pixels of each row of one running down it.
        Geometry::Linear { a, b } if (a.0 - b.0).abs() < 1e-4 => {
            for row in 0..ph {
                let px = bgra(ramp.at(geometry.t(point(0, row))));
                for col in 0..pw {
                    buf[(row * pw + col) * 4..][..4].copy_from_slice(&px);
                }
            }
        }
        _ => {
            for row in 0..ph {
                fill_row(row, &mut buf);
            }
        }
    }
    let image = image::RgbaImage::from_raw(size.0, size.1, buf)?;
    Some(RenderImage::new(vec![image::Frame::new(image)]))
}

/// Fills the gradient as bands of solid color cut to the rounded box
/// (views that are not axis-aligned).
fn draw_bands(
    cx: &mut DrawCx<'_>,
    ramp: &Ramp,
    geometry: Geometry,
    w: f32,
    h: f32,
    radii: [f32; 4],
) {
    let px = cx.transform_scale() * cx.scale;
    let shape = shape::rounded(rect(0.0, 0.0, w, h), circular(radii), px);
    let corners = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)];
    let ts = corners.map(|p| geometry.t(p));
    let (lo, hi) = ts
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &t| (lo.min(t), hi.max(t)));
    let diagonal = w.hypot(h);
    match geometry {
        Geometry::Linear { a, b } => {
            let n = ((diagonal * px / 3.0) as usize).clamp(8, 128);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            // A long side across the line, and the line's point at `t`.
            let len = dx.hypot(dy).max(1e-6);
            let across = (-dy / len * diagonal * 2.0, dx / len * diagonal * 2.0);
            let at = |t: f32| (a.0 + dx * t, a.1 + dy * t);
            let overlap = 1.0 / (len * px);
            for i in 0..n {
                let (t0, t1) = (
                    lo + (hi - lo) * i as f32 / n as f32,
                    lo + (hi - lo) * (i + 1) as f32 / n as f32,
                );
                // Each band reaches a pixel into the next, which covers the
                // seam antialiasing would leave between them.
                let (p0, p1) = (at(t0), at(t1 + overlap));
                let band = [
                    (p0.0 - across.0, p0.1 - across.1),
                    (p1.0 - across.0, p1.1 - across.1),
                    (p1.0 + across.0, p1.1 + across.1),
                    (p0.0 + across.0, p0.1 + across.1),
                ];
                let part = clip_convex(&shape, &band);
                cx.fill_contours(&[part], straight(ramp.at((t0 + t1) / 2.0)));
            }
        }
        Geometry::Radial { c, r } => {
            let n = ((hi * r.0.max(r.1) * px / 3.0) as usize).clamp(8, 96);
            let ellipse = |t: f32| -> Vec<Pt> {
                let k = shape::circle((0.0, 0.0), 1.0, (t * r.0.max(r.1) * px).max(1.0));
                k.into_iter()
                    .map(|(x, y)| (c.0 + x * r.0 * t, c.1 + y * r.1 * t))
                    .collect()
            };
            let overlap = 1.0 / (r.0.max(r.1) * px);
            let mut inner: Vec<Pt> = Vec::new();
            for i in 0..n {
                let (t0, t1) = (hi * i as f32 / n as f32, hi * (i + 1) as f32 / n as f32);
                // Each ring reaches a pixel into the next (covering the seam
                // between them); the last one past the corners.
                let reach = if i + 1 == n { t1 * 1.05 } else { t1 + overlap };
                let mut ring = vec![clip_convex(&shape, &ellipse(reach))];
                if !inner.is_empty() {
                    ring.push(clip_convex(&shape, &inner));
                }
                cx.fill_contours(&ring, straight(ramp.at((t0 + t1) / 2.0)));
                inner = ellipse(t1);
            }
        }
        Geometry::Conic { c, from } => {
            let n = 90;
            let reach = diagonal * 2.0;
            for i in 0..n {
                let (t0, t1) = (i as f32 / n as f32, (i + 1) as f32 / n as f32);
                // `t` turns clockwise from `from`, 0 pointing up.
                let dir = |t: f32| {
                    let a = (from + t * 360.0).to_radians();
                    (c.0 + a.sin() * reach, c.1 - a.cos() * reach)
                };
                // A little past the next wedge, over the seam between them.
                let wedge = [c, dir(t0), dir(t1 + 0.002)];
                let part = clip_convex(&shape, &wedge);
                cx.fill_contours(&[part], straight(ramp.at((t0 + t1) / 2.0)));
            }
        }
    }
}

/// Where a conic gradient is at offset `(dx, dy)` from its center
/// (`0..1`, a turn clockwise from `from` degrees, 0 pointing up).
fn conic_t(dx: f32, dy: f32, from: f32) -> f32 {
    let deg = dx.atan2(-dy).to_degrees();
    ((deg - from) / 360.0).rem_euclid(1.0)
}

/// The ends of a CSS linear gradient line at `angle` (0 up, clockwise)
/// through the center of a `w` x `h` box: long enough that the corners
/// get the end colors.
fn linear_line(angle: Radians, w: f32, h: f32) -> (Pt, Pt) {
    let (s, c) = angle.sin_cos();
    let len = (w * s).abs() + (h * c).abs();
    let (dx, dy) = (s * len / 2.0, -c * len / 2.0);
    let (cx, cy) = (w / 2.0, h / 2.0);
    ((cx - dx, cy - dy), (cx + dx, cy + dy))
}

/// The horizontal and vertical radii of a radial gradient's ending shape
/// centered at `(cx, cy)` in a `w` x `h` box.
fn radial_radii(circle: bool, size: GradientSize, cx: f32, cy: f32, w: f32, h: f32) -> (f32, f32) {
    let near = (cx.min(w - cx).abs(), cy.min(h - cy).abs());
    let far = (cx.max(w - cx).abs(), cy.max(h - cy).abs());
    if circle {
        let r = match size {
            GradientSize::ClosestSide => near.0.min(near.1),
            GradientSize::FarthestSide => far.0.max(far.1),
            GradientSize::ClosestCorner => near.0.hypot(near.1),
            GradientSize::FarthestCorner => far.0.hypot(far.1),
        };
        return (r, r);
    }
    // An ellipse to a corner keeps the side ellipse's aspect ratio.
    let s = std::f32::consts::SQRT_2;
    match size {
        GradientSize::ClosestSide => near,
        GradientSize::FarthestSide => far,
        GradientSize::ClosestCorner => (near.0 * s, near.1 * s),
        GradientSize::FarthestCorner => (far.0 * s, far.1 * s),
    }
}

#[cfg(test)]
mod tests {
    use nsh_core::{
        ColorStop,
        geometry::{DefiniteLength, percentage, px},
    };

    use super::*;

    const RED: Color = Color(0xff00_00ff);
    const BLUE: Color = Color(0x0000_ffff);

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn rgba(c: Color) -> [f32; 4] {
        let (r, g, b, a) = c.to_f64();
        [r as f32, g as f32, b as f32, a as f32]
    }

    fn gradient(kind: GradientKind, stops: &[(Color, Option<f32>)], repeating: bool) -> Gradient {
        Gradient {
            kind,
            stops: stops
                .iter()
                .map(|&(color, position)| ColorStop {
                    color,
                    position: position.map(|p| DefiniteLength::from(percentage(p))),
                })
                .collect(),
            repeating,
        }
    }

    #[test]
    fn linear_lines() {
        let (a, b) = linear_line(deg(90.0), 200.0, 100.0);
        assert!(close(a.0, 0.0) && close(a.1, 50.0));
        assert!(close(b.0, 200.0) && close(b.1, 50.0));
        let (a, b) = linear_line(deg(180.0), 200.0, 100.0);
        assert!(close(a.0, 100.0) && close(a.1, 0.0));
        assert!(close(b.0, 100.0) && close(b.1, 100.0));
        let (a, b) = linear_line(deg(45.0), 100.0, 100.0);
        assert!(close(a.0, 0.0) && close(a.1, 100.0));
        assert!(close(b.0, 100.0) && close(b.1, 0.0));
    }

    #[test]
    fn radial_sizes() {
        let (rx, ry) = radial_radii(true, GradientSize::FarthestCorner, 50.0, 50.0, 100.0, 100.0);
        assert!(close(rx, 50.0 * std::f32::consts::SQRT_2) && close(rx, ry));
        let (rx, ry) = radial_radii(false, GradientSize::FarthestSide, 25.0, 50.0, 200.0, 100.0);
        assert!(close(rx, 175.0) && close(ry, 50.0));
    }

    #[test]
    fn conic_angles_turn_clockwise_from_the_top() {
        assert!(close(conic_t(0.0, -1.0, 0.0), 0.0));
        assert!(close(conic_t(1.0, 0.0, 0.0), 0.25));
        assert!(close(conic_t(0.0, 1.0, 0.0), 0.5));
        assert!(close(conic_t(1.0, 0.0, 90.0), 0.0));
    }

    #[test]
    fn geometry_places_points() {
        let g = Geometry::of(GradientKind::Linear { angle: deg(90.0) }, 200.0, 100.0);
        assert!(close(g.t((0.0, 50.0)), 0.0) && close(g.t((100.0, 0.0)), 0.5));
        let g = Geometry::of(
            GradientKind::Radial {
                circle: true,
                size: GradientSize::ClosestSide,
                x: percentage(0.5),
                y: percentage(0.5),
            },
            100.0,
            100.0,
        );
        assert!(close(g.t((50.0, 50.0)), 0.0) && close(g.t((100.0, 50.0)), 1.0));
    }

    #[test]
    fn ramps_interpolate_premultiplied() {
        let g = gradient(
            GradientKind::Linear { angle: deg(90.0) },
            &[(RED, None), (Color::TRANSPARENT, None)],
            false,
        );
        let ramp = Ramp::new(&g, 100.0, 50.0, rgba);
        // Halfway to transparent keeps the hue at half alpha.
        let c = straight(ramp.at(0.5));
        assert_eq!(c.0 >> 8, 0xff_0000);
        assert!(((c.0 & 0xff) as i32 - 128).abs() <= 1);
        // Clamped past the ends.
        assert_eq!(straight(ramp.at(-1.0)), RED);
        assert_eq!(straight(ramp.at(2.0)).0 & 0xff, 0);
    }

    #[test]
    fn repeating_ramps_wrap() {
        let g = gradient(
            GradientKind::Linear { angle: deg(90.0) },
            &[(RED, Some(0.0)), (BLUE, Some(0.25))],
            true,
        );
        let ramp = Ramp::new(&g, 100.0, 50.0, rgba);
        assert_eq!(straight(ramp.at(0.25 + 0.1)), straight(ramp.at(0.1)));
        assert_eq!(straight(ramp.at(-0.15)), straight(ramp.at(0.1)));
    }

    #[test]
    fn length_stops_follow_the_box() {
        // `repeating-linear-gradient(90deg, red 0px, blue 10px)`.
        let g = Gradient {
            kind: GradientKind::Linear { angle: deg(90.0) },
            stops: vec![
                ColorStop {
                    color: RED,
                    position: Some(DefiniteLength::from(px(0.0))),
                },
                ColorStop {
                    color: BLUE,
                    position: Some(DefiniteLength::from(px(10.0))),
                },
            ],
            repeating: true,
        };
        let wide = Ramp::new(&g, 200.0, 50.0, rgba);
        assert!(close(wide.stops[1].1, 0.05));
        let narrow = Ramp::new(&g, 100.0, 50.0, rgba);
        assert!(close(narrow.stops[1].1, 0.1));
        assert!(narrow.repeating);
    }

    #[test]
    fn recent_images_are_kept() {
        let image = || {
            Arc::new(RenderImage::new(vec![image::Frame::new(
                image::RgbaImage::new(1, 1),
            )]))
        };
        let mut kept = Kept::default();
        for key in 0..MAX_KEPT as u64 {
            assert!(kept.insert(key, image()).is_empty());
        }
        // Full, but all drawn in this pass: nothing goes.
        assert!(kept.insert(1000, image()).is_empty());
        assert_eq!(kept.images.len(), MAX_KEPT + 1);
        // Later, the images not drawn since go; one drawn again stays.
        kept.pass += KEEP_PASSES;
        assert!(kept.get(5).is_some());
        kept.pass += 1;
        let dropped = kept.insert(2000, image());
        assert_eq!(dropped.len(), MAX_KEPT);
        assert!(kept.get(5).is_some() && kept.get(2000).is_some());
        assert!(kept.get(6).is_none());
    }

    #[test]
    fn images_hold_the_gradient() {
        let g = gradient(
            GradientKind::Linear { angle: deg(90.0) },
            &[(RED, None), (BLUE, None)],
            false,
        );
        let ramp = Ramp::new(&g, 100.0, 50.0, rgba);
        let geometry = Geometry::of(g.kind, 100.0, 50.0);
        let image = rasterize(&ramp, geometry, 100.0, 50.0, (10, 5)).unwrap();
        let bytes = image.as_bytes(0).unwrap();
        // BGRA: red on the left, blue on the right, every row alike.
        assert!(bytes[2] > 200 && bytes[0] < 50);
        let last = (10 * 5 - 1) * 4;
        assert!(bytes[last] > 200 && bytes[last + 2] < 50);
        assert_eq!(&bytes[..40], &bytes[40..80]);
        // A different size or color makes a different key.
        let other = Ramp::new(&g, 100.0, 50.0, |c| {
            let mut v = rgba(c);
            v[3] *= 0.5;
            v
        });
        assert_ne!(
            image_key(&ramp, geometry, 100.0, 50.0, (10, 5)),
            image_key(&other, geometry, 100.0, 50.0, (10, 5))
        );
        assert_ne!(
            image_key(&ramp, geometry, 100.0, 50.0, (10, 5)),
            image_key(&ramp, geometry, 100.0, 50.0, (20, 5))
        );
    }
}
