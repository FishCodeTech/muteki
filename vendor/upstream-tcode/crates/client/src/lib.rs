//! Transport-agnostic client endpoint for a tcode host.

pub mod attachment;
pub mod heartbeat;
pub mod host;
pub mod original;
pub mod outbox;
pub use original::OriginalTransfer;
pub mod outgoing;
pub mod pairing;
pub mod recovery;
pub mod speech;
pub mod words;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tcode_protocol::{
    ClientMessage, ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage,
    ProtocolError, Query, QueryResponse, Subscription, Topic, decode_host_line, encode_line,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Up, and how the link is carried when the transport can tell. A path
    /// change is a new `Connected` value, so it reaches every consumer
    /// through the state channel like any other change.
    Connected {
        path: Option<tcode_protocol::PathInfo>,
    },
    /// The link is up and the baseline is being loaded over it. The path is
    /// the same as `Connected` carries and a path change is a new `Syncing`
    /// value; only entering `Syncing` from another state restarts a sync.
    Syncing {
        path: Option<tcode_protocol::PathInfo>,
    },
    Reconnecting {
        attempt: u32,
        reason: Option<ConnectionFailure>,
    },
    Offline {
        reason: ConnectionFailure,
    },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// Transport-neutral cause of a connection failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionFailure {
    /// The transport's own account of why, when it can tell; a browser does
    /// not expose why a WebSocket failed.
    Unreachable(Option<String>),
    Timeout,
    AuthenticationRejected,
    ProtocolMismatch,
    HostClosed,
}

impl ConnectionFailure {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::AuthenticationRejected | Self::ProtocolMismatch)
    }

    /// Older hosts did not send machine-readable rejection reasons.
    pub fn hello_rejected(reason: Option<&str>) -> Self {
        match reason {
            Some("token") => Self::AuthenticationRejected,
            Some("protocol") => Self::ProtocolMismatch,
            reason => Self::Unreachable(reason.map(str::to_owned)),
        }
    }
}

struct SubscriptionRequest {
    id: u64,
    after: Option<u64>,
    consumed: bool,
}

struct Pending {
    sender: async_channel::Sender<HostMessage>,
    query: bool,
    retained: bool,
    started: web_time::Instant,
}

struct Write {
    id: u64,
    entry: outbox::Entry,
    sent: Option<web_time::Instant>,
    adopted: bool,
}

#[derive(Default)]
struct Delivery {
    writes: VecDeque<Write>,
    storage: Option<Arc<dyn outbox::Storage>>,
    failed: VecDeque<(outbox::Entry, ProtocolError)>,
    acknowledged: VecDeque<outbox::Entry>,
}

enum OriginalRoute {
    Local(Option<Arc<dyn original::OriginalOpener>>),
    Remote(Option<Arc<dyn original::OriginalOpener>>),
}

struct HostLinkInner {
    originals: OriginalRoute,
    to_host: outgoing::Outgoing,
    from_host: async_channel::Receiver<String>,
    pending: Mutex<HashMap<u64, Pending>>,
    delivery: Mutex<Delivery>,
    events_tx: async_channel::Sender<EventEnvelope>,
    events_rx: async_channel::Receiver<EventEnvelope>,
    delivery_changes: (async_channel::Sender<()>, async_channel::Receiver<()>),
    next_id: AtomicU64,
    // Keep intent changes and recovery replay atomic: a cloned recovery intent
    // must not be sent after a newer unsubscribe or cursor change.
    subscription_transition: Mutex<()>,
    subscribed_topics: Mutex<HashMap<Topic, Subscription>>,
    subscription_requests: Mutex<HashMap<Topic, SubscriptionRequest>>,
    connection_state: Mutex<ConnectionState>,
    connection_state_tx: async_channel::Sender<ConnectionState>,
    connection_state_rx: async_channel::Receiver<ConnectionState>,
}

/// A client endpoint whose only transport contract is a pair of NDJSON channels.
#[derive(Clone)]
pub struct HostLink {
    inner: Arc<HostLinkInner>,
}

/// A request owns its waiter, including when its future is cancelled.
struct PendingRequest<'a> {
    link: &'a HostLink,
    id: u64,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.link.inner.pending.lock().unwrap().remove(&self.id);
    }
}

pub type CommandFuture =
    Pin<Box<dyn Future<Output = Result<CommandResponse, ProtocolError>> + Send + 'static>>;

impl HostLink {
    pub fn new(
        to_host: impl Into<outgoing::Outgoing>,
        from_host: async_channel::Receiver<String>,
    ) -> Self {
        let (events_tx, events_rx) = async_channel::unbounded();
        let (connection_state_tx, connection_state_rx) = async_channel::unbounded();
        Self {
            inner: Arc::new(HostLinkInner {
                originals: OriginalRoute::Local(None),
                to_host: to_host.into(),
                from_host,
                pending: Mutex::new(HashMap::new()),
                delivery: Mutex::new(Delivery::default()),
                events_tx,
                events_rx,
                delivery_changes: async_channel::bounded(1),
                next_id: AtomicU64::new(1),
                subscription_transition: Mutex::new(()),
                subscribed_topics: Mutex::new(HashMap::new()),
                subscription_requests: Mutex::new(HashMap::new()),
                connection_state: Mutex::new(ConnectionState::Connected { path: None }),
                connection_state_tx,
                connection_state_rx,
            }),
        }
    }

    pub fn new_local(
        to_host: impl Into<outgoing::Outgoing>,
        from_host: async_channel::Receiver<String>,
        originals: Option<Arc<dyn original::OriginalOpener>>,
    ) -> Self {
        let mut link = Self::new(to_host, from_host);
        Arc::get_mut(&mut link.inner).unwrap().originals = OriginalRoute::Local(originals);
        link
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.inner.originals, OriginalRoute::Remote(_))
    }

    pub fn new_remote(
        to_host: impl Into<outgoing::Outgoing>,
        from_host: async_channel::Receiver<String>,
        originals: Option<Arc<dyn original::OriginalOpener>>,
    ) -> Self {
        let mut link = Self::new(to_host, from_host);
        Arc::get_mut(&mut link.inner).unwrap().originals = OriginalRoute::Remote(originals);
        link
    }

    pub fn read_original(&self, target: tcode_protocol::OriginalTarget) -> OriginalTransfer {
        let opener: Arc<dyn original::OriginalOpener> = match &self.inner.originals {
            OriginalRoute::Local(None) => Arc::new(original::LocalOriginal(self.clone())),
            OriginalRoute::Local(Some(opener)) => opener.clone(),
            OriginalRoute::Remote(Some(opener)) => opener.clone(),
            OriginalRoute::Remote(None) => Arc::new(original::UnsupportedOriginal),
        };
        OriginalTransfer::new(opener, target)
    }

    /// Decode and route host output until its transport channel closes.
    ///
    /// The owner of the transport is responsible for running exactly one pump.
    pub async fn pump(&self) {
        self.pump_with_timer(delay).await;
    }

    /// Use the owning executor's clock when it has a deterministic scheduler.
    pub async fn pump_with_timer<F, T>(&self, timer: F)
    where
        F: Fn() -> T,
        T: Future,
    {
        loop {
            let line = futures_lite::future::race(self.inner.from_host.recv(), async {
                timer().await;
                Ok(String::new())
            })
            .await;
            let Ok(line) = line else { break };
            self.check_deadlines(web_time::Instant::now());
            if line.is_empty() {
                continue;
            }
            match decode_host_line(&line) {
                Ok(HostMessage::Event(envelope)) => {
                    if self
                        .inner
                        .subscribed_topics
                        .lock()
                        .unwrap()
                        .contains_key(&envelope.topic)
                    {
                        let _ = self.inner.events_tx.send(envelope).await;
                    }
                }
                Ok(
                    message @ (HostMessage::Ack { id, .. } | HostMessage::QueryResult { id, .. }),
                ) => {
                    if matches!(message, HostMessage::Ack { .. }) {
                        let mut delivery = self.inner.delivery.lock().unwrap();
                        if let Some(index) = delivery.writes.iter().position(|write| write.id == id)
                        {
                            let write = delivery.writes.remove(index).unwrap();
                            if let HostMessage::Ack {
                                result: Err(error), ..
                            } = &message
                            {
                                delivery
                                    .failed
                                    .push_back((write.entry.clone(), error.clone()));
                                while delivery.failed.len() > outbox::MAX_ITEMS {
                                    delivery.failed.pop_front();
                                }
                            }
                            if let HostMessage::Ack { result: Ok(_), .. } = &message
                                && !write.adopted
                                && matches!(
                                    write.entry.command,
                                    Command::SendTurn { .. }
                                        | Command::ScheduleTurn { .. }
                                        | Command::Steer { .. }
                                        | Command::ConfirmRelayAndSend { .. }
                                        | Command::OrchestrateTurn { .. }
                                )
                            {
                                delivery.acknowledged.push_back(write.entry);
                                while delivery.acknowledged.len() > outbox::MAX_ITEMS
                                    || serde_json::to_vec(&delivery.acknowledged)
                                        .map_or(true, |bytes| bytes.len() > outbox::MAX_BYTES)
                                {
                                    delivery.acknowledged.pop_front();
                                }
                            }
                            let _ = self.inner.delivery_changes.0.try_send(());
                            if let Err(error) = persist(&delivery) {
                                log::error!(
                                    "could not prune acknowledged outbox entry: {}",
                                    error.message
                                );
                            }
                        }
                    }
                    if let Some(waiter) = self.inner.pending.lock().unwrap().remove(&id) {
                        let _ = waiter.sender.try_send(message);
                    }
                    self.flush_outbox();
                }
                Err(error) => log::error!("failed to decode host message: {}", error.message),
            }
        }
        self.inner.pending.lock().unwrap().clear();
        self.inner.events_tx.close();
    }

    /// Restore before starting the pump or admitting new work.
    pub fn restore_outbox(&self, storage: Arc<dyn outbox::Storage>) -> Result<(), ProtocolError> {
        let entries = storage.load()?;
        if entries.len() > outbox::MAX_ITEMS
            || serde_json::to_vec(&entries)
                .map_err(outbox::storage_error)?
                .len()
                > outbox::MAX_BYTES
        {
            return Err(outbox::storage_error("Persisted outbox exceeds its bounds"));
        }
        let mut delivery = self.inner.delivery.lock().unwrap();
        delivery.storage = Some(storage);
        for entry in entries {
            delivery.writes.push_back(Write {
                id: self.next_id(),
                entry,
                sent: None,
                adopted: false,
            });
        }
        Ok(())
    }

    pub fn pending_commands(&self) -> Vec<(String, Command)> {
        self.inner
            .delivery
            .lock()
            .unwrap()
            .writes
            .iter()
            .map(|write| (write.entry.key.clone(), write.entry.command.clone()))
            .collect()
    }

    pub fn delivery_changes(&self) -> async_channel::Receiver<()> {
        self.inner.delivery_changes.1.clone()
    }

    pub fn acknowledged_messages(&self) -> Vec<outbox::Entry> {
        self.inner
            .delivery
            .lock()
            .unwrap()
            .acknowledged
            .iter()
            .cloned()
            .collect()
    }

    pub fn retire_acknowledged_message(&self, key: &str) {
        let mut delivery = self.inner.delivery.lock().unwrap();
        delivery.acknowledged.retain(|entry| entry.key != key);
        // A replica can arrive before its command receipt. Remember adoption
        // on the pending write so its later receipt cannot revive a placeholder.
        for write in &mut delivery.writes {
            if write.entry.key == key {
                write.adopted = true;
            }
        }
    }

    pub fn failed_commands(&self) -> Vec<(outbox::Entry, ProtocolError)> {
        self.inner
            .delivery
            .lock()
            .unwrap()
            .failed
            .iter()
            .cloned()
            .collect()
    }

    pub fn discard_failed(&self, key: &str) {
        self.inner
            .delivery
            .lock()
            .unwrap()
            .failed
            .retain(|(entry, _)| entry.key != key);
        let _ = self.inner.delivery_changes.0.try_send(());
    }

    pub fn retry_failed(&self, key: &str) -> Result<(), ProtocolError> {
        let command = self
            .inner
            .delivery
            .lock()
            .unwrap()
            .failed
            .iter()
            .find(|(entry, _)| entry.key == key)
            .map(|(entry, _)| entry.command.clone());
        if let Some(command) = command {
            self.dispatch(command)?;
            self.discard_failed(key);
        }
        Ok(())
    }

    fn enqueue(&self, id: u64, command: Command) -> Result<(), ProtocolError> {
        let state = command
            .contains_secret()
            .then(|| self.inner.connection_state.lock().unwrap());
        if state.as_ref().is_some_and(|state| {
            !matches!(
                **state,
                ConnectionState::Connected { .. } | ConnectionState::Syncing { .. }
            )
        }) {
            return Err(error(
                "disconnected",
                "Saving a secret requires a connection",
            ));
        }
        let entry = outbox::Entry {
            key: uuid::Uuid::new_v4().to_string(),
            command,
        };
        // Reject unencodable platform paths before they can block the FIFO.
        encode_line(&ClientMessage {
            principal: None,
            id,
            key: Some(entry.key.clone()),
            payload: ClientPayload::Command(entry.command.clone()),
        })?;
        let mut delivery = self.inner.delivery.lock().unwrap();
        delivery.writes.push_back(Write {
            id,
            entry,
            sent: None,
            adopted: false,
        });
        let mut evicted = Vec::new();
        while delivery.writes.len() > outbox::MAX_ITEMS
            || snapshot_bytes(&delivery)? > outbox::MAX_BYTES
        {
            evicted.push(delivery.writes.pop_front().unwrap());
        }
        if let Err(error) = persist(&delivery) {
            if let Some(write) = delivery
                .writes
                .iter()
                .chain(evicted.iter())
                .find(|write| write.id == id)
            {
                let entry = write.entry.clone();
                delivery.failed.push_back((entry, error.clone()));
                let _ = self.inner.delivery_changes.0.try_send(());
            }
            delivery.writes.retain(|write| write.id != id);
            for write in evicted.into_iter().rev().filter(|write| write.id != id) {
                delivery.writes.push_front(write);
            }
            return Err(error);
        }
        for write in &evicted {
            delivery.failed.push_back((
                write.entry.clone(),
                error(
                    "outbox_full",
                    "The oldest pending write exceeded the outbox limit",
                ),
            ));
        }
        while delivery.failed.len() > outbox::MAX_ITEMS {
            delivery.failed.pop_front();
        }
        drop(delivery);
        drop(state);
        let _ = self.inner.delivery_changes.0.try_send(());
        let mut rejected = false;
        for write in evicted {
            rejected |= write.id == id;
            if let Some(waiter) = self.inner.pending.lock().unwrap().remove(&write.id) {
                let _ = waiter.sender.try_send(HostMessage::Ack {
                    id: write.id,
                    result: Err(error(
                        "outbox_full",
                        "The oldest pending write exceeded the outbox limit",
                    )),
                });
            }
        }
        self.flush_outbox();
        if rejected {
            Err(error("outbox_full", "Write exceeds the outbox byte limit"))
        } else {
            Ok(())
        }
    }

    /// Send every unsent retained write in queue order without waiting for
    /// Acks: the transport stream and the host both preserve line order, so
    /// a write costs a one-way trip instead of a round trip. Sent writes form
    /// a prefix of the queue; a full outgoing channel ends the pass and the
    /// rest go out on a later flush. A reconnect clears `sent` on every entry
    /// and the whole queue is redelivered under its original keys.
    fn flush_outbox(&self) {
        if !matches!(
            self.connection_state(),
            ConnectionState::Connected { .. } | ConnectionState::Syncing { .. }
        ) {
            return;
        }
        let mut delivery = self.inner.delivery.lock().unwrap();
        let now = web_time::Instant::now();
        for write in delivery
            .writes
            .iter_mut()
            .skip_while(|write| write.sent.is_some())
        {
            let message = ClientMessage {
                principal: None,
                id: write.id,
                key: Some(write.entry.key.clone()),
                payload: ClientPayload::Command(write.entry.command.clone()),
            };
            let Ok(line) = encode_line(&message) else {
                break;
            };
            if self.inner.to_host.try_send(line).is_err() {
                break;
            }
            write.sent = Some(now);
        }
    }

    fn check_deadlines(&self, now: web_time::Instant) {
        if !matches!(
            self.connection_state(),
            ConnectionState::Connected { .. } | ConnectionState::Syncing { .. }
        ) {
            return;
        }
        self.inner.pending.lock().unwrap().retain(|id, waiter| {
            if waiter.query
                && now.duration_since(waiter.started).as_millis()
                    >= u128::from(heartbeat::QUERY_TIMEOUT_MS)
            {
                let _ = waiter.sender.try_send(HostMessage::QueryResult {
                    id: *id,
                    result: Err(error(
                        "timeout",
                        "No query response within 15 seconds while connected",
                    )),
                });
                false
            } else {
                true
            }
        });
        // The oldest sent write bounds the stall: later writes were sent after it.
        let stalled = self
            .inner
            .delivery
            .lock()
            .unwrap()
            .writes
            .iter()
            .find_map(|write| write.sent)
            .is_some_and(|sent| {
                now.duration_since(sent).as_millis() >= u128::from(heartbeat::COMMAND_TIMEOUT_MS)
            });
        let stalled = stalled
            || self.inner.pending.lock().unwrap().values().any(|waiter| {
                !waiter.query
                    && !waiter.retained
                    && now.duration_since(waiter.started).as_millis()
                        >= u128::from(heartbeat::COMMAND_TIMEOUT_MS)
            });
        if stalled {
            log::warn!("command acknowledgement stalled for 30 seconds; reconnecting");
            self.set_connection_state(ConnectionState::Reconnecting {
                attempt: 1,
                reason: Some(ConnectionFailure::Timeout),
            });
            self.wake(recovery::Wake::Reconnect);
        } else {
            self.flush_outbox();
        }
    }

    fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn send_payload(&self, id: u64, payload: ClientPayload) -> Result<(), ProtocolError> {
        let line = encode_line(&ClientMessage {
            principal: None,
            key: None,
            id,
            payload,
        })?;
        self.inner
            .to_host
            .try_send(line)
            .map_err(|error| ProtocolError {
                code: if error.is_full() {
                    "queue_full"
                } else {
                    "transport_closed"
                }
                .into(),
                message: if error.is_full() {
                    "QueueFull"
                } else {
                    "transport closed"
                }
                .into(),
            })
    }

    fn begin_request(
        &self,
        id: u64,
        payload: ClientPayload,
    ) -> Result<async_channel::Receiver<HostMessage>, ProtocolError> {
        let (sender, receiver) = async_channel::bounded(1);
        let query = matches!(payload, ClientPayload::Query(_));
        let state_guard = query.then(|| self.inner.connection_state.lock().unwrap());
        if state_guard.as_ref().is_some_and(|state| {
            !matches!(
                **state,
                ConnectionState::Connected { .. } | ConnectionState::Syncing { .. }
            )
        }) {
            return Err(error("disconnected", "Read requires a connection"));
        }
        self.inner.pending.lock().unwrap().insert(
            id,
            Pending {
                sender,
                query,
                retained: matches!(&payload, ClientPayload::Command(command) if command.requires_delivery_key()),
                started: web_time::Instant::now(),
            },
        );
        if let ClientPayload::Command(command) = &payload
            && command.requires_delivery_key()
        {
            if let Err(error) = self.enqueue(id, command.clone()) {
                self.inner.pending.lock().unwrap().remove(&id);
                return Err(error);
            }
            return Ok(receiver);
        }
        if let Err(error) = self.send_payload(id, payload) {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        Ok(receiver)
    }

    async fn request(&self, payload: ClientPayload) -> Result<HostMessage, ProtocolError> {
        let id = self.next_id();
        let _pending = PendingRequest { link: self, id };
        self.begin_request(id, payload)?
            .recv()
            .await
            .map_err(transport_error)
    }

    pub fn dispatch(&self, command: Command) -> Result<(), ProtocolError> {
        let id = self.next_id();
        if command.requires_delivery_key() {
            self.enqueue(id, command)
        } else {
            self.send_payload(id, ClientPayload::Command(command))
        }
    }

    pub async fn command(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        match self.request(ClientPayload::Command(command)).await? {
            HostMessage::Ack { result, .. } => result,
            other => Err(unexpected_response("command ack", &other)),
        }
    }

    pub fn command_with_id(&self, command: Command) -> (u64, CommandFuture) {
        let id = self.next_id();
        let link = self.clone();
        let future = async move {
            let _pending = PendingRequest { link: &link, id };
            let receiver = link.begin_request(id, ClientPayload::Command(command))?;
            match receiver.recv().await.map_err(transport_error)? {
                HostMessage::Ack { result, .. } => result,
                other => Err(unexpected_response("command ack", &other)),
            }
        };
        (id, Box::pin(future))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn command_blocking(&self, command: Command) -> Result<CommandResponse, ProtocolError> {
        let id = self.next_id();
        match self
            .begin_request(id, ClientPayload::Command(command))?
            .recv_blocking()
            .map_err(transport_error)?
        {
            HostMessage::Ack { result, .. } => result,
            other => Err(unexpected_response("command ack", &other)),
        }
    }

    pub async fn query(&self, query: Query) -> Result<QueryResponse, ProtocolError> {
        match self.request(ClientPayload::Query(query)).await? {
            HostMessage::QueryResult { result, .. } => result,
            other => Err(unexpected_response("query result", &other)),
        }
    }

    pub fn subscribe(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        let _transition = self.inner.subscription_transition.lock().unwrap();
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .insert(subscription.topic.clone(), subscription.clone());
        self.send_subscription(subscription)
    }

    fn send_subscription(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        let id = self.next_id();
        self.inner.subscription_requests.lock().unwrap().insert(
            subscription.topic.clone(),
            SubscriptionRequest {
                id,
                after: subscription.after,
                consumed: false,
            },
        );
        if matches!(
            self.connection_state(),
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
        ) {
            return Ok(());
        }
        self.send_payload(id, ClientPayload::Subscribe(subscription))
    }

    /// A newer cursor supersedes an older subscription reply, including replies
    /// already queued for the UI when it advanced its replica. Check on application,
    /// not just in the transport pump, to avoid a stale tail resetting live records.
    pub fn subscription_reply_is_current(&self, envelope: &EventEnvelope) -> bool {
        envelope
            .request_id
            .is_none_or(|id| self.subscription_request_id(&envelope.topic) == Some(id))
    }

    pub fn subscription_request_id(&self, topic: &Topic) -> Option<u64> {
        self.inner
            .subscription_requests
            .lock()
            .unwrap()
            .get(topic)
            .filter(|request| !request.consumed)
            .map(|request| request.id)
    }

    /// One correlated snapshot per request, including an empty continuation.
    pub fn consume_session_snapshot(&self, envelope: &EventEnvelope) -> Option<Option<u64>> {
        let id = envelope.request_id?;
        let mut requests = self.inner.subscription_requests.lock().unwrap();
        let request = requests.get_mut(&envelope.topic)?;
        if request.id != id || request.consumed {
            return None;
        }
        request.consumed = true;
        Some(request.after)
    }

    pub fn unsubscribe(&self, subscription: Subscription) -> Result<(), ProtocolError> {
        let _transition = self.inner.subscription_transition.lock().unwrap();
        let removed = self
            .inner
            .subscribed_topics
            .lock()
            .unwrap()
            .remove(&subscription.topic);
        self.inner
            .subscription_requests
            .lock()
            .unwrap()
            .remove(&subscription.topic);
        if removed.is_none() {
            return Ok(());
        }
        let id = self.next_id();
        if matches!(
            self.connection_state(),
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
        ) {
            return Ok(());
        }
        self.send_payload(id, ClientPayload::Unsubscribe(subscription))
    }

    /// Request another history window, advancing the cursor already applied locally.
    pub fn update_after(&self, topic: &Topic, after: u64) -> Result<(), ProtocolError> {
        let _transition = self.inner.subscription_transition.lock().unwrap();
        if let Some(subscription) = self.advance_after(topic, after) {
            self.send_subscription(subscription)?;
        }
        Ok(())
    }

    /// Advance recovery memory without asking the host to resend an empty tail.
    pub fn remember_after(&self, topic: &Topic, after: u64) {
        let _transition = self.inner.subscription_transition.lock().unwrap();
        let Some(subscription) = self.advance_after(topic, after) else {
            return;
        };
        // Supersede already queued replies without creating another wire request.
        let id = self.next_id();
        self.inner.subscription_requests.lock().unwrap().insert(
            subscription.topic.clone(),
            SubscriptionRequest {
                id,
                after: subscription.after,
                consumed: false,
            },
        );
    }

    /// Remember the applied index fingerprint for recovery without sending a request.
    pub fn remember_known_index(&self, topic: &Topic, fingerprint: Option<String>) {
        let _transition = self.inner.subscription_transition.lock().unwrap();
        if let Some(subscription) = self.inner.subscribed_topics.lock().unwrap().get_mut(topic) {
            subscription.known_index = fingerprint;
        }
    }

    fn advance_after(&self, topic: &Topic, after: u64) -> Option<Subscription> {
        let mut topics = self.inner.subscribed_topics.lock().unwrap();
        let subscription = topics.get_mut(topic)?;
        if subscription.after == Some(after) {
            return None;
        }
        subscription.after = Some(after);
        Some(subscription.clone())
    }

    pub fn subscriptions(&self) -> Vec<Subscription> {
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    /// Decoded host events. Correlated responses never enter this stream.
    pub fn events(&self) -> async_channel::Receiver<EventEnvelope> {
        self.inner.events_rx.clone()
    }

    pub fn subscribed_topics(&self) -> Vec<Topic> {
        self.inner
            .subscribed_topics
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    pub fn queued_outgoing(&self) -> usize {
        self.inner.to_host.queued()
    }

    pub fn wake(&self, wake: recovery::Wake) {
        self.inner.to_host.wake(wake);
    }

    pub fn connection_state(&self) -> ConnectionState {
        self.inner.connection_state.lock().unwrap().clone()
    }

    pub fn set_connection_state(&self, state: ConnectionState) {
        let _transition = self.inner.subscription_transition.lock().unwrap();
        let mut current = self.inner.connection_state.lock().unwrap();
        let previous = std::mem::replace(&mut *current, state.clone());
        if matches!(
            state,
            ConnectionState::Reconnecting { .. } | ConnectionState::Offline { .. }
        ) {
            let mut pending = self.inner.pending.lock().unwrap();
            pending.retain(|id, waiter| {
                if waiter.query {
                    let _ = waiter.sender.try_send(HostMessage::QueryResult {
                        id: *id,
                        result: Err(error(
                            "disconnected",
                            "Connection lost; retry this read after syncing",
                        )),
                    });
                    false
                } else if !waiter.retained {
                    let _ = waiter.sender.try_send(HostMessage::Ack {
                        id: *id,
                        result: Err(error(
                            "disconnected",
                            "Connection lost before acknowledgement",
                        )),
                    });
                    false
                } else {
                    true
                }
            });
            drop(pending);
            for write in &mut self.inner.delivery.lock().unwrap().writes {
                write.sent = None;
            }
        }
        drop(current);
        let restores_subscriptions = match &state {
            ConnectionState::Syncing { .. } => !matches!(previous, ConnectionState::Syncing { .. }),
            ConnectionState::Connected { .. } => !matches!(
                previous,
                ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
            ),
            _ => false,
        };
        if restores_subscriptions {
            // Scope is answered synchronously by the host. Keep it ahead of the
            // other baselines so the store can recognize this recovery even if
            // its separate connection-state queue is delayed.
            let mut subscriptions = self.subscriptions();
            subscriptions.sort_by_key(|subscription| subscription.topic != Topic::Scope);
            for subscription in subscriptions {
                let _ = self.send_subscription(subscription);
            }
        }
        if matches!(
            state,
            ConnectionState::Syncing { .. } | ConnectionState::Connected { .. }
        ) {
            self.flush_outbox();
        }
        let _ = self.inner.connection_state_tx.try_send(state);
    }

    pub fn connection_state_changes(&self) -> async_channel::Receiver<ConnectionState> {
        self.inner.connection_state_rx.clone()
    }

    /// Close this client attachment without asking the host to shut down.
    ///
    /// Closing both transport directions wakes [`Self::pump`], fails pending
    /// requests, and tells reconnecting adapters to stop. The host and any
    /// other links attached to it keep running.
    pub fn close(&self) {
        self.inner.to_host.close();
        self.inner.from_host.close();
        self.inner.connection_state_tx.close();
        self.inner.delivery_changes.0.close();
    }

    pub async fn shutdown(&self) -> Result<(), ProtocolError> {
        let result = self.command(Command::ShutdownAllAndFlush).await;
        self.close();
        match result? {
            CommandResponse::Unit => Ok(()),
            other => Err(ProtocolError {
                code: "unexpected_response".into(),
                message: format!("expected unit shutdown ack, got {other:?}"),
            }),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn shutdown_blocking(&self) -> Result<(), ProtocolError> {
        let result = self.command_blocking(Command::ShutdownAllAndFlush);
        self.close();
        match result? {
            CommandResponse::Unit => Ok(()),
            other => Err(ProtocolError {
                code: "unexpected_response".into(),
                message: format!("expected unit shutdown ack, got {other:?}"),
            }),
        }
    }
}

fn unexpected_response(expected: &str, actual: &HostMessage) -> ProtocolError {
    ProtocolError {
        code: "unexpected_response".into(),
        message: format!("expected {expected}, got {actual:?}"),
    }
}

fn transport_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError {
        code: "transport_closed".into(),
        message: error.to_string(),
    }
}

fn error(code: &str, message: &str) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn snapshot(delivery: &Delivery) -> Vec<outbox::Entry> {
    delivery
        .writes
        .iter()
        .filter(|write| !write.entry.command.contains_secret())
        .map(|write| write.entry.clone())
        .collect()
}

fn snapshot_bytes(delivery: &Delivery) -> Result<usize, ProtocolError> {
    let stored = serde_json::to_vec(&snapshot(delivery))
        .map_err(outbox::storage_error)?
        .len();
    let wire = delivery.writes.iter().try_fold(0_usize, |bytes, write| {
        encode_line(&ClientMessage {
            principal: None,
            id: write.id,
            key: Some(write.entry.key.clone()),
            payload: ClientPayload::Command(write.entry.command.clone()),
        })
        .map(|line| bytes.saturating_add(line.len()))
    })?;
    Ok(stored.max(wire))
}

fn persist(delivery: &Delivery) -> Result<(), ProtocolError> {
    if let Some(storage) = &delivery.storage {
        storage.save(&snapshot(delivery))?;
    }
    Ok(())
}

async fn delay() {
    #[cfg(not(target_arch = "wasm32"))]
    async_io::Timer::after(std::time::Duration::from_millis(25)).await;
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::{JsCast as _, prelude::*};
        let receiver = {
            let (sender, receiver) = async_channel::bounded(1);
            let callback = Closure::once_into_js(move || {
                let _ = sender.try_send(());
            });
            let global = js_sys::global();
            let timeout: js_sys::Function =
                js_sys::Reflect::get(&global, &JsValue::from_str("setTimeout"))
                    .unwrap()
                    .unchecked_into();
            let _ = timeout.call2(&global, &callback, &JsValue::from_f64(25.));
            receiver
        };
        let _ = receiver.recv().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tcode_protocol::{IndexSnapshot, ServerEvent};

    #[test]
    fn query_deadline_and_command_stall_use_connected_clock() {
        let (outgoing, receiver) = outgoing::channel();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(outgoing, from_host);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut query = std::pin::pin!(link.query(Query::Ping));
        assert!(query.as_mut().poll(&mut cx).is_pending());
        let start = link
            .inner
            .pending
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .started;
        link.check_deadlines(start + std::time::Duration::from_millis(14_999));
        assert!(query.as_mut().poll(&mut cx).is_pending());
        link.check_deadlines(start + std::time::Duration::from_millis(15_000));
        assert!(
            matches!(query.as_mut().poll(&mut cx), std::task::Poll::Ready(Err(error)) if error.code == "timeout")
        );
        link.dispatch(Command::RenameSession {
            session_id: "one".into(),
            title: "new".into(),
        })
        .unwrap();
        let sent = link
            .inner
            .delivery
            .lock()
            .unwrap()
            .writes
            .front()
            .unwrap()
            .sent
            .unwrap();
        link.check_deadlines(sent + std::time::Duration::from_secs(30));
        assert_eq!(receiver.wake.try_recv().unwrap(), recovery::Wake::Reconnect);
        assert!(matches!(
            link.connection_state(),
            ConnectionState::Reconnecting {
                reason: Some(ConnectionFailure::Timeout),
                ..
            }
        ));
        assert_eq!(link.pending_commands().len(), 1);
    }

    #[derive(Default)]
    struct MemoryStorage(Mutex<Vec<outbox::Entry>>);
    impl outbox::Storage for MemoryStorage {
        fn load(&self) -> Result<Vec<outbox::Entry>, ProtocolError> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn save(&self, entries: &[outbox::Entry]) -> Result<(), ProtocolError> {
            *self.0.lock().unwrap() = entries.to_vec();
            Ok(())
        }
    }

    #[test]
    fn secrets_keep_reconnect_delivery_keys_without_entering_the_persisted_outbox() {
        let storage = Arc::new(MemoryStorage::default());
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        link.restore_outbox(storage.clone()).unwrap();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut pump = std::pin::pin!(link.pump_with_timer(std::future::pending::<()>));
        for command in [
            Command::SetHostToken {
                host: "github.com".into(),
                token: Some("fixture-private-token".into()),
                email: None,
            },
            Command::SetProfileSecret {
                profile_id: "claude".into(),
                name: "KEY".into(),
                value: Some("fixture-private-token".into()),
            },
            Command::CreateThirdPartyProfile {
                name: "sample".into(),
                base_url: "https://example.com".into(),
                model: None,
                api_key: "fixture-private-token".into(),
            },
        ] {
            link.dispatch(command.clone()).unwrap();
            let request =
                tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            assert!(request.key.is_some());
            assert_eq!(request.payload, ClientPayload::Command(command));
            assert!(storage.0.lock().unwrap().is_empty());
            link.set_connection_state(ConnectionState::Reconnecting {
                attempt: 1,
                reason: None,
            });
            link.set_connection_state(ConnectionState::Syncing { path: None });
            let replay = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
            assert_eq!(replay.key, request.key);
            assert_eq!(replay.payload, request.payload);
            incoming
                .try_send(
                    encode_line(&HostMessage::Ack {
                        id: replay.id,
                        result: Ok(CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            assert!(pump.as_mut().poll(&mut cx).is_pending());
            assert!(link.pending_commands().is_empty());
            assert!(storage.0.lock().unwrap().is_empty());
        }
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        assert!(
            link.dispatch(Command::SetHostToken {
                host: "github.com".into(),
                token: Some("fixture-private-token".into()),
                email: None,
            })
            .is_err()
        );
        assert!(storage.0.lock().unwrap().is_empty());
        link.close();
        assert!(pump.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn durable_writes_recreate_in_order_and_oldest_surplus_fails() {
        let storage = Arc::new(MemoryStorage::default());
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        link.restore_outbox(storage.clone()).unwrap();
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut first = std::pin::pin!(link.command(Command::RenameSession {
            session_id: "one".into(),
            title: "0".into()
        }));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        for i in 1..=256 {
            link.dispatch(Command::RenameSession {
                session_id: "one".into(),
                title: i.to_string(),
            })
            .unwrap();
        }
        assert!(
            matches!(first.as_mut().poll(&mut cx), std::task::Poll::Ready(Err(error)) if error.code == "outbox_full")
        );
        let entries = storage.0.lock().unwrap().clone();
        assert_eq!(entries.len(), 256);
        link.close();
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let recreated = HostLink::new(to_host, from_host);
        recreated.restore_outbox(storage.clone()).unwrap();
        recreated.set_connection_state(ConnectionState::Syncing { path: None });
        let mut pump = std::pin::pin!(recreated.pump_with_timer(std::future::pending::<()>));
        // Every restored write is on the wire before the first Ack arrives.
        let requests: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let line = outgoing.try_recv().unwrap();
                let request = tcode_protocol::decode_client_line(&line).unwrap();
                assert_eq!(request.key.as_deref(), Some(entry.key.as_str()));
                assert!(
                    matches!(&request.payload, ClientPayload::Command(Command::RenameSession { title, .. }) if *title == (index + 1).to_string())
                );
                request
            })
            .collect();
        assert!(outgoing.try_recv().is_err());
        for (index, request) in requests.iter().enumerate() {
            incoming
                .try_send(
                    encode_line(&HostMessage::Ack {
                        id: request.id,
                        result: Ok(CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            assert!(pump.as_mut().poll(&mut cx).is_pending());
            assert_eq!(storage.0.lock().unwrap().len(), entries.len() - index - 1);
        }
        assert!(recreated.pending_commands().is_empty());
        recreated.close();
        assert!(pump.as_mut().poll(&mut cx).is_ready());
    }

    fn rename(title: usize) -> Command {
        Command::RenameSession {
            session_id: "one".into(),
            title: title.to_string(),
        }
    }

    fn request(outgoing: &async_channel::Receiver<String>) -> ClientMessage {
        tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap()
    }

    fn ack(id: u64, result: Result<CommandResponse, ProtocolError>) -> String {
        encode_line(&HostMessage::Ack { id, result }).unwrap()
    }

    fn pending_titles(link: &HostLink) -> Vec<String> {
        link.pending_commands()
            .into_iter()
            .map(|(_, command)| match command {
                Command::RenameSession { title, .. } => title,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn retained_writes_are_pipelined_and_acks_clear_them_wherever_they_sit() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut pump = std::pin::pin!(link.pump_with_timer(std::future::pending::<()>));
        let (ids, mut futures): (Vec<_>, Vec<_>) = (0..3)
            .map(|title| link.command_with_id(rename(title)))
            .unzip();
        // The request is admitted on first poll.
        for future in &mut futures {
            assert!(future.as_mut().poll(&mut cx).is_pending());
        }
        let requests: Vec<_> = (0..3).map(|_| request(&outgoing)).collect();
        assert!(outgoing.try_recv().is_err(), "all three were sent unacked");
        assert_eq!(
            requests
                .iter()
                .map(|request| request.id)
                .collect::<Vec<_>>(),
            ids
        );
        let keys: Vec<_> = link
            .pending_commands()
            .into_iter()
            .map(|(key, _)| Some(key))
            .collect();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.key.clone())
                .collect::<Vec<_>>(),
            keys
        );
        assert_eq!(pending_titles(&link), ["0", "1", "2"]);
        // A rejection of the middle write, arriving before the first Ack, fails only itself.
        incoming
            .try_send(ack(ids[1], Err(error("rejected", "no"))))
            .unwrap();
        assert!(pump.as_mut().poll(&mut cx).is_pending());
        assert_eq!(pending_titles(&link), ["0", "2"]);
        assert!(matches!(
            futures[1].as_mut().poll(&mut cx),
            std::task::Poll::Ready(Err(error)) if error.code == "rejected"
        ));
        assert_eq!(link.failed_commands().len(), 1);
        for (index, id) in [(0, ids[0]), (2, ids[2])] {
            incoming
                .try_send(ack(id, Ok(CommandResponse::Unit)))
                .unwrap();
            assert!(pump.as_mut().poll(&mut cx).is_pending());
            assert!(matches!(
                futures[index].as_mut().poll(&mut cx),
                std::task::Poll::Ready(Ok(CommandResponse::Unit))
            ));
        }
        assert!(link.pending_commands().is_empty());
        assert!(outgoing.try_recv().is_err(), "nothing was resent");
        link.close();
        assert!(pump.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn reconnect_resends_every_unacked_write_in_order_under_its_original_key() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut pump = std::pin::pin!(link.pump_with_timer(std::future::pending::<()>));
        for title in 0..3 {
            link.dispatch(rename(title)).unwrap();
        }
        let first: Vec<_> = (0..3).map(|_| request(&outgoing)).collect();
        incoming
            .try_send(ack(first[0].id, Ok(CommandResponse::Unit)))
            .unwrap();
        assert!(pump.as_mut().poll(&mut cx).is_pending());
        assert_eq!(pending_titles(&link), ["1", "2"]);
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        assert!(outgoing.try_recv().is_err(), "nothing goes out while down");
        assert!(
            link.inner
                .delivery
                .lock()
                .unwrap()
                .writes
                .iter()
                .all(|write| write.sent.is_none())
        );
        link.set_connection_state(ConnectionState::Syncing { path: None });
        let resent: Vec<_> = (0..2).map(|_| request(&outgoing)).collect();
        assert!(outgoing.try_recv().is_err());
        for (original, again) in first[1..].iter().zip(&resent) {
            assert_eq!(original.key, again.key);
            assert_eq!(original.payload, again.payload);
        }
        link.close();
        assert!(pump.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn a_full_outgoing_channel_pauses_the_flush_and_a_later_one_resumes_in_order() {
        let (to_host, outgoing) = async_channel::bounded(2);
        let (incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut pump = std::pin::pin!(link.pump_with_timer(std::future::pending::<()>));
        for title in 0..4 {
            link.dispatch(rename(title)).unwrap();
        }
        assert_eq!(pending_titles(&link), ["0", "1", "2", "3"]);
        let sent_flags = |link: &HostLink| {
            link.inner
                .delivery
                .lock()
                .unwrap()
                .writes
                .iter()
                .map(|write| write.sent.is_some())
                .collect::<Vec<_>>()
        };
        assert_eq!(sent_flags(&link), [true, true, false, false]);
        let first = request(&outgoing);
        // The Ack frees one slot; the flush it triggers sends exactly the next write.
        incoming
            .try_send(ack(first.id, Ok(CommandResponse::Unit)))
            .unwrap();
        assert!(pump.as_mut().poll(&mut cx).is_pending());
        assert_eq!(sent_flags(&link), [true, true, false]);
        let second = request(&outgoing);
        let third = request(&outgoing);
        assert!(outgoing.try_recv().is_err());
        incoming
            .try_send(ack(second.id, Ok(CommandResponse::Unit)))
            .unwrap();
        assert!(pump.as_mut().poll(&mut cx).is_pending());
        assert_eq!(sent_flags(&link), [true, true]);
        let fourth = request(&outgoing);
        let titles: Vec<_> = [first, second, third, fourth]
            .iter()
            .map(|request| match &request.payload {
                ClientPayload::Command(Command::RenameSession { title, .. }) => title.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(titles, ["0", "1", "2", "3"]);
        link.close();
        assert!(pump.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn inflight_query_fails_immediately_on_disconnect() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let mut query = std::pin::pin!(link.query(Query::Ping));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(query.as_mut().poll(&mut cx).is_pending());
        outgoing.try_recv().unwrap();
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        match query.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(Err(error)) => assert_eq!(error.code, "disconnected"),
            other => panic!("query must fail on disconnect, got {other:?}"),
        }
        assert!(link.inner.pending.lock().unwrap().is_empty());
    }

    /// A pull request write resent after its answer was lost could post twice, so a lost
    /// connection fails it, and reconnecting sends nothing again.
    #[test]
    fn a_pull_request_action_in_flight_fails_on_disconnect_and_is_not_resent() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let mut action = std::pin::pin!(link.command(Command::RunPullRequestAction {
            session_id: "one".into(),
            key: serde_json::from_value(
                serde_json::json!({"host": "github.com", "repository": "octo/repo", "number": 7}),
            )
            .unwrap(),
            action: tcode_protocol::PullRequestAction::Comment {
                body: "Once".into(),
            },
        }));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(action.as_mut().poll(&mut cx).is_pending());
        assert_eq!(request(&outgoing).key, None);
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        match action.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(Err(error)) => assert_eq!(error.code, "disconnected"),
            other => panic!("the action must fail on disconnect, got {other:?}"),
        }
        link.set_connection_state(ConnectionState::Syncing { path: None });
        assert!(outgoing.try_recv().is_err(), "nothing was resent");
        assert!(link.pending_commands().is_empty());
    }

    #[test]
    fn full_transport_queue_rejects_requests_without_leaving_a_waiter() {
        let (outgoing, _receiver) = outgoing::channel();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(outgoing, from_host);
        for _ in 0..outgoing::MAX_LINES {
            link.dispatch(Command::OpenLatestSession).unwrap();
        }
        let error = smol::block_on(link.command(Command::OpenLatestSession)).unwrap_err();
        assert_eq!(error.code, "queue_full");
        assert_eq!(link.queued_outgoing(), 256);
        assert!(link.inner.pending.lock().unwrap().is_empty());
    }

    /// A path change while the baseline loads is a new `Syncing` value for
    /// the screen, not a lost link: nothing is resubscribed, so the reply
    /// already in flight stays current.
    #[test]
    fn a_path_change_while_syncing_does_not_restart_the_sync() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let subscription = Subscription {
            known_index: None,
            topic: Topic::SessionEvents {
                session_id: "one".into(),
            },
            after: None,
        };
        link.subscribe(subscription.clone()).unwrap();
        outgoing.try_recv().unwrap();
        link.set_connection_state(ConnectionState::Syncing { path: None });
        let request = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(outgoing.try_recv().is_err());
        let lan = tcode_protocol::PathInfo {
            direct: true,
            relay: None,
            lan: true,
            probing_direct: false,
        };
        link.set_connection_state(ConnectionState::Syncing {
            path: Some(lan.clone()),
        });
        assert!(outgoing.try_recv().is_err(), "the path alone resubscribed");
        assert_eq!(
            link.connection_state(),
            ConnectionState::Syncing { path: Some(lan) }
        );
        let reply = EventEnvelope {
            request_id: Some(request.id),
            topic: subscription.topic,
            event: ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: 0,
                records: vec![],
            },
        };
        assert!(link.subscription_reply_is_current(&reply));
    }

    #[test]
    fn cancelling_request_releases_pending_waiter() {
        let (to_host, _outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        for _ in 0..20 {
            let mut query = std::pin::pin!(link.query(Query::Ping));
            assert!(
                std::future::Future::poll(
                    query.as_mut(),
                    &mut std::task::Context::from_waker(std::task::Waker::noop())
                )
                .is_pending()
            );
        }
        assert!(link.inner.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn hello_rejection_reasons_preserve_legacy_retry_behavior() {
        for (wire, expected) in [
            (Some("token"), ConnectionFailure::AuthenticationRejected),
            (Some("protocol"), ConnectionFailure::ProtocolMismatch),
            (None, ConnectionFailure::Unreachable(None)),
            (
                Some("invalid hello or token"),
                ConnectionFailure::Unreachable(Some("invalid hello or token".into())),
            ),
            (
                Some("future reason"),
                ConnectionFailure::Unreachable(Some("future reason".into())),
            ),
        ] {
            assert_eq!(ConnectionFailure::hello_rejected(wire), expected);
        }
    }

    #[test]
    fn reconnect_replays_the_cursor_applied_by_the_store() {
        let (to_host, outgoing) = async_channel::unbounded();
        let (_incoming, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        link.subscribe(Subscription {
            known_index: None,
            topic: Topic::Scope,
            after: None,
        })
        .unwrap();
        outgoing.try_recv().unwrap();
        let topic = Topic::SessionEvents {
            session_id: "one".into(),
        };
        link.subscribe(Subscription {
            known_index: None,
            topic: topic.clone(),
            after: None,
        })
        .unwrap();
        outgoing.try_recv().unwrap();
        link.update_after(&topic, 7).unwrap();
        let updated = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            updated.payload,
            ClientPayload::Subscribe(Subscription { after: Some(7), .. })
        ));
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        link.set_connection_state(ConnectionState::Syncing { path: None });
        let scope = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert!(matches!(
            scope.payload,
            ClientPayload::Subscribe(Subscription {
                topic: Topic::Scope,
                ..
            })
        ));

        let replay = tcode_protocol::decode_client_line(&outgoing.try_recv().unwrap()).unwrap();
        assert_eq!(updated.payload, replay.payload);
        let queued_reply = EventEnvelope {
            request_id: Some(replay.id),
            topic: topic.clone(),
            event: ServerEvent::SessionSnapshot {
                total: 0,
                total_turns: 0,
                truncated: false,
                from: 0,
                end: 0,
                records: vec![],
            },
        };
        assert!(link.subscription_reply_is_current(&queued_reply));
        link.unsubscribe(Subscription {
            known_index: None,
            topic,
            after: None,
        })
        .unwrap();
        assert!(!link.subscription_reply_is_current(&queued_reply));
    }

    #[test]
    fn correlates_responses_forwards_events_and_remembers_topics() {
        let (to_host, client_lines) = async_channel::unbounded();
        let (host_lines, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let pump = std::thread::spawn({
            let link = link.clone();
            move || smol::block_on(link.pump())
        });
        let server = std::thread::spawn(move || {
            let request = loop {
                let line = client_lines.recv_blocking().unwrap();
                let request = tcode_protocol::decode_client_line(&line).unwrap();
                if matches!(request.payload, ClientPayload::Command(_)) {
                    break request;
                }
            };
            host_lines
                .send_blocking(
                    encode_line(&HostMessage::Ack {
                        id: request.id + 100,
                        result: Ok(CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            host_lines
                .send_blocking(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event: ServerEvent::IndexSnapshot(IndexSnapshot {
                            summary: Default::default(),
                            sessions: Vec::new(),
                            projects: Vec::new(),
                        }),
                    }))
                    .unwrap(),
                )
                .unwrap();
            host_lines
                .send_blocking(
                    encode_line(&HostMessage::Ack {
                        id: request.id,
                        result: Ok(CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            host_lines.close();
        });

        link.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::Index,
        })
        .unwrap();
        link.subscribe(Subscription {
            known_index: None,
            after: None,
            topic: Topic::Index,
        })
        .unwrap();
        assert_eq!(link.subscribed_topics(), vec![Topic::Index]);
        assert_eq!(
            link.command_blocking(Command::OpenLatestSession).unwrap(),
            CommandResponse::Unit
        );
        assert_eq!(link.events().recv_blocking().unwrap().topic, Topic::Index);
        server.join().unwrap();
        pump.join().unwrap();
    }
}
