use super::super::*;
use crate::scroll::ScrollableElement as _;

/// How tall a request's detail grows before it scrolls inside the card, so the
/// options stay on screen; no owner sizes a card's scrolling body.
const DETAIL_MAX_HEIGHT: f32 = 240.;

impl Composer {
    pub(in super::super) fn render_approval_panel(
        &self,
        request: &ApprovalRequest,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = match &request.kind {
            ApprovalKind::ExecCommand { .. } => {
                if self.compact {
                    crate::tr!("mobile.command_requested")
                } else {
                    crate::tr!("approval.command_requested")
                }
            }
            ApprovalKind::FileRead { .. } => {
                if self.compact {
                    crate::tr!("mobile.read_requested")
                } else {
                    crate::tr!("approval.file_read_requested")
                }
            }
            ApprovalKind::FileChange { .. } => {
                if self.compact {
                    crate::tr!("mobile.files_requested")
                } else {
                    crate::tr!("approval.file_requested")
                }
            }
            ApprovalKind::ToolUse { .. } => {
                if self.compact {
                    crate::tr!("mobile.tool_requested")
                } else {
                    crate::tr!("approval.tool_requested")
                }
            }
        };
        let muted = cx.theme().muted_foreground;

        let detail_bg = cx.theme().muted;
        let detail_area = |content: AnyElement| {
            div()
                .id("approval-detail-scroll")
                .w_full()
                .max_h(px(DETAIL_MAX_HEIGHT))
                .overflow_y_scroll_area()
                .p_2()
                .rounded(crate::material::radius_card(cx))
                .bg(detail_bg)
                .child(content)
                .into_any_element()
        };
        let detail: AnyElement = match &request.kind {
            ApprovalKind::ExecCommand { command, cwd, .. } => detail_area(
                v_flex()
                    .gap_1()
                    .child(div().text_code(cx).child(command.clone()))
                    .when_some(cwd.clone(), |this, cwd| {
                        this.child(
                            div()
                                .text_secondary(self.compact, cx)
                                .child(crate::tr!("approval.in_directory", cwd = cwd)),
                        )
                    })
                    .into_any_element(),
            ),
            ApprovalKind::FileChange { changes, .. } => {
                let changes = Rc::new(changes.clone());
                let last = changes.len().saturating_sub(1);
                div()
                    .debug_selector(|| "approval-detail".into())
                    .w_full()
                    .rounded(crate::material::radius_card(cx))
                    .bg(detail_bg)
                    .child(
                        crate::scroll::VirtualList::measured(
                            "approval-detail-scroll",
                            changes.len(),
                            move |index, _, cx| {
                                let change = &changes[index];
                                div()
                                    .debug_selector(move || format!("approval-change-{index}"))
                                    .when(index < last, |row| row.pb_0p5())
                                    .text_code(cx)
                                    .child(format!(
                                        "{} {}",
                                        file_change_kind_label(change.kind),
                                        change.path
                                    ))
                            },
                        )
                        .w_full()
                        .max_h(px(DETAIL_MAX_HEIGHT))
                        .p_2(),
                    )
                    .into_any_element()
            }
            ApprovalKind::FileRead { detail } => {
                detail_area(div().text_code(cx).child(detail.clone()).into_any_element())
            }
            ApprovalKind::ToolUse {
                name,
                input,
                detail,
            } => {
                let input = input.to_string();
                detail_area(
                    v_flex()
                        .gap_1()
                        .when(
                            !detail.is_empty() && *detail != *name && *detail != input,
                            |this| {
                                this.child(div().text_body(self.compact, cx).child(detail.clone()))
                            },
                        )
                        .child(div().text_code(cx).child(format!("{name} {input}")))
                        .into_any_element(),
                )
            }
        };

        let pending = self
            .workspace_store
            .read(cx)
            .approval_delivery_pending(&request.id);
        let expanded = self.approval_expanded;
        let cancel_id = request.id.clone();

        crate::material::card_raised(cx)
            .w_full()
            .when(pending, |card| {
                card.child(div().text_secondary(self.compact, cx).child(crate::tr!(
                    if self
                        .workspace_store
                        .read(cx)
                        .connection_state()
                        .is_connected()
                    {
                        "chat.sending"
                    } else {
                        "chat.waiting_connection"
                    }
                )))
            })
            .when(self.compact && !self.interactive(cx), |card| {
                card.child(
                    div()
                        .text_body(self.compact, cx)
                        .text_color(muted)
                        .child(crate::tr!("mobile.offline_action")),
                )
            })
            .child(
                crate::material::accessible_clickable(
                    h_flex(),
                    "approval-header",
                    Role::Button,
                    summary.clone(),
                    cx,
                )
                .when(self.compact, |header| {
                    header.min_h(px(crate::material::TOUCH_TARGET))
                })
                .w_full()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.approval_expanded = !this.approval_expanded;
                    cx.notify();
                }))
                .child(div().text_caption(cx).child(if self.compact {
                    crate::tr!("mobile.approval")
                } else {
                    crate::tr!("approval.pending")
                }))
                .child(div().flex_1().text_title(cx).child(summary))
                .when(count > 1, |this| {
                    this.child(
                        div()
                            .text_secondary(self.compact, cx)
                            .child(format!("1/{count}")),
                    )
                })
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .text_color(muted),
                ),
            )
            .when(expanded, |this| this.child(detail))
            .child({
                let mut options = h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .flex_wrap();
                for option in &request.options {
                    let request_id = request.id.clone();
                    let option_id = option.id.clone();
                    let rejects = matches!(
                        option.kind,
                        ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways
                    );
                    let button = Button::new(gpui::SharedString::from(format!(
                        "approval-option-{}",
                        option.id
                    )))
                    .chip()
                    .touch(self.compact)
                    .disabled(!self.interactive(cx) || pending)
                    .label(option.label.clone())
                    .when(rejects, |button| button.danger().outline())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.respond(
                            request_id.clone(),
                            ApprovalDecision::Option(option_id.clone()),
                            cx,
                        );
                    }));
                    options = options.child(button);
                }
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(options)
                    .child(
                        Button::new("approval-cancel")
                            .ghost()
                            .chip()
                            .touch(self.compact)
                            .disabled(!self.interactive(cx) || pending)
                            .label(crate::tr!("approval.cancel_turn"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.respond(cancel_id.clone(), ApprovalDecision::Cancel, cx);
                            })),
                    )
            })
            .into_any_element()
    }

    pub(in super::super) fn respond(
        &mut self,
        request_id: String,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        self.workspace_store.update(cx, |store, _cx| {
            store.respond_approval(request_id, decision)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ScrollDelta, ScrollWheelEvent, TestAppContext, VisualTestContext, point, size};

    /// A composer, phone or desktop, whose session awaits approval of a patch
    /// to `paths`.
    fn composer_with_patch(
        cx: &mut TestAppContext,
        compact: bool,
        paths: impl Iterator<Item = String>,
    ) -> &mut VisualTestContext {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(
                "approval-list-test",
            ))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let changes = paths
            .map(|path| agent::FileChange {
                path,
                kind: agent::FileChangeKind::Modify,
                diff: None,
            })
            .collect();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("approval-list".into(), std::env::temp_dir(), cx);
            for event in [
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn".into(),
                },
                agent::AgentEvent::ApprovalRequested(ApprovalRequest {
                    id: "patch".into(),
                    parent_item_id: None,
                    turn_id: None,
                    kind: ApprovalKind::FileChange {
                        changes,
                        reason: None,
                    },
                    options: Vec::new(),
                }),
            ] {
                state.provider_event_for_test(&id, event, cx);
            }
            (id.clone(), state.residents.live[&id].timeline.clone())
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let (_composer, cx) = cx.add_window_view(|window, cx| {
            Composer::new_with_layout(store.clone(), compact, window, cx)
        });
        cx.simulate_resize(if compact {
            size(px(393.), px(852.))
        } else {
            size(px(1280.), px(800.))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        // The first layout finds the list's width; the next frame lays out
        // rows at it.
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            _ = window.draw(cx);
        });
        cx
    }

    #[gpui::test]
    fn a_long_file_change_request_lays_out_only_the_visible_files(cx: &mut TestAppContext) {
        let cx = composer_with_patch(
            cx,
            true,
            (0..300).map(|index| format!("src/file-{index}.rs")),
        );
        let first = cx
            .debug_bounds("approval-change-0")
            .expect("first change row");
        assert!(
            cx.debug_bounds("approval-change-250").is_none(),
            "the approval card must not lay out all 300 file rows"
        );

        // Rows are measured as they come into view, so the list reaches its
        // end over successive gestures.
        for _ in 0..20 {
            cx.simulate_event(ScrollWheelEvent {
                position: first.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(-2_000.))),
                ..Default::default()
            });
            cx.update(|window, cx| _ = window.draw(cx));
        }
        assert!(cx.debug_bounds("approval-change-299").is_some());
        assert!(cx.debug_bounds("approval-change-0").is_none());
    }

    #[gpui::test]
    fn a_short_request_shows_every_wrapped_path(cx: &mut TestAppContext) {
        let cx = composer_with_patch(
            cx,
            true,
            (0..2).map(|index| format!("src/{}file-{index}.rs", "deeply/nested/".repeat(6))),
        );
        let detail = cx.debug_bounds("approval-detail").expect("detail box");
        let first = cx.debug_bounds("approval-change-0").expect("first path");
        let second = cx.debug_bounds("approval-change-1").expect("second path");
        assert!(first.size.height > px(30.), "a long path wraps on a phone");
        assert!(second.top() >= first.bottom());
        assert!(
            second.bottom() <= detail.bottom(),
            "a short list grows to show every path"
        );
    }

    #[gpui::test]
    fn a_desktop_request_opens_with_what_it_asks_to_do(cx: &mut TestAppContext) {
        let cx = composer_with_patch(cx, false, std::iter::once("src/main.rs".to_owned()));
        assert!(
            cx.debug_bounds("approval-change-0").is_some(),
            "the request's detail is shown before anyone clicks its header"
        );
    }
}
