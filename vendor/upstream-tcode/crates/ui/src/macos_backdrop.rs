//! The macOS window backdrop: one Liquid Glass `NSGlassEffectView` under the
//! GPUI view, left exactly as AppKit builds it.
//!
//! GPUI's `WindowBackgroundAppearance::Blurred` strips its material down to a
//! colorless blur and has no Liquid Glass option, so the window opens
//! `Transparent` and the system glass goes underneath instead. The glass
//! carries its own tint, honours Reduce Transparency and the Liquid Glass
//! slider, and follows the appearance it is given.

use gpui::{App, Global, Window};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSGlassEffectView, NSGlassEffectViewStyle, NSView,
    NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::theme::ThemeMode;

struct SystemBackdrop(Retained<NSGlassEffectView>);
impl Global for SystemBackdrop {}

/// Whether the window carries a system material that already tints the
/// backdrop, so the theme canvas must not tint it a second time.
pub(crate) fn installed(cx: &App) -> bool {
    cx.has_global::<SystemBackdrop>()
}

/// Slide the glass under GPUI's view. A no-op if the native window cannot be
/// reached, in which case the theme canvas keeps tinting the transparent window.
pub(crate) fn install(window: &Window, cx: &mut App) {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // SAFETY: GPUI hands out its live NSView, which outlives this call.
    let gpui_view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let Some(content) = gpui_view.window().and_then(|window| window.contentView()) else {
        return;
    };
    let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), content.bounds());
    glass.setStyle(NSGlassEffectViewStyle::Regular);
    glass.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    content.addSubview_positioned_relativeTo(&glass, NSWindowOrderingMode::Below, None);
    cx.set_global(SystemBackdrop(glass));
}

/// The glass follows the window's appearance, but the theme mode can be forced
/// from Settings; pin the glass to the theme so a dark palette never sits on
/// light glass.
pub(crate) fn sync_appearance(mode: ThemeMode, cx: &App) {
    let Some(backdrop) = cx.try_global::<SystemBackdrop>() else {
        return;
    };
    let name = if mode.is_dark() {
        unsafe { NSAppearanceNameDarkAqua }
    } else {
        unsafe { NSAppearanceNameAqua }
    };
    backdrop
        .0
        .setAppearance(NSAppearance::appearanceNamed(name).as_deref());
}
