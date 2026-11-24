//! The rmcp tool surface. Each tool turns its arguments into a [`PreviewOp`],
//! runs it through the [`Broker`], and maps the reply into an MCP result.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use std::sync::Arc;

use crate::{Broker, PREVIEW_PRESETS, PreviewOp, PreviewReply};

/// Optional-URL argument for `preview_open`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenParams {
    /// URL to load. When omitted, the tool just shows the current webview.
    #[serde(default)]
    pub url: Option<String>,
}

/// Required-URL argument for `preview_navigate`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NavigateParams {
    /// Absolute URL (or `localhost:PORT`) to navigate the preview to.
    pub url: String,
}

/// A JavaScript expression to evaluate in the page.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvaluateParams {
    /// A JS expression; its value is returned (must be JSON-serializable).
    pub js: String,
}

/// A CSS selector to target.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickParams {
    /// CSS selector of the element to click (first match).
    pub selector: String,
}

/// A CSS selector plus text to type.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeParams {
    /// CSS selector of the field to type into (first match).
    pub selector: String,
    /// Text to set as the field's value.
    pub text: String,
}

/// Fixed-canvas dimensions or a named device preset.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResizeParams {
    /// Canvas mode: `fill` uses the whole panel; `fixed` uses a device-sized canvas.
    pub mode: String,
    /// Fixed canvas width in CSS pixels.
    #[serde(default)]
    pub width: Option<i64>,
    /// Fixed canvas height in CSS pixels.
    #[serde(default)]
    pub height: Option<i64>,
    /// Named device preset, as an alternative to width and height.
    #[serde(default)]
    pub preset: Option<String>,
    /// `portrait` (default) or `landscape`.
    #[serde(default)]
    pub orientation: Option<String>,
}

/// A key and optional modifier keys to dispatch.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PressParams {
    /// A `KeyboardEvent.key` value such as `Enter`, `Escape`, `Tab`, `a`, or `F5`.
    pub key: String,
    /// Zero or more of `Alt`, `Control`, `Meta`, and `Shift`.
    #[serde(default)]
    pub modifiers: Vec<String>,
}

/// Relative scrolling parameters.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScrollParams {
    /// Horizontal scroll delta in CSS pixels.
    #[serde(default)]
    pub delta_x: Option<f64>,
    /// Vertical scroll delta in CSS pixels.
    #[serde(default)]
    pub delta_y: Option<f64>,
    /// CSS selector of a scroll container; omitted to scroll the window.
    #[serde(default)]
    pub selector: Option<String>,
}

/// Page conditions to poll until all are satisfied.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitForParams {
    /// CSS selector that must match an element.
    #[serde(default)]
    pub selector: Option<String>,
    /// Substring that must appear in the document text.
    #[serde(default)]
    pub text: Option<String>,
    /// Substring that must appear in the current URL.
    #[serde(default)]
    pub url_includes: Option<String>,
    /// Maximum wait in milliseconds (default 15000, clamped to 1000..=60000).
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

const MIN_CANVAS_DIMENSION: i64 = 240;
const MAX_CANVAS_DIMENSION: i64 = 3840;
const MAX_CANVAS_AREA: u64 = 3840 * 2160;

fn resize_op(params: ResizeParams) -> Result<PreviewOp, String> {
    let landscape = match params.orientation.as_deref().unwrap_or("portrait") {
        "portrait" => false,
        "landscape" => true,
        other => {
            return Err(format!(
                "invalid orientation {other:?}; use portrait or landscape"
            ));
        }
    };
    match params.mode.as_str() {
        "fill" => Ok(PreviewOp::Resize {
            width: None,
            height: None,
        }),
        "fixed" => {
            let (mut width, mut height) = if let Some(preset) = params.preset {
                if params.width.is_some() || params.height.is_some() {
                    return Err(
                        "preset is an alternative to width and height; provide only one".into(),
                    );
                }
                PREVIEW_PRESETS
                    .iter()
                    .find(|(id, _, _)| *id == preset)
                    .map(|(_, width, height)| (*width, *height))
                    .ok_or_else(|| format!("unknown preview preset {preset:?}"))?
            } else {
                let width = params
                    .width
                    .ok_or_else(|| "fixed mode requires width and height, or a preset".to_string())?
                    .clamp(MIN_CANVAS_DIMENSION, MAX_CANVAS_DIMENSION)
                    as u32;
                let height = params
                    .height
                    .ok_or_else(|| "fixed mode requires width and height, or a preset".to_string())?
                    .clamp(MIN_CANVAS_DIMENSION, MAX_CANVAS_DIMENSION)
                    as u32;
                (width, height)
            };
            if u64::from(width) * u64::from(height) > MAX_CANVAS_AREA {
                return Err(format!(
                    "fixed canvas area must not exceed {MAX_CANVAS_AREA} CSS pixels"
                ));
            }
            if landscape {
                std::mem::swap(&mut width, &mut height);
            }
            Ok(PreviewOp::Resize {
                width: Some(width),
                height: Some(height),
            })
        }
        other => Err(format!("invalid mode {other:?}; use fill or fixed")),
    }
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// The MCP server handler: one shared [`Broker`] plus the generated tool router.
#[derive(Clone)]
pub struct PreviewTools {
    broker: Broker,
    session_id: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl PreviewTools {
    pub fn new(broker: Broker, session_id: String) -> Self {
        Self {
            broker,
            session_id,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Open the Tcode preview browser, optionally at a URL, and return its status. \
                       For browser work, call preview_status first and call preview_open if no automation-capable preview is attached before concluding the browser is unavailable. \
                       Do not fall back to Chrome, Playwright, or another browser merely because the preview is initially closed or a first call fails; fall back only when preview_open explicitly reports unsupported or unavailable."
    )]
    async fn preview_open(&self, Parameters(params): Parameters<OpenParams>) -> CallToolResult {
        self.run(PreviewOp::Open { url: params.url }).await
    }

    #[tool(description = "Navigate the Tcode preview browser to a URL and return its status.")]
    async fn preview_navigate(
        &self,
        Parameters(params): Parameters<NavigateParams>,
    ) -> CallToolResult {
        self.run(PreviewOp::Navigate { url: params.url }).await
    }

    #[tool(
        description = "Report the preview browser's current URL, title, and loading state; call this first for browser work. \
                       If no automation-capable preview is attached, call preview_open before concluding the browser is unavailable. \
                       Do not fall back to Chrome, Playwright, or another browser merely because the preview is initially closed or a first call fails; fall back only when preview_open explicitly reports unsupported or unavailable. \
                       Includes load_error with the platform error when the last navigation failed, e.g. an untrusted certificate."
    )]
    async fn preview_status(&self) -> CallToolResult {
        self.run(PreviewOp::Status).await
    }

    #[tool(
        description = "Evaluate a JavaScript expression in the preview page and return its value."
    )]
    async fn preview_evaluate(
        &self,
        Parameters(params): Parameters<EvaluateParams>,
    ) -> CallToolResult {
        self.run(PreviewOp::Evaluate { js: params.js }).await
    }

    #[tool(description = "Click the first element matching a CSS selector in the preview page.")]
    async fn preview_click(&self, Parameters(params): Parameters<ClickParams>) -> CallToolResult {
        self.run(PreviewOp::Click {
            selector: params.selector,
        })
        .await
    }

    #[tool(
        description = "Type text into the first element matching a CSS selector in the preview page."
    )]
    async fn preview_type(&self, Parameters(params): Parameters<TypeParams>) -> CallToolResult {
        self.run(PreviewOp::Type {
            selector: params.selector,
            text: params.text,
        })
        .await
    }

    #[tool(
        description = "Resize the preview canvas. mode is fill or fixed. Fixed mode accepts width/height or one preset: iphone-se, iphone-xr, iphone-12-pro, iphone-14-pro-max, pixel-7, galaxy-s20-ultra, ipad-mini, ipad-air, ipad-pro-12-9, surface-pro-7."
    )]
    async fn preview_resize(&self, Parameters(params): Parameters<ResizeParams>) -> CallToolResult {
        let op = match resize_op(params) {
            Ok(op) => op,
            Err(message) => return tool_error(message),
        };
        self.run(op).await
    }

    #[tool(
        description = "Press a key in the preview page, optionally with Alt, Control, Meta, or Shift modifiers."
    )]
    async fn preview_press(&self, Parameters(params): Parameters<PressParams>) -> CallToolResult {
        if let Some(modifier) = params
            .modifiers
            .iter()
            .find(|modifier| !matches!(modifier.as_str(), "Alt" | "Control" | "Meta" | "Shift"))
        {
            return tool_error(format!(
                "invalid modifier {modifier:?}; use Alt, Control, Meta, or Shift"
            ));
        }
        self.run(PreviewOp::Press {
            key: params.key,
            modifiers: params.modifiers,
        })
        .await
    }

    #[tool(
        description = "Scroll the preview window or a CSS-selected container by deltaX and deltaY."
    )]
    async fn preview_scroll(&self, Parameters(params): Parameters<ScrollParams>) -> CallToolResult {
        if params.delta_x.is_none() && params.delta_y.is_none() {
            return tool_error("preview_scroll requires deltaX or deltaY");
        }
        self.run(PreviewOp::Scroll {
            delta_x: params.delta_x.unwrap_or(0.0),
            delta_y: params.delta_y.unwrap_or(0.0),
            selector: params.selector,
        })
        .await
    }

    #[tool(
        description = "Wait for a selector, document-text substring, and/or URL substring in the preview page."
    )]
    async fn preview_wait_for(
        &self,
        Parameters(params): Parameters<WaitForParams>,
    ) -> CallToolResult {
        if params.selector.is_none() && params.text.is_none() && params.url_includes.is_none() {
            return tool_error("preview_wait_for requires selector, text, or urlIncludes");
        }
        let timeout_ms = params.timeout_ms.unwrap_or(15_000).clamp(1_000, 60_000) as u64;
        self.run(PreviewOp::WaitFor {
            selector: params.selector,
            text: params.text,
            url_includes: params.url_includes,
            timeout_ms,
        })
        .await
    }

    #[tool(
        description = "Snapshot the preview page: URL, title, visible text, and interactive elements (role/name/selector). Prefer snapshot-provided locators over coordinates."
    )]
    async fn preview_snapshot(&self) -> CallToolResult {
        self.run(PreviewOp::Snapshot).await
    }

    #[tool(description = "Capture a screenshot of the preview page as a PNG.")]
    async fn preview_screenshot(&self) -> CallToolResult {
        self.run(PreviewOp::Screenshot).await
    }

    /// Route one op through the broker and map its reply into a tool result.
    async fn run(&self, op: PreviewOp) -> CallToolResult {
        log::info!("preview-mcp: tool invoked: {op:?}");
        match self
            .broker
            .invoke(|reply| crate::BrokerRequest {
                session_id: self.session_id.clone(),
                op,
                reply,
            })
            .await
        {
            Ok(PreviewReply::Json(value)) => {
                let text =
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Ok(PreviewReply::Image { mime, data_base64 }) => {
                CallToolResult::success(vec![ContentBlock::image(data_base64, mime)])
            }
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        }
    }
}

/// Every tool's name and the description its model reads.
pub fn tool_descriptions() -> Vec<(String, String)> {
    PreviewTools::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.as_deref().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// The server's `instructions`, which the client reads at initialization.
pub const INSTRUCTIONS: &str = "Drive the Tcode embedded preview browser: open/navigate URLs, inspect and \
                 automate the page, resize its canvas, press keys, scroll, wait for page \
                 conditions, and capture screenshots. For browser work, call preview_status first \
                 and preview_open when no automation-capable preview is attached; use another \
                 browser only if preview_open explicitly reports unsupported or unavailable.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PreviewTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(INSTRUCTIONS)
    }
}

pub type Service = StreamableHttpService<PreviewTools, LocalSessionManager>;

pub fn service(broker: Broker, session_id: String) -> Service {
    StreamableHttpService::new(
        move || Ok(PreviewTools::new(broker.clone(), session_id.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(
        requests: async_channel::Sender<crate::BrokerRequest>,
        timeout: std::time::Duration,
    ) -> Broker {
        Broker::new(
            requests,
            timeout,
            mcp_host::BrokerErrors {
                unavailable: "preview UI is not available",
                dropped: "preview UI dropped the request",
                timed_out: "preview operation timed out",
            },
        )
    }

    #[tokio::test]
    async fn tools_preserve_session_and_map_replies_and_disconnects() {
        let (tx, rx) = async_channel::unbounded::<crate::BrokerRequest>();
        let broker = broker(tx, std::time::Duration::from_secs(30));
        let resolver = tokio::spawn(async move {
            for (op, reply) in [
                (
                    PreviewOp::Status,
                    PreviewReply::Json(serde_json::json!({"url":"https://x/"})),
                ),
                (
                    PreviewOp::Screenshot,
                    PreviewReply::Image {
                        mime: "image/png".into(),
                        data_base64: "AAA".into(),
                    },
                ),
            ] {
                let request = rx.recv().await.unwrap();
                assert_eq!(request.session_id, "session-a");
                assert_eq!(request.op, op);
                request.reply.send(Ok(reply)).await.unwrap();
            }
        });
        let tools = PreviewTools::new(broker, "session-a".into());
        let status = tools.preview_status().await;
        assert_eq!(status.is_error, Some(false));
        assert!(
            matches!(status.content.as_slice(), [ContentBlock::Text(text)]
            if serde_json::from_str::<serde_json::Value>(&text.text).unwrap() == serde_json::json!({"url":"https://x/"}))
        );
        let shot = tools.preview_screenshot().await;
        assert_eq!(shot.is_error, Some(false));
        assert!(
            matches!(shot.content.as_slice(), [ContentBlock::Image(image)]
            if image.mime_type == "image/png" && image.data == "AAA")
        );
        resolver.await.unwrap();
        let disconnected = tools.preview_status().await;
        assert_eq!(disconnected.is_error, Some(true));
        assert!(
            matches!(disconnected.content.as_slice(), [ContentBlock::Text(text)] if text.text == "preview UI is not available")
        );
    }
}
