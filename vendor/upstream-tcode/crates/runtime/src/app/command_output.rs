//! Command output rendered by the host and pushed on [`Topic::CommandOutput`].

use super::*;
use tcode_protocol::{CommandResponse, HostMessage, TerminalFrame};

/// Each render replays the output's tail into a fresh emulator, so a growing
/// command is pushed at a fraction of a live terminal's frame rate.
pub(crate) const PUSH_INTERVAL: Duration = Duration::from_millis(100);

/// One subscribed `(session, item, cols)` while any client holds it.
pub(super) struct CommandOutputFeed {
    /// Tells a completion for an earlier subscription of the same topic apart.
    generation: u64,
    /// Advanced by every record that touches the item.
    revision: u64,
    /// The revision the latest push render read.
    rendered: u64,
    rendering: bool,
    timer_armed: bool,
    last_started: Option<Instant>,
    last_sent: Option<TerminalFrame>,
}

impl AppState {
    pub(super) fn open_command_output(&mut self, topic: &Topic) {
        self.next_command_output_feed += 1;
        self.command_output_feeds.insert(
            topic.clone(),
            CommandOutputFeed {
                generation: self.next_command_output_feed,
                revision: 0,
                rendered: 0,
                rendering: false,
                timer_armed: false,
                last_started: None,
                last_sent: None,
            },
        );
    }

    pub(super) fn close_command_output(&mut self, topic: &Topic) {
        self.command_output_feeds.remove(topic);
    }

    /// Answer a subscriber with the output as it stands now. A refused render
    /// ends the subscription: the item is unknown or out of scope for everyone.
    pub(super) fn reply_command_output(
        &mut self,
        request_id: Option<u64>,
        topic: Topic,
        cx: &mut HostCx,
    ) {
        if !self.command_output_feeds.contains_key(&topic) {
            self.open_command_output(&topic);
        }
        let feed = &self.command_output_feeds[&topic];
        let (generation, revision) = (feed.generation, feed.revision);
        let render = self.render_command_output(&topic, cx);
        let host_cx = cx.clone();
        cx.spawn_detached(async move {
            let result = render.await;
            host_cx.enqueue(move |state, cx| {
                let current = state
                    .command_output_feeds
                    .get(&topic)
                    .is_some_and(|feed| feed.generation == generation);
                let result = match result {
                    Ok(frame) => {
                        if current {
                            state.command_output_replied(&topic, revision, &frame);
                            cx.emit(HostEvent::Domain(EventEnvelope {
                                request_id,
                                topic: topic.clone(),
                                event: ServerEvent::CommandOutputReplaced(Box::new(frame)),
                            }));
                            state.schedule_command_output(&topic, cx);
                        }
                        Ok(CommandResponse::Unit)
                    }
                    Err(error) => {
                        if current {
                            state.subscriptions.remove(&topic);
                            state.close_command_output(&topic);
                        }
                        Err(error)
                    }
                };
                if let Some(id) = request_id {
                    cx.send_message(HostMessage::Ack { id, result });
                }
            });
        });
    }

    /// A reply read at `revision` reached its subscriber. When the output
    /// moved on while it rendered, a newer push may already have arrived there
    /// and been overwritten by this reply, so the latest output goes out again.
    fn command_output_replied(&mut self, topic: &Topic, revision: u64, frame: &TerminalFrame) {
        let Some(feed) = self.command_output_feeds.get_mut(topic) else {
            return;
        };
        if feed.revision == revision {
            feed.last_sent = Some(frame.clone());
        } else {
            feed.rendered = revision;
            feed.last_sent = None;
        }
    }

    /// A record touched `item_id`; every width subscribed to it is rendered
    /// again.
    pub(super) fn command_output_changed(
        &mut self,
        session_id: &str,
        item_id: &str,
        cx: &mut HostCx,
    ) {
        let topics: Vec<Topic> = self
            .command_output_feeds
            .keys()
            .filter(|topic| {
                matches!(
                    topic,
                    Topic::CommandOutput { session_id: session, item_id: item, .. }
                        if session == session_id && item == item_id
                )
            })
            .cloned()
            .collect();
        for topic in topics {
            if let Some(feed) = self.command_output_feeds.get_mut(&topic) {
                feed.revision += 1;
            }
            self.schedule_command_output(&topic, cx);
        }
    }

    /// Render now when the last render started a [`PUSH_INTERVAL`] ago,
    /// otherwise coalesce into one render at that boundary.
    fn schedule_command_output(&mut self, topic: &Topic, cx: &mut HostCx) {
        let Some(feed) = self.command_output_feeds.get_mut(topic) else {
            return;
        };
        if feed.rendering || feed.timer_armed || feed.rendered == feed.revision {
            return;
        }
        let Some(delay) = feed
            .last_started
            .map(|started| started.elapsed())
            .filter(|elapsed| *elapsed < PUSH_INTERVAL)
            .map(|elapsed| PUSH_INTERVAL - elapsed)
        else {
            self.push_command_output(topic, cx);
            return;
        };
        feed.timer_armed = true;
        let generation = feed.generation;
        let topic = topic.clone();
        let tick = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(delay).await;
            tick.enqueue(move |state, cx| {
                let Some(feed) = state
                    .command_output_feeds
                    .get_mut(&topic)
                    .filter(|feed| feed.generation == generation)
                else {
                    return;
                };
                feed.timer_armed = false;
                state.schedule_command_output(&topic, cx);
            });
        });
    }

    fn push_command_output(&mut self, topic: &Topic, cx: &mut HostCx) {
        let Some(feed) = self.command_output_feeds.get_mut(topic) else {
            return;
        };
        feed.rendering = true;
        feed.rendered = feed.revision;
        feed.last_started = Some(Instant::now());
        let generation = feed.generation;
        let render = self.render_command_output(topic, cx);
        let topic = topic.clone();
        let host_cx = cx.clone();
        cx.spawn_detached(async move {
            let result = render.await;
            host_cx.enqueue(move |state, cx| {
                let Some(feed) = state
                    .command_output_feeds
                    .get_mut(&topic)
                    .filter(|feed| feed.generation == generation)
                else {
                    return;
                };
                feed.rendering = false;
                if let Ok(frame) = result
                    && feed.last_sent.as_ref() != Some(&frame)
                {
                    feed.last_sent = Some(frame.clone());
                    cx.emit(HostEvent::Domain(EventEnvelope {
                        request_id: None,
                        topic: topic.clone(),
                        event: ServerEvent::CommandOutputReplaced(Box::new(frame)),
                    }));
                }
                state.schedule_command_output(&topic, cx);
            });
        });
    }

    fn render_command_output(
        &mut self,
        topic: &Topic,
        cx: &mut HostCx,
    ) -> HostTask<Result<TerminalFrame, ProtocolError>> {
        let Topic::CommandOutput {
            session_id,
            item_id,
            cols,
        } = topic
        else {
            unreachable!("only command output topics have feeds");
        };
        let cols = *cols;
        let read = self.stored_command_output(session_id, item_id.clone(), cx);
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            let output = read.await?;
            Ok(host_cx
                .unblock(move || crate::terminal::render_command_output(&output, cols))
                .await)
        })
    }
}
