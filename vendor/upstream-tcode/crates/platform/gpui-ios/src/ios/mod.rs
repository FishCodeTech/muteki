//! iOS-only implementation details.

mod dispatcher;
mod display;
pub mod ffi;
mod platform;
mod raw_handles;
mod text_system;
mod window;

pub(crate) use dispatcher::IosDispatcher;
pub(crate) use display::IosDisplay;
pub(crate) use platform::IosPlatform;
pub(crate) use text_system::IosTextSystem;
pub(crate) use window::IosWindow;

use std::{cell::OnceCell, rc::Rc};

thread_local! {
    static PLATFORM: OnceCell<Rc<IosPlatform>> = const { OnceCell::new() };
}

pub(crate) fn platform() -> Rc<dyn gpui::Platform> {
    assert_main_thread();
    PLATFORM.with(|slot| {
        let platform = slot.get_or_init(|| Rc::new(IosPlatform::new()));
        Rc::clone(platform) as Rc<dyn gpui::Platform>
    })
}

pub(crate) fn with_platform(f: impl FnOnce(&IosPlatform)) {
    PLATFORM.with(|slot| {
        if let Some(platform) = slot.get() {
            f(platform);
        }
    });
}

pub(crate) fn is_main_thread() -> bool {
    unsafe { libc::pthread_main_np() != 0 }
}

pub(crate) fn assert_main_thread() {
    assert!(
        is_main_thread(),
        "GPUI iOS must be entered on the main thread"
    );
}
