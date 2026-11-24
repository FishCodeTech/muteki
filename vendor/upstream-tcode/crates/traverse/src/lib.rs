//! Traverse: the iroh transport for native Tcode clients.
//!
//! One machine runs a [`TraverseHost`]; devices pair with it once and then
//! open a reconnecting [`Transport`](tcode_client::host::Transport) to it.
//! Everything runs on one process-wide tokio runtime; the public API is
//! synchronous and hands out channels, so the GPUI side never sees tokio.
//!
//! On a LAN a paired machine is found again without any service: it
//! advertises DNS-SD and devices resolve it through [`lan`].
//!
//! Two optional layers share the runtime: `native` is the client host and
//! the Preview adapters a native client shows a machine's pages through, and
//! `browser` is the plain HTTP listener a headless machine serves the browser
//! client from.

#[cfg(feature = "browser")]
pub mod browser;
pub mod client;
pub mod group;
pub mod host;
pub mod hosts;
#[cfg(any(feature = "browser", feature = "native"))]
mod http;
pub mod identity;
pub mod lan;
pub mod manifest;
pub mod mux;
#[cfg(feature = "native")]
pub mod native_host;
mod original;
#[cfg(feature = "native")]
pub mod preview;
mod runtime;
mod tunnel;
pub mod wire;

pub use client::{AttachmentTunnels, PairError, connect, pair, pair_blocking};
pub use host::{DeviceInfo, EndpointAddrSnapshot, HostConfig, Invitation, TraverseHost};
pub use identity::DeviceIdentity;
pub use mux::{Connection, HostMux};
#[cfg(feature = "native")]
pub use native_host::NativeClientHost;
pub use runtime::{block_on, runtime};
pub use tcode_protocol::PathInfo;
