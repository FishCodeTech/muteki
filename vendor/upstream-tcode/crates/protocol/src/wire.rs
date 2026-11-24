use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Command, CommandResponse, EventEnvelope, Query, QueryResponse, Topic};

/// Bound on one encoded network protocol line in either direction. In-process
/// connections have no frame limit. Network queues admit the largest
/// single payload — one attachment of [`tcode_core::attachments::MAX_BYTES`]
/// bytes, base64-encoded — always fits.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl ProtocolError {
    pub fn out_of_scope(message: impl Into<String>) -> Self {
        Self {
            code: "out_of_scope".to_string(),
            message: message.into(),
        }
    }

    pub fn decode(message: impl Into<String>) -> Self {
        Self {
            code: "decode_error".to_string(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub payload: ClientPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<Principal>,
}

/// Host-internal: set by transport bridges; absent means the local in-process pipe (Full).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Principal {
    Full,
    Space {
        #[serde(default)]
        policy_revision: u64,
        space_id: String,
        space_name: String,
        project_ids: Vec<String>,
        device_id: String,
        device_name: String,
    },
}

#[allow(clippy::large_enum_variant)] // Boxing would complicate the public contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum ClientPayload {
    Command(Command),
    Query(Query),
    Subscribe(Subscription),
    Unsubscribe(Subscription),
}

#[allow(clippy::large_enum_variant)] // Wire messages favor a direct typed API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum HostMessage {
    Ack {
        id: u64,
        result: Result<CommandResponse, ProtocolError>,
    },
    QueryResult {
        id: u64,
        result: Result<QueryResponse, ProtocolError>,
    },
    Event(EventEnvelope),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// Fingerprint of the applied index; only index topics use this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_index: Option<String>,
    pub topic: Topic,
    #[serde(default)]
    pub after: Option<u64>,
}

/// Encode one NDJSON record, including its trailing newline.
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, ProtocolError> {
    serde_json::to_string(value)
        .map(|mut line| {
            line.push('\n');
            line
        })
        .map_err(|error| ProtocolError {
            code: "encode_error".to_string(),
            message: error.to_string(),
        })
}

/// Decode one client NDJSON record.
///
/// Unknown variants of data-carrying enums become a structured error here
/// rather than escaping as a panic.
pub fn decode_client_line(line: &str) -> Result<ClientMessage, ProtocolError> {
    decode_line(line)
}

/// Decode one host NDJSON record.
pub fn decode_host_line(line: &str) -> Result<HostMessage, ProtocolError> {
    decode_line(line)
}

fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, ProtocolError> {
    serde_json::from_str(line.trim_end()).map_err(|error| ProtocolError::decode(error.to_string()))
}

pub(crate) mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        STANDARD.decode(&value).map_err(D::Error::custom)
    }
}
