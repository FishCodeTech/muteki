use tcode_protocol::{HostMessage, MAX_LINE_BYTES, QueryResponse, encode_line};

/// Process-local capabilities and resource limits selected by the host adapter.
#[derive(Clone, Copy, Debug)]
pub struct AttachmentPolicy {
    pub read_budget: Option<u64>,
    pub bulk_stream: bool,
    pub transfers: Option<TransferLimits>,
    pub queue: QueuePolicy,
}

#[derive(Clone, Copy, Debug)]
pub struct TransferLimits {
    pub total: usize,
    pub per_device: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum QueuePolicy {
    Unbounded,
    Disconnect {
        max_line_bytes: usize,
        max_queued_bytes: usize,
        max_queued_messages: usize,
    },
}

impl AttachmentPolicy {
    pub const LOCAL: Self = Self {
        read_budget: None,
        bulk_stream: false,
        transfers: None,
        queue: QueuePolicy::Unbounded,
    };

    pub fn network() -> Self {
        let empty = HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::FileBytes(Vec::new())),
        };
        let envelope = encode_line(&empty).unwrap().len();
        Self {
            // Reserve the largest reply id and base64's four bytes per three.
            read_budget: Some((MAX_LINE_BYTES.saturating_sub(envelope) / 4 * 3) as u64),
            bulk_stream: true,
            transfers: Some(TransferLimits {
                total: 8,
                per_device: 2,
            }),
            queue: QueuePolicy::Disconnect {
                max_line_bytes: MAX_LINE_BYTES,
                max_queued_bytes: 2 * MAX_LINE_BYTES,
                max_queued_messages: 1024,
            },
        }
    }

    pub fn read_limit(self, requested: Option<u64>) -> Option<u64> {
        match (self.read_budget, requested) {
            (Some(budget), Some(requested)) => Some(budget.min(requested)),
            (budget, requested) => budget.or(requested),
        }
    }
}

/// Trusted ingress metadata is carried outside the serialized client message.
pub struct AttachmentMessage {
    pub line: String,
    pub policy: AttachmentPolicy,
}

/// A process-local sender bound to its attachment's policy at construction.
#[derive(Clone)]
pub struct AttachmentSender {
    sender: async_channel::Sender<AttachmentMessage>,
    policy: AttachmentPolicy,
}

impl AttachmentSender {
    pub fn new(sender: async_channel::Sender<AttachmentMessage>, policy: AttachmentPolicy) -> Self {
        Self { sender, policy }
    }

    pub fn try_send(&self, line: String) -> Result<(), async_channel::TrySendError<String>> {
        self.sender
            .try_send(AttachmentMessage {
                line,
                policy: self.policy,
            })
            .map_err(|error| match error {
                async_channel::TrySendError::Closed(message) => {
                    async_channel::TrySendError::Closed(message.line)
                }
                async_channel::TrySendError::Full(message) => {
                    async_channel::TrySendError::Full(message.line)
                }
            })
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn send_blocking(&self, line: String) -> Result<(), async_channel::SendError<String>> {
        self.sender
            .send_blocking(AttachmentMessage {
                line,
                policy: self.policy,
            })
            .map_err(|error| async_channel::SendError(error.0.line))
    }

    pub fn close(&self) -> bool {
        self.sender.close()
    }

    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}
