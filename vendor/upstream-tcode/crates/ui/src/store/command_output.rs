//! Command output screens the host renders and pushes for the selected thread.

use std::rc::Rc;

use tcode_protocol::{Subscription, TerminalFrame, Topic};

use super::WorkspaceStore;

/// What a client shows for one command execution.
#[derive(Default)]
pub(crate) struct CommandOutputReplica {
    /// The width subscribed to, while the output is on screen.
    watched: Option<u16>,
    /// The latest screen, kept across width changes and while unwatched so
    /// the panel never goes blank waiting for the next one.
    frame: Option<Rc<TerminalFrame>>,
}

impl CommandOutputReplica {
    pub(crate) fn watched(&self) -> Option<u16> {
        self.watched
    }

    pub(crate) fn frame(&self) -> Option<&Rc<TerminalFrame>> {
        self.frame.as_ref()
    }
}

impl WorkspaceStore {
    pub(crate) fn command_output(&self, item_id: &str) -> Option<&CommandOutputReplica> {
        self.command_outputs.get(item_id)
    }

    /// Hold `item_id`'s output at `cols` on the selected thread. The host
    /// answers once, then pushes again while the output grows.
    pub(crate) fn watch_command_output(&mut self, item_id: &str, cols: u16) {
        let Some(session_id) = self.selected_session_id.clone() else {
            return;
        };
        let replica = self.command_outputs.entry(item_id.to_owned()).or_default();
        if replica.watched == Some(cols) {
            return;
        }
        let previous = replica.watched.replace(cols);
        if let Some(previous) = previous {
            let _ =
                self.host
                    .unsubscribe(command_output_subscription(&session_id, item_id, previous));
        }
        let _ = self
            .host
            .subscribe(command_output_subscription(&session_id, item_id, cols));
    }

    pub(crate) fn unwatch_command_output(&mut self, item_id: &str) {
        let Some(session_id) = self.selected_session_id.as_deref() else {
            return;
        };
        let Some(cols) = self
            .command_outputs
            .get_mut(item_id)
            .and_then(|replica| replica.watched.take())
        else {
            return;
        };
        let _ = self
            .host
            .unsubscribe(command_output_subscription(session_id, item_id, cols));
    }

    pub(super) fn clear_command_outputs(&mut self) {
        let outputs = std::mem::take(&mut self.command_outputs);
        let Some(session_id) = self.selected_session_id.as_deref() else {
            return;
        };
        for (item_id, replica) in outputs {
            if let Some(cols) = replica.watched {
                let _ = self
                    .host
                    .unsubscribe(command_output_subscription(session_id, &item_id, cols));
            }
        }
    }

    pub(super) fn apply_command_output(
        &mut self,
        session_id: &str,
        item_id: &str,
        cols: u16,
        frame: &TerminalFrame,
    ) {
        if self.selected_session_id.as_deref() != Some(session_id) {
            return;
        }
        if let Some(replica) = self
            .command_outputs
            .get_mut(item_id)
            .filter(|replica| replica.watched == Some(cols))
        {
            replica.frame = Some(Rc::new(frame.clone()));
        }
    }
}

fn command_output_subscription(session_id: &str, item_id: &str, cols: u16) -> Subscription {
    Subscription {
        known_index: None,
        topic: Topic::CommandOutput {
            session_id: session_id.to_owned(),
            item_id: item_id.to_owned(),
            cols,
        },
        after: None,
    }
}
