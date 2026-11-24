use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{ProtocolError, Query};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OriginalTarget {
    File {
        path: PathBuf,
    },
    Item {
        session_id: String,
        item_id: String,
        image_index: usize,
    },
}

impl OriginalTarget {
    pub fn query(&self) -> Query {
        match self.clone() {
            Self::File { path } => Query::ReadFileBytes {
                path,
                max_bytes: None,
            },
            Self::Item {
                session_id,
                item_id,
                image_index,
            } => Query::ReadItemImage {
                session_id,
                item_id,
                image_index,
                max_bytes: None,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalMetadata {
    pub len: u64,
    pub mime: Option<String>,
    /// The image's displayed width and height in pixels, orientation applied, when its header
    /// names them; known before its bytes arrive.
    pub dimensions: Option<(u32, u32)>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OriginalRequest {
    Original { target: OriginalTarget },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OriginalReply {
    OriginalReady {
        len: u64,
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dimensions: Option<(u32, u32)>,
    },
    OriginalFailed {
        error: ProtocolError,
    },
    OriginalComplete,
}
