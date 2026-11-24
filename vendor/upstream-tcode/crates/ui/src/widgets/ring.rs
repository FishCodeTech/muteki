use gpui::{Bounds, Hsla, IntoElement, PathBuilder, Pixels, Styled as _, canvas, point, px};

/// Draw the small progress ring: a muted full-circle track plus a `pct`-swept
/// arc (starting at 12 o'clock), sampled as a stroked polyline.
pub(crate) fn ring_canvas(pct: f32, fg: Hsla, track: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, _| {
            let center = bounds.center();
            let radius = px(6.5);
            let width = px(2.5);
            if let Some(path) = stroked_arc(center, radius, 0.0, 360.0, width) {
                window.paint_path(path, track);
            }
            let pct = pct.clamp(0.0, 100.0);
            if pct > 0.0 {
                let end = -90.0 + pct / 100.0 * 360.0;
                if let Some(path) = stroked_arc(center, radius, -90.0, end, width) {
                    window.paint_path(path, fg);
                }
            }
        },
    )
    .size(px(16.))
}

/// Build a stroked arc path from `start_deg` to `end_deg` (degrees, clockwise
/// from 3 o'clock) as a sampled polyline of the given stroke width.
fn stroked_arc(
    center: gpui::Point<Pixels>,
    radius: Pixels,
    start_deg: f32,
    end_deg: f32,
    width: Pixels,
) -> Option<gpui::Path<Pixels>> {
    let mut builder = PathBuilder::stroke(width);
    let steps = 48;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let angle = (start_deg + (end_deg - start_deg) * t).to_radians();
        let p = point(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        );
        if i == 0 {
            builder.move_to(p);
        } else {
            builder.line_to(p);
        }
    }
    builder.build().ok()
}
