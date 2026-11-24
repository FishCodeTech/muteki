//! The platform's touch feedback, where it has one. Desktops and the web have
//! none.

/// The haptic a phone plays when a long press is recognized, before the
/// finger lifts: the moment the press has done its work and can be released.
pub(crate) fn long_press() {
    #[cfg(target_os = "ios")]
    gpui_ios::haptic_long_press();
    #[cfg(all(feature = "native-preview", target_os = "android"))]
    gpui_android::haptic_long_press();
}

/// The lighter tap a phone plays when a drag crosses a point that changes
/// what releasing it does: a dragged row lifting out of its place, or a
/// swiped row passing the distance at which it acts, either way.
pub(crate) fn threshold() {
    #[cfg(target_os = "ios")]
    gpui_ios::haptic_threshold();
    #[cfg(all(feature = "native-preview", target_os = "android"))]
    gpui_android::haptic_threshold();
}

/// The tick a phone plays when a dragged item moves to another slot.
pub(crate) fn selection() {
    #[cfg(target_os = "ios")]
    gpui_ios::haptic_selection();
    #[cfg(all(feature = "native-preview", target_os = "android"))]
    gpui_android::haptic_selection();
}
