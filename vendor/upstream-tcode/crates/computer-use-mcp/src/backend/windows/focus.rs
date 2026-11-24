use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, IsIconic,
    IsWindow, SW_RESTORE, SetCursorPos, SetForegroundWindow, ShowWindow,
};

use super::RootInfo;

const FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);

pub(super) struct ForegroundGuard {
    previous: Option<HWND>,
}

impl ForegroundGuard {
    pub(super) fn acquire(root: &RootInfo) -> Self {
        let target = hwnd(root.window_id);
        if target.is_invalid() {
            log::debug!("cannot foreground root {}: its HWND is null", root.ref_id);
            return Self::noop();
        }

        // SAFETY: These calls only inspect/manipulate HWND values supplied by root enumeration.
        unsafe {
            if !IsWindow(Some(target)).as_bool() {
                log::debug!(
                    "cannot foreground root {}: HWND is no longer valid",
                    root.ref_id
                );
                return Self::noop();
            }
            let previous = GetForegroundWindow();
            if previous == target {
                return Self::noop();
            }
            if IsIconic(target).as_bool() {
                let _ = ShowWindow(target, SW_RESTORE);
            }
            if switch_foreground(target) {
                Self {
                    previous: (!previous.is_invalid()).then_some(previous),
                }
            } else {
                log::debug!(
                    "could not foreground root {} within {}ms",
                    root.ref_id,
                    FOCUS_TIMEOUT.as_millis()
                );
                Self::noop()
            }
        }
    }

    fn noop() -> Self {
        Self { previous: None }
    }
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        let Some(previous) = self.previous else {
            return;
        };
        // SAFETY: The handle is checked immediately before the best-effort restoration.
        unsafe {
            if IsWindow(Some(previous)).as_bool() && !switch_foreground(previous) {
                log::debug!("could not restore the previous foreground window");
            }
        }
    }
}

pub(super) struct CursorGuard {
    position: Option<POINT>,
}

impl CursorGuard {
    pub(super) fn acquire() -> Self {
        let mut position = POINT::default();
        // SAFETY: `position` points to initialized, writable memory.
        match unsafe { GetCursorPos(&mut position) } {
            Ok(()) => Self {
                position: Some(position),
            },
            Err(error) => {
                log::debug!("could not save the cursor position: {error}");
                Self { position: None }
            }
        }
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        if let Some(position) = self.position {
            // SAFETY: Restoring a previously returned cursor coordinate has no memory-safety
            // requirements. Failure is intentionally best-effort.
            if let Err(error) = unsafe { SetCursorPos(position.x, position.y) } {
                log::debug!("could not restore the cursor position: {error}");
            }
        }
    }
}

fn hwnd(window_id: u32) -> HWND {
    HWND(window_id as usize as *mut _)
}

// SAFETY: The caller must provide a live top-level HWND. All attachment state is detached before
// returning, including when a foreground operation fails.
unsafe fn switch_foreground(target: HWND) -> bool {
    let _ = unsafe { SetForegroundWindow(target) };
    if unsafe { wait_for_foreground(target) } {
        return true;
    }

    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return false;
    }
    let foreground_thread = unsafe { GetWindowThreadProcessId(foreground, None) };
    let current_thread = unsafe { GetCurrentThreadId() };
    if foreground_thread == 0 || foreground_thread == current_thread {
        return false;
    }
    if !unsafe { AttachThreadInput(current_thread, foreground_thread, true) }.as_bool() {
        return false;
    }
    let _attachment = ThreadInputAttachment {
        current_thread,
        foreground_thread,
    };
    let _ = unsafe { SetForegroundWindow(target) };
    let _ = unsafe { BringWindowToTop(target) };
    unsafe { wait_for_foreground(target) }
}

unsafe fn wait_for_foreground(target: HWND) -> bool {
    let deadline = Instant::now() + FOCUS_TIMEOUT;
    loop {
        if unsafe { GetForegroundWindow() } == target {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(FOCUS_POLL_INTERVAL);
    }
}

struct ThreadInputAttachment {
    current_thread: u32,
    foreground_thread: u32,
}

impl Drop for ThreadInputAttachment {
    fn drop(&mut self) {
        // SAFETY: This exactly reverses the successful attachment that created this guard.
        let _ = unsafe { AttachThreadInput(self.current_thread, self.foreground_thread, false) };
    }
}
