//! The queued messages above the field. Rows drag to change the order the
//! queue sends in: by the row with a mouse, by its handle on touch, where a
//! press on the row scrolls the strip.
//!
//! gpui-base has no sortable-list primitive, so the drag composes GPUI's
//! `on_drag`, `on_drag_move` and `on_drop`, and GPUI does not cancel a drag on
//! Escape by itself.

use super::super::*;
use crate::scroll::ScrollableElement as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use gpui::{DragMoveEvent, KeystrokeEvent, MouseButton, Pixels, SharedString, WeakEntity};
#[cfg(not(target_family = "wasm"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_family = "wasm")]
use web_time::{SystemTime, UNIX_EPOCH};

/// The drag payload; the composer keeps the drag's state in [`QueueDrag`].
pub(in super::super) struct DraggedQueued {
    id: u64,
    label: SharedString,
    compact: bool,
}

pub(in super::super) struct QueueDrag {
    id: u64,
    /// The row the dragged one lands before; `None` is the end.
    before: Option<u64>,
    /// `before` as the drag began, where a drop moves nothing.
    origin: Option<u64>,
    width: Pixels,
    auto_scroll: gpui_base::AutoScroll,
    _escape: Subscription,
}

/// The lifted row that follows the pointer.
struct QueuedDragView {
    composer: WeakEntity<Composer>,
    label: SharedString,
    compact: bool,
}

impl Render for QueuedDragView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = self
            .composer
            .upgrade()
            .and_then(|composer| Some(composer.read(cx).queue_drag.as_ref()?.width))
            .unwrap_or(px(320.));
        div()
            .w(width)
            .px_1()
            .py_0p5()
            .rounded(crate::material::radius_overlay(cx))
            .bg(cx.theme().list_active)
            .shadow_lg()
            .text_body(self.compact, cx)
            .text_color(cx.theme().foreground)
            .overflow_hidden()
            .child(crate::widgets::fade_text(self.label.clone()))
    }
}

impl Composer {
    /// The queued messages the strip lists, in queue order: a message whose
    /// send is still on its way to the host shows in the timeline instead.
    fn queued_rows(
        &self,
        cx: &App,
    ) -> Option<(
        crate::store::ComposerQueue,
        Vec<tcode_protocol::QueuedMessageStatus>,
    )> {
        let mut queue = self.workspace_store.read(cx).composer_state().queue?;
        let deliveries = self.workspace_store.read(cx).delivery_messages();
        let rows = std::mem::take(&mut queue.messages)
            .into_iter()
            .filter(|message| {
                !deliveries.iter().any(|(key, _, failure, acknowledged)| {
                    !acknowledged
                        && failure.is_none()
                        && message.delivery_key.as_deref() == Some(key.as_str())
                })
            })
            .collect();
        Some((queue, rows))
    }

    fn begin_queue_drag(
        composer: WeakEntity<Self>,
        dragged: &DraggedQueued,
        cx: &mut App,
    ) -> Entity<QueuedDragView> {
        let escape = {
            let composer = composer.clone();
            cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && composer
                        .update(cx, |this, cx| {
                            let cancelled = this.queue_drag.take().is_some();
                            if cancelled {
                                cx.stop_active_drag(window);
                                cx.notify();
                            }
                            cancelled
                        })
                        .unwrap_or(false)
                {
                    cx.stop_propagation();
                }
            })
        };
        let _ = composer.update(cx, |this, cx| {
            let origin = this.queued_rows(cx).and_then(|(_, rows)| {
                let index = rows.iter().position(|row| row.id == dragged.id)?;
                rows.get(index + 1).map(|row| row.id)
            });
            this.queue_drag = Some(QueueDrag {
                id: dragged.id,
                before: origin,
                origin,
                width: px(320.),
                auto_scroll: Default::default(),
                _escape: escape,
            });
            cx.notify();
        });
        crate::haptics::threshold();
        cx.new(|_| QueuedDragView {
            composer,
            label: dragged.label.clone(),
            compact: dragged.compact,
        })
    }

    /// The pointer is over row `over`, in its lower half when `lower`. The
    /// gap counts the rows other than the dragged one, so a row sliding
    /// through the pointer never moves the gap back.
    fn queue_drag_over(&mut self, over: u64, lower: bool, width: Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.queue_drag.as_ref() else {
            return;
        };
        let mut before = drag.before;
        if over != drag.id {
            let others: Vec<u64> = self
                .queued_rows(cx)
                .map(|(_, rows)| rows)
                .unwrap_or_default()
                .into_iter()
                .map(|row| row.id)
                .filter(|id| *id != drag.id)
                .collect();
            if let Some(index) = others.iter().position(|id| *id == over) {
                before = if lower {
                    others.get(index + 1).copied()
                } else {
                    Some(over)
                };
            }
        }
        let drag = self.queue_drag.as_mut().expect("checked above");
        if drag.before != before {
            crate::haptics::selection();
        }
        if drag.before != before || drag.width != width {
            drag.before = before;
            drag.width = width;
            cx.notify();
        }
    }

    /// Scroll the strip under a drag held near its top or bottom edge.
    fn queue_drag_auto_scroll(
        &mut self,
        event: &DragMoveEvent<DraggedQueued>,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.queue_drag.as_mut() else {
            return;
        };
        let position = event.event.position;
        let delta = (event.bounds.left() <= position.x && position.x <= event.bounds.right())
            .then(|| gpui_base::AutoScroll::compute_delta(position.y, event.bounds))
            .flatten();
        drag.auto_scroll.set(delta, cx, |delta, this, cx| {
            let handle = &this.queue_scroll;
            let max = handle.max_offset().y;
            let mut offset = handle.offset();
            offset.y = (offset.y - delta).clamp(-max, px(0.));
            handle.set_offset(offset);
            cx.notify();
        });
    }

    fn drop_queued_row(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.queue_drag.take() else {
            return;
        };
        cx.notify();
        if drag.before != drag.origin {
            self.workspace_store
                .update(cx, |store, _cx| store.move_queued(drag.id, drag.before));
        }
    }

    /// Queued messages with edit, send-now and removal actions. Scheduled
    /// messages also show a live countdown.
    pub(in super::super) fn render_queue_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // A drag released outside the strip ends without a drop.
        if self.queue_drag.is_some() && !cx.has_active_drag() {
            self.queue_drag = None;
        }
        let Some((queue, mut queued)) = self.queued_rows(cx) else {
            self.scheduled_countdown_tick = None;
            return None;
        };
        let can_steer = queue.can_steer;
        let agent = queue.agent;
        let has_scheduled = queued
            .iter()
            .any(|message| message.fire_at_unix_secs.is_some());
        if has_scheduled && self.scheduled_countdown_tick.is_none() {
            self.scheduled_countdown_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        } else if !has_scheduled {
            // Dropping the task cancels its timer, so at most one repaint loop
            // exists and it stops as soon as the last scheduled row disappears.
            self.scheduled_countdown_tick = None;
        }
        if queued.is_empty() {
            return None;
        }
        let reorderable = queued.len() > 1 && self.interactive(cx);
        let dragging = self.queue_drag.as_ref().map(|drag| drag.id);
        if let Some(drag) = &self.queue_drag
            && let Some(from) = queued.iter().position(|message| message.id == drag.id)
        {
            let message = queued.remove(from);
            let to = drag
                .before
                .and_then(|before| queued.iter().position(|message| message.id == before))
                .unwrap_or(queued.len());
            queued.insert(to, message);
        }

        let mut strip = v_flex().w_full().gap_1();
        for (index, message) in queued.into_iter().enumerate() {
            let id = message.id;
            let editable = message.editable;
            let text = message.text.clone();
            let label: SharedString = crate::chat::model::one_line(
                &tcode_core::notice::host_notice(&message.text).map_or_else(
                    || message.text.clone(),
                    |notice| crate::chat::components::disclosure::notice_label(&notice),
                ),
            )
            .into();
            let dragged = (reorderable && editable).then(|| DraggedQueued {
                id,
                label: label.clone(),
                compact: self.compact,
            });
            let scheduled = message.fire_at_unix_secs.is_some();
            let steer_tooltip = if scheduled {
                crate::tr!("composer.send_now").into_owned()
            } else if can_steer {
                crate::tr!("composer.steer_queued").into_owned()
            } else {
                crate::tr!("composer.steer_unsupported_tooltip", agent = agent).into_owned()
            };
            let countdown = message.fire_at_unix_secs.map(|fire_at| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                crate::time::format_countdown(fire_at.saturating_sub(now))
            });
            let row = h_flex()
                .id(("queued-row", id as usize))
                .debug_selector(move || format!("queued-row-{id}"))
                .flex_none()
                .w_full()
                .gap_1()
                .items_center()
                .px_1()
                .when(index > 0, |row| {
                    row.pt_1().border_t_1().border_color(cx.theme().border)
                })
                .when(dragging == Some(id), |row| row.opacity(0.4))
                .on_drag_move::<DraggedQueued>(cx.listener(
                    move |this, event: &DragMoveEvent<DraggedQueued>, _, cx| {
                        if event.bounds.contains(&event.event.position) {
                            let lower = event.event.position.y > event.bounds.center().y;
                            this.queue_drag_over(id, lower, event.bounds.size.width, cx);
                        }
                    },
                ));
            let composer = cx.entity().downgrade();
            let begin = move |dragged: &DraggedQueued, _, _: &mut Window, cx: &mut App| {
                Self::begin_queue_drag(composer.clone(), dragged, cx)
            };
            let (row, grip) = match dragged {
                Some(dragged) if self.compact => (
                    row,
                    Some(
                        div()
                            .id(("queue-grip", id as usize))
                            .debug_selector(move || format!("queue-grip-{id}"))
                            .role(Role::Button)
                            .aria_label(crate::tr!("composer.reorder_queued"))
                            .flex_none()
                            .size(px(crate::material::TOUCH_TARGET))
                            .flex()
                            .items_center()
                            .justify_center()
                            // The handle alone starts a drag, so a press must
                            // not reach the row.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Icon::new(IconName::GripVertical)
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_drag(dragged, begin),
                    ),
                ),
                Some(dragged) => (row.cursor_grab().on_drag(dragged, begin), None),
                None => (row, None),
            };
            strip = strip.child(
                row.children(grip)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(self.compact, cx)
                            .text_color(cx.theme().foreground)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(label)),
                    )
                    .when_some(countdown, |row, countdown| {
                        row.child(
                            div()
                                .flex_none()
                                .text_secondary(self.compact, cx)
                                .child(countdown),
                        )
                    })
                    .child(
                        Button::new(("queue-steer", id as usize))
                            .debug_selector(move || format!("queue-steer-{id}"))
                            .ghost()
                            .xsmall()
                            .touch(self.compact)
                            .icon(IconName::ArrowUp)
                            // Scheduled rows always support send-now: the
                            // runtime removes the deadline and uses the normal
                            // send/queue path even when native steering is absent.
                            .disabled(
                                !editable || !self.interactive(cx) || (!scheduled && !can_steer),
                            )
                            .tooltip(steer_tooltip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.workspace_store
                                    .update(cx, |store, _cx| store.steer_queued(id));
                            })),
                    )
                    .child(
                        Button::new(("queue-drop", id as usize))
                            .debug_selector(move || format!("queue-drop-{id}"))
                            .ghost()
                            .xsmall()
                            .touch(self.compact)
                            .icon(IconName::Close)
                            .disabled(!editable || !self.interactive(cx))
                            .tooltip(crate::tr!("composer.drop_queued"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.drop_queued_and_refill(id, text.clone(), window, cx);
                            })),
                    )
                    .context_menu({
                        let text = message.text.clone();
                        let interactive = editable && self.interactive(cx);
                        move |menu, _, _| {
                            menu.menu(
                                crate::tr!("chat.copy_text").into_owned(),
                                Box::new(CopyText(text.clone())),
                            )
                            .separator()
                            .menu_with_enable(
                                if scheduled {
                                    crate::tr!("composer.send_now").into_owned()
                                } else {
                                    crate::tr!("composer.steer_queued").into_owned()
                                },
                                Box::new(ComposerMenu::QueueSteer(id)),
                                interactive && (scheduled || can_steer),
                            )
                            .menu_with_enable(
                                crate::tr!("composer.edit_queued").into_owned(),
                                Box::new(ComposerMenu::QueueEdit {
                                    id,
                                    text: text.clone(),
                                }),
                                interactive,
                            )
                            .menu_with_enable(
                                crate::tr!("composer.drop_queued").into_owned(),
                                Box::new(ComposerMenu::QueueDrop(id)),
                                interactive,
                            )
                        }
                    }),
            );
        }
        Some(
            div()
                .id("queued-messages-scroll")
                .w_full()
                // A few queued rows before the strip scrolls, so the field
                // under it stays in reach; no owner sizes a composer strip.
                .max_h(px(180.))
                .on_drop(cx.listener(|this, _: &DraggedQueued, _, cx| this.drop_queued_row(cx)))
                .on_drag_move(
                    cx.listener(|this, event: &DragMoveEvent<DraggedQueued>, _, cx| {
                        this.queue_drag_auto_scroll(event, cx)
                    }),
                )
                .overflow_y_scroll_area()
                .track_scroll(&self.queue_scroll)
                .child(strip)
                .into_any_element(),
        )
    }
}
