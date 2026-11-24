//! Live feature configuration, pushed by the app whenever settings load or
//! change. The MCP server outlives any single settings snapshot, so tools read
//! the current value at call time instead of capturing one at startup.

use std::sync::RwLock;
use tcode_core::settings::ComputerUseSettings;
pub use tcode_core::settings::ImageMode;

static CONFIG: RwLock<ComputerUseSettings> = RwLock::new(ComputerUseSettings {
    enabled: false,
    allow_input: true,
    image_mode: ImageMode::Auto,
    allow_foreground_fallback: false,
    show_agent_cursor: true,
});

pub fn set(config: ComputerUseSettings) {
    let mut current = CONFIG.write().unwrap();
    crate::backend::set_feedback_enabled(config.enabled && config.show_agent_cursor);
    *current = config;
}

pub fn get() -> ComputerUseSettings {
    CONFIG.read().unwrap().clone()
}
