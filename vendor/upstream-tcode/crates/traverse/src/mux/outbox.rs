use async_channel::{Receiver, Sender};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, TerminalFrame, encode_line};

use tcode_client::attachment::QueuePolicy;

struct Line {
    text: String,
    replace: Option<String>,
}

#[derive(Default)]
struct Pending {
    lines: VecDeque<Line>,
    // Includes the line waiting for the one-slot output channel.
    bytes: usize,
    closed: bool,
    terminals: HashMap<u64, Box<TerminalFrame>>,
}

pub(super) struct Outbox {
    pending: Arc<Mutex<Pending>>,
    wake: Sender<()>,
    input: Receiver<String>,
    queue: QueuePolicy,
}

impl Outbox {
    pub(super) fn new(
        input: Receiver<String>,
        queue: QueuePolicy,
    ) -> (Self, impl Future<Output = ()> + Send, Receiver<String>) {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (wake, awakened) = async_channel::bounded(1);
        let (output, received) = async_channel::bounded(1);
        let outbox = Self {
            pending: pending.clone(),
            wake,
            input,
            queue,
        };
        let forward = async move {
            loop {
                let line = {
                    let mut pending = pending.lock().unwrap();
                    if pending.closed {
                        return;
                    }
                    pending.lines.pop_front()
                };
                let Some(line) = line else {
                    if awakened.recv().await.is_err() {
                        return;
                    }
                    continue;
                };
                let bytes = line.text.len();
                if output.send(line.text).await.is_err() {
                    return;
                }
                let mut pending = pending.lock().unwrap();
                if !pending.closed {
                    pending.bytes -= bytes;
                }
            }
        };
        (outbox, forward, received)
    }

    pub(super) fn forget_terminal(&self, terminal_id: u64) {
        self.pending.lock().unwrap().terminals.remove(&terminal_id);
    }

    pub(super) fn try_send_terminal(&self, mut envelope: EventEnvelope) -> Result<(), ()> {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            return Err(());
        }
        let (terminal_id, ordered) = match &envelope.event {
            ServerEvent::TerminalFrame { terminal_id, frame } => {
                pending.terminals.insert(*terminal_id, frame.clone());
                (*terminal_id, false)
            }
            ServerEvent::TerminalDelta { terminal_id, delta } => {
                if let Some(frame) = pending.terminals.get_mut(terminal_id) {
                    frame.apply(delta);
                }
                (*terminal_id, delta.bell || delta.clipboard.is_some())
            }
            _ => unreachable!(),
        };
        // Bell and clipboard effects must reach the client in order, even when
        // the surrounding grid updates can be replaced by a complete frame.
        let replace = (envelope.request_id.is_none()
            && !ordered
            && pending.terminals.contains_key(&terminal_id))
        .then(|| format!("terminal:{terminal_id}"));
        if let Some(key) = &replace
            && pending
                .lines
                .iter()
                .rev()
                .take_while(|line| line.replace.is_some())
                .any(|line| line.replace.as_ref() == Some(key))
        {
            envelope.event = ServerEvent::TerminalFrame {
                terminal_id,
                frame: pending.terminals[&terminal_id].clone(),
            };
        }
        drop(pending);
        self.try_send(encode_line(&HostMessage::Event(envelope)).unwrap(), replace)
    }

    pub(super) fn try_send(&self, text: String, replace: Option<String>) -> Result<(), ()> {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed || self.input.is_closed() {
            return Err(());
        }
        if let Some(key) = &replace {
            // Never move a replacement across a reply or an ordered event.
            let previous = pending
                .lines
                .iter()
                .rposition(|line| line.replace.is_none() || line.replace.as_ref() == Some(key));
            if let Some(index) = previous
                && pending.lines[index].replace.as_ref() == Some(key)
            {
                let old = pending.lines.remove(index).unwrap();
                pending.bytes -= old.text.len();
            }
        }
        if let QueuePolicy::Disconnect {
            max_line_bytes,
            max_queued_bytes,
            max_queued_messages,
        } = self.queue
            && (text.len() > max_line_bytes
                || pending.bytes + text.len() > max_queued_bytes
                || pending.lines.len() >= max_queued_messages)
        {
            drop(pending);
            self.close();
            return Err(());
        }
        pending.bytes += text.len();
        pending.lines.push_back(Line { text, replace });
        drop(pending);
        let _ = self.wake.try_send(());
        Ok(())
    }

    fn close(&self) {
        let mut pending = self.pending.lock().unwrap();
        pending.closed = true;
        pending.lines.clear();
        pending.terminals.clear();
        pending.bytes = 0;
        self.input.close();
        self.wake.close();
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        // Host EOF must drain accepted replies before the forwarder closes input.
        self.wake.close();
    }
}
