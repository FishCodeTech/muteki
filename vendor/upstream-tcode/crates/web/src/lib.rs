//! Browser shell. Native workspace builds intentionally compile an empty lib.
#![cfg(target_family = "wasm")]

mod host;
mod keyboard;
mod transport;

use std::{borrow::Cow, cell::RefCell, rc::Rc};

use host::{WebHost, window};
#[cfg(feature = "debug-exports")]
use tcode_client::host::ClientHost as _;
use wasm_bindgen::prelude::*;

thread_local! {
    static APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
    static CANVAS_OBSERVER: RefCell<Option<CanvasObserver>> = const { RefCell::new(None) };
}

struct CanvasObserver {
    observer: web_sys::MutationObserver,
    _callback: Closure<dyn FnMut(js_sys::Array)>,
}

impl Drop for CanvasObserver {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

/// Adopt GPUI's asynchronously prepared canvas, including replacements
/// created when WebGPU falls back to WebGL.
fn prepare_canvas(canvas_id: &str) -> Result<(), JsValue> {
    let document = window().document().ok_or("missing document")?;
    let placeholder = document
        .get_element_by_id(canvas_id)
        .ok_or("missing canvas")?;
    if !placeholder.is_instance_of::<web_sys::HtmlCanvasElement>() {
        return Err(JsValue::from_str("start requires a canvas element"));
    }
    placeholder.remove();
    let canvas_id = canvas_id.to_owned();
    let callback = Closure::wrap(Box::new(move |records: js_sys::Array| {
        for record in records.iter() {
            let record: web_sys::MutationRecord = record.unchecked_into();
            let nodes = record.added_nodes();
            for index in 0..nodes.length() {
                if let Some(canvas) = nodes
                    .item(index)
                    .and_then(|node| node.dyn_into::<web_sys::HtmlCanvasElement>().ok())
                {
                    canvas.set_id(&canvas_id);
                    let _ = canvas.set_attribute("aria-label", "Tcode remote client");
                }
            }
        }
    }) as Box<dyn FnMut(js_sys::Array)>);
    let observer = web_sys::MutationObserver::new(callback.as_ref().unchecked_ref())?;
    let options = web_sys::MutationObserverInit::new();
    options.set_child_list(true);
    observer.observe_with_options(document.body().ok_or("missing body")?.as_ref(), &options)?;
    CANVAS_OBSERVER.with(|slot| {
        *slot.borrow_mut() = Some(CanvasObserver {
            observer,
            _callback: callback,
        })
    });
    Ok(())
}

#[wasm_bindgen]
pub async fn start(canvas_id: &str) -> Result<(), JsValue> {
    if APPLICATION.with(|slot| slot.borrow().is_some()) {
        return Ok(());
    }
    console_error_panic_hook::set_once();
    gpui_web::init_logging();
    prepare_canvas(canvas_id)?;
    let host = Rc::new(WebHost);
    // The login page already signed this browser in with the serving
    // machine; the shell reopens that record.
    let initial = tcode_ui::last_host_target(host.as_ref());
    // CJK the bundled fonts lack is drawn with the browser's own fonts
    // rather than as missing-glyph boxes.
    let platform = Rc::new(gpui_web::WebPlatform::new_with_backend_and_font_fallback(
        true,
        gpui_web::WebBackendPreference::Auto,
        gpui_web::CanvasFontFallback::EmojiAndCjk,
    ));
    let http_client = std::sync::Arc::new(platform.fetch_http_client());
    let application = gpui::Application::with_platform(platform)
        .with_http_client(http_client)
        .with_assets(tcode_ui::assets::Assets)
        .run_embedded(|cx| {
            let host: Rc<dyn tcode_client::host::ClientHost> = host;
            tcode_ui::run_shell(
                cx,
                host.clone(),
                tcode_ui::ShellOptions {
                    window: gpui::WindowOptions {
                        // GPUI takes the browser window's size from the canvas
                        // element; a fixed size here would be a lie.
                        window_bounds: None,
                        titlebar: None,
                        window_background: gpui::WindowBackgroundAppearance::Opaque,
                        ..Default::default()
                    },
                    fonts: vec![
                        Cow::Borrowed(include_bytes!("../assets/NotoSans-Regular.ttf")),
                        Cow::Borrowed(include_bytes!("../assets/NotoSansSC-Regular.otf")),
                        Cow::Borrowed(include_bytes!("../../../assets/fonts/DMSans[wght].ttf")),
                    ],
                    theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                    activate: true,
                    // `sys_locale` reads nothing in a browser; the browser's
                    // own language is the system one.
                    system_locale: window().navigator().language(),
                    setup: tcode_ui::ShellSetup {
                        initial,
                        initial_pairing_error: None,
                        client_host: Some(host),
                        // A browser tab runs no host of its own.
                        local: None,
                        local_failure: None,
                        seed_blocking: false,
                        restore_navigation: false,
                    },
                    ..Default::default()
                }
                .with_bundled_monospace(),
            );
            if let Some(document) = window().document() {
                if let Some(loading) = document.get_element_by_id("loading") {
                    loading.remove();
                }
                if let Some(body) = document.body() {
                    let _ = body.set_attribute("data-tcode-ready", "true");
                }
            }
        });
    APPLICATION.with(|slot| *slot.borrow_mut() = Some(application));
    keyboard::install();
    Ok(())
}

#[wasm_bindgen]
#[cfg(feature = "debug-exports")]
pub async fn debug_connect() -> String {
    let started = APPLICATION.with(|slot| slot.borrow().is_some());
    if !started {
        return serde_json::json!({"error":"call start first"}).to_string();
    }
    let host = WebHost;
    let Some(paired) = host.last_host_id().and_then(|id| {
        host.load_hosts()
            .into_iter()
            .find(|host| host.host_id == id)
    }) else {
        return serde_json::json!({"error":"not signed in"}).to_string();
    };
    let transport = host.connect(&paired);
    let _ = transport.to_host.try_send(
        r#"{"id":1,"payload":{"type":"subscribe","content":{"topic":{"type":"index"}}}}"#.into(),
    );
    let line = transport
        .from_host
        .recv()
        .await
        .unwrap_or_else(|error| serde_json::json!({"error":error.to_string()}).to_string());
    transport.to_host.close();
    transport.from_host.close();
    line
}
