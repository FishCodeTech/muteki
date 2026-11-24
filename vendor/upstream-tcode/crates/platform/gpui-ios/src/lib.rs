//! UIKit/Metal platform backend for gpui-fast.
//!
//! The UIKit host owns the application run loop and a `CAMetalLayer`-backed
//! view. This crate adapts that view to GPUI and exposes the small C ABI used
//! by the Swift host.

use std::rc::Rc;

pub mod text_input;

#[cfg(any(target_os = "ios", test))]
mod key_input;

#[cfg(target_os = "ios")]
mod ios;

#[cfg(target_os = "ios")]
pub use ios::ffi::*;

/// Returns the lazily-created process platform.
///
/// On iOS this must be called from the application main thread. GPUI itself is
/// single-threaded on that thread; background tasks use GCD global queues.
#[cfg(target_os = "ios")]
pub fn platform() -> Rc<dyn gpui::Platform> {
    ios::platform()
}

/// iOS is the only supported runtime for this platform crate.
#[cfg(not(target_os = "ios"))]
pub fn platform() -> Rc<dyn gpui::Platform> {
    panic!("gpui-ios::platform() is only available when target_os = \"ios\"")
}

/// UIKit's trusted paste control as the shared edit menu lays it over its
/// Paste item: where, and in the menu's colours and corners, so the control
/// reads as one of the menu's items.
#[cfg(target_os = "ios")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PasteControl {
    pub bounds: gpui::Bounds<gpui::Pixels>,
    pub foreground: gpui::Hsla,
    pub background: gpui::Hsla,
    pub corner_radius: gpui::Pixels,
}

/// Place UIKit's trusted paste control over the shared edit menu's Paste
/// item, or hide it.
#[cfg(target_os = "ios")]
pub fn set_paste_control(control: Option<PasteControl>) {
    ios::ffi::host_set_paste_control(control);
}

/// Keep asynchronous UIKit paste attached to the focused editor's identity.
#[cfg(target_os = "ios")]
pub fn set_paste_input(owner: u64) {
    ios::ffi::host_set_paste_input(owner);
}

/// Play the haptic UIKit gives a recognized long press.
#[cfg(target_os = "ios")]
pub fn haptic_long_press() {
    ios::ffi::host_haptic_long_press();
}

/// Play UIKit's light impact.
#[cfg(target_os = "ios")]
pub fn haptic_threshold() {
    ios::ffi::host_haptic_threshold();
}

/// Play UIKit's selection-changed tick.
#[cfg(target_os = "ios")]
pub fn haptic_selection() {
    ios::ffi::host_haptic_selection();
}

/// Make UIKit's authorized payload available to the editor's synchronous Paste action.
#[cfg(target_os = "ios")]
pub fn with_native_paste(item: gpui::ClipboardItem, paste: impl FnOnce()) {
    ios::with_platform(|platform| platform.with_native_paste(item, paste));
}
