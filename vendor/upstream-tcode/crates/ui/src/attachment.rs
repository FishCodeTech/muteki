//! The window's attachment to a host: one transport, its pump, and the
//! workspace store built on top of them.
//!
//! The window outlives any single attachment. Switching hosts replaces
//! everything in here and nothing above it, so the guarantees are narrow and
//! worth stating:
//!
//! - **Nothing blocks the window.** Teardown hands back the retiring pump's
//!   completion as a value to await, rather than joining it on the UI thread —
//!   which a browser, running the whole client on one executor, cannot do at
//!   all.
//! - **A retiring task cannot reach the next attachment.** Closing the link
//!   closes its channels first, so anything still in flight is dropped rather
//!   than delivered into the store that replaced it.
//! - **Detaching is not shutting down.** The client's link closes; the host
//!   keeps running, and every other client attached to it keeps working.

use std::rc::Rc;

use gpui::{App, AppContext as _, Entity, Task};
use tcode_client::HostLink;
use tcode_client::host::{ClientHost, Transport};

use crate::remote::AttachmentTarget;
use crate::store::{WorkspaceAttachment, WorkspaceStore};

/// How this window reaches a host running inside its own process. Desktop
/// bootstrap supplies one; a client that can never host has none, so
/// [`AttachmentTarget::Local`] simply has nothing to open. `Err` says why that
/// host is not running; asking again starts it again.
pub type LocalTransport = Rc<dyn Fn(&mut App) -> Result<Transport, String>>;

struct AttachmentRuntime {
    link: HostLink,
    finished: async_channel::Receiver<()>,
    tasks: Vec<Task<()>>,
}

impl AttachmentRuntime {
    fn start_link(
        link: HostLink,
        states: async_channel::Receiver<tcode_client::ConnectionState>,
        cx: &mut App,
    ) -> Self {
        let (finished, pump_finished) = async_channel::bounded(1);
        let pump = link.clone();
        let state_link = link.clone();
        let executor = cx.background_executor().clone();
        Self {
            finished: pump_finished,
            tasks: vec![
                cx.background_spawn(async move {
                    pump.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                        .await;
                    let _ = finished.try_send(());
                }),
                // A local transport has no state channel behind it; the closed
                // receiver ends this task on its first poll.
                cx.background_spawn(async move {
                    while let Ok(state) = states.recv().await {
                        state_link.set_connection_state(state);
                    }
                }),
            ],
            link,
        }
    }

    /// Close the transport and hand back whether its pump finished. The tasks
    /// ride along with the watcher so they can drain, and the closed link keeps
    /// them from delivering anything into the attachment that replaced this one.
    fn close(self, cx: &mut App) -> async_channel::Receiver<bool> {
        self.link.close();
        let (done, observed) = async_channel::bounded(1);
        let finished = self.finished;
        let tasks = self.tasks;
        cx.background_spawn(async move {
            let completed = finished.recv().await.is_ok();
            if !completed {
                log::error!("a detached host link pump was dropped before it finished");
            }
            drop(tasks);
            let _ = done.try_send(completed);
        })
        .detach();
        observed
    }
}

/// One live attachment: where it points, how it talks, and what it replicates.
pub struct Attachment {
    pub target: AttachmentTarget,
    pub store: Entity<WorkspaceStore>,
    runtime: AttachmentRuntime,
}

impl Attachment {
    /// Open a link to `target` and build its store, over `local` when the
    /// target is this process's own host. `None` when this client has no way
    /// to reach that target — a browser asked to attach locally has no local
    /// host to attach to.
    pub fn open(
        target: AttachmentTarget,
        local: Option<Transport>,
        client_host: Option<Rc<dyn ClientHost>>,
        seed_blocking: bool,
        cx: &mut App,
    ) -> Option<Self> {
        let (transport, identity) = match &target {
            AttachmentTarget::Local => (local?, WorkspaceAttachment::Local),
            AttachmentTarget::Remote(host) => (
                client_host.as_ref()?.connect(host),
                WorkspaceAttachment::Remote {
                    host_id: host.host_id.clone(),
                    host_name: host.name.clone(),
                },
            ),
        };
        let storage = match &target {
            AttachmentTarget::Remote(host) => client_host
                .as_ref()
                .and_then(|client| client.outbox_storage(&host.host_id)),
            AttachmentTarget::Local => client_host
                .as_ref()
                .and_then(|client| client.outbox_storage("local")),
        };
        let link = if matches!(target, AttachmentTarget::Local) {
            HostLink::new_local(transport.to_host, transport.from_host, transport.originals)
        } else {
            HostLink::new_remote(transport.to_host, transport.from_host, transport.originals)
        };
        if let Some(storage) = storage {
            if matches!(target, AttachmentTarget::Remote(_)) {
                link.set_connection_state(tcode_client::ConnectionState::Reconnecting {
                    attempt: 1,
                    reason: None,
                });
            }
            if let Err(error) = link.restore_outbox(storage) {
                log::error!("could not restore outbox: {}", error.message);
                link.close();
                return None;
            }
        }
        let local = matches!(target, AttachmentTarget::Local);
        // Local construction may seed synchronously; remote construction must
        // register its subscriptions before a restored write can be replayed.
        let runtime =
            local.then(|| AttachmentRuntime::start_link(link.clone(), transport.state.clone(), cx));
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                identity,
                client_host,
                transport.current_host,
                seed_blocking && local,
                cx,
            )
        });
        let runtime =
            runtime.unwrap_or_else(|| AttachmentRuntime::start_link(link, transport.state, cx));
        Some(Self {
            target,
            store,
            runtime,
        })
    }

    pub fn link(&self) -> HostLink {
        self.runtime.link.clone()
    }

    /// Retire this attachment. The returned receiver reports whether the pump
    /// finished; awaiting it is optional, and never on the window's path.
    pub fn close(self, cx: &mut App) -> async_channel::Receiver<bool> {
        self.store.update(cx, |store, cx| store.detach(cx));
        self.runtime.close(cx)
    }
}

/// Whether two targets name the same pairing: the machine by its id, not by
/// the address a particular record happens to carry, and the space it was
/// joined through, since a full pairing and a space membership of the same
/// machine are different links.
pub fn same_target(left: &AttachmentTarget, right: &AttachmentTarget) -> bool {
    match (left, right) {
        (AttachmentTarget::Local, AttachmentTarget::Local) => true,
        (AttachmentTarget::Remote(left), AttachmentTarget::Remote(right)) => {
            left.host_id == right.host_id && left.space_id == right.space_id
        }
        _ => false,
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use gpui::TestAppContext;
    use tcode_protocol::{
        ClientPayload, Command, EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic,
        decode_client_line, encode_line,
    };

    use super::*;

    fn index_event() -> String {
        encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                summary: Default::default(),
                sessions: Vec::new(),
                projects: Vec::new(),
            }),
        }))
        .unwrap()
    }

    fn transport(
        to_host: async_channel::Sender<String>,
        from_host: async_channel::Receiver<String>,
    ) -> Transport {
        let (_, state) = async_channel::unbounded();
        Transport {
            originals: None,
            to_host: to_host.into(),
            from_host,
            state,
            current_host: None,
        }
    }

    /// Leaving a host is not stopping it: the client's own pump ends, its
    /// transport refuses late events, and no shutdown command is ever sent.
    #[gpui::test]
    fn closing_an_attachment_finishes_its_pump_without_shutting_down_the_host(
        cx: &mut TestAppContext,
    ) {
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let attachment = cx.update(|cx| {
            Attachment::open(
                AttachmentTarget::Local,
                Some(transport(to_host, from_host)),
                None,
                false,
                cx,
            )
            .expect("local attachment")
        });
        cx.run_until_parked();

        let finished = cx.update(|cx| attachment.close(cx));
        cx.run_until_parked();
        assert_eq!(
            finished.try_recv().ok(),
            Some(true),
            "pump must finish after transport close"
        );
        assert!(
            incoming.try_send(index_event()).is_err(),
            "a detached transport must reject late host events"
        );

        let mut lines = Vec::new();
        while let Ok(line) = outgoing.try_recv() {
            lines.push(line);
        }
        assert!(!lines.is_empty());
        assert!(lines.into_iter().all(|line| {
            !matches!(
                decode_client_line(&line).unwrap().payload,
                ClientPayload::Command(Command::ShutdownAllAndFlush)
            )
        }));
    }
}
