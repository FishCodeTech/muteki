//! Muteki's shared Rust frontend. AppKit and WebAssembly mount this same tree.
mod api;
mod composer;
mod icons;
mod markdown;
mod model_picker;
mod shell;
mod state;
mod surfaces;
mod settings;
mod theme;
mod usage;
use nsh_core::prelude::*;
use state::{AppState, Page};

#[native_module("muteki/ui")]
fn native(registry: &mut Registry) {
    registry.assets(assets!());
    registry.register::<MutekiApp>();
}

#[component]
pub fn MutekiApp() -> impl IntoElement {
    let app = AppState::new();
    app.refresh_threads();
    app.load_preferences();
    app.load_catalog();
    effect(move || {
        use nsh_core::platform::Appearance;
        set_color_scheme(app.prefs.with(|v| match v["values"]["theme"].as_str() {
            Some("dark") => Appearance::Dark,
            Some("light") => Appearance::Light,
            _ => Appearance::System,
        }));
    });
    set_interval(std::time::Duration::from_millis(900), move || {
        if app.running() {
            app.refresh_thread();
        }
    });
    on_key("mod+n", move |_| app.new_chat());
    on_key("mod+enter", move |_| {
        if app.page.get_untracked() == Page::Chat {
            app.send();
        }
    });
    shell::workspace(app)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
