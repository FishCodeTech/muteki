//! Admission is bounded before crossing the transport task boundary. A line
//! remains charged while a disconnected adapter holds it for retry.
use crate::recovery::Wake;
use async_channel::{Receiver, Sender};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

pub const MAX_LINES: usize = 256;
/// The queue must admit one line of the protocol's maximum size on its own.
pub const MAX_BYTES: usize = tcode_protocol::MAX_LINE_BYTES;

#[derive(Default)]
struct Queue {
    lines: VecDeque<String>,
    subscriptions: VecDeque<(String, String)>,
    count: usize,
    bytes: usize,
}

#[derive(Clone)]
pub struct Outgoing {
    sender: Destination,
    queue: Option<Arc<Mutex<Queue>>>,
    wake: Option<Sender<Wake>>,
}

#[derive(Clone)]
enum Destination {
    Serialized(Sender<String>),
    Attached(crate::attachment::AttachmentSender),
}

impl Destination {
    fn try_send(&self, line: String) -> Result<(), async_channel::TrySendError<String>> {
        match self {
            Self::Serialized(sender) => sender.try_send(line),
            Self::Attached(sender) => sender.try_send(line),
        }
    }
    fn close(&self) -> bool {
        match self {
            Self::Serialized(sender) => sender.close(),
            Self::Attached(sender) => sender.close(),
        }
    }
    fn is_closed(&self) -> bool {
        match self {
            Self::Serialized(sender) => sender.is_closed(),
            Self::Attached(sender) => sender.is_closed(),
        }
    }
}

pub struct OutgoingReceiver {
    receiver: Receiver<String>,
    queue: Arc<Mutex<Queue>>,
    pub wake: Receiver<Wake>,
}

pub fn channel() -> (Outgoing, OutgoingReceiver) {
    let (sender, receiver) = async_channel::bounded(1);
    let (wake_tx, wake) = async_channel::unbounded();
    let queue = Arc::new(Mutex::new(Queue::default()));
    (
        Outgoing {
            sender: Destination::Serialized(sender),
            queue: Some(queue.clone()),
            wake: Some(wake_tx),
        },
        OutgoingReceiver {
            receiver,
            queue,
            wake,
        },
    )
}

impl From<Sender<String>> for Outgoing {
    fn from(sender: Sender<String>) -> Self {
        Self {
            sender: Destination::Serialized(sender),
            queue: None,
            wake: None,
        }
    }
}

impl From<crate::attachment::AttachmentSender> for Outgoing {
    fn from(sender: crate::attachment::AttachmentSender) -> Self {
        Self {
            sender: Destination::Attached(sender),
            queue: None,
            wake: None,
        }
    }
}

impl Outgoing {
    pub fn try_send(&self, line: String) -> Result<(), async_channel::TrySendError<String>> {
        let Some(queue) = &self.queue else {
            return self.sender.try_send(line);
        };
        if self.sender.is_closed() {
            return Err(async_channel::TrySendError::Closed(line));
        }
        let mut queue = queue.lock().unwrap();
        if let Some(key) = subscription_key(&line) {
            // Recovery queues Scope before the other baselines. Coalescing must
            // keep the latest requests in admission order, even under backpressure.
            queue.subscriptions.retain(|(topic, _)| topic != &key);
            queue.subscriptions.push_back((key, line));
        } else {
            if queue.count >= MAX_LINES || line.len() > MAX_BYTES.saturating_sub(queue.bytes) {
                log::error!("outgoing queue full; rejecting new non-subscription line");
                return Err(async_channel::TrySendError::Full(line));
            }
            queue.count += 1;
            queue.bytes += line.len();
            queue.lines.push_back(line);
        }
        // A single notification covers all queued work, including coalesced topics.
        let _ = self.sender.try_send(String::new());
        Ok(())
    }

    pub async fn send(&self, line: String) -> Result<(), async_channel::TrySendError<String>> {
        self.try_send(line)
    }

    pub fn send_blocking(&self, line: String) -> Result<(), async_channel::TrySendError<String>> {
        self.try_send(line)
    }

    pub fn close(&self) -> bool {
        self.sender.close()
    }
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
    pub fn queued(&self) -> usize {
        self.queue
            .as_ref()
            .map_or(0, |queue| queue.lock().unwrap().count)
    }
    pub fn wake(&self, wake: Wake) {
        if let Some(sender) = &self.wake {
            let _ = sender.try_send(wake);
        }
    }
}

impl OutgoingReceiver {
    pub async fn recv(&self) -> Result<String, async_channel::RecvError> {
        loop {
            {
                let mut queue = self.queue.lock().unwrap();
                if let Some((_, line)) = queue.subscriptions.pop_front() {
                    return Ok(line);
                }
                if let Some(line) = queue.lines.pop_front() {
                    return Ok(line);
                }
            }
            self.receiver.recv().await?;
        }
    }
    pub fn discard_retained_writes(&self, buffered: &mut VecDeque<String>) {
        buffered.retain(|line| {
            if retained_write(line) {
                self.sent(line);
                false
            } else {
                true
            }
        });
        let mut queue = self.queue.lock().unwrap();
        let lines = std::mem::take(&mut queue.lines);
        for line in lines {
            if retained_write(&line) {
                queue.count -= 1;
                queue.bytes -= line.len();
            } else {
                queue.lines.push_back(line);
            }
        }
    }

    pub fn sent(&self, line: &str) {
        if subscription_key(line).is_none() {
            let mut queue = self.queue.lock().unwrap();
            queue.count -= 1;
            queue.bytes -= line.len();
        }
    }
    pub fn close(&self) -> bool {
        self.receiver.close()
    }
    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }
}

pub fn subscription_key(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let payload = value.get("payload")?;
    if !matches!(payload.get("type")?.as_str()?, "subscribe" | "unsubscribe") {
        return None;
    }
    serde_json::to_string(payload.get("content")?.get("topic")?).ok()
}

fn retained_write(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .is_some_and(|value| value.get("key").is_some_and(serde_json::Value::is_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_counts_inflight_bytes_and_preserves_older_lines() {
        let (sender, receiver) = channel();
        for index in 0..MAX_LINES {
            sender.try_send(index.to_string()).unwrap();
        }
        assert!(matches!(
            sender.try_send("rejected".into()),
            Err(async_channel::TrySendError::Full(_))
        ));
        let first = smol::block_on(receiver.recv()).unwrap();
        assert_eq!(first, "0");
        assert_eq!(
            sender.queued(),
            256,
            "inflight lines remain charged until sent"
        );
        receiver.sent(&first);
        sender.try_send("accepted".into()).unwrap();
        for index in 1..MAX_LINES {
            assert_eq!(smol::block_on(receiver.recv()).unwrap(), index.to_string());
        }
        assert_eq!(smol::block_on(receiver.recv()).unwrap(), "accepted");
        let (sender, receiver) = channel();
        sender.try_send("x".repeat(MAX_BYTES)).unwrap();
        assert!(sender.try_send("x".into()).is_err());
        let line = smol::block_on(receiver.recv()).unwrap();
        receiver.sent(&line);
        assert_eq!(sender.queued(), 0);
        assert!(sender.try_send("x".repeat(MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn subscriptions_coalesce_per_topic_and_unsubscribe_survives_a_full_queue() {
        let (sender, receiver) = channel();
        sender.try_send("x".repeat(MAX_BYTES)).unwrap();
        for id in 0..1000 {
            sender.try_send(format!(r#"{{"id":{id},"payload":{{"type":"subscribe","content":{{"topic":{{"type":"index"}},"after":null}}}}}}"#)).unwrap();
        }
        sender.try_send(r#"{"id":1000,"payload":{"type":"unsubscribe","content":{"topic":{"type":"index"}}}}"#.into()).unwrap();
        sender.try_send(r#"{"id":1001,"payload":{"type":"subscribe","content":{"topic":{"type":"settings"}}}}"#.into()).unwrap();
        let line = smol::block_on(receiver.recv()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            serde_json::json!({"id":1000,"payload":{"type":"unsubscribe","content":{"topic":{"type":"index"}}}})
        );
        let other = smol::block_on(receiver.recv()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&other).unwrap()["id"],
            1001
        );
        assert_eq!(sender.queued(), 1);
        assert_eq!(smol::block_on(receiver.recv()).unwrap().len(), MAX_BYTES);
    }
}
