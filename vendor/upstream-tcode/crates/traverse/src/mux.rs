//! Fan one host pipe out to many client attachments. Transport-agnostic: the
//! host side of any transport attaches one [`Connection`] per client stream
//! and pumps NDJSON lines through it.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_channel::{Receiver, Sender};
use tcode_client::attachment::{AttachmentMessage, AttachmentPolicy};
use tcode_protocol::{Principal, encode_line};

mod outbox;
use outbox::Outbox;

/// One logical client endpoint attached to a [`HostMux`].
pub struct Connection {
    pub to_host: Sender<String>,
    pub from_host: Receiver<String>,
}

#[derive(Clone)]
pub struct HostMux {
    inner: Arc<Inner>,
}

struct Inner {
    ingress: Sender<Ingress>,
    next_connection: AtomicU64,
    originals: Option<Arc<dyn tcode_client::original::OriginalBackend>>,
    original_slots: std::sync::Mutex<HashMap<String, usize>>,
}

struct Attachment {
    outbox: Outbox,
    principal: serde_json::Value,
    policy: AttachmentPolicy,
}

enum Ingress {
    Add(u64, Box<Attachment>),
    Line(u64, String),
    Closed(u64),
}

impl HostMux {
    pub fn new(to_host: Sender<AttachmentMessage>, from_host: Receiver<String>) -> Self {
        let (ingress, ingress_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("tcode-mux".into())
            .spawn(move || futures_lite::future::block_on(pump(to_host, from_host, ingress_rx)))
            .expect("failed to spawn mux thread");
        Self {
            inner: Arc::new(Inner {
                ingress,
                next_connection: AtomicU64::new(1),
                originals: None,
                original_slots: Default::default(),
            }),
        }
    }

    pub fn with_original_backend(
        mut self,
        backend: Arc<dyn tcode_client::original::OriginalBackend>,
    ) -> Self {
        Arc::get_mut(&mut self.inner).unwrap().originals = Some(backend);
        self
    }

    pub(crate) async fn open_original(
        &self,
        device: String,
        principal: Principal,
        target: tcode_protocol::OriginalTarget,
        policy: AttachmentPolicy,
    ) -> Result<AdmittedOriginal, tcode_protocol::ProtocolError> {
        if !policy.bulk_stream {
            return Err(tcode_client::original::error(
                "attachment has no bulk stream",
            ));
        }
        let permit = {
            let mut slots = self.inner.original_slots.lock().unwrap();
            if policy.transfers.is_some_and(|limits| {
                slots.values().sum::<usize>() >= limits.total
                    || slots.get(&device).copied().unwrap_or(0) >= limits.per_device
            }) {
                return Err(tcode_protocol::ProtocolError {
                    code: "busy".into(),
                    message: "too many original reads".into(),
                });
            }
            *slots.entry(device.clone()).or_default() += 1;
            OriginalPermit {
                inner: self.inner.clone(),
                device,
            }
        };
        let backend = self
            .inner
            .originals
            .as_ref()
            .ok_or_else(|| tcode_client::original::error("original backend unavailable"))?;
        let source = backend.open(principal, target).await?;
        Ok(AdmittedOriginal {
            source,
            _permit: permit,
        })
    }

    pub fn attach(&self, principal: Principal, policy: AttachmentPolicy) -> Connection {
        let connection_id = self.inner.next_connection.fetch_add(1, Ordering::Relaxed);
        let (client_tx, client_rx) = async_channel::unbounded();
        let (output_tx, forward, output_rx) = Outbox::new(client_rx.clone(), policy.queue);
        let ingress = self.inner.ingress.clone();
        let _ = ingress.try_send(Ingress::Add(
            connection_id,
            Box::new(Attachment {
                outbox: output_tx,
                principal: serde_json::to_value(principal).unwrap(),
                policy,
            }),
        ));
        std::thread::Builder::new()
            .name(format!("tcode-mux-{connection_id}"))
            .spawn(move || {
                futures_lite::future::block_on(futures_lite::future::race(
                    async {
                        while let Ok(line) = client_rx.recv().await {
                            if client_rx.is_closed() {
                                break;
                            }
                            if ingress
                                .send(Ingress::Line(connection_id, line))
                                .await
                                .is_err()
                            {
                                client_rx.close();
                                // Host EOF still owns replies waiting in the output queue.
                                // Let the forwarder finish instead of winning this race.
                                std::future::pending::<()>().await;
                            }
                        }
                    },
                    forward,
                ));
                client_rx.close();
                let _ = ingress.send_blocking(Ingress::Closed(connection_id));
            })
            .expect("failed to spawn mux connection thread");
        Connection {
            to_host: client_tx,
            from_host: output_rx,
        }
    }
}

pub(crate) struct AdmittedOriginal {
    pub source: tcode_client::original::OriginalSource,
    _permit: OriginalPermit,
}

struct OriginalPermit {
    inner: Arc<Inner>,
    device: String,
}
impl Drop for OriginalPermit {
    fn drop(&mut self) {
        let mut slots = self.inner.original_slots.lock().unwrap();
        if let Some(count) = slots.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 {
                slots.remove(&self.device);
            }
        }
    }
}

async fn pump(
    to_host: Sender<AttachmentMessage>,
    from_host: Receiver<String>,
    ingress: Receiver<Ingress>,
) {
    let mut clients = HashMap::<u64, Box<Attachment>>::new();
    let mut subscriptions = HashMap::<u64, HashSet<String>>::new();
    let mut routes = HashMap::<u64, (u64, u64)>::new();
    let mut pending_subscriptions = HashMap::<u64, String>::new();
    let mut deferred_unsubscribes = HashMap::<String, (serde_json::Value, AttachmentPolicy)>::new();
    let mut next_global_id = 1_u64;

    loop {
        enum Input {
            Client(Result<Ingress, async_channel::RecvError>),
            Host(Result<String, async_channel::RecvError>),
        }
        let input =
            futures_lite::future::race(async { Input::Client(ingress.recv().await) }, async {
                Input::Host(from_host.recv().await)
            })
            .await;
        match input {
            Input::Client(Ok(Ingress::Add(id, attachment))) => {
                clients.insert(id, attachment);
                subscriptions.insert(id, HashSet::new());
            }
            Input::Client(Ok(Ingress::Closed(id))) => {
                let Some(attachment) = clients.remove(&id) else {
                    continue;
                };
                let Attachment {
                    principal, policy, ..
                } = *attachment;
                let mut topics = subscriptions.remove(&id).unwrap_or_default();
                pending_subscriptions.retain(|global_id, topic| {
                    if routes.get(global_id).is_some_and(|route| route.0 == id) {
                        topics.insert(topic.clone());
                        false
                    } else {
                        true
                    }
                });
                for topic in topics {
                    if release_subscription(
                        topic,
                        (principal.clone(), policy),
                        &subscriptions,
                        &pending_subscriptions,
                        &mut deferred_unsubscribes,
                        &to_host,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                routes.retain(|_, route| route.0 != id);
            }
            Input::Client(Ok(Ingress::Line(connection_id, line))) => {
                if !clients.contains_key(&connection_id) {
                    continue;
                }
                let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let Some(local_id) = value.get("id").and_then(serde_json::Value::as_u64) else {
                    continue;
                };
                if let Some((kind, topic)) = subscription_change(&value) {
                    let topics = subscriptions.entry(connection_id).or_default();
                    if kind == "subscribe" {
                        pending_subscriptions.insert(next_global_id, topic.clone());
                    } else {
                        topics.remove(&topic);
                        if let Ok(tcode_protocol::Topic::Terminal { terminal_id }) =
                            serde_json::from_str(&topic)
                            && let Some(sender) = clients.get(&connection_id)
                        {
                            sender.outbox.forget_terminal(terminal_id);
                        }
                        pending_subscriptions.retain(|global_id, pending_topic| {
                            pending_topic != &topic
                                || routes
                                    .get(global_id)
                                    .is_none_or(|route| route.0 != connection_id)
                        });
                        let another_subscriber =
                            subscriptions.values().any(|topics| topics.contains(&topic));
                        if another_subscriber
                            || pending_subscriptions
                                .values()
                                .any(|pending| pending == &topic)
                        {
                            if !another_subscriber {
                                deferred_unsubscribes.insert(
                                    topic.clone(),
                                    (
                                        clients[&connection_id].principal.clone(),
                                        clients[&connection_id].policy,
                                    ),
                                );
                            }
                            let ack = tcode_protocol::HostMessage::Ack {
                                id: local_id,
                                result: Ok(tcode_protocol::CommandResponse::Unit),
                            };
                            if let Some(sender) = clients.get(&connection_id) {
                                let _ = sender.outbox.try_send(encode_line(&ack).unwrap(), None);
                            }
                            continue;
                        }
                        deferred_unsubscribes.remove(&topic);
                    }
                }
                let Some(attachment) = clients.get(&connection_id) else {
                    continue;
                };
                value["principal"] = attachment.principal.clone();
                value["id"] = next_global_id.into();
                routes.insert(next_global_id, (connection_id, local_id));
                next_global_id = next_global_id.wrapping_add(1).max(1);
                if to_host
                    .send(AttachmentMessage {
                        line: encode_line(&value).unwrap(),
                        policy: attachment.policy,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Input::Client(Err(_)) | Input::Host(Err(_)) => break,
            Input::Host(Ok(line)) => {
                let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match value.get("type").and_then(serde_json::Value::as_str) {
                    Some("event") => {
                        let Some(content) = value.get("content") else {
                            continue;
                        };
                        let Some(topic) = content
                            .get("topic")
                            .and_then(|topic| serde_json::to_string(topic).ok())
                        else {
                            continue;
                        };
                        if let Some(request_id) = content
                            .get("request_id")
                            .and_then(serde_json::Value::as_u64)
                        {
                            if let Some((connection_id, local_id)) =
                                routes.get(&request_id).copied()
                            {
                                if pending_subscriptions.get(&request_id) == Some(&topic) {
                                    pending_subscriptions.remove(&request_id);
                                    deferred_unsubscribes.remove(&topic);
                                    if let Some(topics) = subscriptions.get_mut(&connection_id) {
                                        topics.insert(topic.clone());
                                    }
                                }
                                if subscriptions
                                    .get(&connection_id)
                                    .is_some_and(|topics| topics.contains(&topic))
                                    && let Some(sender) = clients.get(&connection_id)
                                {
                                    value["content"]["request_id"] = local_id.into();
                                    if matches!(
                                        value
                                            .pointer("/content/event/type")
                                            .and_then(serde_json::Value::as_str),
                                        Some("terminal_frame" | "terminal_delta")
                                    ) {
                                        if let Ok(tcode_protocol::HostMessage::Event(envelope)) =
                                            serde_json::from_value(value.clone())
                                        {
                                            let _ = sender.outbox.try_send_terminal(envelope);
                                        }
                                    } else {
                                        let _ = sender
                                            .outbox
                                            .try_send(encode_line(&value).unwrap(), None);
                                    }
                                }
                            }
                        } else {
                            let replace = content
                                .get("event")
                                .and_then(|event| event.get("type"))
                                .and_then(serde_json::Value::as_str)
                                .filter(|kind| {
                                    matches!(
                                        *kind,
                                        "session_plan_replaced"
                                            | "git_status_replaced"
                                            | "command_output_replaced"
                                            | "providers_replaced"
                                            | "index_summary_replaced"
                                    )
                                })
                                .map(|kind| format!("{kind}:{topic}"));
                            for (id, sender) in &clients {
                                if subscriptions
                                    .get(id)
                                    .is_some_and(|topics| topics.contains(&topic))
                                {
                                    if matches!(
                                        content
                                            .pointer("/event/type")
                                            .and_then(serde_json::Value::as_str),
                                        Some("terminal_frame" | "terminal_delta")
                                    ) {
                                        if let Ok(tcode_protocol::HostMessage::Event(envelope)) =
                                            serde_json::from_value(value.clone())
                                        {
                                            let _ = sender.outbox.try_send_terminal(envelope);
                                        }
                                    } else {
                                        let _ =
                                            sender.outbox.try_send(line.clone(), replace.clone());
                                    }
                                }
                            }
                        }
                    }
                    Some("ack" | "query_result") => {
                        let Some(global_id) = value
                            .get("content")
                            .and_then(|content| content.get("id"))
                            .and_then(serde_json::Value::as_u64)
                        else {
                            continue;
                        };
                        let Some((connection_id, local_id)) = routes.remove(&global_id) else {
                            continue;
                        };
                        if let Some(topic) = pending_subscriptions.remove(&global_id) {
                            if value["type"] == "ack"
                                && value["content"]["result"].get("Ok").is_some()
                            {
                                if let Some(topics) = subscriptions.get_mut(&connection_id) {
                                    topics.insert(topic.clone());
                                }
                                deferred_unsubscribes.remove(&topic);
                            } else if let Some(principal) = deferred_unsubscribes.remove(&topic)
                                && release_subscription(
                                    topic,
                                    principal,
                                    &subscriptions,
                                    &pending_subscriptions,
                                    &mut deferred_unsubscribes,
                                    &to_host,
                                )
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        value["content"]["id"] = local_id.into();
                        if let Some(attachment) = clients.get(&connection_id) {
                            let _ = attachment
                                .outbox
                                .try_send(encode_line(&value).unwrap(), None);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

// A departing client may have a snapshot still in flight. Keep the host's
// subscription until other pending requests resolve, then release it if none succeeded.
async fn release_subscription(
    topic: String,
    attachment: (serde_json::Value, AttachmentPolicy),
    subscriptions: &HashMap<u64, HashSet<String>>,
    pending: &HashMap<u64, String>,
    deferred: &mut HashMap<String, (serde_json::Value, AttachmentPolicy)>,
    to_host: &Sender<AttachmentMessage>,
) -> Result<(), async_channel::SendError<AttachmentMessage>> {
    if subscriptions.values().any(|topics| topics.contains(&topic)) {
        deferred.remove(&topic);
        return Ok(());
    }
    if pending.values().any(|pending| pending == &topic) {
        deferred.insert(topic, attachment);
        return Ok(());
    }
    deferred.remove(&topic);
    let (principal, policy) = attachment;
    let topic: serde_json::Value = serde_json::from_str(&topic).unwrap();
    to_host
        .send(AttachmentMessage {
            line: encode_line(&serde_json::json!({
                "id": 0, "principal": principal,
                "payload": {"type":"unsubscribe", "content":{"topic":topic}},
            }))
            .unwrap(),
            policy,
        })
        .await
}

fn subscription_change(value: &serde_json::Value) -> Option<(&str, String)> {
    let payload = value.get("payload")?;
    let kind = payload.get("type")?.as_str()?;
    if !matches!(kind, "subscribe" | "unsubscribe") {
        return None;
    }
    Some((
        kind,
        serde_json::to_string(payload.get("content")?.get("topic")?).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_subscribers_receive_events_and_last_unsubscribe_releases_topic() {
        let (to_host, host_rx) = async_channel::unbounded();
        let (host_tx, from_host) = async_channel::unbounded();
        let mux = HostMux::new(to_host, from_host);
        let one = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let two = mux.attach(
            tcode_protocol::Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let session_topic =
            serde_json::json!({"type":"session_events","content":{"session_id":"one"}});
        let send = |connection: &Connection, kind: &str, id: u64, topic: &serde_json::Value| {
            connection
                .to_host
                .send_blocking(
                    encode_line(&serde_json::json!({
                        "id": id,
                        "payload": {"type": kind, "content": {"topic": topic}},
                    }))
                    .unwrap(),
                )
                .unwrap();
        };
        let approve = |connection: &Connection| {
            let forwarded: serde_json::Value =
                serde_json::from_str(&host_rx.recv_blocking().unwrap().line).unwrap();
            host_tx
                .send_blocking(
                    encode_line(&tcode_protocol::HostMessage::Ack {
                        id: forwarded["id"].as_u64().unwrap(),
                        result: Ok(tcode_protocol::CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            assert!(matches!(
                tcode_protocol::decode_host_line(&connection.from_host.recv_blocking().unwrap())
                    .unwrap(),
                tcode_protocol::HostMessage::Ack { result: Ok(_), .. }
            ));
        };
        send(&one, "subscribe", 1, &session_topic);
        approve(&one);
        let event = r#"{"type":"event","content":{"topic":{"type":"session_events","content":{"session_id":"one"}},"event":{"type":"session_snapshot","content":{"from":0,"end":0,"records":[],"total":0,"total_turns":0,"truncated":false}}}}"#;
        host_tx.send_blocking(event.into()).unwrap();
        assert_eq!(one.from_host.recv_blocking().unwrap(), event);
        assert!(two.from_host.try_recv().is_err());
        send(&two, "subscribe", 2, &session_topic);
        approve(&two);
        send(&one, "unsubscribe", 3, &session_topic);
        let ack = one.from_host.recv_blocking().unwrap();
        assert!(ack.contains("ack"));
        assert!(
            host_rx.try_recv().is_err(),
            "another client still owns this subscription"
        );
        host_tx.send_blocking(event.into()).unwrap();
        assert_eq!(two.from_host.recv_blocking().unwrap(), event);
        assert!(one.from_host.try_recv().is_err());

        let index_topic = serde_json::json!({"type":"index"});
        for client in [&one, &two] {
            send(client, "subscribe", 4, &index_topic);
            approve(client);
        }
        drop(one.to_host);
        // The closed output proves the mux processed the first detach.
        assert!(one.from_host.recv_blocking().is_err());
        let index_event = r#"{"type":"event","content":{"topic":{"type":"index"},"event":{"type":"index_snapshot","content":{"sessions":[],"projects":[],"worktree_shared":[],"archived_revision":0}}}}"#;
        host_tx.send_blocking(index_event.into()).unwrap();
        assert_eq!(two.from_host.recv_blocking().unwrap(), index_event);
        assert!(
            host_rx.try_recv().is_err(),
            "disconnecting one client keeps the other's Index subscription"
        );
        let pending = mux.attach(
            Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let pending_topic =
            serde_json::json!({"type":"session_events","content":{"session_id":"cold"}});
        send(&pending, "subscribe", 5, &pending_topic);
        let _ = host_rx.recv_blocking().unwrap();
        drop(pending.to_host);
        let unsubscribe: serde_json::Value =
            serde_json::from_str(&host_rx.recv_blocking().unwrap().line).unwrap();
        assert_eq!(unsubscribe["payload"]["type"], "unsubscribe");
        assert_eq!(unsubscribe["payload"]["content"]["topic"], pending_topic);
        assert_eq!(unsubscribe["principal"], serde_json::json!({"type":"full"}));
        assert!(pending.from_host.recv_blocking().is_err());
        let delayed = mux.attach(
            Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let refused = mux.attach(
            Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        send(&delayed, "subscribe", 6, &pending_topic);
        let _ = host_rx.recv_blocking().unwrap();
        send(&refused, "subscribe", 7, &pending_topic);
        let forwarded: serde_json::Value =
            serde_json::from_str(&host_rx.recv_blocking().unwrap().line).unwrap();
        drop(delayed.to_host);
        assert!(delayed.from_host.recv_blocking().is_err());
        assert!(
            host_rx.try_recv().is_err(),
            "the other subscription reply is still pending"
        );
        host_tx
            .send_blocking(
                encode_line(&tcode_protocol::HostMessage::Ack {
                    id: forwarded["id"].as_u64().unwrap(),
                    result: Err(tcode_protocol::ProtocolError::out_of_scope("cold thread")),
                })
                .unwrap(),
            )
            .unwrap();
        let reply =
            tcode_protocol::decode_host_line(&refused.from_host.recv_blocking().unwrap()).unwrap();
        assert!(matches!(
            reply,
            tcode_protocol::HostMessage::Ack {
                id: 7,
                result: Err(_)
            }
        ));
        let unsubscribe: serde_json::Value =
            serde_json::from_str(&host_rx.recv_blocking().unwrap().line).unwrap();
        assert_eq!(unsubscribe["payload"]["type"], "unsubscribe");
        assert_eq!(unsubscribe["payload"]["content"]["topic"], pending_topic);
        drop(refused.to_host);
        assert!(refused.from_host.recv_blocking().is_err());
        drop(two.to_host);
        let mut released = HashSet::new();
        for _ in 0..2 {
            let unsubscribe: serde_json::Value =
                serde_json::from_str(&host_rx.recv_blocking().unwrap().line).unwrap();
            assert_eq!(unsubscribe["payload"]["type"], "unsubscribe");
            released.insert(unsubscribe["payload"]["content"]["topic"].to_string());
        }
        assert_eq!(
            released,
            HashSet::from([session_topic.to_string(), index_topic.to_string()])
        );
    }

    #[test]
    fn refused_subscription_does_not_receive_another_clients_broadcasts() {
        use tcode_protocol::{
            ClientMessage, ClientPayload, Command, CommandResponse, EventEnvelope, HostMessage,
            ProtocolError, ServerEvent, Subscription, Topic,
        };

        let (to_host, host_rx) = async_channel::unbounded();
        let (host_tx, from_host) = async_channel::unbounded();
        let mux = HostMux::new(to_host, from_host);
        let one = mux.attach(
            Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let two = mux.attach(
            Principal::Full,
            tcode_client::attachment::AttachmentPolicy::network(),
        );
        let topic = Topic::SessionEvents {
            session_id: "private".into(),
        };
        for (client, result) in [
            (&one, Ok(CommandResponse::Unit)),
            (&two, Err(ProtocolError::out_of_scope("private thread"))),
        ] {
            client
                .to_host
                .send_blocking(
                    encode_line(&ClientMessage {
                        id: 1,
                        key: None,
                        principal: None,
                        payload: ClientPayload::Subscribe(Subscription {
                            known_index: None,
                            topic: topic.clone(),
                            after: None,
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            let request =
                tcode_protocol::decode_client_line(&host_rx.recv_blocking().unwrap().line).unwrap();
            host_tx
                .send_blocking(
                    encode_line(&HostMessage::Ack {
                        id: request.id,
                        result: result.clone(),
                    })
                    .unwrap(),
                )
                .unwrap();
            assert_eq!(
                tcode_protocol::decode_host_line(&client.from_host.recv_blocking().unwrap())
                    .unwrap(),
                HostMessage::Ack { id: 1, result }
            );
        }
        let broadcast = HostMessage::Event(EventEnvelope {
            request_id: None,
            topic,
            event: ServerEvent::SessionSnapshot {
                from: 0,
                end: 0,
                records: vec![],
                total: 0,
                total_turns: 0,
                truncated: false,
            },
        });
        host_tx
            .send_blocking(encode_line(&broadcast).unwrap())
            .unwrap();
        assert_eq!(
            tcode_protocol::decode_host_line(&one.from_host.recv_blocking().unwrap()).unwrap(),
            broadcast
        );
        // A later correlated reply proves the mux has routed the preceding broadcast.
        two.to_host
            .send_blocking(
                encode_line(&ClientMessage {
                    id: 2,
                    key: None,
                    principal: None,
                    payload: ClientPayload::Command(Command::CycleProjectSort),
                })
                .unwrap(),
            )
            .unwrap();
        let request =
            tcode_protocol::decode_client_line(&host_rx.recv_blocking().unwrap().line).unwrap();
        host_tx
            .send_blocking(
                encode_line(&HostMessage::Ack {
                    id: request.id,
                    result: Ok(CommandResponse::Unit),
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            tcode_protocol::decode_host_line(&two.from_host.recv_blocking().unwrap()).unwrap(),
            HostMessage::Ack {
                id: 2,
                result: Ok(CommandResponse::Unit)
            }
        );
        assert!(two.from_host.try_recv().is_err());
    }

    #[test]
    fn colliding_client_ids_route_to_the_owner_without_rewriting_nested_payloads() {
        for local_id in [7, u64::MAX] {
            let (to_host, host_rx) = async_channel::unbounded();
            let (host_tx, from_host) = async_channel::unbounded();
            let mux = HostMux::new(to_host, from_host);
            let principals = [
                Principal::Full,
                Principal::Space {
                    policy_revision: 0,
                    space_id: "space".into(),
                    space_name: "Shared".into(),
                    project_ids: vec!["project".into()],
                    device_id: "device".into(),
                    device_name: "Phone".into(),
                },
            ];
            let clients = principals.clone().map(|principal| {
                mux.attach(
                    principal,
                    tcode_client::attachment::AttachmentPolicy::network(),
                )
            });
            let requests = ["index", "providers"].map(|topic| {
                serde_json::json!({
                    "id": local_id,
                    "principal": {"type": "full"},
                    "payload": {
                        "type": "subscribe", "content": {"topic": {"type": topic}},
                        "extension": {"id": 17, "request_id": 23, "text": "nested 文本"},
                    },
                })
            });
            for (client, request) in clients.iter().zip(&requests) {
                client
                    .to_host
                    .send_blocking(encode_line(request).unwrap())
                    .unwrap();
            }
            let mut ids = HashMap::new();
            for _ in 0..2 {
                let forwarded = host_rx.recv_blocking().unwrap().line;
                assert!(forwarded.ends_with('\n'));
                let mut forwarded: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
                let topic = forwarded["payload"]["content"]["topic"]["type"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                ids.insert(topic.clone(), forwarded["id"].as_u64().unwrap());
                forwarded["id"] = local_id.into();
                let index = usize::from(topic == "providers");
                assert_eq!(
                    forwarded["principal"],
                    serde_json::to_value(&principals[index]).unwrap()
                );
                forwarded["principal"] = requests[index]["principal"].clone();
                assert_eq!(forwarded, requests[index]);
            }
            assert_ne!(ids["index"], ids["providers"]);
            for (index, topic) in ["index", "providers"].into_iter().enumerate().rev() {
                for (kind, id_field) in [("event", "request_id"), ("ack", "id")] {
                    let message = serde_json::json!({
                        "type": kind,
                        "content": {
                            id_field: ids[topic], "topic": {"type": topic},
                            "result": {"Ok": {"type": "unit"}},
                            "event": {"type": "index_snapshot", "content": {"sessions": [], "projects": [], "worktree_shared": [], "archived_revision": 0}},
                            "nested": {"id": 123, "request_id": 456, "value": [null, true, "文本"]},
                        },
                    });
                    host_tx
                        .send_blocking(encode_line(&message).unwrap())
                        .unwrap();
                    let reply = clients[index].from_host.recv_blocking().unwrap();
                    assert!(reply.ends_with('\n'));
                    let mut reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
                    assert_eq!(reply["content"][id_field], local_id);
                    reply["content"][id_field] = ids[topic].into();
                    assert_eq!(reply, message);
                    assert!(clients[1 - index].from_host.try_recv().is_err());
                }
            }
        }
    }
}
