//! Dedicated-thread host execution seam.
//!
//! `AppState` is a plain `Send` value owned by the host loop. UI clients can
//! reach it only through serialized protocol messages. Background completions use
//! the same mailbox via [`HostCx::enqueue`].

use std::future::Future;

use tcode_protocol::{
    EventEnvelope, HostMessage, Principal, RuntimeNotification, ServerEvent, Topic, encode_line,
};

use crate::app::AppState;

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // The internal host queue stays directly typed.
pub enum HostEvent {
    Runtime(RuntimeNotification),
    Domain(EventEnvelope),
}

pub(crate) type HostFn = Box<dyn FnOnce(&mut AppState, &mut HostCx) + Send + 'static>;

/// A runtime task handle independent of any UI executor.
pub type HostTask<T> = smol::Task<T>;

/// The only execution/event context accepted by [`AppState`] methods.
///
/// Clones are `Send` and may be held by background work. All state mutation
/// returns through `mailbox`; emitted events enter the serialized host-output
/// stream.
type CompletedCommands = std::collections::HashMap<
    String,
    std::collections::VecDeque<(
        String,
        Result<tcode_protocol::CommandResponse, tcode_protocol::ProtocolError>,
    )>,
>;

#[derive(Clone)]
pub struct HostCx {
    mailbox: smol::channel::Sender<HostFn>,
    events: smol::channel::Sender<String>,
    pub(crate) principal: Principal,
    pub(crate) author: Option<tcode_core::session::Author>,
    /// Who a message admitted now comes from; `None` for what a provider
    /// reports on its own.
    pub(crate) origin: Option<tcode_core::session::MessageOrigin>,
    pub(crate) delivery_key: Option<String>,
    pub(crate) completed: std::sync::Arc<std::sync::Mutex<CompletedCommands>>,
}

impl HostCx {
    pub(crate) fn new(
        mailbox: smol::channel::Sender<HostFn>,
        events: smol::channel::Sender<String>,
    ) -> Self {
        Self {
            mailbox,
            events,
            completed: Default::default(),
            delivery_key: None,
            principal: Principal::Full,
            author: None,
            origin: None,
        }
    }

    pub fn emit(&mut self, event: HostEvent) {
        let envelope = match event {
            HostEvent::Domain(envelope) => envelope,
            HostEvent::Runtime(notification) => EventEnvelope {
                request_id: None,
                topic: Topic::RuntimeEvents,
                event: ServerEvent::Runtime(notification),
            },
        };
        self.send_message(HostMessage::Event(envelope));
    }

    pub(crate) fn send_message(&self, message: HostMessage) {
        let line = match encode_line(&message) {
            Ok(line) => line,
            Err(error) => {
                log::error!("failed to encode host protocol message: {}", error.message);
                return;
            }
        };
        let _ = self.events.try_send(line);
    }

    pub fn spawn_background<T: Send + 'static>(
        &self,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> HostTask<T> {
        smol::spawn(fut)
    }

    /// The approved `smol::unblock` site for the runtime: blocking work is
    /// scheduled here so the host owns every pool hand-off.
    #[allow(clippy::disallowed_methods)]
    pub fn unblock<R, F>(&self, f: F) -> HostTask<R>
    where
        R: Send + 'static,
        F: FnOnce() -> R + Send + 'static,
    {
        self.spawn_background(smol::unblock(f))
    }

    pub fn enqueue(&self, f: impl FnOnce(&mut AppState, &mut HostCx) + Send + 'static) {
        let _ = self.mailbox.try_send(Box::new(f));
    }

    pub(crate) async fn enqueue_and_wait<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut AppState, &mut HostCx) -> R + Send + 'static,
    ) -> Result<R, ()> {
        let (sender, receiver) = smol::channel::bounded(1);
        self.mailbox
            .send(Box::new(move |state, cx| {
                let _ = sender.try_send(f(state, cx));
            }))
            .await
            .map_err(|_| ())?;
        receiver.recv().await.map_err(|_| ())
    }

    pub fn spawn_detached(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.spawn_background(fut).detach();
    }
}
