//! Owned raw UIKit handles used while constructing the wgpu surface.

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};

#[derive(Clone, Debug)]
pub(crate) struct IosRawHandles {
    pub(crate) window: RawWindowHandle,
    pub(crate) display: RawDisplayHandle,
}

// UIKit objects are main-thread-only. The renderer requires these marker
// traits to retain an owned raw-handle provider; all actual accesses in this
// backend remain on the main thread and the Swift host owns the UIView.
unsafe impl Send for IosRawHandles {}
unsafe impl Sync for IosRawHandles {}

impl HasWindowHandle for IosRawHandles {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(self.window) })
    }
}

impl HasDisplayHandle for IosRawHandles {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(self.display) })
    }
}
