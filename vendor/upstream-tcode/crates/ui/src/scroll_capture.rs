//! The chat timeline as the target of the platform's scrolling screenshot.
//!
//! Android's screenshot tool extends a capture by asking the app which view
//! scrolls, then for the pixels that would sit above or below the viewport,
//! one tile at a time. GPUI cannot read its own frames back, so the host
//! answers with the timeline's rectangle, scrolls the timeline to each tile
//! and lets the platform copy the window after the frame is painted. Every
//! call here is in window pixels; the host converts to the device scale.

use gpui::{App, Bounds, Pixels};

pub use crate::chat::CaptureScroll;
use crate::chat::ChatView;
use crate::shell::current_window_shell;

fn with_chat<R>(
    cx: &mut App,
    f: impl FnOnce(&mut ChatView, &mut gpui::Context<ChatView>) -> R,
) -> Option<R> {
    let (window, shell) = current_window_shell(cx)?;
    window
        .update(cx, |_, _, cx| {
            let chat = shell.read(cx).capture_chat(cx)?;
            Some(chat.update(cx, f))
        })
        .ok()
        .flatten()
}

/// The rectangle a capture may scroll, when the timeline is the page on
/// screen and has content.
pub fn viewport(cx: &mut App) -> Option<Bounds<Pixels>> {
    with_chat(cx, |chat, _| chat.capture_viewport()).flatten()
}

/// Device pixels per window pixel for the window being captured.
pub fn scale_factor(cx: &mut App) -> Option<f32> {
    let (window, _) = current_window_shell(cx)?;
    window.update(cx, |_, window, _| window.scale_factor()).ok()
}

/// Freeze the timeline for a capture. `false` when there is nothing to
/// capture; then no other call is made.
pub fn begin(cx: &mut App) -> bool {
    with_chat(cx, |chat, cx| chat.capture_begin(cx)).unwrap_or(false)
}

/// Scroll so that the content `offset` below the viewport's top at the start
/// of the capture is at the top now. `None` once the timeline is no longer
/// being captured.
pub fn scroll(cx: &mut App, offset: Pixels) -> Option<CaptureScroll> {
    with_chat(cx, |chat, cx| chat.capture_scroll(offset, cx)).flatten()
}

/// Whether the next painted frame shows finished content for the rows on
/// screen rather than placeholders still being built.
pub fn settled(cx: &mut App) -> bool {
    with_chat(cx, |chat, _| chat.capture_settled()).unwrap_or(true)
}

/// Put the timeline back where the capture found it.
pub fn end(cx: &mut App) {
    with_chat(cx, |chat, cx| chat.capture_end(cx));
}
