//! Shared preview reverse-RPC payloads used by clients and the MCP broker.

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreviewRequest {
    /// Open a URL (creating/showing the webview); `None` just reports status.
    Open { url: Option<String> },
    /// Navigate the current webview to `url`.
    Navigate { url: String },
    /// Report the current URL / title / loading state.
    Status,
    /// Evaluate a JS expression in the page and return its value.
    Evaluate { js: String },
    /// Dispatch a DOM click at the center of the first `selector` match.
    Click { selector: String },
    /// Focus `selector` and type `text` into it (dispatching input events).
    Type { selector: String, text: String },
    /// Set a fixed WebView canvas, or clear it when both dimensions are `None`.
    Resize {
        width: Option<u32>,
        height: Option<u32>,
    },
    /// Dispatch a keyboard press to the focused page element.
    Press { key: String, modifiers: Vec<String> },
    /// Scroll the window or the first element matching `selector`.
    Scroll {
        delta_x: f64,
        delta_y: f64,
        selector: Option<String>,
    },
    /// Poll page state until all requested conditions match or time out.
    WaitFor {
        selector: Option<String>,
        text: Option<String>,
        url_includes: Option<String>,
        timeout_ms: u64,
    },
    /// Build a DOM outline of interactive elements (role/name/selector), capped.
    Snapshot,
    /// Capture the visible webview region as a PNG.
    Screenshot,
}

/// The UI's answer to a [`PreviewRequest`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PreviewResponse {
    /// A JSON payload (status, snapshot, evaluate result, `{ "ok": true }`, …).
    Json(serde_json::Value),
    /// A base64-encoded image plus its MIME type (screenshot).
    Image { mime: String, data_base64: String },
}
