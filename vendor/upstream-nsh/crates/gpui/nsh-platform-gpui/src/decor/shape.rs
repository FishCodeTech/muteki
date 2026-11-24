//! The geometry of decorations, as polygons: rounded boxes (flattened to
//! the window's pixels), the ring of a border and each side's part of it,
//! dashes and dots along a line, and clipping polygons to convex ones.
//!
//! GPUI draws rounded rectangles exactly only while they stay axis-aligned
//! (its quads); everything else (rotated, skewed or projected views, a
//! border's sides in different colors, dotted lines, clips of rotated
//! views) is filled as paths of these polygons, in the view's coordinates
//! and mapped point by point.

use nsh_core::Points;

use crate::geom::rect;

/// A point.
pub(crate) type Pt = (f32, f32);

/// Horizontal and vertical radii of the corners, clockwise from the top
/// left.
pub(crate) type Corners = [(f32, f32); 4];

/// Most a flattened curve strays from the true one (pixels).
const TOLERANCE: f32 = 0.2;

/// Circular corners.
pub(crate) fn circular(radii: [f32; 4]) -> Corners {
    radii.map(|r| (r, r))
}

/// A corner radius `d` points further out (CSS: only rounded corners grow;
/// shrinking stops at square).
pub(crate) fn grown(r: f32, d: f32) -> f32 {
    if r > 0.0 { (r + d).max(0.0) } else { 0.0 }
}

/// The padding box (inside the borders `widths`: top, right, bottom, left)
/// of a `w` x `h` box; its corners are the outer ones less the borders'
/// widths on each axis, elliptical where they differ, square where a
/// border is wider than its corner.
pub(crate) fn padding_box(
    w: f32,
    h: f32,
    radii: [f32; 4],
    widths: [f32; 4],
) -> (nsh_core::Bounds<Points>, Corners) {
    let [top, right, bottom, left] = widths;
    let r = rect(
        left,
        top,
        (w - left - right).max(0.0),
        (h - top - bottom).max(0.0),
    );
    let c = |r: f32, x: f32, y: f32| ((r - x).max(0.0), (r - y).max(0.0));
    (
        r,
        [
            c(radii[0], left, top),
            c(radii[1], right, top),
            c(radii[2], right, bottom),
            c(radii[3], left, bottom),
        ],
    )
}

/// `c` scaled down as CSS does when adjacent corners overlap on a `w` x
/// `h` box.
pub(crate) fn fit_corners(w: f32, h: f32, c: Corners) -> Corners {
    let sides = [
        (c[0].0 + c[1].0, w),
        (c[1].1 + c[2].1, h),
        (c[2].0 + c[3].0, w),
        (c[3].1 + c[0].1, h),
    ];
    let f = sides
        .iter()
        .filter(|(sum, _)| *sum > 0.0)
        .map(|(sum, len)| len / sum)
        .fold(1.0f32, f32::min);
    if f < 1.0 {
        c.map(|(x, y)| (x * f, y * f))
    } else {
        c
    }
}

/// Each side's part of a `w` x `h` box with border `widths`, clockwise from
/// the top: the corners are split along the lines from the outer corners
/// through the inner ones (as CSS joins differing borders), ending at the
/// box's middle lines.
pub(crate) fn side_quads(w: f32, h: f32, widths: [f32; 4]) -> [[Pt; 4]; 4] {
    let [t, r, b, l] = widths;
    // Each corner's split, from the outer corner inwards.
    let end = |(cx, cy): Pt, (dx, dy): Pt| {
        let (dx, dy) = if dx == 0.0 && dy == 0.0 {
            (
                if cx == 0.0 { 1.0 } else { -1.0 },
                if cy == 0.0 { 1.0 } else { -1.0 },
            )
        } else {
            (dx, dy)
        };
        let kx = if dx != 0.0 {
            (w / 2.0) / dx.abs()
        } else {
            f32::INFINITY
        };
        let ky = if dy != 0.0 {
            (h / 2.0) / dy.abs()
        } else {
            f32::INFINITY
        };
        let k = kx.min(ky);
        (cx + dx * k, cy + dy * k)
    };
    let tl = end((0.0, 0.0), (l, t));
    let tr = end((w, 0.0), (-r, t));
    let br = end((w, h), (-r, -b));
    let bl = end((0.0, h), (l, -b));
    [
        [(0.0, 0.0), (w, 0.0), tr, tl],
        [(w, 0.0), (w, h), br, tr],
        [(w, h), (0.0, h), bl, br],
        [(0.0, h), (0.0, 0.0), tl, bl],
    ]
}

/// How many segments a quarter of an ellipse whose larger radius is
/// `r_px` pixels is flattened into.
pub(crate) fn arc_segments(r_px: f32) -> usize {
    if r_px <= TOLERANCE {
        return 1;
    }
    // A chord of angle θ strays r·(1 − cos(θ/2)) from its arc.
    let step = 2.0 * (1.0 - TOLERANCE / r_px).clamp(-1.0, 1.0).acos();
    ((std::f32::consts::FRAC_PI_2 / step.max(1e-3)).ceil() as usize).clamp(1, 64)
}

/// Appends the outline of the rectangle `r` with corners `c`, clockwise
/// on screen from the right end of the top side, flattened for `px` pixels
/// per unit.
pub(crate) fn rounded_polygon(r: nsh_core::Bounds<Points>, c: Corners, px: f32, out: &mut Vec<Pt>) {
    let c = fit_corners(r.size.width.0, r.size.height.0, c);
    let (x0, y0, x1, y1) = (
        r.origin.x.0,
        r.origin.y.0,
        r.origin.x.0 + r.size.width.0,
        r.origin.y.0 + r.size.height.0,
    );
    // Each corner: its center and the angle its arc starts at (y down, so
    // angles turn clockwise on screen).
    let centers = [
        (x1 - c[1].0, y0 + c[1].1, -90.0f32),
        (x1 - c[2].0, y1 - c[2].1, 0.0),
        (x0 + c[3].0, y1 - c[3].1, 90.0),
        (x0 + c[0].0, y0 + c[0].1, 180.0),
    ];
    let order = [1, 2, 3, 0];
    for (i, &(cx, cy, start)) in centers.iter().enumerate() {
        let (rx, ry) = c[order[i]];
        if rx <= 0.0 || ry <= 0.0 {
            // A square corner.
            let corner = [(x1, y0), (x1, y1), (x0, y1), (x0, y0)][i];
            out.push(corner);
            continue;
        }
        let n = arc_segments(rx.max(ry) * px);
        for k in 0..=n {
            let a = (start + 90.0 * k as f32 / n as f32).to_radians();
            out.push((cx + rx * a.cos(), cy + ry * a.sin()));
        }
    }
}

/// The outline of the rectangle `r` with corners `c`, as
/// [`rounded_polygon`] appends it.
pub(crate) fn rounded(r: nsh_core::Bounds<Points>, c: Corners, px: f32) -> Vec<Pt> {
    let mut out = Vec::new();
    rounded_polygon(r, c, px, &mut out);
    out
}

/// Twice the signed area of `poly` (positive when it turns clockwise on
/// screen, y down).
fn signed_area(poly: &[Pt]) -> f32 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum()
}

/// `subject` clipped to the convex polygon `clip` (Sutherland–Hodgman):
/// exact for any subject, which may come out with zero-width bridges
/// where it is not convex (they fill nothing).
pub(crate) fn clip_convex(subject: &[Pt], clip: &[Pt]) -> Vec<Pt> {
    let mut out: Vec<Pt> = subject.to_vec();
    if clip.len() < 3 {
        return out;
    }
    let orientation = signed_area(clip).signum();
    if orientation == 0.0 {
        return Vec::new();
    }
    let mut input = Vec::with_capacity(out.len());
    let n = clip.len();
    for i in 0..n {
        if out.is_empty() {
            break;
        }
        let (a, b) = (clip[i], clip[(i + 1) % n]);
        let side = |p: Pt| ((b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)) * orientation;
        std::mem::swap(&mut input, &mut out);
        out.clear();
        let m = input.len();
        for j in 0..m {
            let (p, q) = (input[j], input[(j + 1) % m]);
            let (sp, sq) = (side(p), side(q));
            if sp >= 0.0 {
                out.push(p);
            }
            if (sp >= 0.0) != (sq >= 0.0) {
                let t = sp / (sp - sq);
                out.push((p.0 + (q.0 - p.0) * t, p.1 + (q.1 - p.1) * t));
            }
        }
    }
    out
}

/// True if `p` lies inside `poly` (crossing number: any simple polygon).
pub(crate) fn contains(poly: &[Pt], p: Pt) -> bool {
    let n = poly.len();
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + n - 1) % n]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    inside
}

/// The parts of the closed line `line` covered by dashes `dash` long with
/// gaps `gap` long, each as the points of the line it covers. The pattern
/// is stretched to fit the line a whole number of times, so that it closes
/// without a short dash.
pub(crate) fn dashes(line: &[Pt], dash: f32, gap: f32) -> Vec<Vec<Pt>> {
    let n = line.len();
    if n < 2 || dash <= 0.0 {
        return Vec::new();
    }
    // Cumulative lengths around the closed line.
    let mut acc = Vec::with_capacity(n + 1);
    acc.push(0.0f32);
    for i in 0..n {
        let (a, b) = (line[i], line[(i + 1) % n]);
        let last = acc[i];
        acc.push(last + (b.0 - a.0).hypot(b.1 - a.1));
    }
    let total = acc[n];
    let period = dash + gap.max(0.0);
    if total <= 0.0 || period <= 0.0 {
        return Vec::new();
    }
    let count = (total / period).round().max(1.0);
    let k = total / (count * period);
    let at = |s: f32| -> (usize, Pt) {
        let s = s.clamp(0.0, total);
        let i = acc.partition_point(|&v| v <= s).clamp(1, n) - 1;
        let (a, b) = (line[i], line[(i + 1) % n]);
        let len = acc[i + 1] - acc[i];
        let t = if len > 0.0 { (s - acc[i]) / len } else { 0.0 };
        (i, (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t))
    };
    let mut out = Vec::with_capacity(count as usize);
    for d in 0..count as usize {
        // Dashes are centered on their period, so the line's start (a
        // corner's end) falls in a gap's middle.
        let start = (d as f32 * period + gap.max(0.0) / 2.0) * k;
        let end = start + dash * k;
        let (i0, p0) = at(start);
        let (i1, p1) = at(end);
        let mut pts = vec![p0];
        pts.extend((i0 + 1..=i1).map(|i| line[i % n]));
        pts.push(p1);
        out.push(pts);
    }
    out
}

/// Points `spacing` apart around the closed line `line` (stretched to fit
/// it a whole number of times): the centers of a dotted line's dots.
pub(crate) fn dots(line: &[Pt], spacing: f32) -> Vec<Pt> {
    dashes(line, spacing / 2.0, spacing / 2.0)
        .into_iter()
        .filter_map(|d| d.last().copied())
        .collect()
}

/// The polygon of the polyline `line` drawn `width` wide with flat ends.
pub(crate) fn thick(line: &[Pt], width: f32) -> Vec<Pt> {
    let n = line.len();
    if n < 2 {
        return Vec::new();
    }
    let half = width / 2.0;
    let normal = |a: Pt, b: Pt| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = dx.hypot(dy);
        if len > 0.0 {
            (-dy / len, dx / len)
        } else {
            (0.0, 0.0)
        }
    };
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let before = if i > 0 {
            normal(line[i - 1], line[i])
        } else {
            (0.0, 0.0)
        };
        let after = if i + 1 < n {
            normal(line[i], line[i + 1])
        } else {
            (0.0, 0.0)
        };
        let (mut nx, mut ny) = (before.0 + after.0, before.1 + after.1);
        let len = nx.hypot(ny);
        if len > 0.0 {
            (nx, ny) = (nx / len, ny / len);
        }
        // Keep the width across a bend (a miter, limited).
        let cos = (nx * after.0 + ny * after.1).max(nx * before.0 + ny * before.1);
        let m = if cos > 0.25 { half / cos } else { half };
        let p = line[i];
        left.push((p.0 + nx * m, p.1 + ny * m));
        right.push((p.0 - nx * m, p.1 - ny * m));
    }
    left.extend(right.into_iter().rev());
    left
}

/// The polygon of a circle at `c` with radius `r`, flattened for `px`
/// pixels per unit.
pub(crate) fn circle(c: Pt, r: f32, px: f32) -> Vec<Pt> {
    let n = 4 * arc_segments(r * px).max(2);
    (0..n)
        .map(|k| {
            let a = std::f32::consts::TAU * k as f32 / n as f32;
            (c.0 + r * a.cos(), c.1 + r * a.sin())
        })
        .collect()
}

/// The ring between a `w` x `h` box with corners `radii` and its padding
/// box inside the borders `widths`: two contours, filled even-odd.
pub(crate) fn ring(w: f32, h: f32, radii: [f32; 4], widths: [f32; 4], px: f32) -> Vec<Vec<Pt>> {
    let mut out = vec![rounded(rect(0.0, 0.0, w, h), circular(radii), px)];
    let (inner, corners) = padding_box(w, h, radii, widths);
    if inner.size.width.0 > 0.0 && inner.size.height.0 > 0.0 {
        out.push(rounded(inner, corners, px));
    }
    out
}

/// Which side's part ([`side_quads`]) of the box `p` lies in (0 top, 1
/// right, 2 bottom, 3 left).
pub(crate) fn side_of(quads: &[[Pt; 4]; 4], p: Pt) -> usize {
    quads
        .iter()
        .position(|q| contains(q, p))
        .unwrap_or_else(|| {
            // On a split line or outside: the nearest side's middle.
            let mid = |q: &[Pt; 4]| ((q[0].0 + q[1].0) / 2.0, (q[0].1 + q[1].1) / 2.0);
            (0..4)
                .min_by(|&a, &b| {
                    let (ma, mb) = (mid(&quads[a]), mid(&quads[b]));
                    let da = (ma.0 - p.0).hypot(ma.1 - p.1);
                    let db = (mb.0 - p.0).hypot(mb.1 - p.1);
                    da.total_cmp(&db)
                })
                .unwrap_or(0)
        })
}

/// Splits each side's part of the box into two triangles (the parts need
/// not be convex; triangles are, for [`clip_convex`]).
pub(crate) fn side_triangles(quad: &[Pt; 4]) -> [[Pt; 3]; 2] {
    [[quad[0], quad[1], quad[2]], [quad[0], quad[2], quad[3]]]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn area(poly: &[Pt]) -> f32 {
        signed_area(poly).abs() / 2.0
    }

    #[test]
    fn padding_box_corners() {
        // Uniform 2 pt borders inside 10 pt corners: 8 pt inner corners.
        let (r, c) = padding_box(100.0, 50.0, [10.0; 4], [2.0; 4]);
        assert_eq!(
            (r.origin.x.0, r.origin.y.0, r.size.width.0, r.size.height.0),
            (2.0, 2.0, 96.0, 46.0)
        );
        assert_eq!(c[0], (8.0, 8.0));
        // Differing borders make elliptical corners, square past the radius.
        let (_, c) = padding_box(100.0, 50.0, [6.0, 0.0, 1.0, 0.0], [6.0, 2.0, 4.0, 1.0]);
        assert_eq!(c[0], (5.0, 0.0));
        assert_eq!(c[1], (0.0, 0.0));
        assert_eq!(c[2], (0.0, 0.0));
    }

    #[test]
    fn corners_fit_the_box() {
        let c = fit_corners(
            10.0,
            100.0,
            [(10.0, 10.0), (10.0, 10.0), (0.0, 0.0), (0.0, 0.0)],
        );
        assert!(close(c[0].0, 5.0) && close(c[1].0, 5.0));
        let c = fit_corners(100.0, 100.0, [(10.0, 20.0); 4]);
        assert_eq!(c[0], (10.0, 20.0));
    }

    #[test]
    fn side_quads_split_at_the_corners() {
        let q = side_quads(100.0, 100.0, [4.0; 4]);
        assert_eq!(q[0], [(0.0, 0.0), (100.0, 0.0), (50.0, 50.0), (50.0, 50.0)]);
        let q = side_quads(200.0, 100.0, [40.0, 10.0, 10.0, 10.0]);
        let (x, y) = q[0][3];
        assert!(close(y / x, 4.0), "{x} {y}");
        let q = side_quads(100.0, 100.0, [4.0, 4.0, 4.0, 0.0]);
        assert!(close(q[0][3].0, 0.0));
    }

    #[test]
    fn grown_corners() {
        assert_eq!(grown(10.0, 4.0), 14.0);
        assert_eq!(grown(0.0, 4.0), 0.0);
        assert_eq!(grown(3.0, -5.0), 0.0);
    }

    #[test]
    fn arcs_get_finer_as_they_grow() {
        assert_eq!(arc_segments(0.1), 1);
        let (small, big) = (arc_segments(10.0), arc_segments(400.0));
        assert!(small >= 2 && big > small && big <= 64, "{small} {big}");
    }

    #[test]
    fn rounded_polygons_have_the_rounded_area() {
        // A square has its area; a circle (radii half the side) nearly π r².
        let p = rounded(rect(0.0, 0.0, 10.0, 20.0), [(0.0, 0.0); 4], 1.0);
        assert_eq!(p.len(), 4);
        assert!(close(area(&p), 200.0));
        assert!(signed_area(&p) > 0.0, "clockwise on screen");
        let p = rounded(rect(0.0, 0.0, 100.0, 100.0), circular([50.0; 4]), 2.0);
        let ideal = std::f32::consts::PI * 2500.0;
        assert!((area(&p) - ideal).abs() / ideal < 0.005, "{}", area(&p));
        // Every point lies on the box.
        assert!(
            p.iter()
                .all(|&(x, y)| (-1e-3..=100.001).contains(&x) && (-1e-3..=100.001).contains(&y))
        );
    }

    #[test]
    fn clipping_to_convex_polygons() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let shifted = [(5.0, 5.0), (15.0, 5.0), (15.0, 15.0), (5.0, 15.0)];
        assert!(close(area(&clip_convex(&square, &shifted)), 25.0));
        // The clip's orientation does not matter.
        let reversed: Vec<Pt> = shifted.iter().rev().copied().collect();
        assert!(close(area(&clip_convex(&square, &reversed)), 25.0));
        // Disjoint: nothing.
        let far = [(20.0, 20.0), (30.0, 20.0), (30.0, 30.0)];
        assert!(clip_convex(&square, &far).is_empty());
    }

    #[test]
    fn points_inside_polygons() {
        let tri = [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0)];
        assert!(contains(&tri, (2.0, 2.0)));
        assert!(!contains(&tri, (8.0, 8.0)));
    }

    #[test]
    fn dashes_fit_the_line() {
        let square = [(0.0, 0.0), (45.0, 0.0), (45.0, 45.0), (0.0, 45.0)];
        // 180 long, period 10: 18 dashes of 5, centered in their periods.
        let d = dashes(&square, 5.0, 5.0);
        assert_eq!(d.len(), 18);
        assert_eq!(d[0][0], (2.5, 0.0));
        let len = |pts: &Vec<Pt>| {
            pts.windows(2)
                .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
                .sum::<f32>()
        };
        assert!(d.iter().all(|pts| close(len(pts), 5.0)));
        // A dash across a corner follows it.
        assert!(d.iter().any(|pts| pts.contains(&(45.0, 0.0))));
        // Stretched: 180 over a period of 11 rounds to 16 periods.
        assert_eq!(dashes(&square, 5.5, 5.5).len(), 16);
        assert_eq!(dots(&square, 9.0).len(), 20);
    }

    #[test]
    fn thick_lines_keep_their_width() {
        let p = thick(&[(0.0, 0.0), (10.0, 0.0)], 2.0);
        assert!(close(area(&p), 20.0));
        // A right-angle bend keeps the width on both legs.
        let p = thick(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 2.0);
        assert!((area(&p) - 40.0).abs() < 0.5, "{}", area(&p));
    }

    #[test]
    fn rings_and_sides() {
        let r = ring(100.0, 50.0, [10.0; 4], [2.0; 4], 1.0);
        assert_eq!(r.len(), 2);
        let ring_area = area(&r[0]) - area(&r[1]);
        // The middle line's length (straight sides and a circle of radius
        // 9) times the width.
        let middle = 2.0 * (80.0 + 30.0) + std::f32::consts::TAU * 9.0;
        assert!((ring_area - 2.0 * middle).abs() < 5.0, "{ring_area}");
        let q = side_quads(100.0, 50.0, [2.0; 4]);
        assert_eq!(side_of(&q, (50.0, 1.0)), 0);
        assert_eq!(side_of(&q, (99.0, 25.0)), 1);
        assert_eq!(side_of(&q, (50.0, 49.0)), 2);
        assert_eq!(side_of(&q, (1.0, 25.0)), 3);
        // The triangles of a side cover its part.
        let tris = side_triangles(&q[0]);
        assert!(close(area(&tris[0]) + area(&tris[1]), area(&q[0])));
    }
}
