use std::ffi::{CStr, c_char, c_void};
use std::mem;

use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::geometry::DisplayGeometry;
use crate::outline::Frame;

pub(super) type Id = *mut c_void;
type Sel = *mut c_void;
// SAFETY: this callback ABI is the dispatch_function_t signature from libdispatch.
pub(super) type DispatchFn = unsafe extern "C" fn(*mut c_void);

// SAFETY: these declarations match the Objective-C runtime's public C ABI.
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

// SAFETY: AppKit is linked for the Objective-C window and view classes used below.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

// SAFETY: QuartzCore is linked for the Objective-C layer classes used below.
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

// SAFETY: these declarations match CoreGraphics' public display C ABI.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGGetDisplaysWithPoint(
        point: CGPoint,
        max_displays: u32,
        displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGWindowLevelForKey(key: i32) -> i32;
}

// SAFETY: libdispatch is a libSystem component and these declarations match its C ABI.
#[link(name = "System")]
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: Id, context: *mut c_void, work: DispatchFn);
    fn dispatch_after_f(when: u64, queue: Id, context: *mut c_void, work: DispatchFn);
    fn dispatch_time(when: u64, delta: i64) -> u64;
}

macro_rules! invoke {
    ($return_type:ty, $receiver:expr, $selector:expr $(, $argument_type:ty => $argument:expr)* $(,)?) => {{
        // SAFETY: each wrapper below fixes the function signature to the documented
        // Objective-C ABI of the selectors for which that wrapper is used.
        let function: unsafe extern "C" fn(Id, Sel $(, $argument_type)*) -> $return_type =
            unsafe { mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        // SAFETY: receiver and selector were checked for null, and each caller uses
        // the wrapper whose fixed signature matches the selector's documented ABI.
        unsafe { function($receiver, $selector $(, $argument)*) }
    }};
}

pub(super) fn class(name: &CStr) -> Option<Id> {
    // SAFETY: name is a live, nul-terminated C string for this call.
    let value = unsafe { objc_getClass(name.as_ptr()) };
    (!value.is_null()).then_some(value)
}

fn selector(name: &CStr) -> Option<Sel> {
    // SAFETY: name is a live, nul-terminated C string for this call.
    let value = unsafe { sel_registerName(name.as_ptr()) };
    (!value.is_null()).then_some(value)
}

fn can_send(receiver: Id, target: Sel) -> bool {
    if receiver.is_null() || target.is_null() {
        return false;
    }
    let Some(check) = selector(c"respondsToSelector:") else {
        return false;
    };
    invoke!(i8, receiver, check, Sel => target) != 0
}

pub(super) fn send_id(receiver: Id, name: &CStr) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(Id, receiver, selector);
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_cstr(receiver: Id, name: &CStr, value: *const c_char) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) || value.is_null() {
        return None;
    }
    let result = invoke!(Id, receiver, selector, *const c_char => value);
    (!result.is_null()).then_some(result)
}

pub(super) fn send_id_id(receiver: Id, name: &CStr, value: Id) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let result = invoke!(Id, receiver, selector, Id => value);
    (!result.is_null()).then_some(result)
}

pub(super) fn send_id_rect(receiver: Id, name: &CStr, rect: CGRect) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(Id, receiver, selector, CGRect => rect);
    (!value.is_null()).then_some(value)
}

pub(super) fn send_id_window_init(
    receiver: Id,
    name: &CStr,
    rect: CGRect,
    style: usize,
    backing: usize,
    defer: bool,
) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let value = invoke!(
        Id,
        receiver,
        selector,
        CGRect => rect,
        usize => style,
        usize => backing,
        i8 => i8::from(defer),
    );
    (!value.is_null()).then_some(value)
}

pub(super) fn send_void(receiver: Id, name: &CStr) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector);
    true
}

pub(super) fn send_void_id(receiver: Id, name: &CStr, value: Id) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, Id => value);
    true
}

pub(super) fn send_void_bool(receiver: Id, name: &CStr, value: bool) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, i8 => i8::from(value));
    true
}

pub(super) fn send_void_f64(receiver: Id, name: &CStr, value: f64) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, f64 => value);
    true
}

pub(super) fn send_void_f32(receiver: Id, name: &CStr, value: f32) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, f32 => value);
    true
}

pub(super) fn send_void_isize(receiver: Id, name: &CStr, value: isize) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, isize => value);
    true
}

pub(super) fn send_void_usize(receiver: Id, name: &CStr, value: usize) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, usize => value);
    true
}

pub(super) fn send_void_point(receiver: Id, name: &CStr, value: CGPoint) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGPoint => value);
    true
}

pub(super) fn send_void_size(receiver: Id, name: &CStr, value: CGSize) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGSize => value);
    true
}

pub(super) fn send_void_rect(receiver: Id, name: &CStr, value: CGRect) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGRect => value);
    true
}

pub(super) fn dispatch_main(context: *mut c_void, work: DispatchFn) -> bool {
    let queue = dispatch_get_main_queue();
    if queue.is_null() {
        return false;
    }
    // SAFETY: the caller owns context until work runs; libdispatch invokes work
    // exactly once with that unchanged context on the main queue.
    unsafe { dispatch_async_f(queue, context, work) };
    true
}

pub(super) fn dispatch_main_after(delay_ns: i64, work: DispatchFn) -> bool {
    let queue = dispatch_get_main_queue();
    if queue.is_null() {
        return false;
    }
    // SAFETY: queue is the process main queue, dispatch_time accepts a relative
    // nanosecond delta from DISPATCH_TIME_NOW, and the null context needs no owner.
    unsafe {
        let when = dispatch_time(0, delay_ns);
        dispatch_after_f(when, queue, std::ptr::null_mut(), work);
    }
    true
}

// The C SDK defines dispatch_get_main_queue as this inline address operation.
fn dispatch_get_main_queue() -> Id {
    (&raw const _dispatch_main_q).cast_mut().cast::<c_void>()
}

pub(super) fn display_frame_for_ax_point(point: (f64, f64)) -> Option<DisplayGeometry> {
    if !point.0.is_finite() || !point.1.is_finite() {
        return None;
    }
    let mut display = 0_u32;
    let mut count = 0_u32;
    // SAFETY: display and count are valid writable outputs for one display ID.
    let error = unsafe {
        CGGetDisplaysWithPoint(CGPoint::new(point.0, point.1), 1, &mut display, &mut count)
    };
    // SAFETY: CGMainDisplayID takes no arguments and returns a display ID.
    let main_display = unsafe { CGMainDisplayID() };
    if error != 0 || count == 0 {
        display = main_display;
    }
    if display == 0 {
        return None;
    }
    // SAFETY: display came from CoreGraphics and is valid for this bounds query.
    let bounds = unsafe { CGDisplayBounds(display) };
    let ax = Frame {
        x: bounds.origin.x,
        y: bounds.origin.y,
        w: bounds.size.width,
        h: bounds.size.height,
    };
    if main_display == 0 {
        return None;
    }
    // SAFETY: main_display came from CoreGraphics and is valid for this bounds query.
    let main_bounds = unsafe { CGDisplayBounds(main_display) };
    let appkit = Frame {
        x: ax.x,
        y: main_bounds.origin.y + main_bounds.size.height - (ax.y + ax.h),
        w: ax.w,
        h: ax.h,
    };
    (ax.x.is_finite()
        && ax.y.is_finite()
        && ax.w.is_finite()
        && ax.h.is_finite()
        && appkit.x.is_finite()
        && appkit.y.is_finite()
        && appkit.w.is_finite()
        && appkit.h.is_finite()
        && ax.w > 0.0
        && ax.h > 0.0)
        .then_some(DisplayGeometry { ax, appkit })
}

pub(super) fn status_window_level() -> isize {
    const STATUS_WINDOW_LEVEL_KEY: i32 = 9;
    // SAFETY: STATUS_WINDOW_LEVEL_KEY is a documented CGWindowLevelKey value.
    unsafe { CGWindowLevelForKey(STATUS_WINDOW_LEVEL_KEY) as isize }
}

/// `None` when the window is not on screen. Otherwise whether the frontmost
/// window under `point`, among the window and those in front of it, belongs to
/// the window's application, so its own menus and popovers count as showing
/// the point. Other applications' layers from the Dock upward (menu bar,
/// status items, this overlay, system shields) never count as cover.
pub(super) fn window_shows_point(window_id: u32, point: Option<(f64, f64)>) -> Option<bool> {
    use core_foundation::base::{CFGetTypeID, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{
        CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef,
    };
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::window::{
        copy_window_info, kCGWindowAlpha, kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowLayer,
        kCGWindowListOptionIncludingWindow, kCGWindowListOptionOnScreenAboveWindow,
        kCGWindowNumber, kCGWindowOwnerPID,
    };
    const DOCK_WINDOW_LAYER: f64 = 20.0;
    // SAFETY: these are immutable CFString constants exported by CoreGraphics.
    let (bounds_key, number_key, onscreen_key, layer_key, alpha_key, owner_key) = unsafe {
        (
            kCGWindowBounds,
            kCGWindowNumber,
            kCGWindowIsOnscreen,
            kCGWindowLayer,
            kCGWindowAlpha,
            kCGWindowOwnerPID,
        )
    };

    // SAFETY (all helpers): CGWindowListCopyWindowInfo returns an array of
    // dictionaries, and each borrowed value stays owned by that array for this
    // call; values are type-checked before they are wrapped.
    let value = |dictionary: CFDictionaryRef, key: CFStringRef| {
        let value = unsafe { CFDictionaryGetValue(dictionary, key.cast()) } as CFTypeRef;
        (!value.is_null()).then_some(value)
    };
    let number = |dictionary: CFDictionaryRef, key: CFStringRef| {
        value(dictionary, key)
            .filter(|value| unsafe { CFGetTypeID(*value) } == CFNumber::type_id())
            .and_then(|value| unsafe { CFNumber::wrap_under_get_rule(value.cast()) }.to_f64())
    };
    let bounds = |dictionary: CFDictionaryRef| {
        let bounds = value(dictionary, bounds_key)
            .filter(|value| unsafe { CFGetTypeID(*value) == CFDictionaryGetTypeID() })?
            as CFDictionaryRef;
        let field = |name: &str| number(bounds, CFString::new(name).as_concrete_TypeRef());
        Some(Frame {
            x: field("X")?,
            y: field("Y")?,
            w: field("Width")?,
            h: field("Height")?,
        })
    };
    let contains = |frame: Frame, (x, y): (f64, f64)| {
        x >= frame.x && x < frame.x + frame.w && y >= frame.y && y < frame.y + frame.h
    };

    let list = copy_window_info(
        kCGWindowListOptionOnScreenAboveWindow | kCGWindowListOptionIncludingWindow,
        window_id,
    )?;
    let windows: Vec<CFDictionaryRef> = list
        .iter()
        .map(|window| *window as CFDictionaryRef)
        .collect();
    let target = windows
        .iter()
        .copied()
        .find(|dictionary| number(*dictionary, number_key) == Some(f64::from(window_id)))?;
    let onscreen = value(target, onscreen_key).is_some_and(|value| unsafe {
        CFGetTypeID(value) == CFBoolean::type_id()
            && bool::from(CFBoolean::wrap_under_get_rule(value.cast()))
    });
    if !onscreen {
        return None;
    }
    let Some(point) = point else {
        return Some(false);
    };
    let owner = number(target, owner_key);
    // The list runs front to back, ending with the window itself.
    let frontmost_under_point = windows.iter().copied().find(|dictionary| {
        let own = number(*dictionary, owner_key) == owner;
        let layer = number(*dictionary, layer_key).unwrap_or(0.0);
        let alpha = number(*dictionary, alpha_key).unwrap_or(1.0);
        (own || (0.0..DOCK_WINDOW_LAYER).contains(&layer))
            && alpha > 0.0
            && bounds(*dictionary).is_some_and(|frame| contains(frame, point))
    });
    Some(frontmost_under_point.is_some_and(|dictionary| number(dictionary, owner_key) == owner))
}

pub(super) fn send_id_f64(receiver: Id, name: &CStr, value: f64) -> Option<Id> {
    let selector = selector(name)?;
    if !can_send(receiver, selector) {
        return None;
    }
    let result = invoke!(Id, receiver, selector, f64 => value);
    (!result.is_null()).then_some(result)
}

pub(super) fn send_void_id_id(receiver: Id, name: &CStr, first: Id, second: Id) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, Id => first, Id => second);
    true
}

pub(super) fn send_void_rect_bool(receiver: Id, name: &CStr, value: CGRect, display: bool) -> bool {
    let Some(selector) = selector(name) else {
        return false;
    };
    if !can_send(receiver, selector) {
        return false;
    }
    invoke!((), receiver, selector, CGRect => value, i8 => i8::from(display));
    true
}

// These getters inspect the actual AppKit panel in the opt-in native regression.
// CGRect's objc_msgSend return ABI here is specific to the arm64 runner.
#[cfg(all(test, target_arch = "aarch64"))]
pub(super) fn native_panel_state(window: Id) -> (CGRect, bool, bool, u32) {
    let frame = invoke!(CGRect, window, selector(c"frame").unwrap());
    let visible = invoke!(i8, window, selector(c"isVisible").unwrap()) != 0;
    let passthrough = invoke!(i8, window, selector(c"ignoresMouseEvents").unwrap()) != 0;
    let number = invoke!(isize, window, selector(c"windowNumber").unwrap()) as u32;
    (frame, visible, passthrough, number)
}

#[cfg(all(test, target_arch = "aarch64"))]
pub(super) fn layer_position(layer: Id) -> CGPoint {
    invoke!(CGPoint, layer, selector(c"position").unwrap())
}
