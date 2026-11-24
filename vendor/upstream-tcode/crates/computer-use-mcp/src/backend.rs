//! Platform backend contract and platform-neutral input descriptions.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::outline::{Frame, UiNode};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(all(test, not(target_os = "windows")))]
#[path = "backend/windows/map.rs"]
mod windows_map_tests;

const UNSUPPORTED_MESSAGE: &str = "computer use is unsupported on this platform";

/// Kind of desktop root to match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    #[default]
    Window,
    Dialog,
    Sheet,
    Menu,
    Popover,
}

impl fmt::Display for RootKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Window => "window",
            Self::Dialog => "dialog",
            Self::Sheet => "sheet",
            Self::Menu => "menu",
            Self::Popover => "popover",
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RootInfo {
    pub ref_id: String,
    pub app_name: String,
    pub bundle_id: String,
    pub pid: u32,
    pub title: String,
    pub kind: RootKind,
    pub window_id: u32,
    pub frame: Frame,
}

impl RootInfo {
    pub fn identity(&self) -> String {
        format!("{}:{}", self.pid, self.window_id)
    }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[schemars(rename = "FindRootsParams")]
pub struct RootFilters {
    /// Text to match against application and window titles.
    #[serde(default)]
    pub text: Option<String>,
    /// Application name to match.
    #[serde(default)]
    pub app: Option<String>,
    /// Application bundle identifier to match.
    #[serde(default)]
    pub bundle_id: Option<String>,
    /// Process identifier to match.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Desktop root kind to match.
    #[serde(default)]
    pub kind: Option<RootKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePolicy {
    Never,
    Always,
    IfSparse,
}

impl CapturePolicy {
    pub fn should_capture(self, text_sparse: bool) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::IfSparse => text_sparse,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ObserveRequest {
    pub semantic: bool,
    pub capture: CapturePolicy,
}

#[derive(Debug, Clone)]
pub struct RootObservation {
    pub root: RootInfo,
    pub tree: UiNode,
    pub text_sparse: bool,
    pub screenshot: Option<Vec<u8>>,
    pub screenshot_mime: &'static str,
}

/// Supported desktop input action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "UiActionKind")]
pub enum ActionKind {
    Press,
    Click,
    SetText,
    TypeText,
    Keypress,
    Scroll,
    Drag,
    MoveMouse,
}

/// Mouse button used by pointer actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone)]
pub struct ActionRequest {
    pub kind: ActionKind,
    pub target_path: Option<Vec<usize>>,
    pub target_frame: Option<Frame>,
    pub target_role: Option<String>,
    pub target_title: Option<String>,
    pub target_actions: Vec<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub text: Option<String>,
    pub keys: Option<Vec<String>>,
    pub scroll_x: Option<f64>,
    pub scroll_y: Option<f64>,
    pub path: Option<Vec<[f64; 2]>>,
    pub button: MouseButton,
    pub click_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Worked,
    Didnt,
    Unknown,
}

/// Mechanism used to deliver an action to the target application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Ax,
    BackgroundPid,
    ForegroundHid,
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub outcome: ActionOutcome,
    pub message: String,
    pub delivery: Delivery,
}

impl ActionResult {
    pub fn worked(message: impl Into<String>, delivery: Delivery) -> Self {
        Self {
            outcome: ActionOutcome::Worked,
            message: message.into(),
            delivery,
        }
    }

    pub fn didnt(message: impl Into<String>, delivery: Delivery) -> Self {
        Self {
            outcome: ActionOutcome::Didnt,
            message: message.into(),
            delivery,
        }
    }

    pub fn unknown(message: impl Into<String>, delivery: Delivery) -> Self {
        Self {
            outcome: ActionOutcome::Unknown,
            message: message.into(),
            delivery,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendErrorCode {
    UnsupportedPlatform,
    RootNotFound,
    ObservationFailed,
    CaptureFailed,
    InvalidAction,
    OperationFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackendError {
    pub code: BackendErrorCode,
    pub message: String,
}

impl BackendError {
    pub fn new(code: BackendErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn unsupported() -> Self {
        Self::new(BackendErrorCode::UnsupportedPlatform, UNSUPPORTED_MESSAGE)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for BackendError {}

#[cfg(target_os = "macos")]
pub use macos::{observe, perform_action};

pub(crate) fn set_feedback_enabled(enabled: bool) {
    #[cfg(target_os = "macos")]
    macos::set_feedback_enabled(enabled);
    #[cfg(not(target_os = "macos"))]
    let _ = enabled;
}

pub(crate) fn clear_feedback(owner: Option<u64>, action: Option<u64>) {
    #[cfg(target_os = "macos")]
    macos::clear_feedback(owner, action);
    #[cfg(not(target_os = "macos"))]
    let _ = (owner, action);
}
#[cfg(target_os = "windows")]
pub use windows::{observe, perform_action};

/// `paced` delivers the input only once the agent cursor has reached its point.
pub(crate) fn perform_action_with_feedback(
    root: &RootInfo,
    request: &ActionRequest,
    feedback: &crate::feedback::FeedbackRun,
    paced: bool,
) -> Result<ActionResult, BackendError> {
    #[cfg(target_os = "macos")]
    {
        macos::perform_action_with_feedback(root, request, Some(feedback), paced)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (feedback, paced);
        perform_action(root, request)
    }
}

/// Enumerate desktop roots. The host process is never a root: in-process
/// accessibility queries run the host's own accessibility callbacks on the
/// calling (non-main) thread, which aborts the app.
pub fn list_roots(filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
    let own_pid = std::process::id();
    if filters.pid == Some(own_pid) {
        return Err(BackendError::new(
            BackendErrorCode::RootNotFound,
            "the Tcode host process cannot observe itself; launch a separate instance to drive it",
        ));
    }
    #[cfg(target_os = "macos")]
    let roots = macos::list_roots(filters);
    #[cfg(target_os = "windows")]
    let roots = windows::list_roots(filters);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let roots: Result<Vec<RootInfo>, BackendError> = Err(BackendError::unsupported());
    let mut roots = roots?;
    roots.retain(|root| root.pid != own_pid);
    Ok(roots)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn observe(
    _root: &RootInfo,
    _request: ObserveRequest,
) -> Result<RootObservation, BackendError> {
    Err(BackendError::unsupported())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn perform_action(
    _root: &RootInfo,
    _request: &ActionRequest,
) -> Result<ActionResult, BackendError> {
    Err(BackendError::unsupported())
}

/// Process identifier of the currently frontmost macOS application.
pub fn frontmost_pid() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        macos::frontmost_pid()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub command: bool,
    pub control: bool,
    pub option: bool,
    pub shift: bool,
    pub function: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub keycode: u16,
    pub modifiers: KeyModifiers,
}

pub fn parse_key_chord(keys: &[String]) -> Result<KeyChord, String> {
    let parts: Vec<String> = keys
        .iter()
        .flat_map(|part| part.split('+'))
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("keypress requires a key name or chord".into());
    }

    let mut modifiers = KeyModifiers::default();
    let mut keycode = None;
    for part in parts {
        match part.as_str() {
            "cmd" | "command" | "meta" => modifiers.command = true,
            "ctrl" | "control" => modifiers.control = true,
            "alt" | "option" => modifiers.option = true,
            "shift" => modifiers.shift = true,
            "fn" | "function" => modifiers.function = true,
            key => {
                if keycode.is_some() {
                    return Err("keypress accepts exactly one non-modifier key".into());
                }
                keycode = keycode_for_name(key);
                if keycode.is_none() {
                    return Err(format!("unknown key name: {key}"));
                }
            }
        }
    }
    let keycode = keycode.ok_or_else(|| "keypress chord has no non-modifier key".to_string())?;
    Ok(KeyChord { keycode, modifiers })
}

#[cfg(target_os = "windows")]
pub fn keycode_for_name(name: &str) -> Option<u16> {
    windows_keycode_for_name(name)
}

#[cfg(not(target_os = "windows"))]
pub fn keycode_for_name(name: &str) -> Option<u16> {
    macos_keycode_for_name(name)
}

/// US ANSI virtual key codes. Key names are layout-independent controls or
/// physical letter/number keys; text entry uses Unicode events instead.
pub fn macos_keycode_for_name(name: &str) -> Option<u16> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0B,
        "q" => 0x0C,
        "w" => 0x0D,
        "e" => 0x0E,
        "r" => 0x0F,
        "y" => 0x10,
        "t" => 0x11,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "6" => 0x16,
        "5" => 0x17,
        "=" | "equal" => 0x18,
        "9" => 0x19,
        "7" => 0x1A,
        "-" | "minus" => 0x1B,
        "8" => 0x1C,
        "0" => 0x1D,
        "]" | "right_bracket" => 0x1E,
        "o" => 0x1F,
        "u" => 0x20,
        "[" | "left_bracket" => 0x21,
        "i" => 0x22,
        "p" => 0x23,
        "enter" | "return" => 0x24,
        "l" => 0x25,
        "j" => 0x26,
        "'" | "quote" => 0x27,
        "k" => 0x28,
        ";" | "semicolon" => 0x29,
        "\\" | "backslash" => 0x2A,
        "," | "comma" => 0x2B,
        "/" | "slash" => 0x2C,
        "n" => 0x2D,
        "m" => 0x2E,
        "." | "period" => 0x2F,
        "tab" => 0x30,
        "space" => 0x31,
        "`" | "grave" => 0x32,
        "delete" | "backspace" => 0x33,
        "escape" | "esc" => 0x35,
        "f17" => 0x40,
        "f18" => 0x4F,
        "f19" => 0x50,
        "f20" => 0x5A,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f3" => 0x63,
        "f8" => 0x64,
        "f9" => 0x65,
        "f11" => 0x67,
        "f13" => 0x69,
        "f16" => 0x6A,
        "f14" => 0x6B,
        "f10" => 0x6D,
        "f12" => 0x6F,
        "f15" => 0x71,
        "help" | "insert" => 0x72,
        "home" => 0x73,
        "page_up" | "pageup" => 0x74,
        "forward_delete" => 0x75,
        "f4" => 0x76,
        "end" => 0x77,
        "f2" => 0x78,
        "page_down" | "pagedown" => 0x79,
        "f1" => 0x7A,
        "left" | "left_arrow" => 0x7B,
        "right" | "right_arrow" => 0x7C,
        "down" | "down_arrow" => 0x7D,
        "up" | "up_arrow" => 0x7E,
        _ => return None,
    })
}

/// Windows virtual-key codes. Printable names identify the corresponding
/// physical US keyboard keys; text entry uses `KEYEVENTF_UNICODE` instead.
pub fn windows_keycode_for_name(name: &str) -> Option<u16> {
    let name = name.trim().to_ascii_lowercase();
    let bytes = name.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        return Some(bytes[0].to_ascii_uppercase().into());
    }
    if bytes.len() == 1 && bytes[0].is_ascii_digit() {
        return Some(bytes[0].into());
    }
    if let Some(number) = name
        .strip_prefix('f')
        .and_then(|value| value.parse::<u16>().ok())
        && (1..=20).contains(&number)
    {
        return Some(0x70 + number - 1);
    }
    Some(match name.as_str() {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "space" => 0x20,
        "delete" | "backspace" => 0x08,
        "forward_delete" => 0x2E,
        "escape" | "esc" => 0x1B,
        "help" | "insert" => 0x2D,
        "home" => 0x24,
        "page_up" | "pageup" => 0x21,
        "end" => 0x23,
        "page_down" | "pagedown" => 0x22,
        "left" | "left_arrow" => 0x25,
        "up" | "up_arrow" => 0x26,
        "right" | "right_arrow" => 0x27,
        "down" | "down_arrow" => 0x28,
        ";" | "semicolon" => 0xBA,
        "=" | "equal" => 0xBB,
        "," | "comma" => 0xBC,
        "-" | "minus" => 0xBD,
        "." | "period" => 0xBE,
        "/" | "slash" => 0xBF,
        "`" | "grave" => 0xC0,
        "[" | "left_bracket" => 0xDB,
        "\\" | "backslash" => 0xDC,
        "]" | "right_bracket" => 0xDD,
        "'" | "quote" => 0xDE,
        _ => return None,
    })
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
pub(super) fn matches_root_filters(root: &RootInfo, filters: &RootFilters) -> bool {
    if filters.pid.is_some_and(|pid| root.pid != pid)
        || filters.kind.is_some_and(|kind| root.kind != kind)
    {
        return false;
    }
    if filters
        .app
        .as_deref()
        .is_some_and(|app| !contains_case_insensitive(&root.app_name, app))
        || filters
            .bundle_id
            .as_deref()
            .is_some_and(|bundle| !contains_case_insensitive(&root.bundle_id, bundle))
    {
        return false;
    }
    filters.text.as_deref().is_none_or(|text| {
        contains_case_insensitive(&root.app_name, text)
            || contains_case_insensitive(&root.title, text)
            || contains_case_insensitive(&root.bundle_id, text)
    })
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_uses_stable_mcp_wire_labels() {
        for (delivery, expected) in [
            (Delivery::Ax, "ax"),
            (Delivery::BackgroundPid, "background_pid"),
            (Delivery::ForegroundHid, "foreground_hid"),
            (Delivery::None, "none"),
        ] {
            assert_eq!(serde_json::to_value(delivery).unwrap(), expected);
        }
    }

    #[test]
    fn list_roots_rejects_the_host_process_before_platform_access() {
        let error = list_roots(&RootFilters {
            pid: Some(std::process::id()),
            ..RootFilters::default()
        })
        .unwrap_err();
        assert_eq!(error.code, BackendErrorCode::RootNotFound);
    }

    #[test]
    fn key_chords_resolve_platform_codes_and_reject_ambiguous_input() {
        for (name, macos, windows) in [
            ("enter", 0x24, 0x0D),
            ("left_arrow", 0x7B, 0x25),
            ("F12", 0x6F, 0x7B),
        ] {
            assert_eq!(macos_keycode_for_name(name), Some(macos));
            assert_eq!(windows_keycode_for_name(name), Some(windows));
        }
        for parts in [
            vec!["cmd+shift+s".into()],
            vec![" Command ".into(), "SHIFT".into(), "s".into()],
        ] {
            let chord = parse_key_chord(&parts).unwrap();
            assert_eq!(
                chord.keycode,
                if cfg!(target_os = "windows") {
                    0x53
                } else {
                    0x01
                }
            );
            assert_eq!(
                chord.modifiers,
                KeyModifiers {
                    command: true,
                    shift: true,
                    ..KeyModifiers::default()
                }
            );
        }
        for parts in [
            vec![],
            vec!["cmd".into()],
            vec!["a+b".into()],
            vec!["unknown-key".into()],
        ] {
            assert!(parse_key_chord(&parts).is_err(), "{parts:?}");
        }
    }

    #[test]
    fn root_filters_match_all_windows_fields_case_insensitively() {
        let root = RootInfo {
            app_name: "Windows Terminal".into(),
            bundle_id: "WindowsTerminal.exe".into(),
            pid: 42,
            title: "PowerShell — tcode".into(),
            kind: RootKind::Window,
            ..RootInfo::default()
        };

        assert!(matches_root_filters(
            &root,
            &RootFilters {
                text: Some("TCODE".into()),
                app: Some("terminal".into()),
                bundle_id: Some("windowsterminal".into()),
                pid: Some(42),
                kind: Some(RootKind::Window),
            }
        ));
        assert!(!matches_root_filters(
            &root,
            &RootFilters {
                pid: Some(7),
                ..RootFilters::default()
            }
        ));
        assert!(!matches_root_filters(
            &root,
            &RootFilters {
                kind: Some(RootKind::Dialog),
                ..RootFilters::default()
            }
        ));
    }
}
