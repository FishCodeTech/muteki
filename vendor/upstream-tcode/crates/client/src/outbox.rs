//! Ordered write ownership, independent of the lifetime of a transport socket.
use serde::{Deserialize, Serialize};
use tcode_protocol::{Command, ProtocolError};

pub const MAX_ITEMS: usize = 256;
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Platform persistence must replace the complete snapshot atomically.
pub trait Storage: Send + Sync + 'static {
    fn load(&self) -> Result<Vec<Entry>, ProtocolError>;
    fn save(&self, entries: &[Entry]) -> Result<(), ProtocolError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub command: Command,
}

pub fn storage_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError {
        code: "outbox_storage".into(),
        message: error.to_string(),
    }
}
