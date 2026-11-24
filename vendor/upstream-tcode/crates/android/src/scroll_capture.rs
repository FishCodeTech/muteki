//! Serves the system screenshot tool's scrolling capture from the chat
//! timeline.
//!
//! The activity asks, in physical pixels, which rectangle scrolls and then
//! for one tile after another relative to that rectangle at the start. GPUI
//! has no pixel readback, so each tile is answered by scrolling the timeline,
//! presenting a frame and telling the activity how far the frame is scrolled;
//! the activity copies the window itself.

use gpui::{App, AsyncApp, Bounds, Pixels, px};
use gpui_android::ScrollCaptureRequest;

/// Frames a tile may take to settle: rows measured or built for the first
/// time move the list, and the frame on screen must match the reply.
const SETTLE_FRAMES: u32 = 12;

pub(crate) fn handle(request: ScrollCaptureRequest, cx: &mut App) {
    match request {
        ScrollCaptureRequest::Search { request } => {
            let bounds = tcode_ui::scroll_capture::viewport(cx)
                .zip(tcode_ui::scroll_capture::scale_factor(cx))
                .map(|(bounds, scale)| physical_bounds(bounds, scale));
            gpui_android::scroll_capture_bounds(request, bounds);
        }
        ScrollCaptureRequest::Start => {
            tcode_ui::scroll_capture::begin(cx);
        }
        ScrollCaptureRequest::Image { request, top } => {
            let Some(scale) = tcode_ui::scroll_capture::scale_factor(cx) else {
                gpui_android::scroll_capture_rendered(request, None);
                return;
            };
            present_tile(request, px(top as f32 / scale), scale, SETTLE_FRAMES, cx);
        }
        ScrollCaptureRequest::End => tcode_ui::scroll_capture::end(cx),
    }
}

/// Scroll to `target` and reply once a presented frame shows it: the scroll
/// is repeated after each frame until it changes nothing and no row is still
/// being built, or the frames run out.
fn present_tile(request: u64, target: Pixels, scale: f32, frames: u32, cx: &mut App) {
    let Some(scroll) = tcode_ui::scroll_capture::scroll(cx, target) else {
        gpui_android::scroll_capture_rendered(request, None);
        return;
    };
    let shown = !scroll.moved && frames < SETTLE_FRAMES && tcode_ui::scroll_capture::settled(cx);
    if shown || frames == 0 {
        let scrolled = (f32::from(scroll.reached) * scale).round() as i32;
        gpui_android::scroll_capture_rendered(request, Some(scrolled));
        return;
    }
    let async_cx: AsyncApp = cx.to_async();
    gpui_android::after_next_frame(move || {
        async_cx.update(|cx| present_tile(request, target, scale, frames.saturating_sub(1), cx));
    });
}

fn physical_bounds(bounds: Bounds<Pixels>, scale: f32) -> [i32; 4] {
    let edge = |value: Pixels| (f32::from(value) * scale).round() as i32;
    [
        edge(bounds.left()),
        edge(bounds.top()),
        edge(bounds.right()),
        edge(bounds.bottom()),
    ]
}
