//! The native Agent runtime: engine descriptors, binary resolution, the
//! access-mode policy and the live session pool.
//!
//! Sessions are run by the vendored tcode adapters (`vendor/upstream-tcode`),
//! which own each engine's wire protocol. This module owns what Muteki adds on
//! top: which engines and transports exist, which binary to run, how a
//! conversation's access mode is enforced, and the mapping from a thread to its
//! live session.

pub mod access;
pub mod binaries;
pub mod catalog;
pub mod credentials;
#[cfg(test)]
mod claude_plan_tests;
pub mod descriptors;
pub mod host_login;
pub mod ledger;
pub mod native_drivers;
pub mod pool;
pub mod translate;
pub mod version_check;
pub mod version_policy;

pub use descriptors::{
    AdapterDescriptor, AgentCapabilities, CustomTransport, EngineDescriptor, NativeTransport,
};
pub use pool::{Pool, Session, StartRequest};

/// A runtime failure with a stable code. Control flow never depends on the
/// message text, and an unimplemented capability is reported as such instead of
/// being answered by a different transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl RuntimeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: true,
        }
    }

    /// No descriptor declares this engine or adapter id.
    pub fn unknown_provider(value: &str) -> Self {
        Self::new(
            "provider.descriptor_unknown",
            format!("no provider descriptor for {value}"),
        )
    }

    /// The capability exists in the product but has no implementation on this
    /// transport. Never answered by silently using another one.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("capability.unsupported", message)
    }

    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
        })
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RuntimeError {}
