use std::cell::Cell;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use core_foundation::base::{CFRelease, CFTypeRef};
use core_foundation::mach_port::{
    CFMachPortCreateRunLoopSource, CFMachPortInvalidate, CFMachPortRef,
};
use core_foundation::runloop::{
    CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRun, CFRunLoopStop,
    kCFRunLoopCommonModes,
};
use core_graphics::event::CGEvent;
use core_graphics::geometry::CGPoint;
use core_graphics::sys::CGEventRef;

use super::super::{BackendError, BackendErrorCode, MouseButton, RootInfo};
use super::{ax, input};
use crate::outline::Frame;

const FIELD_MOUSE_CLICK_STATE: u32 = 1;
const FIELD_MOUSE_PRESSURE: u32 = 2;
const FIELD_TARGET_PID: u32 = 39;
const FIELD_TARGET_WINDOW: u32 = 51;
const FIELD_PRIVATE_ROUTING: u32 = 58;
const FIELD_WINDOW_UNDER_POINTER: u32 = 91;
const FIELD_WINDOW_UNDER_POINTER_CAN_HANDLE: u32 = 92;

type SetWindowLocationFn = unsafe extern "C" fn(CGEventRef, CGPoint);
type Pid = c_int;

const RTLD_LAZY: c_int = 1;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreateForPid(
        pid: Pid,
        place: u32,
        options: u32,
        mask: u64,
        callback: extern "C" fn(*const c_void, u32, CGEventRef, *mut c_void) -> CGEventRef,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventPostToPid(pid: Pid, event: CGEventRef);
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    fn objc_msgSend();
}

pub(super) struct BackgroundDispatcher {
    pid: Pid,
    window_id: i64,
    window_frame: Frame,
    last_point: Cell<CGPoint>,
}

impl BackgroundDispatcher {
    pub(super) fn new(root: &RootInfo) -> Result<Self, BackendError> {
        let pid = Pid::try_from(root.pid)
            .map_err(|_| operation(format!("target pid {} is out of range", root.pid)))?;
        if root.window_id == 0 || !root.frame.has_area() {
            return Err(operation(
                "background PID delivery requires a concrete window id and frame",
            ));
        }
        let (center_x, center_y) = root.frame.center();
        Ok(Self {
            pid,
            window_id: i64::from(root.window_id),
            window_frame: root.frame,
            last_point: Cell::new(CGPoint::new(center_x, center_y)),
        })
    }

    pub(super) fn click(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
    ) -> Result<(), BackendError> {
        self.prime()?;
        let point = CGPoint::new(x, y);
        for (index, (down, up)) in input::click_events(x, y, button, click_count)?
            .into_iter()
            .enumerate()
        {
            self.post_mouse(&down, point, i64::from(click_count), 1.0);
            thread::sleep(Duration::from_millis(30));
            self.post_mouse(&up, point, i64::from(click_count), 0.0);
            if index + 1 < click_count as usize {
                thread::sleep(Duration::from_millis(45));
            }
        }
        self.last_point.set(point);
        Ok(())
    }

    /// Chromium swallows the first click a background window receives as its
    /// activation click. This click at (-1, -1) lies outside every window, so
    /// it is the one swallowed there, and lands on nothing in an app that would
    /// take it as a real click.
    fn prime(&self) -> Result<(), BackendError> {
        let point = CGPoint::new(-1.0, -1.0);
        let Some((down, up)) = input::click_events(point.x, point.y, MouseButton::Left, 1)?
            .into_iter()
            .next()
        else {
            return Err(operation("could not build the background click primer"));
        };
        self.post_mouse(&down, point, 1, 1.0);
        thread::sleep(Duration::from_millis(30));
        self.post_mouse(&up, point, 1, 0.0);
        thread::sleep(Duration::from_millis(20));
        Ok(())
    }

    pub(super) fn move_mouse(&self, x: f64, y: f64) -> Result<(), BackendError> {
        let point = CGPoint::new(x, y);
        let event = input::move_mouse_event(x, y)?;
        self.post_mouse(&event, point, 0, 0.0);
        self.last_point.set(point);
        Ok(())
    }

    pub(super) fn scroll(&self, x: f64, y: f64) -> Result<(), BackendError> {
        let point = self.last_point.get();
        let event = input::scroll_event(x, y)?;
        event.set_location(point);
        event.set_integer_value_field(FIELD_WINDOW_UNDER_POINTER, self.window_id);
        event.set_integer_value_field(FIELD_WINDOW_UNDER_POINTER_CAN_HANDLE, self.window_id);
        self.stamp_addressing(&event);
        set_window_location(&event, window_local_point(point, self.window_frame));
        event.post_to_pid(self.pid);
        Ok(())
    }

    pub(super) fn drag(&self, path: &[[f64; 2]], button: MouseButton) -> Result<(), BackendError> {
        let events = input::drag_events(path, button)?;
        self.prime()?;
        let final_index = events.len().saturating_sub(1);
        for (index, event) in events.into_iter().enumerate() {
            let point_index = index.min(path.len().saturating_sub(1));
            let point = CGPoint::new(path[point_index][0], path[point_index][1]);
            let pressure = if index == final_index { 0.0 } else { 1.0 };
            self.post_mouse(&event, point, 1, pressure);
            self.last_point.set(point);
            if index > 0 && index < final_index {
                thread::sleep(Duration::from_millis(12));
            }
        }
        Ok(())
    }

    pub(super) fn keypress(&self, keys: &[String]) -> Result<(), BackendError> {
        for event in input::keypress_events(keys)? {
            self.stamp_addressing(&event);
            event.post_to_pid(self.pid);
        }
        Ok(())
    }

    pub(super) fn type_text(&self, text: &str) -> Result<(), BackendError> {
        for [down, up] in input::text_event_pairs(text)? {
            self.stamp_addressing(&down);
            down.post_to_pid(self.pid);
            self.stamp_addressing(&up);
            up.post_to_pid(self.pid);
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn post_mouse(&self, event: &CGEvent, point: CGPoint, click_state: i64, pressure: f64) {
        event.set_integer_value_field(FIELD_MOUSE_CLICK_STATE, click_state);
        event.set_double_value_field(FIELD_MOUSE_PRESSURE, pressure);
        event.set_integer_value_field(FIELD_WINDOW_UNDER_POINTER, self.window_id);
        event.set_integer_value_field(FIELD_WINDOW_UNDER_POINTER_CAN_HANDLE, self.window_id);
        self.stamp_addressing(event);
        set_window_location(event, window_local_point(point, self.window_frame));
        event.post_to_pid(self.pid);
    }

    fn stamp_addressing(&self, event: &CGEvent) {
        stamp_addressing(event, self.pid, self.window_id);
    }
}

fn stamp_addressing(event: &CGEvent, pid: Pid, window_id: i64) {
    event.set_integer_value_field(FIELD_TARGET_PID, i64::from(pid));
    event.set_integer_value_field(FIELD_TARGET_WINDOW, window_id);
    event.set_integer_value_field(FIELD_PRIVATE_ROUTING, 1);
}

fn window_local_point(point: CGPoint, frame: Frame) -> CGPoint {
    CGPoint::new(point.x - frame.x, point.y - frame.y)
}

fn set_window_location(event: &CGEvent, point: CGPoint) {
    let Some(setter) = sky_light_set_window_location() else {
        return;
    };
    // SAFETY: the optional SkyLight symbol was resolved with the verified
    // CGEventSetWindowLocation ABI, and `event` is live for this call.
    unsafe { setter(raw_event_ref(event), point) };
}

fn raw_event_ref(event: &CGEvent) -> CGEventRef {
    let borrowed: &core_graphics::event::CGEventRef = event;
    std::ptr::from_ref(borrowed).cast_mut().cast()
}

fn sky_light_set_window_location() -> Option<SetWindowLocationFn> {
    static SETTER: OnceLock<Option<SetWindowLocationFn>> = OnceLock::new();
    *SETTER.get_or_init(|| {
        // SAFETY: the path and flags are valid C inputs; a null handle is
        // treated as an unavailable optional private framework.
        let handle = unsafe {
            dlopen(
                c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight".as_ptr(),
                RTLD_LAZY,
            )
        };
        if handle.is_null() {
            log::debug!("SkyLight is unavailable; skipping window-local event stamping");
            return None;
        }
        // SAFETY: RTLD_DEFAULT is the verified macOS sentinel and the symbol
        // name is a static NUL-terminated C string.
        let symbol = unsafe {
            dlsym(
                (-2_isize) as *mut c_void,
                c"CGEventSetWindowLocation".as_ptr(),
            )
        };
        if symbol.is_null() {
            log::debug!(
                "CGEventSetWindowLocation is unavailable; skipping window-local event stamping"
            );
            None
        } else {
            // SAFETY: the resolved symbol uses the verified
            // CGEventSetWindowLocation(CGEventRef, CGPoint) ABI.
            Some(unsafe { std::mem::transmute::<*mut c_void, SetWindowLocationFn>(symbol) })
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TapKind {
    Previous,
    Target,
}

pub(super) struct TapContext {
    kind: TapKind,
    armed: AtomicBool,
}

impl TapContext {
    fn new(kind: TapKind) -> Self {
        Self {
            kind,
            armed: AtomicBool::new(true),
        }
    }
}

pub(super) struct TapRuntime {
    ports: [CFMachPortRef; 2],
    run_loop: CFRunLoopRef,
    thread: Option<JoinHandle<()>>,
}

pub(super) enum BackgroundActivation {
    AlreadyForeground,
    Active {
        target_pid: Pid,
        window_id: i64,
        runtime: TapRuntime,
        previous_context: Box<TapContext>,
        target_context: Box<TapContext>,
    },
}

impl BackgroundActivation {
    pub(super) fn acquire(root: &RootInfo) -> Result<Self, BackendError> {
        let previous_pid = ax::frontmost_application_pid().ok_or_else(|| {
            operation("could not determine the frontmost application for background delivery")
        })?;
        if previous_pid == root.pid {
            return Ok(Self::AlreadyForeground);
        }
        let previous_pid = Pid::try_from(previous_pid)
            .map_err(|_| operation("frontmost application pid is out of range"))?;
        let target_pid = Pid::try_from(root.pid)
            .map_err(|_| operation("target application pid is out of range"))?;
        if root.window_id == 0 || !root.frame.has_area() {
            return Err(operation(
                "background activation requires a concrete target window",
            ));
        }

        let mut previous_context = Box::new(TapContext::new(TapKind::Previous));
        let mut target_context = Box::new(TapContext::new(TapKind::Target));
        let previous_context_ptr = (&mut *previous_context as *mut TapContext) as usize;
        let target_context_ptr = (&mut *target_context as *mut TapContext) as usize;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("tcode-cu-background-taps".into())
            .spawn(move || {
                run_tap_thread(
                    previous_pid,
                    target_pid,
                    previous_context_ptr,
                    target_context_ptr,
                    ready_tx,
                );
            })
            .map_err(|error| operation(format!("could not spawn focus-tap thread: {error}")))?;

        let ready = match ready_rx.recv() {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(operation(error));
            }
            Err(error) => {
                let _ = thread.join();
                return Err(operation(format!(
                    "focus-tap thread stopped during setup: {error}"
                )));
            }
        };
        let guard = Self::Active {
            target_pid,
            window_id: i64::from(root.window_id),
            runtime: TapRuntime {
                ports: ready.ports.map(|port| port as CFMachPortRef),
                run_loop: ready.run_loop as CFRunLoopRef,
                thread: Some(thread),
            },
            previous_context,
            target_context,
        };

        post_appkit_event(target_pid, i64::from(root.window_id), 1);
        thread::sleep(Duration::from_millis(20));
        Ok(guard)
    }
}

impl Drop for BackgroundActivation {
    fn drop(&mut self) {
        let Self::Active {
            target_pid,
            window_id,
            runtime,
            previous_context,
            target_context,
        } = self
        else {
            return;
        };

        if ax::frontmost_application_pid() != u32::try_from(*target_pid).ok() {
            post_appkit_event(*target_pid, *window_id, 2);
            thread::sleep(Duration::from_millis(20));
        }
        previous_context.armed.store(false, Ordering::Release);
        target_context.armed.store(false, Ordering::Release);
        for port in runtime.ports {
            // SAFETY: each port is a live create-rule event tap owned by the
            // tap thread and remains live until that thread is joined below.
            unsafe { CFMachPortInvalidate(port) };
        }
        // SAFETY: this is the live dedicated run loop returned by the tap
        // thread; stopping it is thread-safe and causes the thread to exit.
        unsafe { CFRunLoopStop(runtime.run_loop) };
        if let Some(thread) = runtime.thread.take()
            && thread.join().is_err()
        {
            log::debug!("background focus-tap thread panicked while stopping");
        }
    }
}

struct TapThreadReady {
    ports: [usize; 2],
    run_loop: usize,
}

fn run_tap_thread(
    previous_pid: Pid,
    target_pid: Pid,
    previous_context: usize,
    target_context: usize,
    ready: mpsc::SyncSender<Result<TapThreadReady, String>>,
) {
    let previous = create_tap(previous_pid, previous_context as *mut c_void);
    let previous = match previous {
        Ok(tap) => tap,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let target = create_tap(target_pid, target_context as *mut c_void);
    let target = match target {
        Ok(tap) => tap,
        Err(error) => {
            release_tap(previous);
            let _ = ready.send(Err(error));
            return;
        }
    };

    // SAFETY: this runs on the dedicated tap thread and returns that thread's
    // live run loop, to which the two valid sources are added.
    let run_loop = unsafe { CFRunLoopGetCurrent() };
    // SAFETY: the run loop, sources, and tap ports are live; common-modes is a
    // static Core Foundation mode constant and enabling a new tap is valid.
    unsafe {
        CFRunLoopAddSource(run_loop, previous.source, kCFRunLoopCommonModes);
        CFRunLoopAddSource(run_loop, target.source, kCFRunLoopCommonModes);
        CGEventTapEnable(previous.port, true);
        CGEventTapEnable(target.port, true);
    }
    if ready
        .send(Ok(TapThreadReady {
            ports: [previous.port as usize, target.port as usize],
            run_loop: run_loop as usize,
        }))
        .is_err()
    {
        release_tap(previous);
        release_tap(target);
        return;
    }
    // SAFETY: the current thread owns this dedicated run loop and runs it
    // until BackgroundActivation::drop calls CFRunLoopStop.
    unsafe { CFRunLoopRun() };
    release_tap(previous);
    release_tap(target);
}

struct RawTap {
    port: CFMachPortRef,
    source: core_foundation::runloop::CFRunLoopSourceRef,
}

fn create_tap(pid: Pid, context: *mut c_void) -> Result<RawTap, String> {
    // SAFETY: the callback has C ABI and `context` points to a boxed TapContext
    // that the guard keeps alive until after this tap thread is joined.
    let port = unsafe { CGEventTapCreateForPid(pid, 0, 0, u64::MAX, focus_tap_callback, context) };
    if port.is_null() {
        return Err(format!(
            "could not create the background focus-suppression tap for pid {pid}"
        ));
    }
    // SAFETY: `port` is a live CFMachPort create-rule object; null source is
    // handled as a recoverable setup failure.
    let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), port, 0) };
    if source.is_null() {
        // SAFETY: `port` is the create-rule object returned above and is being
        // invalidated and released exactly once on this failure path.
        unsafe {
            CFMachPortInvalidate(port);
            CFRelease(port.cast::<c_void>() as CFTypeRef);
        }
        return Err(format!(
            "could not create a run-loop source for the background tap on pid {pid}"
        ));
    }
    Ok(RawTap { port, source })
}

fn release_tap(tap: RawTap) {
    // SAFETY: both values are create-rule Core Foundation objects owned by
    // this tap thread and are released exactly once here.
    unsafe {
        CFMachPortInvalidate(tap.port);
        CFRelease(tap.source.cast::<c_void>() as CFTypeRef);
        CFRelease(tap.port.cast::<c_void>() as CFTypeRef);
    }
}

extern "C" fn focus_tap_callback(
    _proxy: *const c_void,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }
    // SAFETY: user_info points to the boxed TapContext kept alive by the guard
    // until its tap has been invalidated and its run-loop thread joined.
    let context = unsafe { &*(user_info as *const TapContext) };
    if should_drop_focus_event(
        context.kind,
        event_type,
        context.armed.load(Ordering::Acquire),
    ) {
        ptr::null_mut()
    } else {
        event
    }
}

fn should_drop_focus_event(kind: TapKind, event_type: u32, armed: bool) -> bool {
    armed && kind == TapKind::Previous && matches!(event_type, 13 | 19 | 20)
}

fn post_appkit_event(pid: Pid, window_id: i64, subtype: i16) {
    if window_id == 0 {
        return;
    }
    let Some(pool) = ObjcPool::new() else {
        log::debug!("could not create an autorelease pool for appKitDefined primer");
        return;
    };
    // SAFETY: NSEvent is a stable AppKit class lookup; a null result is
    // handled by skipping this optional primer.
    let event_class = unsafe { objc_getClass(c"NSEvent".as_ptr()) };
    if event_class.is_null() {
        log::debug!("NSEvent is unavailable; skipping appKitDefined primer");
        drop(pool);
        return;
    }
    // SAFETY: this selector is the verified NSEvent class method and all
    // arguments use their macOS ABI types.
    let selector = unsafe {
        sel_registerName(c"otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:".as_ptr())
    };
    if selector.is_null() {
        log::debug!("NSEvent primer selector is unavailable; skipping primer");
        return;
    }
    type OtherEvent = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        usize,
        CGPoint,
        usize,
        f64,
        isize,
        *mut c_void,
        i16,
        isize,
        isize,
    ) -> *mut c_void;
    // SAFETY: objc_msgSend is cast to the exact verified NSEvent class-method
    // ABI used immediately below.
    let send_other: OtherEvent =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    // SAFETY: receiver, selector, CGPoint, and scalar arguments match the
    // OtherEvent ABI; returned nil is handled gracefully.
    let ns_event = unsafe {
        send_other(
            event_class,
            selector,
            13,
            CGPoint::new(0.0, 0.0),
            0,
            0.0,
            window_id as isize,
            ptr::null_mut(),
            subtype,
            0,
            0,
        )
    };
    if ns_event.is_null() {
        log::debug!("NSEvent returned nil; skipping appKitDefined primer subtype {subtype}");
        return;
    }
    // SAFETY: CGEvent is the stable property selector on a live NSEvent.
    let cg_selector = unsafe { sel_registerName(c"CGEvent".as_ptr()) };
    if cg_selector.is_null() {
        log::debug!("NSEvent CGEvent selector is unavailable; skipping primer");
        return;
    }
    type GetCgEvent = unsafe extern "C" fn(*mut c_void, *mut c_void) -> CGEventRef;
    // SAFETY: objc_msgSend is cast to the verified zero-argument CGEvent
    // property getter ABI used immediately below.
    let get_cg_event: GetCgEvent =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    // SAFETY: ns_event and cg_selector are live Objective-C values with the
    // getter ABI above; returned nil is handled gracefully.
    let cg_event = unsafe { get_cg_event(ns_event, cg_selector) };
    if cg_event.is_null() {
        log::debug!("NSEvent CGEvent returned nil; skipping primer subtype {subtype}");
        return;
    }
    // SAFETY: cg_event is borrowed from the live NSEvent for this scope, and
    // both CoreGraphics calls use the verified raw CGEvent ABI synchronously.
    unsafe {
        CGEventSetIntegerValueField(cg_event, FIELD_TARGET_PID, i64::from(pid));
        CGEventSetIntegerValueField(cg_event, FIELD_TARGET_WINDOW, window_id);
        CGEventSetIntegerValueField(cg_event, FIELD_PRIVATE_ROUTING, 1);
        CGEventPostToPid(pid, cg_event);
    }
}

struct ObjcPool(*mut c_void);

impl ObjcPool {
    fn new() -> Option<Self> {
        // SAFETY: NSAutoreleasePool is a stable Foundation class lookup; null
        // is handled as an unavailable optional primer path.
        let class = unsafe { objc_getClass(c"NSAutoreleasePool".as_ptr()) };
        if class.is_null() {
            return None;
        }
        // SAFETY: `new` is the standard zero-argument Objective-C selector.
        let selector = unsafe { sel_registerName(c"new".as_ptr()) };
        if selector.is_null() {
            return None;
        }
        type SendId = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
        // SAFETY: objc_msgSend is cast to the exact zero-argument object-return
        // ABI used for +[NSAutoreleasePool new].
        let send: SendId = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        // SAFETY: class and selector are valid for the SendId ABI above.
        let pool = unsafe { send(class, selector) };
        (!pool.is_null()).then_some(Self(pool))
    }
}

impl Drop for ObjcPool {
    fn drop(&mut self) {
        // SAFETY: `drain` is the stable zero-argument selector for the live
        // NSAutoreleasePool owned by this guard.
        let selector = unsafe { sel_registerName(c"drain".as_ptr()) };
        if selector.is_null() {
            return;
        }
        type SendVoid = unsafe extern "C" fn(*mut c_void, *mut c_void);
        // SAFETY: objc_msgSend is cast to the exact void-return, zero-argument
        // ABI for -[NSAutoreleasePool drain].
        let send: SendVoid = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
        // SAFETY: self.0 is the pool created in ObjcPool::new and selector is
        // valid for the SendVoid ABI above.
        unsafe { send(self.0, selector) };
    }
}

fn operation(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCode::OperationFailed, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_local_transform_uses_ax_top_left_origin() {
        let frame = Frame {
            x: -420.0,
            y: 180.0,
            w: 800.0,
            h: 600.0,
        };
        let point = window_local_point(CGPoint::new(-20.0, 530.0), frame);
        assert_eq!((point.x, point.y), (400.0, 350.0));
    }

    #[test]
    fn focus_tap_drops_only_previous_pid_focus_types_while_armed() {
        for event_type in [13, 19, 20] {
            assert!(should_drop_focus_event(TapKind::Previous, event_type, true));
            assert!(!should_drop_focus_event(TapKind::Target, event_type, true));
            assert!(!should_drop_focus_event(
                TapKind::Previous,
                event_type,
                false
            ));
        }
        for event_type in [0, 1, 12, 14, 18, 21, u32::MAX] {
            assert!(!should_drop_focus_event(
                TapKind::Previous,
                event_type,
                true
            ));
        }
    }
}
