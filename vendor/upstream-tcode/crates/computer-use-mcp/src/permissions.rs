//! Desktop computer-use permission checks and requests, shared by the backend
//! and the Settings → Computer Use / Browser pages.
//!
//! tcode is itself the signed `.app` the grants attach to, so there is no
//! helper-app attribution to worry about. Screen Recording grants only take
//! effect after the app restarts (macOS shows its own "Quit & Reopen" dialog);
//! callers must persist any restart-continuity marker *before* requesting.

pub use tcode_core::permissions::{ComputerUsePermissions, PermissionKind, PermissionStatus};

/// The explicit user-facing action to perform for a missing permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionGrantAction {
    Request,
    OpenSettings,
}

/// Chooses the next action for a missing permission without invoking TCC or
/// opening another application. The Settings UI owns those platform effects.
#[derive(Debug, Default)]
pub struct PermissionGrantFlow {
    attempted: PermissionStatus,
}

impl PermissionGrantFlow {
    /// Return the action the UI should label without mutating the flow.
    pub fn action(&self, kind: PermissionKind) -> PermissionGrantAction {
        if self.attempted.granted(kind) {
            PermissionGrantAction::OpenSettings
        } else {
            PermissionGrantAction::Request
        }
    }

    /// Return the explicit effect for this click and advance a first request to
    /// the System Settings fallback for later clicks.
    pub fn advance(&mut self, kind: PermissionKind) -> PermissionGrantAction {
        let action = self.action(kind);
        if action == PermissionGrantAction::Request {
            match kind {
                PermissionKind::Accessibility => self.attempted.accessibility = true,
                PermissionKind::ScreenRecording => self.attempted.screen_recording = true,
            }
        }
        action
    }
}

/// Non-prompting snapshot of both TCC grants for this process.
pub fn check() -> PermissionStatus {
    imp::check()
}

/// Whether the executable on disk no longer matches the code this process is
/// running, which happens when the bundle is rebuilt, re-signed or replaced
/// under a running instance. TCC validates every request against the code on
/// disk, so both grants stop working until the app restarts even though
/// System Settings still lists them as granted and [`check`] keeps returning
/// the process-cached verdict from launch.
pub fn code_changed_on_disk() -> bool {
    imp::code_changed_on_disk()
}

/// Non-prompting host snapshot including whether these grants apply at all.
pub fn host_status() -> ComputerUsePermissions {
    if cfg!(target_os = "macos") {
        ComputerUsePermissions::MacOs(check())
    } else if cfg!(target_os = "windows") {
        ComputerUsePermissions::NotRequired
    } else {
        ComputerUsePermissions::Unsupported
    }
}

/// Fire the native request for one permission kind. The system prompt may
/// complete asynchronously or stop appearing after an earlier attempt. Callers
/// should offer [`open_settings_pane`] as a later, explicit fallback instead of
/// opening it in the same action as this request. The return value is the native
/// API's passthrough result, not a completion signal; use [`check`] for state.
pub fn request(kind: PermissionKind) -> bool {
    imp::request(kind)
}

/// Deep-link System Settings to the Privacy & Security pane for `kind`.
pub fn open_settings_pane(kind: PermissionKind) {
    imp::open_settings_pane(kind)
}

/// Start a fresh instance of tcode and return; the caller is responsible for
/// quitting the current instance afterwards. Prefers relaunching the enclosing
/// `.app` bundle (so LaunchServices identity — and thus TCC attribution — is
/// preserved); falls back to re-spawning the bare executable in dev builds.
pub fn relaunch_app() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(std::path::Path::to_path_buf);
    match bundle {
        Some(app) => {
            tcode_services::process::command("open")
                .arg("-n")
                .arg(app)
                .spawn()?;
        }
        None => {
            tcode_services::process::command(exe).spawn()?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{PermissionKind, PermissionStatus};
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    type SecCodeRef = *const std::ffi::c_void;
    type OSStatus = i32;
    const SEC_CS_DEFAULT_FLAGS: u32 = 0;
    const ERR_SEC_CS_STATIC_CODE_CHANGED: OSStatus = -67034;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecCodeCopySelf(flags: u32, out: *mut SecCodeRef) -> OSStatus;
        fn SecCodeCheckValidity(
            code: SecCodeRef,
            flags: u32,
            requirement: *const std::ffi::c_void,
        ) -> OSStatus;
    }

    pub(super) fn check() -> PermissionStatus {
        PermissionStatus {
            accessibility: unsafe { AXIsProcessTrusted() },
            screen_recording: unsafe { CGPreflightScreenCaptureAccess() },
        }
    }

    pub(super) fn code_changed_on_disk() -> bool {
        let mut code: SecCodeRef = std::ptr::null();
        // SAFETY: SecCodeCopySelf follows the create rule; the reference is
        // released below after the validity check.
        let status = unsafe { SecCodeCopySelf(SEC_CS_DEFAULT_FLAGS, &mut code) };
        if status != 0 || code.is_null() {
            return false;
        }
        let validity =
            unsafe { SecCodeCheckValidity(code, SEC_CS_DEFAULT_FLAGS, std::ptr::null()) };
        unsafe { core_foundation::base::CFRelease(code) };
        validity == ERR_SEC_CS_STATIC_CODE_CHANGED
    }

    pub(super) fn request(kind: PermissionKind) -> bool {
        match kind {
            PermissionKind::Accessibility => unsafe {
                let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
                let options = CFDictionary::from_CFType_pairs(&[(
                    key.as_CFType(),
                    CFBoolean::true_value().as_CFType(),
                )]);
                AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
            },
            PermissionKind::ScreenRecording => unsafe { CGRequestScreenCaptureAccess() },
        }
    }

    pub(super) fn open_settings_pane(kind: PermissionKind) {
        let pane = match kind {
            PermissionKind::Accessibility => "Privacy_Accessibility",
            PermissionKind::ScreenRecording => "Privacy_ScreenCapture",
        };
        let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
        let _ = tcode_services::process::command("open").arg(url).spawn();
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{PermissionKind, PermissionStatus};

    pub(super) fn check() -> PermissionStatus {
        // The Windows uiautomation/input/screenshot APIs do not use the
        // macOS-style application-level grants represented by this API.
        PermissionStatus {
            accessibility: true,
            screen_recording: true,
        }
    }

    pub(super) fn code_changed_on_disk() -> bool {
        false
    }

    pub(super) fn request(_kind: PermissionKind) -> bool {
        true
    }

    pub(super) fn open_settings_pane(_kind: PermissionKind) {}
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::{PermissionKind, PermissionStatus};

    pub(super) fn check() -> PermissionStatus {
        // Unsupported platforms have no TCC; the settings UI shows the
        // platform as unsupported rather than ungranted.
        PermissionStatus::default()
    }

    pub(super) fn code_changed_on_disk() -> bool {
        false
    }

    pub(super) fn request(_kind: PermissionKind) -> bool {
        false
    }

    pub(super) fn open_settings_pane(_kind: PermissionKind) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_permission_requests_once_before_offering_settings() {
        let mut flow = PermissionGrantFlow::default();
        for kind in [
            PermissionKind::Accessibility,
            PermissionKind::ScreenRecording,
        ] {
            assert_eq!(flow.action(kind), PermissionGrantAction::Request);
            assert_eq!(flow.advance(kind), PermissionGrantAction::Request);
            assert_eq!(flow.action(kind), PermissionGrantAction::OpenSettings);
            assert_eq!(flow.advance(kind), PermissionGrantAction::OpenSettings);
        }
    }
}
