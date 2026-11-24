mod dispatcher;
mod display;
mod host;
mod platform;
mod text;
pub mod webview;
mod window;

use android_activity::AndroidApp;
use std::{cell::RefCell, rc::Rc};

pub(crate) use platform::AndroidPlatform;

pub use host::{
    ScrollCaptureRequest, haptic_long_press, haptic_selection, haptic_threshold,
    scroll_capture_bounds, scroll_capture_rendered,
};
#[doc(hidden)]
pub use host::{
    commit_text as jni_commit_text, delete_backward as jni_delete_backward,
    finish_composing_text as jni_finish_composing_text, frame as jni_frame,
    input_state as jni_input_state, key_event as jni_key_event, on_back as jni_on_back,
    on_insets as jni_on_insets, refresh_rate as jni_refresh_rate,
    scroll_capture as jni_scroll_capture, set_composing_text as jni_set_composing_text,
};

/// Installs the observer for the system screenshot tool's scrolling capture.
/// Without one, every capture search is declined.
pub fn set_scroll_capture_callback(callback: impl FnMut(ScrollCaptureRequest) + 'static) {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("gpui-android::init_platform() must be called first")
            .set_scroll_capture_callback(Box::new(callback));
    });
}

/// Runs `callback` on the GPUI thread once the next frame is presented.
pub fn after_next_frame(callback: impl FnOnce() + 'static) {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("gpui-android::init_platform() must be called first")
            .after_next_frame(Box::new(callback));
    });
}

thread_local! {
    static PLATFORM: RefCell<Option<Rc<AndroidPlatform>>> = const { RefCell::new(None) };
}

/// Creates the process-wide platform. Must be called once, from `android_main`.
pub fn init_platform(app: &AndroidApp) {
    host::initialize(app);
    PLATFORM.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            log::warn!("replacing a previous gpui-android platform instance");
        }
        *slot = Some(Rc::new(AndroidPlatform::new(app.clone())));
    });
}

pub fn platform() -> Rc<dyn gpui::Platform> {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .cloned()
            .expect("gpui-android::init_platform() must be called before platform()")
    })
}

/// Installs a process-level observer for unhandled Android back actions.
///
/// A window-specific handler registered through `PlatformWindow` takes
/// precedence. This hook makes the back button observable by simple hosts.
pub fn set_back_callback(callback: impl FnMut() + 'static) {
    PLATFORM.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("gpui-android::init_platform() must be called first")
            .set_process_back_callback(Box::new(callback));
    });
}
