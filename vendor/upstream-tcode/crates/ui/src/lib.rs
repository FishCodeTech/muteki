mod acp_panel;
mod add_project_dialog;
mod agents_panel;
pub mod assets;
/// The window's link to a host: its transport tasks and workspace store.
pub mod attachment;
mod attachments;
mod change_project_root_dialog;
pub mod chat;
mod commit_dialog;
mod composer;
mod composer_trigger;
mod context_meter;
mod control;
mod conversation_ui;
pub(crate) mod diff;
mod edge_back;
#[doc(hidden)]
pub mod gallery_support;
pub(crate) mod git;
mod haptics;
mod highlight;
mod host_permissions;
pub mod i18n;
pub mod icon;
mod image_viewer;
/// macOS TCC permission status and grant flow. Compiled only where the platform
/// actually has one; other attachments query the host for read-only status.
#[cfg(all(feature = "local-permissions", target_os = "macos"))]
mod local_permissions;
pub mod markdown;
// Shared material helpers are also used by the phone shell.
#[cfg(target_os = "macos")]
mod macos_backdrop;
pub mod material;
mod move_thread_dialog;
mod orchestrate_settings;
pub mod overlay;
/// The shared pairing form: origin validation, stale-result
/// generations and fixed-origin behavior, reused by every client shell.
pub mod pairing;
pub mod palette;
mod pasteboard;
mod plan_panel;
mod plugins_settings;
mod preview_panel;
mod project_icon;
pub(crate) mod provider_card;
mod provider_dialog;
mod provider_model_picker;
pub(crate) mod provider_models;
pub(crate) mod provider_status;
pub mod remote;
mod run;
pub(crate) mod runtime_event;
mod scroll;
pub mod scroll_capture;
pub mod settings;
mod settings_page;
pub mod settings_rows;
mod shell;
mod shortcut;
pub mod sidebar;
pub mod sizing;
mod source_control_settings;
pub mod store;
mod terminal_drawer;
mod terminal_key_bar;
pub mod theme;
mod thread_export;
pub mod time;
pub(crate) mod toast;
mod touch_selection;
pub mod typography;
pub(crate) mod usage;
pub mod widgets;
mod window_caption;
pub(crate) mod window_seam;
mod window_state;
mod workspace_walk;

pub use i18n::{
    LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, apply_locale, resolve_locale, set_locale,
    translate, translate_with_args,
};
pub use preview_panel::preview_backend;
pub use run::{
    ShellOptions, THEME_JSON, flattened_theme_json, init_client, last_host_target,
    open_client_window, run_shell,
};
pub(crate) use shell::window_drag_area;
pub use shell::{
    AppShell, Quit, ShellSetup, TogglePalette, can_swipe_back, handle_back, update_edge_back,
};
pub use thread_export::format_size;
pub use window_seam::force_mobile_layout;
pub use window_state::{OpenThread, WindowState};

/// Where this client may keep its own files (the WebView2 profile is the only
/// current user). Bootstrap owns the location; the UI never resolves it, so a
/// remote attachment cannot be tricked into reading the host's data directory.
static CLIENT_DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_client_data_dir(dir: std::path::PathBuf) {
    let _ = CLIENT_DATA_DIR.set(dir);
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn client_data_dir() -> Option<&'static std::path::Path> {
    CLIENT_DATA_DIR.get().map(std::path::PathBuf::as_path)
}

mod pull_requests;
