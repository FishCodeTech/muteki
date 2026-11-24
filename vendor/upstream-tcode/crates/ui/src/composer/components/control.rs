use super::super::*;
use tcode_core::control::ReferenceOrigin;
use tcode_protocol::{ControlStatus, ThreadReferenceStatus};

impl Composer {
    fn control_status(&self, cx: &App) -> Option<(String, ControlStatus)> {
        let store = self.workspace_store.read(cx);
        let status = store.session_status()?;
        if status.draft {
            return None;
        }
        Some((store.active_session_id()?, status.control.clone()?))
    }

    /// The grant requests the thread's model made with `request_grant`. Only
    /// these buttons answer one; nothing said in the chat does.
    pub(in super::super) fn render_control_request_panel(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (session_id, status) = self.control_status(cx)?;
        let count = status.pending_requests.len();
        let request = status.pending_requests.into_iter().next()?;
        let respond = |approve: bool| {
            let session_id = session_id.clone();
            let request_id = request.id.clone();
            cx.listener(move |this: &mut Self, _, _, cx| {
                let session_id = session_id.clone();
                let request_id = request_id.clone();
                this.workspace_store.update(cx, |store, _| {
                    store.respond_control_grant(session_id, request_id, approve)
                });
            })
        };
        let summary = crate::tr!(
            "control.request.summary",
            summary = crate::control::grant_summary(&request.grant)
        )
        .into_owned();
        Some(
            crate::material::card_raised(cx)
                .w_full()
                .debug_selector(|| "control-request".into())
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_caption(cx)
                                .child(crate::tr!("control.request.caption")),
                        )
                        .child(div().flex_1().text_title(cx).child(summary))
                        .when(count > 1, |row| {
                            row.child(
                                div()
                                    .text_secondary(self.compact, cx)
                                    .child(format!("1/{count}")),
                            )
                        }),
                )
                .when(!request.reason.trim().is_empty(), |card| {
                    card.child(div().text_body(self.compact, cx).child(crate::tr!(
                        "control.request.reason",
                        reason = request.reason.as_str()
                    )))
                })
                .child(
                    div()
                        .text_secondary(self.compact, cx)
                        .child(crate::tr!("control.request.note")),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Button::new("control-request-approve")
                                .primary()
                                .chip()
                                .touch(self.compact)
                                .disabled(!self.interactive(cx))
                                .label(crate::tr!("control.request.approve"))
                                .on_click(respond(true)),
                        )
                        .child(
                            Button::new("control-request-decline")
                                .danger()
                                .outline()
                                .chip()
                                .touch(self.compact)
                                .disabled(!self.interactive(cx))
                                .label(crate::tr!("control.request.decline"))
                                .on_click(respond(false)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// One chip per thread this one may read, in the composer's chip row.
    pub(in super::super) fn render_reference_chips(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (session_id, status) = self.control_status(cx)?;
        if status.references.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .children(
                    status
                        .references
                        .into_iter()
                        .enumerate()
                        .map(|(index, reference)| {
                            let session_id = session_id.clone();
                            let thread_id = reference.thread_id.clone();
                            Button::new(("thread-reference-chip", index))
                                .debug_selector(move || format!("thread-reference-chip-{index}"))
                                .ghost()
                                .small()
                                .chip()
                                .text_meta_mono(self.compact, cx)
                                .label(format!("{}  ×", reference_chip_label(&reference)))
                                .tooltip(reference_chip_tooltip(&reference))
                                .aria_label(
                                    crate::tr!("control.reference.remove").into_owned()
                                        + ": "
                                        + &reference_title(&reference),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let session_id = session_id.clone();
                                    let thread_id = thread_id.clone();
                                    this.workspace_store.update(cx, |store, _| {
                                        store.remove_thread_reference(session_id, thread_id)
                                    });
                                }))
                        }),
                )
                .into_any_element(),
        )
    }

    /// Reference `thread_id` from the active thread. When the host answers that the new
    /// reference crosses a project or provider boundary, the user is told once and may take
    /// it back.
    pub(in super::super) fn reference_thread(
        &mut self,
        thread_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.workspace_store.read(cx);
        let Some(session_id) = store.active_session_id() else {
            return;
        };
        let title = store
            .thread_meta(&thread_id)
            .map(|meta| meta.title.clone())
            .unwrap_or_default();
        let added = self.workspace_store.update(cx, |store, cx| {
            store.command(
                tcode_protocol::Command::AddThreadReference {
                    session_id: session_id.clone(),
                    thread_id: thread_id.clone(),
                },
                cx,
            )
        });
        let store = self.workspace_store.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(tcode_protocol::CommandResponse::ThreadReferenceAdded {
                boundary_notice: true,
            }) = added.await
            else {
                return;
            };
            let _ = this.update_in(cx, |_, window, cx| {
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let store = store.clone();
                    let session_id = session_id.clone();
                    let thread_id = thread_id.clone();
                    alert
                        .title(crate::tr!("control.reference.notice_title"))
                        .description(crate::tr!(
                            "control.reference.notice_description",
                            title = title.as_str()
                        ))
                        .button_props(
                            DialogButtons::default()
                                .ok_text(crate::tr!("control.reference.notice_ok"))
                                .cancel_text(crate::tr!("control.reference.notice_cancel"))
                                .show_cancel(true),
                        )
                        .on_cancel(move |_, _, cx| {
                            let session_id = session_id.clone();
                            let thread_id = thread_id.clone();
                            store.update(cx, |store, _| {
                                store.remove_thread_reference(session_id, thread_id)
                            });
                            true
                        })
                })
            });
        })
        .detach();
    }
}

fn reference_title(reference: &ThreadReferenceStatus) -> String {
    if reference.deleted || reference.title.is_empty() {
        crate::tr!("control.settings.deleted_thread").into_owned()
    } else {
        reference.title.clone()
    }
}

/// "@title · model · shared": who started the reference, and whether its
/// content crosses a project or provider boundary or is gone.
fn reference_chip_label(reference: &ThreadReferenceStatus) -> String {
    let mut label = format!("@{}", reference_title(reference));
    if reference.by == ReferenceOrigin::Model {
        label.push_str(" · ");
        label.push_str(&crate::tr!("control.reference.model"));
    }
    if reference.deleted {
        label.push_str(" · ");
        label.push_str(&crate::tr!("control.reference.deleted"));
    } else if reference.boundary_notice {
        label.push_str(" · ");
        label.push_str(&crate::tr!("control.reference.boundary"));
    }
    label
}

fn reference_chip_tooltip(reference: &ThreadReferenceStatus) -> String {
    let mut tooltip = crate::tr!(
        "control.reference.chip_tooltip",
        title = reference_title(reference)
    )
    .into_owned();
    if reference.boundary_notice && !reference.deleted {
        tooltip.push(' ');
        tooltip.push_str(&crate::tr!("control.reference.boundary_tooltip"));
    }
    tooltip
}
