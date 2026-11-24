//! Rushdown-backed Markdown rendering and document text selection.
//!
//! The GPUI element architecture is adapted from gpui-component's Apache-2.0
//! `crates/ui/src/text` implementation. Parsing and highlighting use tcode's
//! rushdown IR and syntect bridge.

mod fitted_image;
mod image_link;
mod inline;
mod inline_flow;
mod link_target;
mod math;
mod math_layout;
mod math_parse;
mod mermaid;
pub(crate) mod nodes;
pub(crate) mod parse;
mod render;
mod selection;
mod state;
mod utils;
mod view;

use gpui::{App, KeyBinding};
use gpui_base::input::SelectAll;

pub use link_target::OpenThreadAnchor;
#[cfg(test)]
pub(crate) use parse::parse;
pub use selection::{SelectionArea, SelectionDocument};
pub use state::{ImageResolver, MarkdownState};
pub use view::{MarkdownView, MenuExtension};

pub(super) const CONTEXT: &str = "MarkdownView";

/// Register Markdown select-all bindings. Copying is the selection area's.
pub fn init(cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(
            latex_rust::STIX_TWO_MATH_OTF,
        )])
        .expect("embedded math font must load");
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
    ]);
}
