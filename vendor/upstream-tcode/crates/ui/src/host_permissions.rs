//! Read-only system grants from the attached agent host, on every client.
use gpui::{
    Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Subscription, Task,
    Window, div,
};
use gpui_base::h_flex;
use tcode_core::permissions::ComputerUsePermissions;

use crate::{
    sizing::Sizable as _, store::WorkspaceStore, theme::ActiveTheme as _,
    typography::TextRole as _, widgets::button::Button,
};

pub(crate) struct HostPermissions {
    store: Entity<WorkspaceStore>,
    result: Option<Result<ComputerUsePermissions, String>>,
    request: Option<Task<()>>,
    connected: bool,
    _connection: Subscription,
}

impl HostPermissions {
    pub(crate) fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let connected = store.read(cx).connection_state().is_connected();
        let connection = cx.observe(&store, |this, store, cx| {
            let connected = store.read(cx).connection_state().is_connected();
            if this.connected != connected {
                this.connected = connected;
                // A disconnected snapshot is no longer evidence of current grants.
                // Cancel its request so a delayed answer cannot restore old status.
                this.request = None;
                this.result = None;
                if connected {
                    this.refresh(cx);
                }
                cx.notify();
            }
        });
        let mut panel = Self {
            store,
            result: None,
            request: None,
            connected,
            _connection: connection,
        };
        panel.refresh(cx);
        panel
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.connected || self.request.is_some() {
            return;
        }
        self.result = None;
        let task = self
            .store
            .update(cx, |store, cx| store.computer_use_permissions(cx));
        self.request = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.result = Some(result);
                this.request = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for HostPermissions {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let mut body = crate::material::group_row_stacked(cx).text_body(compact, cx);
        match &self.result {
            Some(Ok(ComputerUsePermissions::MacOs(status))) => {
                for (name, granted) in [
                    ("permissions.accessibility.name", status.accessibility),
                    ("permissions.screen_recording.name", status.screen_recording),
                ] {
                    let (label, bg, fg) = if granted {
                        (
                            "permissions.granted",
                            cx.theme().success,
                            cx.theme().success_foreground,
                        )
                    } else {
                        (
                            "permissions.missing",
                            cx.theme().warning,
                            cx.theme().warning_foreground,
                        )
                    };
                    body = body.child(
                        h_flex()
                            .w_full()
                            .gap_3()
                            .items_center()
                            .child(div().flex_1().min_w_0().child(crate::tr!(name)))
                            .child(crate::material::semantic_chip(
                                crate::tr!(label),
                                bg.opacity(0.12),
                                fg,
                                cx,
                            )),
                    );
                }
                if let Some(host) = self.store.read(cx).remote_host_name() {
                    body =
                        body.child(div().text_secondary(compact, cx).child(
                            crate::tr!("permissions.manage_on_host", host = host).into_owned(),
                        ));
                }
                body = body.child(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("permissions.restart_on_host")),
                );
            }
            Some(Ok(ComputerUsePermissions::NotRequired)) => {
                body = body.child(crate::tr!("permissions.not_required"));
            }
            Some(Ok(ComputerUsePermissions::Unsupported)) => {
                body = body.child(crate::tr!("permissions.host_unsupported"));
            }
            Some(Err(error)) => {
                body = body
                    .child(
                        div()
                            .text_color(cx.theme().danger_foreground)
                            .child(crate::tr!("permissions.check_failed")),
                    )
                    .child(div().text_secondary(compact, cx).child(error.clone()));
            }
            None => {
                body = body.child(crate::tr!(if self.connected {
                    "permissions.checking"
                } else {
                    "permissions.disconnected"
                }));
            }
        }
        let mut button = Button::new("host-permissions-recheck")
            .outline()
            .small()
            .label(crate::tr!("permissions.recheck"))
            .disabled(!self.connected || self.request.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        if compact {
            button = button.w_full();
        }
        crate::material::group(cx).child(body.child(div().child(button)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use tcode_client::{ConnectionState, HostLink};
    use tcode_protocol::{
        ClientPayload, EventEnvelope, HostMessage, IndexSnapshot, Query, ServerEvent, Topic,
        decode_client_line, encode_line,
    };

    #[gpui::test]
    fn host_grants_refresh_and_never_survive_disconnect_or_failed_recheck(cx: &mut TestAppContext) {
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                crate::store::WorkspaceAttachment::Remote {
                    host_id: "permission-host".into(),
                    host_name: "Agent Mac".into(),
                },
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &replies, deferred, cx);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let baseline = || {
            for (topic, event) in [
                (
                    Topic::Index,
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        projects: vec![],
                        sessions: vec![],
                    }),
                ),
                (
                    Topic::Settings,
                    ServerEvent::SettingsSnapshot(Default::default()),
                ),
            ] {
                replies
                    .send_blocking(
                        encode_line(&HostMessage::Event(EventEnvelope {
                            request_id: None,
                            topic,
                            event,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            }
        };
        baseline();
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        let panel = cx.new(|cx| HostPermissions::new(store.clone(), cx));
        cx.run_until_parked();
        let next_query = || {
            while let Ok(line) = requests.try_recv() {
                let message = decode_client_line(&line).unwrap();
                if let ClientPayload::Query(query) = message.payload {
                    assert_eq!(query, Query::ComputerUsePermissions);
                    return message.id;
                }
            }
            panic!("expected host permission query");
        };
        let reply = |id, result: &str| {
            replies
                .send_blocking(format!(
                    "{{\"type\":\"query_result\",\"content\":{{\"id\":{id},\"result\":{result}}}}}\n"
                ))
                .unwrap();
        };
        // These platform facts come from the wire, independently of the test
        // runner's OS and grants. Rechecking must replace the previous result.
        let id = next_query();
        reply(
            id,
            r#"{"Ok":{"type":"computer_use_permissions","content":{"platform":"mac_os","status":{"accessibility":true,"screen_recording":false}}}}"#,
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.result,
                Some(Ok(ComputerUsePermissions::MacOs(
                    tcode_core::permissions::PermissionStatus {
                        accessibility: true,
                        screen_recording: false,
                    }
                )))
            )
        });
        panel.update(cx, |panel, cx| panel.refresh(cx));
        cx.run_until_parked();
        let stale_id = next_query();
        link.set_connection_state(ConnectionState::Reconnecting {
            attempt: 1,
            reason: None,
        });
        cx.run_until_parked();
        reply(
            stale_id,
            r#"{"Ok":{"type":"computer_use_permissions","content":{"platform":"not_required"}}}"#,
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.result.is_none(),
                "late success must not overwrite disconnection"
            );
            assert!(panel.request.is_none());
        });
        link.set_connection_state(ConnectionState::Connected { path: None });
        cx.run_until_parked();
        let mut scope_request = None;
        while let Ok(line) = requests.try_recv() {
            let message = decode_client_line(&line).unwrap();
            if matches!(
                message.payload,
                ClientPayload::Subscribe(tcode_protocol::Subscription {
                    topic: Topic::Scope,
                    ..
                })
            ) {
                scope_request = Some(message.id);
            }
        }
        crate::store::tests::seed_full_scope_reply(
            &store,
            &replies,
            scope_request.expect("reconnected Scope subscription"),
            Vec::new(),
            cx,
        );
        baseline();
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.run_until_parked();
        reply(
            next_query(),
            r#"{"Ok":{"type":"computer_use_permissions","content":{"platform":"not_required"}}}"#,
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.result, Some(Ok(ComputerUsePermissions::NotRequired)))
        });
        panel.update(cx, |panel, cx| panel.refresh(cx));
        cx.run_until_parked();
        reply(
            next_query(),
            r#"{"Err":{"code":"unsupported","message":"old host"}}"#,
        );
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.result, Some(Err("old host".into())))
        });
    }
}
