//! In-process [Model Context Protocol] server exposing the preview browser to agents.
//!
//! Agent CLIs connect over loopback streamable HTTP with per-session bearer
//! tokens. [`Broker`] passes each [`PreviewOp`] to the host, which routes it
//! through serialized reverse RPC to a local or remote client owning a WebView.
//! The client returns a [`PreviewReply`] over the same protocol.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io

use std::time::Duration;

pub mod js;
pub mod ports;
mod tools;

pub use tools::{INSTRUCTIONS, tool_descriptions};

/// Fixed preview canvas presets as `(id, portrait_width, portrait_height)` in
/// CSS pixels.
pub const PREVIEW_PRESETS: &[(&str, u32, u32)] = &[
    ("iphone-se", 375, 667),
    ("iphone-xr", 414, 896),
    ("iphone-12-pro", 390, 844),
    ("iphone-14-pro-max", 430, 932),
    ("pixel-7", 412, 915),
    ("galaxy-s20-ultra", 412, 915),
    ("ipad-mini", 768, 1024),
    ("ipad-air", 820, 1180),
    ("ipad-pro-12-9", 1024, 1366),
    ("surface-pro-7", 912, 1368),
];

pub use tcode_protocol::{PreviewRequest as PreviewOp, PreviewResponse as PreviewReply};

/// One in-flight automation request handed to the host: a [`PreviewOp`] plus a
/// bounded channel for its reply. `Ok` = success payload,
/// `Err` = human-readable failure (surfaced to the agent as a tool error).
#[derive(Debug)]
pub struct BrokerRequest {
    pub session_id: String,
    pub op: PreviewOp,
    pub reply: async_channel::Sender<Result<PreviewReply, String>>,
}

/// The server-side half of the broker: MCP tool handlers call [`Broker::invoke`]
/// to run an op against the UI and await the reply. Cloneable so every tool
/// call shares the one request channel.
pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type TokenRegistry = mcp_host::TokenRegistry<tools::Service>;

/// A running preview MCP server: the URL + per-session bearer-token issuer to
/// register with agents, and the receiver the host pumps to route automation.
pub struct PreviewMcpServer {
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/preview`.
    pub url: String,
    /// Per-session bearer-token registry.
    pub tokens: TokenRegistry,
    /// Automation requests for the host to route to a client WebView. Dropping
    /// this single-consumer receiver makes [`Broker::invoke`] fail fast.
    pub requests: async_channel::Receiver<BrokerRequest>,
}

pub fn start(host: &mut mcp_host::Host) -> PreviewMcpServer {
    let url = host.url("/preview");
    let (req_tx, req_rx) = async_channel::unbounded::<BrokerRequest>();
    let broker = Broker::new(
        req_tx,
        Duration::from_secs(65),
        mcp_host::BrokerErrors {
            unavailable: "preview UI is not available",
            dropped: "preview UI dropped the request",
            timed_out: "preview operation timed out",
        },
    );
    let tokens = TokenRegistry::new(move |session_id| tools::service(broker.clone(), session_id));
    host.mount(mcp_host::route("/preview", &tokens));

    log::info!("preview-mcp: serving at {url}");
    PreviewMcpServer {
        url,
        tokens,
        requests: req_rx,
    }
}
