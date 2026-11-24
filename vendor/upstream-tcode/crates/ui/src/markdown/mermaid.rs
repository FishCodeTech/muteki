//! Mermaid fences render as diagrams: a pure-Rust engine lays the source out
//! to SVG, which GPUI rasterizes like any other picture.

use std::{cell::RefCell, sync::Arc};

use gpui::{Image, ImageFormat};
use mermaid_rs_renderer::{LayoutConfig, RenderOptions, Theme, render_strict};

use super::render::RecentCache;

const DIAGRAM_CACHE_CAPACITY: usize = 32;

#[derive(Clone, Hash, PartialEq, Eq)]
struct DiagramKey {
    code: String,
    dark: bool,
}

thread_local! {
    static DIAGRAMS: RefCell<RecentCache<DiagramKey, Option<Arc<Image>>>> =
        RefCell::new(RecentCache::new(DIAGRAM_CACHE_CAPACITY));
}

pub(super) fn is_mermaid(lang: Option<&str>) -> bool {
    lang.is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"))
}

/// The laid-out diagram, or `None` while the source does not parse, which is
/// the ordinary state of a fence still streaming in: the caller shows the
/// code until it does.
pub(super) fn diagram(code: &str, dark: bool) -> Option<Arc<Image>> {
    let key = DiagramKey {
        code: code.to_string(),
        dark,
    };
    DIAGRAMS.with(|cache| {
        cache
            .borrow_mut()
            .get_or_insert_with(key, || render(code, dark))
    })
}

fn render(code: &str, dark: bool) -> Option<Arc<Image>> {
    let mut theme = if dark { Theme::dark() } else { Theme::modern() };
    // The fence paints the surface; the engine's own page color would sit as a
    // slab of another shade inside it.
    theme.background = "transparent".to_string();
    let options = RenderOptions {
        theme,
        layout: LayoutConfig::default(),
    };
    // Every token of a streaming fence reaches the engine as a new partial
    // source; a panic on one of them must not take the window down.
    let svg = std::panic::catch_unwind(|| render_strict(code, options))
        .ok()?
        .ok()?;
    Some(Arc::new(Image::from_bytes(
        ImageFormat::Svg,
        svg.into_bytes(),
    )))
}
